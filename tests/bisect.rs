//! Integration tests for `git bisect` orchestration.
//!
//! Each test builds a throwaway repo with a known breaking commit and asserts
//! RootGuard identifies exactly that commit — and leaves the tree clean.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git failed to spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a repo with `n` good commits, one breaking commit, then one more
/// good commit on top. Returns (dir, breaking_commit_sha).
fn build_repo(name: &str) -> (PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("rootguard-bisect-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t.io"]);
    git(&dir, &["config", "user.name", "RootGuardTest"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);

    for i in 1..=6 {
        std::fs::write(dir.join("app.py"), format!("def main():\n    return {i}\n")).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", &format!("rev{i}")]);
    }

    // The breaking commit.
    std::fs::write(
        dir.join("app.py"),
        "def main():\n    raise RuntimeError('boom')\n",
    )
    .unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "introduce bug"]);
    let breaking = String::from_utf8(git_capture(&dir, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_string();

    // A later commit that does NOT fix the bug — HEAD must still fail,
    // otherwise there is nothing to bisect.
    std::fs::write(dir.join("README.md"), "project notes\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "docs change"]);

    (dir, breaking)
}

fn git_capture(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git failed to spawn");
    out.stdout
}

const TEST_CMD: &str = r#"python3 -c "import app; app.main()""#;

#[test]
fn finds_the_known_breaking_commit() {
    let (dir, breaking) = build_repo("known");
    let res =
        rootguard::gitintel::bisect(&dir, None, None, TEST_CMD).expect("bisect should succeed");
    let culprit = res.culprit.expect("must identify a culprit");
    assert_eq!(
        culprit.sha, breaking,
        "culprit mismatch — expected the seeded breaking commit"
    );
    assert_eq!(culprit.summary, "introduce bug");
    assert!(res.tested >= 1, "must have tested at least one revision");
    // Auto-detected good endpoint must be verified (not HEAD).
    assert!(
        res.auto_good.is_some(),
        "good endpoint should be auto-detected"
    );
}

#[test]
fn tree_is_clean_and_head_restored_after_bisect() {
    let (dir, _) = build_repo("restore");
    let head_before = String::from_utf8(git_capture(&dir, &["rev-parse", "HEAD"])).unwrap();

    rootguard::gitintel::bisect(&dir, None, None, TEST_CMD).expect("bisect should succeed");

    let head_after = String::from_utf8(git_capture(&dir, &["rev-parse", "HEAD"])).unwrap();
    assert_eq!(head_before, head_after, "HEAD must be restored");

    let status = String::from_utf8(git_capture(&dir, &["status", "--porcelain"])).unwrap();
    assert!(
        status.trim().is_empty(),
        "working tree must be clean after bisect, got:\n{status}"
    );

    // No bisect session left dangling.
    let has_bisect = std::path::Path::new(&dir)
        .join(".git")
        .join("BISECT_LOG")
        .exists();
    assert!(!has_bisect, "bisect session must be reset");
}

#[test]
fn refuses_dirty_tree() {
    let (dir, _) = build_repo("dirty");
    std::fs::write(dir.join("uncommitted.txt"), "dirty").unwrap();
    let err = rootguard::gitintel::bisect(&dir, None, None, TEST_CMD)
        .expect_err("dirty tree must be refused");
    assert!(
        err.to_string().contains("dirty"),
        "error should mention dirty tree: {err}"
    );
    // No bisect session should have started.
    assert!(!dir.join(".git").join("BISECT_LOG").exists());
}

#[test]
fn rejects_test_that_never_fails() {
    // Good repo: `main()` always succeeds → test never fails at any revision.
    let dir = std::env::temp_dir().join("rootguard-bisect-neverfails");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t.io"]);
    git(&dir, &["config", "user.name", "RootGuardTest"]);
    for i in 1..=4 {
        std::fs::write(dir.join("app.py"), format!("def main():\n    return {i}\n")).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", &format!("rev{i}")]);
    }

    let err = rootguard::gitintel::bisect(&dir, None, None, TEST_CMD)
        .expect_err("a test that never fails must be rejected");
    assert!(
        err.to_string().contains("does not fail"),
        "unexpected error: {err}"
    );
    assert!(!dir.join(".git").join("BISECT_LOG").exists());
    let status = String::from_utf8(git_capture(&dir, &["status", "--porcelain"])).unwrap();
    assert!(status.trim().is_empty(), "tree must stay clean: {status}");
}
