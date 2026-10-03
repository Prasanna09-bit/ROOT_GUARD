use serde::{Deserialize, Serialize};

use crate::gitintel::GitContext;
use crate::normalize::NormalizedError;
use crate::reason::Chain;

/// Schema version of the YAML report (bump on breaking changes).
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub rootguard: u32,
    pub observed: Observed,
    pub normalized: Normalized,
    pub analysis: Chain,
    /// V2 fills this with t1/t2/t3 verification results.
    pub verification: Option<serde_yaml::Value>,
    /// V2 fills this with generated regression guards.
    pub guards: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observed {
    pub source: String,
    pub fingerprint: String,
    pub git: GitSnapshot,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSnapshot {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub clean: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Normalized {
    pub kind: String,
    pub message: String,
    pub symbol: Option<String>,
    pub location: Option<String>,
    pub parse_confident: bool,
}

impl Report {
    pub fn new(
        source: &str,
        exit_code: Option<i32>,
        err: &NormalizedError,
        fingerprint: &str,
        git: &GitContext,
        chain: Chain,
    ) -> Self {
        Report {
            rootguard: SCHEMA,
            observed: Observed {
                source: source.to_string(),
                fingerprint: fingerprint.to_string(),
                git: GitSnapshot {
                    head: git.head.clone(),
                    branch: git.branch.clone(),
                    clean: git.clean,
                },
                exit_code,
            },
            normalized: Normalized {
                kind: err.kind.as_str().to_string(),
                message: err.message.clone(),
                symbol: err.symbol.clone(),
                location: err.location.as_ref().map(|l| l.cite()),
                parse_confident: err.parse_confident,
            },
            analysis: chain,
            verification: None,
            guards: vec![],
        }
    }

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).unwrap_or_else(|e| format!("# serialize error: {e}"))
    }

    pub fn to_text(&self) -> String {
        let mut o = String::new();
        o.push_str("RootGuard analysis\n");
        o.push_str(&"=".repeat(72));
        o.push('\n');

        o.push_str(&format!(
            "fingerprint : {}\nkind        : {}\nsource      : {}\n",
            self.observed.fingerprint, self.normalized.kind, self.observed.source
        ));
        if let Some(ec) = self.observed.exit_code {
            o.push_str(&format!("exit code   : {ec}\n"));
        }
        o.push_str(&format!(
            "git         : {} ({})\n",
            self.observed
                .git
                .head
                .as_deref()
                .map(|h| &h[..h.len().min(8)])
                .unwrap_or("no git"),
            if self.observed.git.clean {
                "clean"
            } else {
                "dirty"
            }
        ));
        if let Some(s) = &self.normalized.symbol {
            o.push_str(&format!("symbol      : {s}\n"));
        }
        o.push('\n');

        o.push_str("Failure chain (5 whys)\n");
        o.push_str(&"-".repeat(72));
        o.push('\n');
        for (i, step) in self.analysis.steps.iter().enumerate() {
            let tag = match step.confidence {
                crate::reason::Confidence::Observed => "OBSERVED",
                crate::reason::Confidence::Hypothesis => "HYPOTHESIS",
            };
            o.push_str(&format!("{}. [{}] {}\n", i + 1, step.level, tag));
            for line in wrap(&step.text, 68) {
                o.push_str(&format!("     {line}\n"));
            }
            for ev in &step.evidence {
                o.push_str(&format!("     → {}: {}\n", ev.cite, ev.note));
            }
            o.push('\n');
        }

        if !self.analysis.suspects.is_empty() {
            o.push_str("Suspect commits\n");
            o.push_str(&"-".repeat(72));
            o.push('\n');
            for s in &self.analysis.suspects {
                o.push_str(&format!(
                    "  {}  {}  {}\n      why: {}\n",
                    s.short, s.author, s.summary, s.why
                ));
            }
            o.push('\n');
        }

        if !self.analysis.sites.is_empty() {
            o.push_str(&format!(
                "Related sites in this failure class ({})\n",
                self.analysis.sites.len()
            ));
            o.push_str(&"-".repeat(72));
            o.push('\n');
            for site in self.analysis.sites.iter().take(20) {
                o.push_str(&format!("  {}:{}\n", site.file, site.line));
            }
            if self.analysis.sites.len() > 20 {
                o.push_str(&format!(
                    "  … and {} more\n",
                    self.analysis.sites.len() - 20
                ));
            }
            o.push('\n');
        }

        o.push_str(&format!(
            "verification : {}\nguards       : {} (V2)\n",
            if self.verification.is_some() {
                "present"
            } else {
                "not run (V2)"
            },
            self.guards.len()
        ));
        o
    }
}

/// Greedy word wrap — keeps output stable across terminals.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(cur);
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_is_stable() {
        let out = wrap("a b c d e f g h", 5);
        assert_eq!(out, vec!["a b c", "d e f", "g h"]);
    }

    #[test]
    fn wrap_empty() {
        assert_eq!(wrap("", 10), vec![""]);
    }
}
