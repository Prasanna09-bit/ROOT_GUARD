use std::path::Path;
use std::process::Command;
use std::sync::LazyLock;

use super::Commit;

/// `<sha> is the first '<term>' commit` — the line `git bisect` prints once it
/// has narrowed to a single commit.
///
/// git ≤ 2.54 emits it unquoted (`bisect.c:1116` prints
/// `"%s is the first %s commit\n"`); git ≥ 2.55 quotes the term so translators
/// may reorder it (`"%s is the first '%s' commit\n"`). The `printf` is *not*
/// wrapped in `_()`, so the surrounding words are byte-identical in every
/// locale — only the quotes differ. The term itself is renameable with
/// `--term-bad` (`bad`, `new`, …), so match any run of non-quote characters
/// instead of the literal `bad`.
///
/// Anchored at column 0 because `show_commit()` prints the winning commit's
/// subject indented — otherwise a commit message could echo this text back and
/// win the match. Only full object IDs are accepted (40 hex for SHA-1, 64 for
/// SHA-256): `oid_to_hex()` never abbreviates, and full length keeps words like
/// `deadbee` from being mistaken for a SHA.
static FIRST_BAD_LINE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?m)^([0-9a-fA-F]{64}|[0-9a-fA-F]{40}) is the first '?[^'\r\n]+?'? commit[ \t]*\r?$",
    )
    .expect("FIRST_BAD_LINE is a valid regex")
});

/// `# first '<term>' commit: [<sha>] <subject>` — the same fact recorded in
/// `$GIT_DIR/BISECT_LOG` by a literal (non-`_()`) format string
/// (`builtin/bisect.c:762`), so it survives locale and quoting changes alike.
/// Appended only once the first bad commit is known, which makes it a safe
/// fallback even when `git bisect run` exits non-zero.
static FIRST_BAD_LOG_LINE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?m)^# first '?[^'\r\n]+?'? commit: \[([0-9a-fA-F]{64}|[0-9a-fA-F]{40})\]")
        .expect("FIRST_BAD_LOG_LINE is a valid regex")
});

#[derive(Debug, Clone)]
pub struct BisectResult {
    pub culprit: Option<Commit>,
    pub tested: usize,
    pub skipped: usize,
    pub log: Vec<String>,
    pub auto_good: Option<String>,
    pub auto_bad: Option<String>,
}

/// Automated `git bisect run`.
///
/// * `good` defaults to the merge-base with `origin/main` (or `main`, then `master`).
/// * `bad` defaults to `HEAD`.
/// * Restores the original HEAD on every exit path.
pub fn run(
    root: &Path,
    good: Option<&str>,
    bad: Option<&str>,
    test_cmd: &str,
) -> anyhow::Result<BisectResult> {
    ensure_clean(root)?;
    ensure_repo(root)?;

    let orig_head = capture(root, &["rev-parse", "HEAD"])?.trim().to_string();

    let mut auto_good: Option<String> = None;
    let good_ref = match good {
        Some(g) => g.to_string(),
        None => {
            let g = detect_good(root, test_cmd)?;
            auto_good = Some(g.clone());
            g
        }
    };
    let auto_bad = bad.map(|s| s.to_string());
    let bad_ref = bad.unwrap_or("HEAD").to_string();

    if same_ref(root, &good_ref, &bad_ref) {
        anyhow::bail!(
            "good ({good_ref}) and bad ({bad_ref}) resolve to the same commit — \
             nothing to bisect"
        );
    }

    // Probe both endpoints before starting: fail early with a clear message.
    // probe() == true means the test FAILS at that revision.
    if !probe(root, test_cmd) {
        scrub(root);
        restore(root, &orig_head)?;
        anyhow::bail!(
            "test does not fail at {bad_ref} (it passed) — your --test command is wrong \
             or the bug is already fixed"
        );
    }
    scrub(root);
    checkout(root, &good_ref)?;
    let good_fails = probe(root, test_cmd);
    scrub(root);
    restore(root, &orig_head)?;
    if good_fails {
        anyhow::bail!(
            "test also fails at {good_ref} — bisect would have no good endpoint; \
             pass an older --good <ref>"
        );
    }

    let start = Command::new("git")
        .args(["bisect", "start", &bad_ref, &good_ref])
        .current_dir(root)
        .output()?;
    if !start.status.success() {
        restore(root, &orig_head)?;
        anyhow::bail!(
            "git bisect start failed: {}",
            String::from_utf8_lossy(&start.stderr).trim()
        );
    }

    // `git bisect run` checks out revisions itself, so every test invocation
    // must leave the tree clean or the next checkout aborts. Wrap the user's
    // command: run it, capture the status, scrub artifacts, return the status.
    let wrapped = format!(
        "export PYTHONDONTWRITEBYTECODE=1; {test_cmd}; rc=$?; \
         git checkout --quiet -- . >/dev/null 2>&1; \
         git clean -qfd >/dev/null 2>&1; exit $rc"
    );
    let run = Command::new("git")
        .arg("bisect")
        .arg("run")
        .arg("sh")
        .arg("-c")
        .arg(&wrapped)
        .current_dir(root)
        .output();

    let mut log: Vec<String> = String::from_utf8_lossy(&start.stdout)
        .lines()
        .map(str::to_string)
        .chain(
            String::from_utf8_lossy(&start.stderr)
                .lines()
                .map(str::to_string),
        )
        .collect();
    let mut culprit = None;
    let mut tested = 0usize;
    let mut skipped = 0usize;

    match run {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);

            // Walk each stream on its own: concatenating stdout and stderr
            // without a separator can glue the last stdout line onto the first
            // stderr line, hiding both from every parser below.
            for l in stdout.lines().chain(stderr.lines()) {
                log.push(l.to_string());
                // Each tested revision prints the command being run, then a
                // `Bisecting: …` line for the *next* selection.
                if l.starts_with("running '") {
                    tested += 1;
                }
                if l.contains("uncommit changes") || l.contains("cannot skip") {
                    skipped += 1;
                }
            }
            // Success output ends with `<sha> is the first '<term>' commit`
            // (unquoted before git 2.55). If that line is unreadable, fall
            // back to the structured metadata git keeps for itself.
            let mut sha =
                parse_first_bad_commit(&stdout).or_else(|| parse_first_bad_commit(&stderr));
            if sha.is_none() {
                sha = structured_first_bad(root, out.status.success());
            }
            if let Some(sha) = sha {
                match commit_detail(root, &sha) {
                    Some(c) => culprit = Some(c),
                    None => log.push(format!("first bad commit {sha} could not be resolved")),
                }
            } else if out.status.success() {
                log.push("bisect finished but no first-bad-commit line found".into());
            } else {
                // Some histories (merges, skips) make `run` exit non-zero; still
                // try to read current bisect HEAD.
                if let Ok(head) = capture(root, &["bisect", "view", "--name-only"]) {
                    log.push(format!("bisect view: {}", head.trim()));
                }
            }
        }
        Err(e) => {
            log.push(format!("failed to launch git bisect run: {e}"));
        }
    }

    // Always tear down bisect state and restore HEAD.
    let _ = Command::new("git")
        .args(["bisect", "reset"])
        .current_dir(root)
        .output();
    restore(root, &orig_head)?;

    if culprit.is_none() {
        anyhow::bail!(
            "bisect did not identify a first bad commit.\nlog:\n{}",
            log.join("\n")
        );
    }

    Ok(BisectResult {
        culprit,
        tested,
        skipped,
        log,
        auto_good: auto_good.or_else(|| good.map(|s| s.to_string())),
        auto_bad,
    })
}

/// Pull the first-bad SHA out of `git bisect run` output.
///
/// Accepts both the quoted (git ≥ 2.55) and unquoted (≤ 2.54) forms, any
/// `--term-bad` vocabulary, trailing whitespace and CRLF line endings.
/// Returns `None` when nothing matches — callers must treat that as "keep
/// looking", never as an empty SHA.
fn parse_first_bad_commit(output: &str) -> Option<String> {
    let sha = FIRST_BAD_LINE.captures(output)?.get(1)?.as_str();
    is_full_oid(sha).then(|| sha.to_string())
}

/// Structured fallbacks for the first-bad commit, used when the human-readable
/// line can't be parsed (format drift, unexpected locale, glued streams).
///
/// 1. `$GIT_DIR/BISECT_LOG` — `# first '<term>' commit: [<sha>] <subject>`,
///    written only once the answer is known.
/// 2. `refs/bisect/bad` — advanced to the first bad commit when `git bisect run`
///    succeeds. While a session is still open it merely holds the *current*
///    candidate, so it is trusted only on a zero exit status.
fn structured_first_bad(root: &Path, run_succeeded: bool) -> Option<String> {
    if let Some(sha) = first_bad_from_bisect_log(root) {
        return Some(sha);
    }
    if !run_succeeded {
        return None;
    }
    let raw = capture(
        root,
        &["rev-parse", "--verify", "--quiet", "refs/bisect/bad"],
    )
    .ok()?;
    let sha = raw.trim();
    is_full_oid(sha).then(|| sha.to_string())
}

/// Read `# first '<term>' commit: [<sha>]` out of `$GIT_DIR/BISECT_LOG`.
fn first_bad_from_bisect_log(root: &Path) -> Option<String> {
    let path = capture(root, &["rev-parse", "--git-path", "BISECT_LOG"]).ok()?;
    let text = std::fs::read_to_string(root.join(path.trim())).ok()?;
    first_bad_from_log(&text)
}

/// Same extraction, against an already-read `BISECT_LOG` body.
fn first_bad_from_log(text: &str) -> Option<String> {
    let sha = FIRST_BAD_LOG_LINE.captures(text)?.get(1)?.as_str();
    is_full_oid(sha).then(|| sha.to_string())
}

/// A complete object ID: 40 hex chars for SHA-1, 64 for SHA-256.
fn is_full_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn ensure_repo(root: &Path) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        anyhow::bail!("{} is not a git repository", root.display());
    }
    Ok(())
}

fn ensure_clean(root: &Path) -> anyhow::Result<()> {
    let out = capture(root, &["status", "--porcelain"])?;
    if !out.trim().is_empty() {
        anyhow::bail!(
            "working tree is dirty — commit or stash first:\n{}",
            out.trim()
        );
    }
    Ok(())
}

/// Find a commit where `test_cmd` PASSES.
///
/// Strategy, cheapest first:
///  1. merge-base with origin/main | main | master (skipped if == HEAD)
///  2. exponential backwards walk: HEAD~1, HEAD~2, HEAD~4, HEAD~8 …
///
/// Every candidate is *verified* by actually running the test there — a
/// "good" endpoint we never checked is how bisect silently returns nonsense.
fn detect_good(root: &Path, test_cmd: &str) -> anyhow::Result<String> {
    let head = capture(root, &["rev-parse", "HEAD"])?.trim().to_string();

    let mut candidates: Vec<String> = Vec::new();
    for candidate in [
        "origin/main",
        "main",
        "origin/master",
        "master",
        "origin/HEAD",
    ] {
        if let Ok(mb) = capture(root, &["merge-base", "HEAD", candidate]) {
            let mb = mb.trim().to_string();
            let same = capture(root, &["rev-parse", &mb])
                .map(|c| c.trim() == head)
                .unwrap_or(true);
            if !mb.is_empty() && !same {
                candidates.push(mb);
            }
        }
    }
    let mut step: u32 = 1;
    while step <= 1024 {
        let ref_ = format!("HEAD~{step}");
        if capture(root, &["rev-parse", &ref_]).is_ok() {
            candidates.push(ref_);
        } else {
            break;
        }
        step *= 2;
    }

    for c in &candidates {
        scrub(root);
        if checkout(root, c).is_err() {
            continue;
        }
        let passes = !probe(root, test_cmd); // probe()==true means the test fails
        scrub(root);
        restore(root, &head)?;
        if passes {
            return Ok(c.clone());
        }
    }

    anyhow::bail!(
        "could not find any ancestor where `--test` passes — the failure may predate \
         available history, or the test is broken everywhere; pass --good <ref> explicitly"
    )
}

fn same_ref(root: &Path, a: &str, b: &str) -> bool {
    match (
        capture(root, &["rev-parse", a]),
        capture(root, &["rev-parse", b]),
    ) {
        (Ok(x), Ok(y)) => x.trim() == y.trim(),
        _ => false,
    }
}

/// Returns true when the test command FAILS (i.e. "bad") at this revision.
fn probe(root: &Path, test_cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(test_cmd)
        .current_dir(root)
        .env("PYTHONDONTWRITEBYTECODE", "1") // don't let probes drop __pycache__
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(false)
}

/// Remove artifacts a probe may have left behind so the next checkout works.
///
/// Safe because `run()` requires a fully clean tree up front — any tracked
/// modification or untracked file present now was produced by our own probe.
fn scrub(root: &Path) {
    let _ = Command::new("git")
        .args(["checkout", "--quiet", "--", "."])
        .current_dir(root)
        .output();
    let _ = Command::new("git")
        .args(["clean", "-qfd"])
        .current_dir(root)
        .output();
}

fn restore(root: &Path, head: &str) -> anyhow::Result<()> {
    scrub(root);
    let _ = Command::new("git")
        .args(["bisect", "reset"])
        .current_dir(root)
        .output();
    scrub(root);
    checkout(root, head)
}

fn checkout(root: &Path, ref_: &str) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(["checkout", "--quiet", ref_])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "failed to checkout {ref_}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn commit_detail(root: &Path, sha: &str) -> Option<Commit> {
    let out = capture(
        root,
        &["show", "-s", "--pretty=format:%H|%h|%an|%s|%aI", sha],
    )
    .ok()?;
    super::parse_commits(&out).into_iter().next()
}

fn capture(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| anyhow::anyhow!("git failed: {e}"))?;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::{first_bad_from_log, is_full_oid, parse_first_bad_commit};

    const SHA: &str = "7ca4e2bc068b86cc4b741933f77d0adf421e7863";
    const SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    /// Realistic `git bisect run` stdout: probe noise, the winning line, then
    /// `show_commit()` output whose *indented* subject echoes the same words.
    fn stdout_block(first_bad_line: &str) -> String {
        let picked = format!("[{SHA}] commit 6");
        let subject = format!("    Subject echoing: {first_bad_line}");
        let shown = format!("commit {SHA}");
        [
            "running 'sh' '-c' 'test'",
            "Bisecting: 1 revision left to test after this (roughly 1 step)",
            picked.as_str(),
            "running 'sh' '-c' 'test'",
            first_bad_line,
            shown.as_str(),
            "    Author: t <t@t.io>",
            subject.as_str(),
            "bisect found first bad commit",
        ]
        .join("\n")
    }

    #[test]
    fn parses_unquoted_output_from_git_up_to_2_54() {
        let out = stdout_block(&format!("{SHA} is the first bad commit"));
        assert_eq!(parse_first_bad_commit(&out).as_deref(), Some(SHA));
    }

    #[test]
    fn parses_quoted_output_from_git_2_55_onwards() {
        let out = stdout_block(&format!("{SHA} is the first 'bad' commit"));
        assert_eq!(parse_first_bad_commit(&out).as_deref(), Some(SHA));
    }

    #[test]
    fn parses_final_line_without_trailing_newline() {
        let out = format!("{SHA} is the first 'bad' commit");
        assert_eq!(parse_first_bad_commit(&out).as_deref(), Some(SHA));
    }

    #[test]
    fn tolerates_crlf_line_endings() {
        let out = format!(
            "running 'sh' '-c' 'test'\r\n{SHA} is the first 'bad' commit\r\ncommit {SHA}\r\n"
        );
        assert_eq!(parse_first_bad_commit(&out).as_deref(), Some(SHA));
    }

    #[test]
    fn matches_any_term_bad_new_or_renamed() {
        for line in [
            format!("{SHA} is the first new commit"),
            format!("{SHA} is the first 'new' commit"),
            format!("{SHA} is the first bad commit"),
            format!("{SHA} is the first 'bad' commit"),
        ] {
            assert_eq!(
                parse_first_bad_commit(&line).as_deref(),
                Some(SHA),
                "line: {line}"
            );
        }
    }

    #[test]
    fn ignores_red_herring_lines() {
        let partial = &SHA[..39];
        for line in [
            String::new(),
            "bisect found first bad commit".to_string(),
            "bisect found first 'bad' commit".to_string(),
            format!("commit {SHA}"),
            // show_commit()'s indented subject — a commit message could echo
            // the exact winning words back at us.
            format!("    {SHA} is the first bad commit"),
            format!("# possible first 'bad' commit: [{SHA}] older commit"),
            format!("{partial} is the first bad commit"),
            "deadbee is the first bad commit".to_string(),
        ] {
            assert!(
                parse_first_bad_commit(&line).is_none(),
                "must not parse: {line:?}"
            );
        }
    }

    #[test]
    fn parses_bisect_log_entries_from_every_git_version() {
        for line in [
            format!("# first bad commit: [{SHA}] introduce bug"),
            format!("# first 'bad' commit: [{SHA}] introduce bug"),
            format!("# first 'new' commit: [{SHA}] introduce bug"),
        ] {
            assert_eq!(
                first_bad_from_log(&line).as_deref(),
                Some(SHA),
                "line: {line}"
            );
        }
    }

    #[test]
    fn ignores_bisect_log_lines_that_are_not_the_answer() {
        for line in [
            format!("# bad: [{SHA}] docs change"),
            format!("# good: [{SHA}] rev1"),
            format!("# possible first 'bad' commit: [{SHA}] rev1"),
            "# first bad commit: [deadbee] introduce bug".to_string(),
            String::new(),
        ] {
            assert!(
                first_bad_from_log(&line).is_none(),
                "must not parse: {line:?}"
            );
        }
    }

    #[test]
    fn validates_object_id_shape() {
        assert!(is_full_oid(SHA));
        assert!(is_full_oid(SHA256));
        assert!(!is_full_oid(&SHA[..39]));
        assert!(!is_full_oid(&SHA[..38]));
        assert!(!is_full_oid(""));
        assert!(!is_full_oid("7ca4e2bc068b86cc4b741933f77d0adf421e786z"));
    }
}
