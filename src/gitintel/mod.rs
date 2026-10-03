use std::path::Path;
use std::process::Command;

pub mod bisect;

#[derive(Debug, Clone)]
pub struct GitContext {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub clean: bool,
}

#[derive(Debug, Clone)]
pub struct BlameLine {
    pub commit: String,
    pub author: String,
    pub summary: String,
    pub time: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub short: String,
    pub author: String,
    pub summary: String,
    pub when: Option<String>,
}

pub fn context(root: &Path) -> GitContext {
    GitContext {
        head: git(root, &["rev-parse", "HEAD"]).ok(),
        branch: git(root, &["rev-parse", "--abbrev-ref", "HEAD"]).ok(),
        clean: git(root, &["status", "--porcelain"])
            .map(|s| s.trim().is_empty())
            .unwrap_or(false),
    }
}

/// Blame a single line: who last touched `file:line`.
pub fn blame_line(root: &Path, file: &str, line: u32) -> Option<BlameLine> {
    let out = git(
        root,
        &[
            "blame",
            "-L",
            &format!("{line},{line}"),
            "--porcelain",
            "--",
            file,
        ],
    )
    .ok()?;
    let mut commit = String::new();
    let mut author = String::new();
    let mut summary = String::new();
    let mut time = None;
    for l in out.lines() {
        if commit.is_empty() {
            // First porcelain line: `<sha> <src-line> <dst-line> [<num-groups>]`
            if let Some(first) = l.split_whitespace().next()
                && is_sha(first)
            {
                commit = first.to_string();
                continue;
            }
        }
        if let Some(rest) = l.strip_prefix("author ") {
            if author.is_empty() {
                author = rest.to_string();
            }
        } else if let Some(rest) = l.strip_prefix("author-time ") {
            if time.is_none() {
                time = Some(rest.to_string());
            }
        } else if let Some(rest) = l.strip_prefix("summary ")
            && summary.is_empty()
        {
            summary = rest.to_string();
        }
    }
    if commit.is_empty() {
        return None;
    }
    Some(BlameLine {
        commit,
        author,
        summary,
        time,
    })
}

/// Recent commits touching `file` (fallback when line blame is unavailable).
pub fn file_history(root: &Path, file: &str, limit: usize) -> Vec<Commit> {
    let out = match git(
        root,
        &[
            "log",
            &format!("-n{limit}"),
            "--pretty=format:%H|%h|%an|%s|%aI",
            "--",
            file,
        ],
    ) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    parse_commits(&out)
}

/// `git bisect` automation: find the first commit where `test_cmd` fails.
///
/// Refuses on a dirty tree. Restores the original HEAD afterwards.
pub fn bisect(
    root: &Path,
    good: Option<&str>,
    bad: Option<&str>,
    test_cmd: &str,
) -> anyhow::Result<bisect::BisectResult> {
    bisect::run(root, good, bad, test_cmd)
}

fn git(root: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.first().unwrap_or(&""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

pub(crate) fn parse_commits(out: &str) -> Vec<Commit> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(5, '|');
            let sha = it.next()?.to_string();
            let short = it.next()?.to_string();
            let author = it.next()?.to_string();
            let summary = it.next()?.to_string();
            let when = it.next().map(|s| s.to_string());
            Some(Commit {
                sha,
                short,
                author,
                summary,
                when,
            })
        })
        .collect()
}

/// True for a full or abbreviated hex git object id.
fn is_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_detection() {
        assert!(is_sha("4241a6f947940caa59c4141ce99e7fafb5b3fbe1"));
        assert!(is_sha("4241a6f"));
        assert!(!is_sha("src/calc.py"));
        assert!(!is_sha(""));
    }

    #[test]
    fn parses_commit_lines() {
        let out = "abc123|abc123|Ada|fix parser|2026-01-01T00:00:00Z\ndef456|def456|Bob|wip|2026-01-02T00:00:00Z";
        let c = parse_commits(out);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].author, "Ada");
        assert_eq!(c[1].summary, "wip");
    }
}
