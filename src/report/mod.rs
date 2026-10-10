use serde::{Deserialize, Serialize};

use crate::gitintel::GitContext;
use crate::normalize::NormalizedError;
use crate::reason::Chain;
use crate::verify::Verification;

/// Schema version of the YAML report (bump on breaking changes).
///
/// V2 fills the fields V1 declared as placeholders (`verification`, `guards`,
/// per-candidate `tier`); the shape is additive, so schema 1 still applies.
pub const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub rootguard: u32,
    pub observed: Observed,
    pub normalized: Normalized,
    pub analysis: Chain,
    /// T1/T2/T3 verification results — `None` unless `--verify` ran.
    pub verification: Option<Verification>,
    /// Generated regression guards (`check | fingerprint | cite`).
    pub guards: Vec<String>,
    /// Prior sightings of this failure, decay applied (V3) — `None` unless
    /// the failure memory was consulted (or disabled with `--no-memory`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<crate::memory::Recall>,
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
            memory: None,
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
        if let Some(m) = &self.memory {
            o.push_str(&m.to_text());
            o.push('\n');
        }

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
                    "  {}  {:<9}  {}  {}\n      why: {}\n",
                    s.short,
                    s.tier.to_string(),
                    s.author,
                    s.summary,
                    s.why
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
                o.push_str(&format!("  {}:{}  [{}]\n", site.file, site.line, site.tier));
            }
            if self.analysis.sites.len() > 20 {
                o.push_str(&format!(
                    "  … and {} more\n",
                    self.analysis.sites.len() - 20
                ));
            }
            o.push('\n');
        }

        o.push_str(&self.ladder_text());
        o
    }

    /// Verification ladder + guards block (V2).
    fn ladder_text(&self) -> String {
        let mut o = String::new();
        match &self.verification {
            None => {
                o.push_str(
                    "verification : not run (pass --verify to execute the T1/T2/T3 ladder)\n",
                );
            }
            Some(v) => {
                o.push_str("Verification ladder\n");
                o.push_str(&"-".repeat(72));
                o.push('\n');
                match &v.t1_instance {
                    Some(t) => {
                        let state = if t.reproduced {
                            "reproduced"
                        } else {
                            "NOT REPRODUCED"
                        };
                        let fp = if t.observed_fingerprint == t.expected_fingerprint {
                            "match".to_string()
                        } else {
                            format!("{} vs {}", t.observed_fingerprint, t.expected_fingerprint)
                        };
                        o.push_str(&format!(
                            "T1 instance  : {state} (exit {}, fingerprint {fp})\n",
                            t.exit_code
                        ));
                    }
                    None => o.push_str("T1 instance  : not run (no --test command)\n"),
                }
                let t2 = &v.t2_citations;
                o.push_str(&format!(
                    "T2 citations : {}/{} resolved\n",
                    t2.resolved, t2.checked
                ));
                for cite in &t2.unresolved {
                    o.push_str(&format!("               unresolved: {cite}\n"));
                }
                match &v.t3_mutation {
                    Some(t) => {
                        if t.mutation_checked {
                            o.push_str(&format!(
                                "T3 mutation  : `{}` fails at HEAD, passes at {} — guard verified\n",
                                t.check,
                                t.good.as_deref().unwrap_or("good")
                            ));
                        } else {
                            let state = match (t.head_fails, t.good_passes) {
                                (false, _) => "guard does not fail at HEAD",
                                (true, Some(false)) => "guard also fails at the good ref",
                                (true, None) => "no counterfactual run",
                                (true, Some(true)) => "not mutation-checked",
                            };
                            o.push_str(&format!("T3 mutation  : {state}\n"));
                        }
                    }
                    None => o.push_str("T3 mutation  : inconclusive (see notes)\n"),
                }
                for n in &v.notes {
                    o.push_str(&format!("  note: {n}\n"));
                }
                o.push_str(&format!("overall tier : {}\n", v.tier));
                o.push('\n');
            }
        }

        if self.guards.is_empty() {
            o.push_str("guards       : none generated\n");
        } else {
            o.push_str(&format!("Guards ({})\n", self.guards.len()));
            o.push_str(&"-".repeat(72));
            o.push('\n');
            for g in &self.guards {
                o.push_str(&format!("  {g}\n"));
            }
            o.push('\n');
        }
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
