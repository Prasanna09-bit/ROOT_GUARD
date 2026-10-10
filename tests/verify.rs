//! Integration tests for the V2 verification ladder.
//!
//! Each test builds a throwaway repo with a seeded runtime bug, feeds
//! RootGuard the real failure output, and asserts the ladder's verdicts —
//! including the fail-safe guarantees (HEAD and the working tree untouched).

use std::path::{Path, PathBuf};
use std::process::Command;

use rootguard::reason::{Confidence, Tier};
use rootguard::report::Report;
use rootguard::verify::Options;

fn git(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).to_string()
}

const GOOD_SRC: &str = "def main():\n    return 1\n\nif __name__ == \"__main__\":\n    main()\n";
const BUG_SRC: &str =
    "def main():\n    raise RuntimeError(\"boom\")\n\nif __name__ == \"__main__\":\n    main()\n";

/// Churn repo: 3 good revisions (line 2 changes every time), one breaking
/// commit replacing it, one docs commit on top. HEAD still fails;
/// `HEAD~2` passes — a real good/bad counterfactual.
fn build_repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rootguard-verify-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t.io"]);
    git(&dir, &["config", "user.name", "RootGuardTest"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);

    for i in 1..=3 {
        std::fs::write(
            dir.join("app.py"),
            GOOD_SRC.replace("return 1", &format!("return {i}")),
        )
        .unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", &format!("rev{i}")]);
    }
    std::fs::write(dir.join("app.py"), BUG_SRC).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "introduce bug"]);
    std::fs::write(dir.join("README.md"), "notes\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "docs change"]);

    dir
}

/// Repo where the bug *inserts* the failing line (nothing occupied that line
/// before): git proves the line entered in the bug commit — CONFIRMED.
fn build_repo_inserted(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rootguard-verify-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t.io"]);
    git(&dir, &["config", "user.name", "RootGuardTest"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);

    // app.py stays byte-identical across good revisions (so the bug's
    // inserted lines have no predecessor); history lives in notes.txt.
    std::fs::write(dir.join("app.py"), GOOD_SRC).unwrap();
    for i in 1..=3 {
        std::fs::write(dir.join("notes.txt"), format!("note {i}\n")).unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", &format!("rev{i}")]);
    }
    std::fs::write(
        dir.join("app.py"),
        "def main():\n    if True:\n        raise RuntimeError(\"boom\")\n    return 1\n\nif __name__ == \"__main__\":\n    main()\n",
    )
    .unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "introduce bug"]);
    std::fs::write(dir.join("README.md"), "notes\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "docs change"]);

    dir
}

const TEST_CMD: &str = "python3 app.py";

/// Run the buggy program and capture its real traceback (the analyzed input).
fn failure_output(dir: &Path) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(TEST_CMD)
        .current_dir(dir)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("python3 failed to spawn");
    assert!(
        !out.status.success(),
        "seeded bug must fail — is python3 on PATH?"
    );
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn verify(dir: &Path, raw: &str, opts: &Options) -> Report {
    rootguard::analyze_verified(raw, dir, "integration", Some(1), opts)
}

#[test]
fn full_ladder_confirms_tier() {
    let dir = build_repo("full");
    let raw = failure_output(&dir);
    let head_before = git(&dir, &["rev-parse", "HEAD"]).trim().to_string();

    let report = verify(
        &dir,
        &raw,
        &Options {
            test_cmd: Some(TEST_CMD),
            good: None,
        },
    );

    // Guard generation: relative, portable, and exactly the reproduction
    // command (the ladder re-runs it in a temporary worktree).
    assert_eq!(report.guards.len(), 1, "guards: {:?}", report.guards);
    assert!(
        report.guards[0].starts_with("python3 app.py | runtime/"),
        "guard must use a root-relative path: {}",
        report.guards[0]
    );

    let v = report.verification.as_ref().expect("verification must run");
    assert_eq!(v.tier, Tier::Confirmed, "ladder notes: {:?}", v.notes);

    let t1 = v.t1_instance.as_ref().expect("T1 must run");
    assert!(t1.reproduced, "T1: {t1:?}");
    assert_ne!(t1.exit_code, 0);
    assert_eq!(
        t1.observed_fingerprint, t1.expected_fingerprint,
        "same failure, same fingerprint"
    );

    assert!(
        v.t2_citations.checked >= 2,
        "symptom + blame cites at least: {t2:?}",
        t2 = v.t2_citations
    );
    assert!(
        v.t2_citations.unresolved.is_empty(),
        "citations must resolve: {:?}",
        v.t2_citations.unresolved
    );

    let t3 = v.t3_mutation.as_ref().expect("T3 must run");
    assert!(t3.head_fails, "guard must detect the live bug");
    assert_eq!(t3.good_passes, Some(true), "guard must pass at good");
    assert!(t3.mutation_checked);

    // Prevention graduates from hypothesis to observed, with the guard cited.
    let prev = report
        .analysis
        .steps
        .iter()
        .find(|s| s.level == "prevention")
        .expect("prevention step");
    assert_eq!(prev.confidence, Confidence::Observed);
    assert!(
        prev.evidence.iter().any(|e| e.cite.starts_with("guard: ")),
        "prevention must cite the guard: {:?}",
        prev.evidence
    );

    // Fail-safe: verification must not move HEAD, dirty the tree, or leak
    // temporary worktrees.
    assert_eq!(
        git(&dir, &["rev-parse", "HEAD"]).trim(),
        head_before,
        "HEAD must not move"
    );
    assert!(
        git(&dir, &["status", "--porcelain"]).trim().is_empty(),
        "tree must stay clean"
    );
    let worktrees = git(&dir, &["worktree", "list"]);
    assert_eq!(
        worktrees.lines().count(),
        1,
        "temporary worktree must be removed:\n{worktrees}"
    );

    // YAML contract: verification and guards serialize and round-trip.
    let yaml = report.to_yaml();
    assert!(yaml.contains("verification:"), "yaml: {yaml}");
    assert!(yaml.contains("t1_instance:"), "yaml: {yaml}");
    assert!(yaml.contains("t3_mutation:"), "yaml: {yaml}");
    assert!(yaml.contains("tier: confirmed"), "yaml: {yaml}");
    let back: Report = serde_yaml::from_str(&yaml).expect("YAML must deserialize");
    assert_eq!(
        back.verification.as_ref().unwrap().tier,
        Tier::Confirmed,
        "round-trip must preserve the tier"
    );
    assert_eq!(back.guards, report.guards);
}

#[test]
fn tiers_reflect_git_evidence_without_verification() {
    // Churn: the failing line changed in every good revision too. Blame is
    // only the last author — strong, but not proof the bug entered there.
    let dir = build_repo("tiers-churn");
    let raw = failure_output(&dir);
    let report = rootguard::analyze(&raw, &dir, "integration", None);

    // Plain analyze: no commands executed, no guards — but evidence tiers.
    assert!(report.guards.is_empty());
    assert!(report.verification.is_none());

    let primary = &report.analysis.suspects[0];
    assert_eq!(primary.summary, "introduce bug");
    assert_eq!(primary.tier, Tier::Likely, "churning line: blame only");

    // The failing site is observed-failing → confirmed regardless of churn.
    let site = report
        .analysis
        .sites
        .iter()
        .find(|s| s.file == "app.py" && s.line == 2)
        .expect("failing site must appear in the class");
    assert_eq!(site.tier, Tier::Confirmed);

    // Insertion: the bug added the failing line to a line that never existed
    // before — git itself proves the code entered there.
    let dir = build_repo_inserted("tiers-insert");
    let raw = failure_output(&dir);
    let report = rootguard::analyze(&raw, &dir, "integration", None);
    let primary = &report.analysis.suspects[0];
    assert_eq!(primary.summary, "introduce bug");
    assert_eq!(
        primary.tier,
        Tier::Confirmed,
        "inserted line: introduction == blame == bug commit"
    );
    let site = report
        .analysis
        .sites
        .iter()
        .find(|s| s.file == "app.py")
        .expect("failing site must appear in the class");
    assert_eq!(site.line, 3, "raise moved to line 3 in the inserted shape");
    assert_eq!(site.tier, Tier::Confirmed);
}

#[test]
fn plain_analyze_keeps_the_v1_contract() {
    let dir = build_repo("v1contract");
    let raw = failure_output(&dir);
    let report = rootguard::analyze(&raw, &dir, "integration", None);

    assert!(report.guards.is_empty());
    assert!(report.verification.is_none());
    let text = report.to_text();
    assert!(text.contains("pass --verify"), "text: {text}");
}

#[test]
fn bad_good_endpoint_demotes_tier_to_possible() {
    let dir = build_repo("badgood");
    let raw = failure_output(&dir);
    // The breaking commit itself fails — not a valid counterfactual.
    let bug = git(&dir, &["rev-parse", "HEAD~1"]).trim().to_string();

    let report = verify(
        &dir,
        &raw,
        &Options {
            test_cmd: Some(TEST_CMD),
            good: Some(&bug),
        },
    );
    let v = report.verification.as_ref().unwrap();

    assert!(
        v.t1_instance.as_ref().unwrap().reproduced,
        "T1 is independent of the bad good-ref"
    );
    assert_eq!(v.tier, Tier::Possible, "notes: {:?}", v.notes);
    let t3 = v.t3_mutation.as_ref().unwrap();
    assert!(t3.head_fails);
    assert_eq!(t3.good_passes, Some(false));
    assert!(!t3.mutation_checked);
    assert!(
        v.notes
            .iter()
            .any(|n| n.contains("not a valid counterfactual")),
        "notes: {:?}",
        v.notes
    );
}

#[test]
fn good_equal_to_head_is_inconclusive_not_a_pass() {
    let dir = build_repo("goodhead");
    let raw = failure_output(&dir);

    let report = verify(
        &dir,
        &raw,
        &Options {
            test_cmd: Some(TEST_CMD),
            good: Some("HEAD"),
        },
    );
    let v = report.verification.as_ref().unwrap();
    assert_eq!(v.tier, Tier::Possible);
    let t3 = v.t3_mutation.as_ref().unwrap();
    assert_eq!(t3.good_passes, None);
    assert!(!t3.mutation_checked);
    assert!(
        v.notes.iter().any(|n| n.contains("resolves to HEAD")),
        "notes: {:?}",
        v.notes
    );
}

#[test]
fn verify_without_test_command_stops_at_likely() {
    let dir = build_repo("notest");
    let raw = failure_output(&dir);

    let report = verify(
        &dir,
        &raw,
        &Options {
            test_cmd: None,
            good: None,
        },
    );
    let v = report.verification.as_ref().unwrap();

    // T1 needs the user's command; without it the ladder cannot be complete.
    assert!(v.t1_instance.is_none());
    assert_eq!(v.tier, Tier::Likely, "notes: {:?}", v.notes);
    assert!(
        v.notes.iter().any(|n| n.contains("t1 skipped")),
        "notes: {:?}",
        v.notes
    );
    // T3 still runs with the generated guard (auto-detected good endpoint).
    let t3 = v.t3_mutation.as_ref().expect("guard-based T3 must run");
    assert!(t3.mutation_checked, "notes: {:?}", v.notes);
}

#[test]
fn dirty_tree_skips_auto_good_and_preserves_user_files() {
    let dir = build_repo("dirty");
    let raw = failure_output(&dir);
    let stray = dir.join("wip-notes.txt");
    std::fs::write(&stray, "uncommitted work\n").unwrap();

    let report = verify(
        &dir,
        &raw,
        &Options {
            test_cmd: Some(TEST_CMD),
            good: None,
        },
    );
    let v = report.verification.as_ref().unwrap();

    assert!(v.t3_mutation.is_none(), "auto-good needs a clean tree");
    assert!(
        v.notes.iter().any(|n| n.contains("--good")),
        "notes: {:?}",
        v.notes
    );
    assert_ne!(v.tier, Tier::Confirmed, "incomplete ladder cannot confirm");
    assert!(stray.exists(), "verification must never delete user files");

    let _ = std::fs::remove_file(stray);
}
