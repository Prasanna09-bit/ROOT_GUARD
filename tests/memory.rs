//! Integration tests for the V3 failure memory.
//!
//! Each test builds a throwaway root, feeds RootGuard a real failure, and
//! asserts the memory's guarantees: sightings accumulate, verifications
//! decay unless revalidated, triage feedback calibrates, and `--no-memory`
//! leaves no trace.

use std::path::{Path, PathBuf};
use std::process::Command;

use rootguard::memory::{self, Record, Verdict};
use rootguard::reason::Tier;
use rootguard::report::Report;
use rootguard::verify::{T2Citations, T3Mutation, Verification};

/// A parseable Python traceback for a failure of class `(symbol, file)`.
fn trace(sym: &str, file: &str, msg: &str) -> String {
    format!(
        "Traceback (most recent call last):\n  File \"{file}\", line 2, in <module>\n\
         \x20   raise {sym}(\"{msg}\")\n{sym}: {msg}\n"
    )
}

fn default_trace() -> String {
    trace("ValueError", "/tmp/demo/app.py", "boom")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rootguard-memory-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn analyze(root: &Path, raw: &str) -> Report {
    rootguard::analyze(raw, root, "integration", Some(1))
}

/// Report as if `--verify` had run the full ladder.
fn verified(mut report: Report, tier: Tier) -> Report {
    report.verification = Some(Verification {
        tier,
        t1_instance: None,
        t2_citations: T2Citations {
            checked: 2,
            resolved: 2,
            unresolved: vec![],
        },
        t3_mutation: Some(T3Mutation {
            check: "python3 app.py".into(),
            head_fails: true,
            good: Some("HEAD~1".into()),
            good_passes: Some(true),
            mutation_checked: true,
        }),
        notes: vec![],
    });
    report.guards = vec!["check | python3 app.py | runtime/app_py".into()];
    report
}

fn record_count(root: &Path) -> usize {
    let d = memory::dir(root);
    std::fs::read_dir(d)
        .map(|e| {
            e.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("yaml"))
                .count()
        })
        .unwrap_or(0)
}

/// Locate the record file holding `fingerprint` (content scan, like recall).
fn record_file(root: &Path, fingerprint: &str) -> PathBuf {
    std::fs::read_dir(memory::dir(root))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            std::fs::read_to_string(p)
                .map(|t| t.contains(fingerprint))
                .unwrap_or(false)
        })
        .expect("record file must exist")
}

#[test]
fn sightings_accumulate_and_recall_renders() {
    let root = scratch("sightings");
    let raw = default_trace();

    let first = memory::remember(&root, &analyze(&root, &raw), None).unwrap();
    assert_eq!(first.observations, 1);
    assert!(first.verified_tier.is_none());

    let mut report = analyze(&root, &raw);
    let second = memory::remember(&root, &report, Some(Verdict::Correct)).unwrap();
    assert_eq!(second.observations, 2);
    assert_eq!(second.correct, 1);
    assert_eq!(record_count(&root), 1);

    // The recall section lands in both renderings of the report.
    report.memory = Some(second);
    let text = report.to_text();
    assert!(text.contains("Failure memory"), "{text}");
    assert!(text.contains("seen 2 times"), "{text}");
    assert!(text.contains("never verified"), "{text}");
    assert!(text.contains("triage: 1 correct / 0 wrong"), "{text}");

    let yaml = report.to_yaml();
    let back: Report = serde_yaml::from_str(&yaml).expect("YAML must deserialize");
    assert_eq!(back.memory.as_ref().unwrap().observations, 2);

    // Plain reports (V1 path) carry no memory section at all.
    let plain = analyze(&root, &raw);
    assert!(plain.memory.is_none());
    assert!(!plain.to_text().contains("Failure memory"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn verification_decays_unless_revalidated() {
    let root = scratch("decay");
    let raw = default_trace();

    let fresh = memory::remember(
        &root,
        &verified(analyze(&root, &raw), Tier::Confirmed),
        None,
    )
    .unwrap();
    assert_eq!(fresh.current_tier, Some(Tier::Confirmed));
    assert!(!fresh.stale);

    // Hand-age the record by 70 days: two decay windows, confirmed → possible.
    let fp = analyze(&root, &raw).observed.fingerprint;
    let mut rec = memory::load(&root, &fp).expect("record exists");
    rec.verified_at = Some(memory::rfc3339(memory::now_secs() - 70 * 86_400));
    let file = record_file(&root, &fp);
    std::fs::write(&file, serde_yaml::to_string(&rec).unwrap()).unwrap();

    let stale = memory::load(&root, &fp).unwrap();
    let view = memory::view(&stale, memory::now_secs());
    assert_eq!(view.verified_tier, Some(Tier::Confirmed), "history kept");
    assert_eq!(view.current_tier, Some(Tier::Possible), "decayed 70d");
    assert!(view.stale);
    assert_eq!(view.age_days, Some(70));

    let mut report = analyze(&root, &raw);
    report.memory = Some(view);
    let text = report.to_text();
    assert!(text.contains("decayed to POSSIBLE"), "{text}");
    assert!(text.contains("revalidate"), "{text}");

    // Re-running the ladder restores trust — revalidation, not time.
    let revalidated = memory::remember(
        &root,
        &verified(analyze(&root, &raw), Tier::Confirmed),
        None,
    )
    .unwrap();
    assert!(!revalidated.stale);
    assert_eq!(revalidated.current_tier, Some(Tier::Confirmed));
    assert_eq!(revalidated.observations, 2, "sightings kept counting");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn triage_feedback_drives_calibration() {
    let root = scratch("triage");
    // Two distinct failure classes: one verified (verdicts agree), one that
    // was only ever seen (verdict disagrees) — the calibration buckets differ.
    let confirmed_raw = trace("ValueError", "/tmp/demo/app.py", "boom");
    let unseen_raw = trace("KeyError", "/tmp/demo/db.py", "missing");

    for _ in 0..2 {
        let report = verified(analyze(&root, &confirmed_raw), Tier::Confirmed);
        memory::remember(&root, &report, Some(Verdict::Correct)).unwrap();
    }
    memory::remember(&root, &analyze(&root, &unseen_raw), Some(Verdict::Wrong)).unwrap();

    let cal = memory::calibrate(&root);
    assert_eq!(cal.records, 2);
    assert_eq!(cal.verified_records, 1);
    assert_eq!(cal.correct, 2);
    assert_eq!(cal.wrong, 1);
    assert_eq!(cal.by_tier.len(), 2);
    assert_eq!(cal.by_tier[0].tier, "confirmed");
    assert_eq!((cal.by_tier[0].correct, cal.by_tier[0].wrong), (2, 0));
    assert_eq!(cal.by_tier[1].tier, "never-verified");
    assert_eq!((cal.by_tier[1].correct, cal.by_tier[1].wrong), (0, 1));

    let text = cal.to_text();
    assert!(text.contains("3 verdicts — 2 correct, 1 wrong"), "{text}");
    assert!(text.contains("66% agreement"), "{text}");

    // Empty memory calibrates to a helpful nothing.
    let empty = scratch("triage-empty");
    let blank = memory::calibrate(&empty);
    assert_eq!(blank.records, 0);
    assert!(blank.to_text().contains("no verdicts yet"));
    let _ = std::fs::remove_dir_all(&empty);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn memory_never_breaks_on_bad_files() {
    let root = scratch("badfiles");
    let d = memory::dir(&root);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("truncated.yaml"),
        "rootguard: 1\nfingerprint: [oops\n",
    )
    .unwrap();
    std::fs::write(d.join("empty.yaml"), "").unwrap();
    std::fs::write(d.join("notyaml.txt"), "ignored").unwrap();

    let report = analyze(&root, &default_trace());
    assert!(
        memory::load(&root, &report.observed.fingerprint).is_none(),
        "garbage must not recall as ours"
    );
    let view = memory::remember(&root, &report, Some(Verdict::Correct)).unwrap();
    assert_eq!(view.observations, 1);
    let cal = memory::calibrate(&root);
    assert_eq!(cal.records, 1, "only the record we wrote parses: {cal:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn record_files_stay_portable_and_scannable() {
    // Fingerprints contain slashes; the on-disk name must not, and the file
    // must round-trip so `calibrate` can scan it back.
    let root = scratch("portable");
    let raw = default_trace();
    memory::remember(&root, &verified(analyze(&root, &raw), Tier::Likely), None).unwrap();

    let d = memory::dir(&root);
    let files: Vec<PathBuf> = std::fs::read_dir(&d)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    assert_eq!(files.len(), 1);
    let name = files[0].file_name().unwrap().to_string_lossy().to_string();
    assert!(!name.contains('/'), "flat filename: {name}");
    assert!(name.ends_with(".yaml"));

    let text = std::fs::read_to_string(&files[0]).unwrap();
    let rec: Record = serde_yaml::from_str(&text).expect("record parses");
    assert_eq!(rec.verified_tier, Some(Tier::Likely));
    assert_eq!(rec.observations, 1);
    assert!(rec.verification.is_some(), "evidence kept");
    assert!(!rec.guards.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

// --- binary-level: `explain` wiring ---------------------------------------

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

/// Seeded-bug repo (same shape as the ladder tests): `HEAD` fails, `HEAD~2`
/// passes, so T3 has a real counterfactual to auto-detect.
fn bug_repo(name: &str) -> PathBuf {
    let dir = scratch(name);
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t.io"]);
    git(&dir, &["config", "user.name", "RootGuardTest"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    for i in 1..=3 {
        std::fs::write(
            dir.join("app.py"),
            format!("def main():\n    return {i}\n\nif __name__ == \"__main__\":\n    main()\n"),
        )
        .unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", &format!("rev{i}")]);
    }
    std::fs::write(
        dir.join("app.py"),
        "def main():\n    raise RuntimeError(\"boom\")\n\nif __name__ == \"__main__\":\n    main()\n",
    )
    .unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "introduce bug"]);
    std::fs::write(dir.join("README.md"), "notes\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "docs change"]);
    dir
}

fn explain_file(root: &Path, input: &Path, extra: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_rootguard"))
        .arg("explain")
        .arg(input)
        .arg("-r")
        .arg(root)
        .arg("-f")
        .arg("yaml")
        .args(extra)
        .output()
        .expect("rootguard failed to spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn memory_is_invisible_to_git_and_blocks_nothing() {
    // Memory lives inside a real repo: it must not dirty `git status`
    // (that would flip snapshots to dirty and block T3's auto-good), and
    // the ladder must still confirm afterwards.
    let root = bug_repo("git-memory");
    let input = std::env::temp_dir().join(format!("rootguard-mem-input-{}", std::process::id()));
    let raw = Command::new("sh")
        .arg("-c")
        .arg("python3 app.py")
        .current_dir(&root)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("python3 failed to spawn");
    assert!(!raw.status.success(), "seeded bug must fail");
    std::fs::write(&input, String::from_utf8_lossy(&raw.stderr).as_bytes()).unwrap();

    // Sighting 1: record written, tree still clean, exclude entry added.
    let head_before = git(&root, &["rev-parse", "HEAD"]).trim().to_string();
    let (ok, out, err) = explain_file(&root, &input, &[]);
    assert!(ok, "explain failed: {err}");
    assert!(out.contains("observations: 1"), "yaml: {out}");
    assert_eq!(record_count(&root), 1);
    assert_eq!(
        git(&root, &["status", "--porcelain"]).trim(),
        "",
        "memory must be invisible to git status"
    );
    let exclude = std::fs::read_to_string(root.join(".git").join("info").join("exclude")).unwrap();
    assert!(
        exclude.contains(".rootguard/"),
        "local exclude must list .rootguard/: {exclude}"
    );
    // Tracked files untouched: no .gitignore was created or edited.
    assert!(!root.join(".gitignore").exists(), "never touch .gitignore");

    // Sighting 2 with the ladder: auto-good must still work — the tree is
    // clean *because* memory is excluded — so the tier confirms.
    let (ok, out, err) = explain_file(&root, &input, &["--verify", "--test", "python3 app.py"]);
    assert!(ok, "explain --verify failed: {err}");
    assert!(out.contains("tier: confirmed"), "yaml: {out}");
    assert!(out.contains("observations: 2"), "yaml: {out}");
    assert!(out.contains("verified_tier: confirmed"), "yaml: {out}");
    assert_eq!(record_count(&root), 1, "records survive verify's git clean");
    assert_eq!(
        git(&root, &["status", "--porcelain"]).trim(),
        "",
        "tree clean after the ladder"
    );
    // HEAD unmoved, as always.
    assert_eq!(
        git(&root, &["rev-parse", "HEAD"]).trim(),
        head_before,
        "HEAD must not move"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_file(&input);
}

fn run_explain(bin: &str, root: &Path, extra: &[&str]) -> (bool, String, String) {
    let err = root.join("failure.txt");
    if !err.exists() {
        std::fs::write(&err, default_trace()).unwrap();
    }
    let mut cmd = Command::new(bin);
    cmd.args([
        "explain",
        err.to_str().unwrap(),
        "-r",
        root.to_str().unwrap(),
        "-f",
        "yaml",
    ]);
    cmd.args(extra);
    let out = cmd.output().expect("rootguard failed to spawn");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn explain_records_memory_and_no_memory_opts_out() {
    let bin = env!("CARGO_BIN_EXE_rootguard");
    let root = scratch("binary");

    // First sighting: record created, recall embedded in the YAML report.
    let (ok, out, err) = run_explain(bin, &root, &[]);
    assert!(ok, "explain failed: {err}");
    assert!(out.contains("memory:"), "yaml: {out}");
    assert!(out.contains("observations: 1"), "yaml: {out}");
    assert!(
        !out.contains("verified_tier"),
        "never verified → no tier keys: {out}"
    );
    assert_eq!(record_count(&root), 1);

    // Second sighting counts up.
    let (ok, out, err) = run_explain(bin, &root, &[]);
    assert!(ok, "explain failed: {err}");
    assert!(out.contains("observations: 2"), "yaml: {out}");

    // --no-memory: neither recalls nor records (file untouched).
    let (ok, out, err) = run_explain(bin, &root, &["--no-memory"]);
    assert!(ok, "explain failed: {err}");
    assert!(!out.contains("memory:"), "yaml must omit memory: {out}");
    assert!(!out.contains("observations:"), "not recalled: {out}");
    assert_eq!(record_count(&root), 1, "--no-memory must not write");

    // --triage with --no-memory is a contradiction, not a silent no-op.
    let out = Command::new(bin)
        .args([
            "explain",
            root.join("failure.txt").to_str().unwrap(),
            "-r",
            root.to_str().unwrap(),
            "--triage",
            "correct",
            "--no-memory",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "--triage + --no-memory must fail");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--no-memory"),
        "error must explain the conflict"
    );

    // --triage records the verdict, and calibrate reports it.
    let (ok, out, err) = run_explain(bin, &root, &["--triage", "correct"]);
    assert!(ok, "explain --triage failed: {err}");
    assert!(out.contains("observations: 3"), "yaml: {out}");
    assert!(out.contains("correct: 1"), "yaml: {out}");

    let cal = Command::new(bin)
        .args(["calibrate", "-r", root.to_str().unwrap(), "-f", "yaml"])
        .output()
        .unwrap();
    assert!(cal.status.success());
    let cal = String::from_utf8_lossy(&cal.stdout).to_string();
    assert!(cal.contains("records: 1"), "calibrate: {cal}");
    assert!(cal.contains("correct: 1"), "calibrate: {cal}");

    let text = Command::new(bin)
        .args(["calibrate", "-r", root.to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout).to_string();
    assert!(text.contains("1 verdicts"), "calibrate text: {text}");
    assert!(text.contains("never-verified"), "calibrate text: {text}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn explain_text_output_shows_the_recall_section() {
    let bin = env!("CARGO_BIN_EXE_rootguard");
    let root = scratch("binary-text");

    let _ = run_explain(bin, &root, &[]);
    let (ok, _, err) = run_explain(bin, &root, &[]);
    assert!(ok, "explain failed: {err}");
    // Third run (text): the section must show all sightings so far.
    let out = Command::new(bin)
        .args([
            "explain",
            root.join("failure.txt").to_str().unwrap(),
            "-r",
            root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.contains("Failure memory"), "{text}");
    assert!(text.contains("seen 3 times"), "{text}");
    assert!(text.contains("never verified"), "{text}");
    assert!(
        text.contains("fingerprint"),
        "still a normal report: {text}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
