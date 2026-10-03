use std::path::Path;
use std::process::Command;

use super::Commit;

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
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            for l in combined.lines() {
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
            // Success output ends with `<sha> is the first bad commit`.
            if let Some(line) = combined
                .lines()
                .find(|l| l.ends_with("is the first bad commit"))
            {
                let sha = line.split_whitespace().next().unwrap_or("").to_string();
                culprit = commit_detail(root, &sha);
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
