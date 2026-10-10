//! RootGuard — failure intelligence for developers.
//!
//! V1 is deterministic: ingest → normalize → fingerprint → blame/scan
//! → 5-why chain → report. V2 adds the verification ladder (T1 instance,
//! T2 citations, T3 mutation-check), confidence tiers for class expansion,
//! and regression-guard generation. No AI, no network, no database.

pub mod codeintel;
pub mod gitintel;
pub mod guard;
pub mod ingest;
pub mod normalize;
pub mod reason;
pub mod report;
pub mod verify;

use std::path::Path;

use normalize::NormalizedError;
use report::Report;

/// Full analysis pipeline for one failure.
///
/// * `source` — where the failure text came from (`arg`, `stdin`, `file:…`, `watch`)
/// * `exit_code` — set when the failure came from a real command
///
/// Pure: never executes anything. See [`analyze_verified`] for the ladder that
/// runs commands to prove the analysis.
pub fn analyze(raw: &str, root: &Path, source: &str, exit_code: Option<i32>) -> Report {
    let err: NormalizedError = normalize::parse(raw);
    let fp = normalize::fingerprint::fingerprint(&err);
    let git = gitintel::context(root);
    let chain = reason::build(&err, root);
    Report::new(source, exit_code, &err, &fp, &git, chain)
}

/// Analyze, then run the verification ladder and generate regression guards.
///
/// Executes `opts.test_cmd` (or the generated guard) at HEAD and at a
/// known-good revision in a temporary worktree — see [`verify::run`] for the
/// exact safety guarantees. The plain [`analyze`] path is unaffected: its
/// report always carries `guards: []` and `verification: ~`.
pub fn analyze_verified(
    raw: &str,
    root: &Path,
    source: &str,
    exit_code: Option<i32>,
    opts: &verify::Options,
) -> Report {
    let mut report = analyze(raw, root, source, exit_code);
    let err = normalize::parse(raw);
    let guards = guard::generate(&err, root);
    let verification = verify::run(&err, root, &mut report, &guards, opts);
    report.guards = guards.iter().map(|g| g.to_string()).collect();
    report.verification = Some(verification);
    report
}
