//! The verification ladder (V2): prove the analysis, then prove the guard.
//!
//! * **T1 — instance**: does the failure still reproduce on demand? Runs the
//!   user's `--test` command at HEAD and re-parses its output against the
//!   analyzed failure.
//! * **T2 — citations**: does every `file:line` / `commit <sha>` citation in
//!   the chain still resolve in this repository?
//! * **T3 — mutation**: does the generated guard discriminate? It must *fail*
//!   at the current (buggy) revision and *pass* at a known-good one — the
//!   counterfactual is checked in a temporary worktree, never by moving the
//!   user's HEAD.
//!
//! Tier aggregation is a pure function of what ran:
//!
//! * `CONFIRMED` — all three stages ran and all passed.
//! * `LIKELY` — every stage that ran passed, but the full ladder did not run.
//! * `POSSIBLE` — nothing ran, or at least one stage that ran did not verify.
//!
//! No stage ever claims more than it observed: a stage that could not run is
//! absent (`None` + a note), never reported as a pass.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::guard::Guard;
use crate::normalize::NormalizedError;
use crate::reason::{Confidence, Evidence, Tier};
use crate::report::Report;

/// Inputs to the ladder. Both stages that need input degrade to absent
/// (`None` + note) when it is missing.
#[derive(Debug, Default, Clone, Copy)]
pub struct Options<'a> {
    /// Command that reproduces the failure (enables T1; becomes T3's check).
    pub test_cmd: Option<&'a str>,
    /// Known-good revision for T3's counterfactual. Auto-detected when unset
    /// (requires a clean tree).
    pub good: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Verification {
    pub tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t1_instance: Option<T1Instance>,
    pub t2_citations: T2Citations,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t3_mutation: Option<T3Mutation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct T1Instance {
    pub command: String,
    pub exit_code: i32,
    pub expected_fingerprint: String,
    pub observed_fingerprint: String,
    pub reproduced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct T2Citations {
    pub checked: usize,
    pub resolved: usize,
    pub unresolved: Vec<String>,
}

impl T2Citations {
    /// A citation check only counts when there was something to check.
    pub fn passed(&self) -> bool {
        self.checked > 0 && self.unresolved.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct T3Mutation {
    pub check: String,
    pub head_fails: bool,
    pub good: Option<String>,
    pub good_passes: Option<bool>,
    pub mutation_checked: bool,
}

/// Run every ladder stage the inputs allow and return the verification record.
///
/// Side effects are limited to: running the check commands, a temporary
/// worktree (removed on all paths), and — only when the tree started clean —
/// removing untracked artifacts the runs created. Tracked files are never
/// reverted.
pub fn run(
    err: &NormalizedError,
    root: &Path,
    report: &mut Report,
    guards: &[Guard],
    opts: &Options,
) -> Verification {
    let mut notes: Vec<String> = Vec::new();
    let clean_before = tree_clean(root);

    // T2 first: it validates the citations as the chain stands now, before
    // the ladder adds its own guard evidence.
    let t2 = check_cites(&report.analysis, root);

    let t1 = opts
        .test_cmd
        .map(|cmd| run_t1(cmd, root, err, &report.observed.fingerprint));

    // T3's check: the user's reproduction command when given (ground truth),
    // else the generated guard.
    let check: Option<&str> = opts
        .test_cmd
        .or_else(|| guards.first().map(|g| g.check.as_str()));

    let mut t3 = None;
    if let Some(check) = check {
        let head_fails = match &t1 {
            // Same command by construction — reuse T1's run, don't run twice.
            Some(t) => t.exit_code != 0,
            None => run_check(root, check).0 != 0,
        };

        match resolve_good(root, opts, check, clean_before) {
            GoodOutcome::Ref(good) => match check_at_good(root, &good, check) {
                Ok(passes) => {
                    let mutation_checked = head_fails && passes;
                    if !mutation_checked {
                        notes.push(if !head_fails {
                            format!(
                                "t3: `{check}` does not fail at HEAD — nothing to detect; \
                                 the bug may already be fixed"
                            )
                        } else {
                            format!(
                                "t3: `{check}` also fails at {good} — the good endpoint is \
                                 not a valid counterfactual; pass an older --good <ref>"
                            )
                        });
                    }
                    t3 = Some(T3Mutation {
                        check: check.to_string(),
                        head_fails,
                        good: Some(good),
                        good_passes: Some(passes),
                        mutation_checked,
                    });
                }
                Err(e) => {
                    notes.push(format!("t3 inconclusive: could not check at good ref: {e}"));
                }
            },
            GoodOutcome::SameAsHead => {
                notes.push("t3 inconclusive: --good resolves to HEAD — no counterfactual".into());
                t3 = Some(T3Mutation {
                    check: check.to_string(),
                    head_fails,
                    good: opts.good.map(str::to_string),
                    good_passes: None,
                    mutation_checked: false,
                });
            }
            GoodOutcome::Unavailable(reason) => notes.push(reason),
        }
    } else {
        notes.push(
            "t3 skipped: no --test command and no guard could be generated for this failure".into(),
        );
    }
    if t1.is_none() {
        notes.push("t1 skipped: pass --test <command> to verify the instance".into());
    }

    // The ladder's own evidence goes on the prevention step; it graduates
    // from hypothesis to observed only when T3 proved the guard flips.
    if let Some(g) = guards.first()
        && let Some(step) = report
            .analysis
            .steps
            .iter_mut()
            .find(|s| s.level == "prevention")
    {
        let mutation_checked = t3.as_ref().is_some_and(|t| t.mutation_checked);
        step.evidence.push(Evidence {
            cite: format!("guard: {}", g.check),
            note: if mutation_checked {
                format!(
                    "generated guard: fails at HEAD, passes at {}",
                    t3.as_ref()
                        .and_then(|t| t.good.as_deref())
                        .unwrap_or("good")
                )
            } else {
                "generated guard (mutation check inconclusive)".into()
            },
        });
        if mutation_checked {
            step.confidence = Confidence::Observed;
        }
    }

    // Only remove artifacts our own runs may have dropped, and only if the
    // tree was clean when we started — never revert tracked changes.
    if clean_before {
        let _ = Command::new("git")
            .args(["clean", "-qfd"])
            .current_dir(root)
            .output();
    }

    Verification {
        tier: aggregate(t1.as_ref(), &t2, t3.as_ref()),
        t1_instance: t1,
        t2_citations: t2,
        t3_mutation: t3,
        notes,
    }
}

/// Pure tier rule: confirmed ⇔ the full ladder ran and every stage passed.
pub fn aggregate(t1: Option<&T1Instance>, t2: &T2Citations, t3: Option<&T3Mutation>) -> Tier {
    let mut stages: Vec<bool> = Vec::new();
    if let Some(t) = t1 {
        stages.push(t.reproduced);
    }
    if t2.checked > 0 {
        stages.push(t2.unresolved.is_empty());
    }
    if let Some(t) = t3 {
        stages.push(t.mutation_checked);
    }
    if stages.len() == 3 && stages.iter().all(|&s| s) {
        Tier::Confirmed
    } else if !stages.is_empty() && stages.iter().all(|&s| s) {
        Tier::Likely
    } else {
        Tier::Possible
    }
}

fn run_t1(cmd: &str, root: &Path, err: &NormalizedError, expected_fp: &str) -> T1Instance {
    let (exit_code, stdout, stderr) = run_check(root, cmd);
    let combined = format!("{stdout}\n{stderr}");
    let observed = crate::normalize::parse(&combined);
    let observed_fp = crate::normalize::fingerprint::fingerprint(&observed);

    // "Reproduced" means the *same failure*, not merely a non-zero exit:
    // kinds must agree, and symbols must agree whenever both sides have one
    // (a missing symbol on either side carries no information).
    let reproduced = exit_code != 0
        && observed.kind == err.kind
        && (err.symbol.is_none() || observed.symbol.is_none() || err.symbol == observed.symbol);

    T1Instance {
        command: cmd.to_string(),
        exit_code,
        expected_fingerprint: expected_fp.to_string(),
        observed_fingerprint: observed_fp,
        reproduced,
    }
}

/// Resolve every checkable citation in the chain against the repository.
///
/// Checkable: `file:line[:col]` (file exists, line in range) and
/// `commit <sha>` (object exists). Purely symbolic cites (`N sites`,
/// `class <kind>`, `guard: …`) are skipped, not counted.
pub fn check_cites(analysis: &crate::reason::Chain, root: &Path) -> T2Citations {
    let mut checked = 0usize;
    let mut resolved = 0usize;
    let mut unresolved: Vec<String> = Vec::new();

    for step in &analysis.steps {
        for ev in &step.evidence {
            if let Some(sha) = ev.cite.strip_prefix("commit ") {
                checked += 1;
                if git_ok(
                    root,
                    &["cat-file", "-e", &format!("{}^{{commit}}", sha.trim())],
                ) {
                    resolved += 1;
                } else {
                    unresolved.push(ev.cite.clone());
                }
            } else if let Some((file, line)) = parse_file_cite(&ev.cite) {
                checked += 1;
                if cite_resolves(root, file, line) {
                    resolved += 1;
                } else {
                    unresolved.push(ev.cite.clone());
                }
            }
        }
    }
    T2Citations {
        checked,
        resolved,
        unresolved,
    }
}

/// `path/to/file.rs:12:5` → `(path, Some(12))`.
///
/// Requires the first segment to look like a path (`.` or `/`) so symbolic
/// cites such as `guard: cargo check` or `class runtime` are skipped, and the
/// line segment to be a number so `C:\…` style oddities don't match.
fn parse_file_cite(cite: &str) -> Option<(&str, Option<u32>)> {
    let (head, rest) = cite.split_once(':')?;
    if !(head.contains('.') || head.contains('/')) {
        return None;
    }
    let line = rest.split(':').next()?.trim().parse::<u32>().ok();
    Some((head, line))
}

fn cite_resolves(root: &Path, file: &str, line: Option<u32>) -> bool {
    let path = if Path::new(file).is_absolute() {
        std::path::PathBuf::from(file)
    } else {
        root.join(file)
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    match line {
        Some(n) => content.lines().count() as u64 >= u64::from(n),
        None => true,
    }
}

enum GoodOutcome {
    Ref(String),
    SameAsHead,
    Unavailable(String),
}

fn resolve_good(root: &Path, opts: &Options, check: &str, clean: bool) -> GoodOutcome {
    match opts.good {
        Some(g) => match resolve(root, g) {
            Some(good_sha) => match resolve(root, "HEAD") {
                Some(head) if head == good_sha => GoodOutcome::SameAsHead,
                _ => GoodOutcome::Ref(g.to_string()),
            },
            None => GoodOutcome::Unavailable(format!("t3 inconclusive: cannot resolve --good {g}")),
        },
        None => {
            if !clean {
                return GoodOutcome::Unavailable(
                    "t3 inconclusive: auto-detecting a good endpoint needs a clean \
                     worktree — pass --good <ref>"
                        .into(),
                );
            }
            match crate::gitintel::bisect::detect_good(root, check) {
                Ok(g) => GoodOutcome::Ref(g),
                Err(e) => GoodOutcome::Unavailable(format!(
                    "t3 inconclusive: no good endpoint found: {e}"
                )),
            }
        }
    }
}

/// Run `check` at `good` inside a throwaway linked worktree.
///
/// The user's HEAD and index are never touched; the worktree is force-removed
/// on every path.
fn check_at_good(root: &Path, good: &str, check: &str) -> anyhow::Result<bool> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let wt = std::env::temp_dir().join(format!(
        "rootguard-verify-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&wt);

    let add = Command::new("git")
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(good)
        .current_dir(root)
        .output()?;
    if !add.status.success() {
        return Err(anyhow::anyhow!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&add.stderr).trim()
        ));
    }

    let (code, _, _) = run_check(&wt, check);

    let _ = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt)
        .current_dir(root)
        .output();
    let _ = Command::new("git")
        .arg("worktree")
        .arg("prune")
        .current_dir(root)
        .output();
    let _ = std::fs::remove_dir_all(&wt);

    Ok(code == 0)
}

/// Run a check command fully captured (no teeing — verification must not
/// spam the report with the child's output).
fn run_check(dir: &Path, cmd: &str) -> (i32, String, String) {
    match Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
    {
        Ok(o) => (
            o.status.code().unwrap_or(1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (127, String::new(), format!("failed to run: {e}")),
    }
}

fn tree_clean(root: &Path) -> bool {
    git_ok(root, &["rev-parse", "--is-inside-work-tree"])
        && git_capture(root, &["status", "--porcelain"]).is_some_and(|s| s.trim().is_empty())
}

fn resolve(root: &Path, ref_: &str) -> Option<String> {
    git_capture(root, &["rev-parse", "--verify", "--quiet", ref_])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn git_ok(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git_capture(root: &Path, args: &[&str]) -> Option<String> {
    let o = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t1(reproduced: bool) -> T1Instance {
        T1Instance {
            command: "cmd".into(),
            exit_code: 1,
            expected_fingerprint: "f".into(),
            observed_fingerprint: "f".into(),
            reproduced,
        }
    }

    fn t2(checked: usize, unresolved: &[&str]) -> T2Citations {
        T2Citations {
            checked,
            resolved: checked - unresolved.len(),
            unresolved: unresolved.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn t3(mutation_checked: bool) -> T3Mutation {
        T3Mutation {
            check: "cmd".into(),
            head_fails: true,
            good: Some("good".into()),
            good_passes: Some(true),
            mutation_checked,
        }
    }

    #[test]
    fn full_ladder_passing_is_confirmed() {
        assert_eq!(
            aggregate(Some(&t1(true)), &t2(3, &[]), Some(&t3(true))),
            Tier::Confirmed
        );
    }

    #[test]
    fn partial_ladder_all_passing_is_likely() {
        assert_eq!(
            aggregate(None, &t2(3, &[]), Some(&t3(true))),
            Tier::Likely,
            "no T1 → at best likely"
        );
        assert_eq!(
            aggregate(Some(&t1(true)), &t2(3, &[]), None),
            Tier::Likely,
            "inconclusive T3 → at best likely"
        );
    }

    #[test]
    fn any_failed_or_nothing_run_is_possible() {
        assert_eq!(
            aggregate(Some(&t1(false)), &t2(3, &[]), Some(&t3(true))),
            Tier::Possible,
            "T1 did not reproduce"
        );
        assert_eq!(
            aggregate(Some(&t1(true)), &t2(3, &["src/gone.rs:9"]), None),
            Tier::Possible,
            "stale citation"
        );
        assert_eq!(
            aggregate(None, &t2(0, &[]), None),
            Tier::Possible,
            "nothing ran"
        );
        assert_eq!(
            aggregate(Some(&t1(true)), &t2(3, &[]), Some(&t3(false))),
            Tier::Possible,
            "guard does not discriminate"
        );
    }

    #[test]
    fn vacuous_citation_check_does_not_pass() {
        assert!(!t2(0, &[]).passed());
        assert!(t2(1, &[]).passed());
        assert!(!t2(2, &["x"]).passed());
    }

    #[test]
    fn file_cite_parsing_skips_symbolic_cites() {
        assert_eq!(
            parse_file_cite("src/db.rs:42:9"),
            Some(("src/db.rs", Some(42)))
        );
        assert_eq!(
            parse_file_cite("/abs/path/app.py:2"),
            Some(("/abs/path/app.py", Some(2)))
        );
        assert_eq!(parse_file_cite("guard: cargo check --locked"), None);
        assert_eq!(parse_file_cite("class runtime"), None);
        assert_eq!(parse_file_cite("commit abc1234"), None);
        assert_eq!(parse_file_cite("7 sites"), None);
        assert_eq!(parse_file_cite("C:\\repo\\src\\main.rs:4"), None);
    }

    #[test]
    fn citation_resolution_against_real_files() {
        let dir = std::env::temp_dir().join(format!("rootguard-t2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();

        assert!(cite_resolves(&dir, "a.rs", Some(2)));
        assert!(!cite_resolves(&dir, "a.rs", Some(3)), "line past EOF");
        assert!(cite_resolves(&dir, "a.rs", None));
        assert!(!cite_resolves(&dir, "missing.rs", Some(1)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
