//! RootGuard — failure intelligence for developers.
//!
//! V1 is deterministic only: ingest → normalize → fingerprint → blame/scan
//! → 5-why chain → report. No AI, no network, no database.

pub mod codeintel;
pub mod gitintel;
pub mod ingest;
pub mod normalize;
pub mod reason;
pub mod report;

use std::path::Path;

use normalize::NormalizedError;
use report::Report;

/// Full analysis pipeline for one failure.
///
/// * `source` — where the failure text came from (`arg`, `stdin`, `file:…`, `watch`)
/// * `exit_code` — set when the failure came from a real command
pub fn analyze(raw: &str, root: &Path, source: &str, exit_code: Option<i32>) -> Report {
    let err: NormalizedError = normalize::parse(raw);
    let fp = normalize::fingerprint::fingerprint(&err);
    let git = gitintel::context(root);
    let chain = reason::build(&err, root);
    Report::new(source, exit_code, &err, &fp, &git, chain)
}
