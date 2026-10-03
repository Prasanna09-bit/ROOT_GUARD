use serde::{Deserialize, Serialize};

use crate::codeintel::SymbolSite;
use crate::gitintel;
use crate::normalize::{ErrorKind, NormalizedError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    /// Backed by direct evidence we could observe (file, line, commit, test output).
    Observed,
    /// Inferred; no direct citation available.
    Hypothesis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    /// `file:line:col`, `commit <sha>` or `symbol @ n sites`.
    pub cite: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    pub level: String,
    pub text: String,
    pub confidence: Confidence,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suspect {
    pub commit: String,
    pub short: String,
    pub author: String,
    pub summary: String,
    pub why: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSiteOut {
    pub file: String,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chain {
    pub steps: Vec<ChainStep>,
    pub suspects: Vec<Suspect>,
    pub sites: Vec<SymbolSiteOut>,
}

/// Build the 5-why chain deterministically (no AI in V1).
///
/// Levels: symptom → immediate-cause → underlying-cause → systemic-cause → prevention.
/// Anything without a citation is explicitly marked `hypothesis`.
pub fn build(err: &NormalizedError, root: &std::path::Path) -> Chain {
    let mut steps = Vec::new();
    let mut suspects = Vec::new();

    // 1. Symptom — verbatim from the failing tool.
    let loc_cite = err
        .location
        .as_ref()
        .map(|l| l.cite())
        .unwrap_or_else(|| "location unknown".into());
    steps.push(ChainStep {
        level: "symptom".into(),
        text: format!("{} [{} error] at {}", err.message, err.kind, loc_cite),
        confidence: Confidence::Observed,
        evidence: err
            .location
            .as_ref()
            .map(|l| Evidence {
                cite: l.cite(),
                note: "primary location reported by the failing tool".into(),
            })
            .into_iter()
            .collect(),
    });

    // 2. Immediate cause — the mechanical reason the tool emitted this.
    steps.push(immediate_cause(err));

    // 3. Underlying cause — blame / history on the failing line.
    steps.push(underlying_cause(err, root, &mut suspects));

    // 4. Systemic cause — repo-wide blast radius of the symbol.
    let symbol = err.symbol.clone();
    let mut sites = match &symbol {
        Some(s) => crate::codeintel::find_symbol(root, s, 50),
        None => Vec::new(),
    };
    // Real code first: "README.md:49" is a citation nobody can act on.
    sites.sort_by_key(|s| (docs_last(&s.file), s.file.clone(), s.line));
    steps.push(systemic_cause(err, symbol.as_deref(), &sites));

    // 5. Prevention — cheapest guard that covers this class.
    steps.push(prevention(err));

    let sites_out = sites
        .iter()
        .map(|s| SymbolSiteOut {
            file: s.file.clone(),
            line: s.line,
        })
        .collect();

    Chain {
        steps,
        suspects,
        sites: sites_out,
    }
}

fn immediate_cause(err: &NormalizedError) -> ChainStep {
    let sym = err.symbol.as_deref().unwrap_or(&err.message);
    let text = match err.kind {
        ErrorKind::Compile | ErrorKind::Type => format!(
            "the compiler/type-checker rejected `{sym}` — the program never ran; \
             resolution failed before execution"
        ),
        ErrorKind::Runtime => format!(
            "execution reached a state the code does not handle: {}",
            err.message
        ),
        ErrorKind::Test => format!("a test assertion failed: {}", err.message),
        ErrorKind::Lint => format!("a static rule flagged this site: {}", err.message),
        ErrorKind::Dep => format!("a dependency could not be resolved: {}", err.message),
        ErrorKind::Build => format!("the build pipeline aborted: {}", err.message),
        ErrorKind::Config => format!(
            "environment/configuration is wrong for this operation: {}",
            err.message
        ),
        ErrorKind::Db | ErrorKind::Api | ErrorKind::Ci | ErrorKind::Unknown => err.message.clone(),
    };
    ChainStep {
        level: "immediate-cause".into(),
        text,
        confidence: if err.parse_confident {
            Confidence::Observed
        } else {
            Confidence::Hypothesis
        },
        evidence: err
            .location
            .as_ref()
            .map(|l| Evidence {
                cite: l.cite(),
                note: "mechanical cause reported by the tool".into(),
            })
            .into_iter()
            .collect(),
    }
}

fn underlying_cause(
    err: &NormalizedError,
    root: &std::path::Path,
    suspects: &mut Vec<Suspect>,
) -> ChainStep {
    let Some(loc) = err.location.as_ref() else {
        return ChainStep {
            level: "underlying-cause".into(),
            text: "no file location in the failure output — cannot attribute an \
                   introducing change without a source location"
                .into(),
            confidence: Confidence::Hypothesis,
            evidence: vec![],
        };
    };

    if let Some(line) = loc.line
        && let Some(b) = gitintel::blame_line(root, &loc.file, line)
    {
        let short: String = b.commit.chars().take(8).collect();
        suspects.push(Suspect {
            commit: b.commit.clone(),
            short: short.clone(),
            author: b.author.clone(),
            summary: b.summary.clone(),
            why: "last author of the exact failing line".into(),
        });
        return ChainStep {
            level: "underlying-cause".into(),
            text: format!(
                "the failing line was last modified in `{short}` — \"{}\" (by {}) — \
                     the strongest available candidate for where this behavior entered",
                b.summary, b.author
            ),
            confidence: Confidence::Observed,
            evidence: vec![
                Evidence {
                    cite: format!("commit {short}"),
                    note: b.summary.clone(),
                },
                Evidence {
                    cite: loc.cite(),
                    note: format!("blamed line, last author: {}", b.author),
                },
            ],
        };
    }

    let hist = gitintel::file_history(root, &loc.file, 5);
    if let Some(c) = hist.first() {
        suspects.push(Suspect {
            commit: c.sha.clone(),
            short: c.short.clone(),
            author: c.author.clone(),
            summary: c.summary.clone(),
            why: "most recent commit touching the failing file".into(),
        });
        return ChainStep {
            level: "underlying-cause".into(),
            text: format!(
                "line-level blame unavailable; most recent change to `{}` is `{}` — \
                 \"{}\" (by {})",
                loc.file, c.short, c.summary, c.author
            ),
            confidence: Confidence::Hypothesis,
            evidence: vec![Evidence {
                cite: format!("commit {}", c.short),
                note: format!("recent history of {}", loc.file),
            }],
        };
    }

    ChainStep {
        level: "underlying-cause".into(),
        text: "no git history for the failing file (uncommitted or not a repo) — \
               introduction point unknown"
            .into(),
        confidence: Confidence::Hypothesis,
        evidence: vec![Evidence {
            cite: loc.cite(),
            note: "no blame/history available".into(),
        }],
    }
}

fn systemic_cause(err: &NormalizedError, symbol: Option<&str>, sites: &[SymbolSite]) -> ChainStep {
    let Some(sym) = symbol else {
        return ChainStep {
            level: "systemic-cause".into(),
            text: "no symbol extracted from the failure message — class expansion \
                   needs an identifier to search for"
                .into(),
            confidence: Confidence::Hypothesis,
            evidence: vec![],
        };
    };

    if sym.trim().len() < 3 {
        return ChainStep {
            level: "systemic-cause".into(),
            text: format!(
                "`{sym}` is too short/common for reliable class expansion — skipped \
                 repository-wide matching to avoid noise"
            ),
            confidence: Confidence::Hypothesis,
            evidence: vec![Evidence {
                cite: format!("symbol `{sym}`"),
                note: "below minimum length for class expansion".into(),
            }],
        };
    }

    if sites.is_empty() {
        return ChainStep {
            level: "systemic-cause".into(),
            text: format!(
                "`{sym}` was not found elsewhere in the repo (unresolved, generated, or \
                 external) — blast radius cannot be broadened from a source search"
            ),
            confidence: Confidence::Hypothesis,
            evidence: vec![Evidence {
                cite: format!("symbol `{sym}`"),
                note: "repository search returned no other references".into(),
            }],
        };
    }

    let same_site = |s: &SymbolSite| {
        err.location
            .as_ref()
            .is_some_and(|l| s.file == l.file && Some(s.line) == l.line)
    };
    let others = sites.iter().filter(|s| !same_site(s)).count();

    if others == 0 {
        return ChainStep {
            level: "systemic-cause".into(),
            text: format!("`{sym}` appears only at the failing site — contained failure"),
            confidence: Confidence::Observed,
            evidence: vec![Evidence {
                cite: format!("symbol `{sym}`"),
                note: "1 site".into(),
            }],
        };
    }

    // Sites are pre-sorted (code before docs), so the first 5 are the
    // actionable ones.
    let preview: Vec<String> = sites
        .iter()
        .filter(|s| !same_site(s))
        .take(5)
        .map(|s| format!("{}:{}", s.file, s.line))
        .collect();

    ChainStep {
        level: "systemic-cause".into(),
        text: format!(
            "`{sym}` is referenced at {others} other sites — the same failure class can \
             surface anywhere this pattern repeats"
        ),
        confidence: Confidence::Observed,
        evidence: vec![Evidence {
            cite: format!("{} sites", sites.len()),
            note: format!("e.g. {}", preview.join(", ")),
        }],
    }
}

/// Sort key: docs/config/lockfiles last, real source first.
fn docs_last(file: &str) -> u8 {
    let lower = file.to_ascii_lowercase();
    let is_doc = lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.ends_with(".toml")
        || lower.ends_with(".lock")
        || lower.ends_with(".json")
        || lower.contains("/fixtures/")
        || lower.contains("/target/")
        || lower.contains("/.github/");
    u8::from(is_doc)
}

fn prevention(err: &NormalizedError) -> ChainStep {
    let guard = match err.kind {
        ErrorKind::Compile | ErrorKind::Type => {
            "a compile/type check or lint rule rejecting this pattern at every site \
             (cheapest: lint rule; then unit test)"
        }
        ErrorKind::Test => {
            "a regression test pinned to this failure's fingerprint, mutation-checked \
             to fail on the reverted fix"
        }
        ErrorKind::Lint => {
            "promote the violated rule to a blocking CI check so it cannot land again"
        }
        ErrorKind::Dep => "a dependency preflight (lockfile/schema validation) in CI before build",
        ErrorKind::Build | ErrorKind::Ci => "a fast pre-merge check covering the broken build step",
        ErrorKind::Config => "an environment validation script run before deploy",
        ErrorKind::Runtime => {
            "a guard at the failing site plus a test asserting recovered behavior"
        }
        ErrorKind::Db | ErrorKind::Api => "a contract/schema check validated in CI",
        ErrorKind::Unknown => {
            "identify the cheapest deterministic check that fails on this bug \
             and passes on main"
        }
    };
    ChainStep {
        level: "prevention".into(),
        text: format!("prevent recurrence: {guard}"),
        confidence: Confidence::Hypothesis,
        evidence: vec![Evidence {
            cite: format!("class {}", err.kind),
            note: "cheapest-guard heuristic for this failure kind".into(),
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::Location;

    #[test]
    fn chain_has_five_levels() {
        let err = NormalizedError {
            kind: ErrorKind::Compile,
            message: "cannot find value `my_thing`".into(),
            symbol: Some("my_thing".into()),
            location: Some(Location {
                file: "src/main.rs".into(),
                line: Some(1),
                column: Some(1),
            }),
            frames: vec![],
            parse_confident: true,
        };
        let root = std::env::temp_dir();
        let chain = build(&err, &root);
        let levels: Vec<&str> = chain.steps.iter().map(|s| s.level.as_str()).collect();
        assert_eq!(
            levels,
            vec![
                "symptom",
                "immediate-cause",
                "underlying-cause",
                "systemic-cause",
                "prevention"
            ]
        );
        // Symptom must be observed + cited.
        assert_eq!(chain.steps[0].confidence, Confidence::Observed);
        assert!(!chain.steps[0].evidence.is_empty());
        // Prevention is always a hypothesis in V1 (we suggest, not verify).
        assert_eq!(chain.steps[4].confidence, Confidence::Hypothesis);
    }
}
