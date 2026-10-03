//! Integration tests: every fixture must parse, produce a cited 5-why chain,
//! and yield a stable fingerprint.

use rootguard::reason::Confidence;

fn fixture(name: &str) -> String {
    let path = format!("{}/fixtures/errors/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("missing fixture {path}: {e}"))
}

#[test]
fn all_fixtures_parse_confidently() {
    for name in [
        "rustc.txt",
        "tsc.txt",
        "python.txt",
        "node.txt",
        "rustc_test.txt",
    ] {
        let raw = fixture(name);
        let err = rootguard::normalize::parse(&raw);
        assert!(
            err.parse_confident,
            "fixture {name} should parse confidently, got message {:?}",
            err.message
        );
        assert!(
            !err.message.is_empty(),
            "fixture {name} produced an empty message"
        );
        assert!(
            err.location.is_some(),
            "fixture {name} should yield a source location"
        );
    }
}

#[test]
fn fixture_kinds_are_classified() {
    let kind_of = |n: &str| rootguard::normalize::parse(&fixture(n)).kind;
    assert_eq!(
        kind_of("rustc.txt"),
        rootguard::normalize::ErrorKind::Compile
    );
    assert_eq!(kind_of("tsc.txt"), rootguard::normalize::ErrorKind::Type);
    assert_eq!(
        kind_of("python.txt"),
        rootguard::normalize::ErrorKind::Runtime
    );
    assert_eq!(
        kind_of("node.txt"),
        rootguard::normalize::ErrorKind::Runtime
    );
}

#[test]
fn chain_is_complete_and_cited() {
    let root = std::env::temp_dir();
    for name in [
        "rustc.txt",
        "tsc.txt",
        "python.txt",
        "node.txt",
        "rustc_test.txt",
    ] {
        let raw = fixture(name);
        let report = rootguard::analyze(&raw, &root, "fixture", None);
        let levels: Vec<&str> = report
            .analysis
            .steps
            .iter()
            .map(|s| s.level.as_str())
            .collect();
        assert_eq!(
            levels,
            vec![
                "symptom",
                "immediate-cause",
                "underlying-cause",
                "systemic-cause",
                "prevention"
            ],
            "fixture {name} chain shape wrong"
        );
        // The symptom must always be observed with at least one citation.
        assert_eq!(report.analysis.steps[0].confidence, Confidence::Observed);
        assert!(
            !report.analysis.steps[0].evidence.is_empty(),
            "fixture {name}: symptom step must carry evidence"
        );
        // Nothing without evidence may claim OBSERVED.
        for step in &report.analysis.steps {
            if step.evidence.is_empty() {
                assert_eq!(
                    step.confidence,
                    Confidence::Hypothesis,
                    "fixture {name}: step {} claims OBSERVED with no evidence",
                    step.level
                );
            }
        }
        // Guards/verification are V2 — always empty in V1.
        assert!(report.guards.is_empty());
        assert!(report.verification.is_none());
    }
}

#[test]
fn fingerprints_are_stable_across_runs_and_paths() {
    let raw = fixture("rustc.txt");
    let root = std::env::temp_dir();
    let a = rootguard::analyze(&raw, &root, "fixture", None);
    let b = rootguard::analyze(&raw, &root, "fixture", None);
    assert_eq!(a.observed.fingerprint, b.observed.fingerprint);

    // Same error, different absolute prefix → same fingerprint.
    let shifted = raw.replace("src/db.rs", "/abs/prefix/src/db.rs");
    let c = rootguard::analyze(&shifted, &root, "fixture", None);
    assert_eq!(
        a.observed.fingerprint, c.observed.fingerprint,
        "absolute path prefix must not change the class fingerprint"
    );
}

#[test]
fn yaml_report_round_trips() {
    let raw = fixture("tsc.txt");
    let root = std::env::temp_dir();
    let report = rootguard::analyze(&raw, &root, "fixture", None);
    let yaml = report.to_yaml();
    let back: rootguard::report::Report =
        serde_yaml::from_str(&yaml).expect("YAML must deserialize");
    assert_eq!(back.rootguard, rootguard::report::SCHEMA);
    assert_eq!(back.analysis.steps.len(), 5);
    assert_eq!(back.normalized.kind, "type");
}

#[test]
fn text_report_contains_every_chain_level() {
    let raw = fixture("python.txt");
    let root = std::env::temp_dir();
    let report = rootguard::analyze(&raw, &root, "fixture", None);
    let text = report.to_text();
    for level in [
        "symptom",
        "immediate-cause",
        "underlying-cause",
        "systemic-cause",
        "prevention",
    ] {
        assert!(text.contains(level), "text report missing {level}");
    }
    assert!(text.contains("fingerprint"));
}
