//! Failure memory (V3): what RootGuard has already seen, whether a human
//! agreed with the verdict, and whether the old evidence still holds.
//!
//! Records live in `<root>/.rootguard/failures/*.yaml`, one file per
//! fingerprint class. Nothing here executes commands or touches the network.
//! Reads are best-effort (a corrupt or foreign file is skipped, never fatal);
//! writes are atomic (temp file + rename).
//!
//! Two rules keep the memory honest:
//!
//! * **Decay** — a stored verification tier is evidence about the *past*.
//!   [`STALE_DAYS`] after it was earned it drops one rank per further
//!   [`STALE_DAYS`] days, floored at `possible`, until a fresh ladder run
//!   revalidates it.
//! * **Revalidation** — only a new `--verify` run refreshes `verified_at`.
//!   Merely seeing the failure again updates sightings, not trust.
//!
//! Triage feedback (`explain --triage correct|wrong`) is what [`calibrate`]
//! aggregates: agreement per tier, so over-confident tier wording becomes
//! measurable instead of assumed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::reason::Tier;
use crate::report::Report;
use crate::verify::Verification;

/// Schema version of a memory record (bump on breaking changes).
pub const SCHEMA: u32 = 1;

/// Verification tiers go stale after this many days, and decay one rank
/// per full window elapsed since then.
pub const STALE_DAYS: u64 = 30;

const DAY_SECS: u64 = 86_400;

/// A recorded triage verdict for one sighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    Correct,
    Wrong,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Feedback {
    pub at: String,
    pub verdict: Verdict,
}

/// One failure class as remembered on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Record {
    pub rootguard: u32,
    pub fingerprint: String,
    pub kind: String,
    pub message: String,
    pub first_seen: String,
    pub last_seen: String,
    pub observations: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_tier: Option<Tier>,
    /// Full ladder record from the last verification, kept as evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feedback: Vec<Feedback>,
}

/// Decay applied: the record as of *now*, for embedding in a report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Recall {
    pub first_seen: String,
    pub last_seen: String,
    pub observations: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_tier: Option<Tier>,
    /// Tier after decay — what is trustworthy today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tier: Option<Tier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_days: Option<u64>,
    pub stale: bool,
    pub correct: u64,
    pub wrong: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TierScore {
    pub tier: String,
    pub correct: u64,
    pub wrong: u64,
}

/// Triage calibration across the whole failure memory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Calibration {
    pub rootguard: u32,
    pub records: usize,
    pub verified_records: usize,
    pub correct: u64,
    pub wrong: u64,
    pub by_tier: Vec<TierScore>,
}

impl Recall {
    pub fn to_text(&self) -> String {
        let mut o = String::new();
        o.push_str("Failure memory\n");
        o.push_str(&"-".repeat(72));
        o.push('\n');
        o.push_str(&format!(
            "  seen {} times since {} (last {})\n",
            self.observations,
            date(&self.first_seen),
            date(&self.last_seen)
        ));
        match (self.verified_tier, self.verified_at.as_deref()) {
            (Some(stored), Some(at)) => {
                let cur = self.current_tier.unwrap_or(stored);
                let age = self.age_days.unwrap_or(0);
                let at = date(at);
                if self.stale {
                    o.push_str(&format!(
                        "  last verified {at} ({age}d ago): {stored} → decayed to {cur}\n"
                    ));
                    o.push_str(
                        "  revalidate: re-run `rootguard explain --verify --test '<cmd>'`\n",
                    );
                } else {
                    o.push_str(&format!(
                        "  last verified {at} ({age}d ago): {cur} (fresh)\n"
                    ));
                }
            }
            (Some(stored), None) => {
                o.push_str(&format!(
                    "  verified {stored} on an unparseable date — treated as stale\n"
                ));
            }
            _ => o.push_str("  never verified — seeing it before is not proof\n"),
        }
        if self.correct + self.wrong > 0 {
            o.push_str(&format!(
                "  triage: {} correct / {} wrong\n",
                self.correct, self.wrong
            ));
        }
        o
    }
}

impl Calibration {
    pub fn to_text(&self) -> String {
        let mut o = String::new();
        o.push_str("RootGuard calibration\n");
        o.push_str(&"=".repeat(72));
        o.push('\n');
        o.push_str(&format!(
            "records       : {} under .rootguard/failures\n",
            self.records
        ));
        let total = self.correct + self.wrong;
        if total == 0 {
            o.push_str(
                "triage        : no verdicts yet — record one with \
                 `rootguard explain --triage correct|wrong`\n",
            );
            if self.records == 0 {
                o.push_str("                (no failure memory yet — run explain on a failure)\n");
            }
            return o;
        }
        o.push_str(&format!(
            "triage        : {total} verdicts — {} correct, {} wrong ({}% agreement)\n",
            self.correct,
            self.wrong,
            self.correct * 100 / total
        ));
        o.push('\n');
        o.push_str("agreement by tier at last verification:\n");
        for s in &self.by_tier {
            o.push_str(&format!(
                "  {:<16}{:>3} correct   {:>3} wrong\n",
                s.tier, s.correct, s.wrong
            ));
        }
        if self.wrong > 0 {
            o.push('\n');
            o.push_str(
                "note: a tier whose verdicts keep coming back wrong is over-promising —\n\
                 \x20     distrust it until a fresh --verify earns it back.\n",
            );
        }
        o
    }
}

/// Directory holding the records for `root`.
pub fn dir(root: &Path) -> PathBuf {
    root.join(".rootguard").join("failures")
}

/// Read the record for `fingerprint`, if one exists and parses.
pub fn load(root: &Path, fingerprint: &str) -> Option<Record> {
    let file = find_file(&dir(root), fingerprint)?;
    let text = std::fs::read_to_string(&file).ok()?;
    serde_yaml::from_str(&text).ok()
}

/// Record this observation (and optional triage verdict), then return the
/// decay-applied view for the report.
///
/// Sightings always count; `verified_at` / tier refresh **only** when the
/// report carries a verification — revalidation must be earned by the ladder.
pub fn remember(root: &Path, report: &Report, verdict: Option<Verdict>) -> anyhow::Result<Recall> {
    let now = now_secs();
    let stamp = rfc3339(now);
    let fp = &report.observed.fingerprint;

    let mut rec = load(root, fp).unwrap_or_else(|| Record {
        rootguard: SCHEMA,
        fingerprint: fp.clone(),
        kind: report.normalized.kind.clone(),
        message: report.normalized.message.clone(),
        first_seen: stamp.clone(),
        last_seen: stamp.clone(),
        observations: 0,
        last_head: None,
        verified_at: None,
        verified_head: None,
        verified_tier: None,
        verification: None,
        guards: vec![],
        feedback: vec![],
    });
    rec.observations += 1;
    rec.last_seen = stamp.clone();
    rec.kind = report.normalized.kind.clone();
    rec.message = report.normalized.message.clone();
    rec.last_head = report.observed.git.head.clone();

    if let Some(v) = &report.verification {
        rec.verified_at = Some(stamp.clone());
        rec.verified_head = report.observed.git.head.clone();
        rec.verified_tier = Some(v.tier);
        rec.verification = Some(v.clone());
        rec.guards = report.guards.clone();
    }
    if let Some(vd) = verdict {
        rec.feedback.push(Feedback {
            at: stamp,
            verdict: vd,
        });
    }

    save(root, &rec)?;
    Ok(view(&rec, now))
}

/// Decay-applied view of `record` as of `now_secs`.
pub fn view(record: &Record, now_secs: u64) -> Recall {
    let parsed = record
        .verified_at
        .as_deref()
        .and_then(parse_rfc3339)
        .map(|vs| now_secs.saturating_sub(vs) / DAY_SECS);
    // A tier whose date is missing or unparseable cannot be shown as fresh.
    let age_days = match (record.verified_tier, parsed) {
        (Some(_), Some(age)) => Some(age),
        (Some(_), None) => Some(STALE_DAYS),
        (None, _) => None,
    };
    let current = record
        .verified_tier
        .map(|t| decay(t, age_days.unwrap_or(STALE_DAYS)));
    Recall {
        first_seen: record.first_seen.clone(),
        last_seen: record.last_seen.clone(),
        observations: record.observations,
        verified_at: record.verified_at.clone(),
        verified_tier: record.verified_tier,
        current_tier: current,
        age_days,
        stale: age_days.is_some_and(|d| d >= STALE_DAYS),
        correct: count_verdict(record, Verdict::Correct),
        wrong: count_verdict(record, Verdict::Wrong),
    }
}

/// One rank down per elapsed [`STALE_DAYS`] window, floored at `possible`.
pub fn decay(tier: Tier, age_days: u64) -> Tier {
    let drops = u8::try_from(age_days / STALE_DAYS).unwrap_or(u8::MAX);
    Tier::from_rank(tier.rank().saturating_sub(drops))
}

/// Aggregate triage verdicts across the whole failure memory.
pub fn calibrate(root: &Path) -> Calibration {
    let mut records = Vec::new();
    let d = dir(root);
    if let Ok(entries) = std::fs::read_dir(&d) {
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("yaml") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&p)
                && let Ok(rec) = serde_yaml::from_str::<Record>(&text)
            {
                records.push(rec);
            }
        }
    }
    records.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));

    // Buckets in fixed order: the three verified tiers, then never-verified.
    let mut counts: [(u64, u64); 4] = [(0, 0); 4];
    for r in &records {
        let bucket = match r.verified_tier {
            Some(Tier::Confirmed) => 0,
            Some(Tier::Likely) => 1,
            Some(Tier::Possible) => 2,
            None => 3,
        };
        for f in &r.feedback {
            match f.verdict {
                Verdict::Correct => counts[bucket].0 += 1,
                Verdict::Wrong => counts[bucket].1 += 1,
            }
        }
    }
    const LABELS: [&str; 4] = ["confirmed", "likely", "possible", "never-verified"];
    let by_tier = LABELS
        .iter()
        .zip(counts)
        .filter(|&(_, (c, w))| c + w > 0)
        .map(|(label, (correct, wrong))| TierScore {
            tier: (*label).to_string(),
            correct,
            wrong,
        })
        .collect();

    Calibration {
        rootguard: SCHEMA,
        records: records.len(),
        verified_records: records.iter().filter(|r| r.verified_tier.is_some()).count(),
        correct: counts.iter().map(|(c, _)| *c).sum(),
        wrong: counts.iter().map(|(_, w)| *w).sum(),
        by_tier,
    }
}

fn count_verdict(record: &Record, verdict: Verdict) -> u64 {
    record
        .feedback
        .iter()
        .filter(|f| f.verdict == verdict)
        .count() as u64
}

/// First file in `dir` whose stored fingerprint matches — scanning by content
/// sidesteps filename collisions entirely (`a__b` vs `a/b` normalize alike).
fn find_file(dir: &Path, fingerprint: &str) -> Option<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("yaml"))
        .collect();
    paths.sort();
    for p in paths {
        if let Ok(text) = std::fs::read_to_string(&p)
            && let Ok(rec) = serde_yaml::from_str::<Record>(&text)
            && rec.fingerprint == fingerprint
        {
            return Some(p);
        }
    }
    None
}

fn save(root: &Path, record: &Record) -> anyhow::Result<()> {
    let d = dir(root);
    std::fs::create_dir_all(&d)?;
    ensure_git_ignored(root);
    let target =
        find_file(&d, &record.fingerprint).unwrap_or_else(|| free_path(&d, &record.fingerprint));
    let tmp = target.with_extension("yaml.tmp");
    std::fs::write(&tmp, serde_yaml::to_string(record)?)?;
    std::fs::rename(&tmp, &target)?;
    Ok(())
}

/// Keep RootGuard's own state out of the user's `git status`.
///
/// Appends `.rootguard/` to the repository's **local** exclude file
/// (`.git/info/exclude`) — never the tracked `.gitignore`, never the index,
/// never a ref. Without this, our records would flip every snapshot to
/// `dirty` and block T3's clean-tree good-endpoint detection. Best-effort:
/// an unusual or read-only repo simply stays un-ignored (memory still works,
/// the tree just shows our directory as untracked).
fn ensure_git_ignored(root: &Path) {
    let git_path = root.join(".git");
    let git_dir = if git_path.is_dir() {
        git_path
    } else if git_path.is_file() {
        // Linked worktree: `.git` is a pointer file (`gitdir: …`).
        let Ok(text) = std::fs::read_to_string(&git_path) else {
            return;
        };
        let Some(rest) = text.trim().strip_prefix("gitdir:") else {
            return;
        };
        let p = Path::new(rest.trim());
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    } else {
        return; // not a git repository
    };

    let exclude = git_dir.join("info").join("exclude");
    if let Ok(existing) = std::fs::read_to_string(&exclude)
        && existing
            .lines()
            .any(|l| l.trim() == ".rootguard/" || l.trim() == ".rootguard")
    {
        return;
    }
    let mut content = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("# RootGuard failure memory (local state)\n.rootguard/\n");
    if let Some(parent) = exclude.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&exclude, content);
}

/// Deterministic first-available name for a new record.
fn free_path(dir: &Path, fingerprint: &str) -> PathBuf {
    let base: String = fingerprint
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let first = dir.join(format!("{base}.yaml"));
    if !first.exists() {
        return first;
    }
    for i in 2..1000 {
        let p = dir.join(format!("{base}.{i}.yaml"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{base}.{}.yaml", std::process::id()))
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// UTC seconds since the epoch → RFC 3339 (`2026-10-10T09:58:59Z`).
pub fn rfc3339(secs: u64) -> String {
    let days = (secs / DAY_SECS) as i64;
    let rem = secs % DAY_SECS;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// RFC 3339 (Z-form, as emitted by [`rfc3339`]) → UTC seconds, `None` when
/// the input isn't a date this module could have written.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let mo: u32 = dp.next()?.parse().ok()?;
    let d: u32 = dp.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || d == 0 || d > 31 || dp.next().is_some() {
        return None;
    }
    let mut tp = time.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let mi: u64 = tp.next()?.parse().ok()?;
    let se: u64 = tp.next()?.parse().ok()?;
    if tp.next().is_some() || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    (days >= 0).then_some(days as u64 * DAY_SECS + h * 3600 + mi * 60 + se)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// `2026-10-10T09:58:59Z` → `2026-10-10` for compact report lines.
fn date(stamp: &str) -> &str {
    if stamp.len() >= 10 {
        &stamp[..10]
    } else {
        stamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reason::Confidence;
    use crate::verify::{T2Citations, T3Mutation};

    fn trace() -> String {
        "Traceback (most recent call last):\n  File \"/tmp/x/app.py\", line 2, in <module>\n\
         \x20   raise ValueError(\"boom\")\nValueError: boom\n"
            .to_string()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rootguard-mem-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn verified_report(root: &Path, tier: Tier) -> Report {
        let mut report = crate::analyze(&trace(), root, "t", Some(1));
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
        report.guards = vec!["check | python3 app.py | runtime/app.py".into()];
        let _ = Confidence::Observed;
        report
    }

    #[test]
    fn rfc3339_round_trips() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        for secs in [1_000_000_000, 1_700_000_000, 1_800_000_000, 99_999_999_999] {
            assert_eq!(parse_rfc3339(&rfc3339(secs)), Some(secs), "secs {secs}");
        }
        assert_eq!(rfc3339(86_400), "1970-01-02T00:00:00Z");
        assert!(parse_rfc3339("not a date").is_none());
        assert!(parse_rfc3339("2026-13-01T00:00:00Z").is_none());
        assert!(parse_rfc3339("2026-10-10").is_none());
    }

    #[test]
    fn decay_drops_one_rank_per_window_and_floors() {
        assert_eq!(decay(Tier::Confirmed, 0), Tier::Confirmed);
        assert_eq!(decay(Tier::Confirmed, 29), Tier::Confirmed);
        assert_eq!(decay(Tier::Confirmed, 30), Tier::Likely);
        assert_eq!(decay(Tier::Confirmed, 59), Tier::Likely);
        assert_eq!(decay(Tier::Confirmed, 60), Tier::Possible);
        assert_eq!(decay(Tier::Confirmed, 36_500), Tier::Possible);
        assert_eq!(decay(Tier::Likely, 30), Tier::Possible);
        assert_eq!(decay(Tier::Likely, 90), Tier::Possible, "floor");
        assert_eq!(decay(Tier::Possible, 90), Tier::Possible);
    }

    #[test]
    fn sightings_accumulate_without_revalidation() {
        let root = scratch("sightings");
        let report = crate::analyze(&trace(), &root, "t", Some(1));

        let v1 = remember(&root, &report, None).unwrap();
        assert_eq!(v1.observations, 1);
        assert!(v1.verified_tier.is_none(), "sighting is not verification");
        assert!(!v1.stale);

        let v2 = remember(&root, &report, Some(Verdict::Correct)).unwrap();
        assert_eq!(v2.observations, 2);
        assert_eq!(v2.correct, 1);
        assert_eq!(v2.verified_tier, None);

        // The record exists on disk with both sightings and the feedback.
        let rec = load(&root, &report.observed.fingerprint).unwrap();
        assert_eq!(rec.observations, 2);
        assert_eq!(rec.feedback.len(), 1);
        assert_eq!(rec.first_seen, v1.first_seen);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn verify_revalidates_and_age_decays_it_back() {
        let root = scratch("revalidates");
        let report = verified_report(&root, Tier::Confirmed);

        let fresh = remember(&root, &report, None).unwrap();
        assert_eq!(fresh.verified_tier, Some(Tier::Confirmed));
        assert_eq!(fresh.current_tier, Some(Tier::Confirmed));
        assert!(!fresh.stale);
        assert_eq!(fresh.age_days, Some(0));

        // Age the stored verification by 70 days and re-read it.
        let mut rec = load(&root, &report.observed.fingerprint).unwrap();
        rec.verified_at = Some(rfc3339(now_secs() - 70 * DAY_SECS));
        let aged = view(&rec, now_secs());
        assert_eq!(aged.verified_tier, Some(Tier::Confirmed), "history kept");
        assert_eq!(aged.current_tier, Some(Tier::Possible), "decayed 70d");
        assert!(aged.stale);
        assert_eq!(aged.age_days, Some(70));

        // A fresh ladder run revalidates: trust is earned back.
        let again = remember(&root, &report, None).unwrap();
        assert!(!again.stale);
        assert_eq!(again.current_tier, Some(Tier::Confirmed));
        assert_eq!(again.observations, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_and_foreign_files_are_ignored() {
        let root = scratch("corrupt");
        let d = dir(&root);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("broken.yaml"), "fingerprint: [oops\n").unwrap();
        std::fs::write(d.join("foreign.yaml"), "hello: not a record\n").unwrap();

        let report = crate::analyze(&trace(), &root, "t", None);
        // Not recalled as ours …
        assert!(load(&root, &report.observed.fingerprint).is_none());
        // … but observing still works and lands beside them.
        let v = remember(&root, &report, None).unwrap();
        assert_eq!(v.observations, 1);

        // A same-named file holding a *different* fingerprint is not ours.
        let other = d.join("collision.yaml");
        let mut foreign = record_for("compile/other/thing_rs");
        foreign.message = "not our failure".into();
        std::fs::write(&other, serde_yaml::to_string(&foreign).unwrap()).unwrap();
        assert!(load(&root, "compile/other/thing_rs").is_some());
        assert!(load(&root, "compile/different/fp").is_none());

        // Calibrate survives the garbage too.
        let cal = calibrate(&root);
        assert!(cal.records >= 2, "foreign + ours: {cal:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn calibration_buckets_by_verified_tier() {
        let root = scratch("calibrate");
        let mut confirmed = record_for("runtime/boom/app_py");
        confirmed.verified_tier = Some(Tier::Confirmed);
        confirmed.feedback = vec![
            Feedback {
                at: rfc3339(0),
                verdict: Verdict::Correct,
            },
            Feedback {
                at: rfc3339(0),
                verdict: Verdict::Correct,
            },
        ];
        let mut unverified = record_for("type/forgot/ts");
        unverified.feedback = vec![Feedback {
            at: rfc3339(0),
            verdict: Verdict::Wrong,
        }];
        let d = dir(&root);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.yaml"), serde_yaml::to_string(&confirmed).unwrap()).unwrap();
        std::fs::write(
            d.join("b.yaml"),
            serde_yaml::to_string(&unverified).unwrap(),
        )
        .unwrap();

        let cal = calibrate(&root);
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
        assert!(text.contains("3 verdicts"), "{text}");
        assert!(text.contains("66% agreement"), "{text}");
        assert!(text.contains("over-promising"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recall_text_reports_decay_and_never_verified() {
        let mut r = Recall {
            first_seen: "2026-01-01T00:00:00Z".into(),
            last_seen: "2026-10-10T00:00:00Z".into(),
            observations: 3,
            verified_at: Some("2026-08-01T00:00:00Z".into()),
            verified_tier: Some(Tier::Confirmed),
            current_tier: Some(Tier::Possible),
            age_days: Some(70),
            stale: true,
            correct: 1,
            wrong: 1,
        };
        let text = r.to_text();
        assert!(text.contains("seen 3 times since 2026-01-01"), "{text}");
        assert!(text.contains("CONFIRMED → decayed to POSSIBLE"), "{text}");
        assert!(text.contains("revalidate"), "{text}");
        assert!(text.contains("1 correct / 1 wrong"), "{text}");

        r.verified_tier = None;
        r.current_tier = None;
        r.age_days = None;
        r.stale = false;
        r.correct = 0;
        r.wrong = 0;
        let text = r.to_text();
        assert!(text.contains("never verified"), "{text}");
        assert!(!text.contains("triage"), "{text}");
    }

    fn record_for(fp: &str) -> Record {
        Record {
            rootguard: SCHEMA,
            fingerprint: fp.into(),
            kind: "runtime".into(),
            message: "boom".into(),
            first_seen: rfc3339(0),
            last_seen: rfc3339(0),
            observations: 1,
            last_head: None,
            verified_at: None,
            verified_head: None,
            verified_tier: None,
            verification: None,
            guards: vec![],
            feedback: vec![],
        }
    }
}
