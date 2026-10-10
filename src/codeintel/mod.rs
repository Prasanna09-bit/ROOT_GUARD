use std::path::{Path, PathBuf};

/// A place in the codebase where a symbol appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSite {
    pub file: String,
    pub line: u32,
    pub text: String,
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    "out",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
    ".vscode",
    ".rootguard",
    "vendor",
    ".next",
    "coverage",
];

const CODE_EXT: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt", "c", "h", "cpp", "hpp", "cs", "rb",
    "php", "swift", "scala", "sh", "sql", "proto", "graphql", "toml", "yaml", "yml", "json", "md",
    "txt", "html", "css", "vue", "svelte",
];

/// Minimum symbol length for class expansion. Short tokens (`a`, `i`, `x`)
/// match half the repository and produce useless noise.
const MIN_SYMBOL_LEN: usize = 3;

/// Express `file` relative to `root` when it lives inside it (resolving
/// symlinks in both operands); otherwise return it unchanged.
///
/// The one comparison rule for "is this the same file?": tool output may
/// report absolute paths (python tracebacks), repo walks may start from
/// `.` or an absolute root — both sides go through here before comparing.
pub fn relativize(root: &Path, file: &str) -> String {
    let p = Path::new(file);
    if p.is_absolute() {
        if let Ok(root_c) = root.canonicalize()
            && let Ok(rel) = p.strip_prefix(&root_c)
        {
            return rel.to_string_lossy().into_owned();
        }
        // macOS-style symlinked temp roots: canonicalize the file too
        // (requires it to exist, hence a separate step).
        if let Ok(root_c) = root.canonicalize()
            && let Ok(p_c) = p.canonicalize()
            && let Ok(rel) = p_c.strip_prefix(&root_c)
        {
            return rel.to_string_lossy().into_owned();
        }
        // Lexical fallback against the root as given (works for paths that
        // do not exist yet, e.g. citations to generated files).
        if let Ok(rel) = p.strip_prefix(root) {
            return rel.to_string_lossy().into_owned();
        }
        return file.to_string();
    }
    file.trim_start_matches("./").to_string()
}

/// Find all occurrences of `symbol` as a whole word under `root`.
///
/// Deterministic: pure regex + filesystem walk, no LSP required.
/// Returns at most `max` sites, sorted for stable output.
pub fn find_symbol(root: &Path, symbol: &str, max: usize) -> Vec<SymbolSite> {
    if symbol.trim().is_empty() || symbol.trim().len() < MIN_SYMBOL_LEN || max == 0 {
        return Vec::new();
    }
    let pattern = format!(r"\b{}\b", regex::escape(symbol));
    let re = match regex::RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };

    let mut hits: Vec<SymbolSite> = Vec::new();
    walk(root, &re, max, 0, &mut hits);
    // Report sites relative to `root` so they compose with locations from
    // tool output (which are usually cwd-relative), regardless of whether
    // the caller passed ".", "repo" or an absolute path.
    for s in &mut hits {
        if let Ok(r) = Path::new(&s.file).strip_prefix(root) {
            s.file = r.to_string_lossy().into_owned();
        }
    }
    hits.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    hits.truncate(max);
    hits
}

fn walk(dir: &Path, re: &regex::Regex, max: usize, depth: u32, hits: &mut Vec<SymbolSite>) {
    if depth > 12 || hits.len() >= max {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    // Sort for deterministic traversal.
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();

    for path in paths {
        if hits.len() >= max {
            return;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if SKIP_DIRS.contains(&name) {
                continue;
            }
            walk(&path, re, max, depth + 1, hits);
        } else if path.is_file() && is_code_file(name) {
            scan_file(&path, re, max, hits);
        }
    }
}

fn is_code_file(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => CODE_EXT.contains(&ext),
        None => false,
    }
}

fn scan_file(path: &Path, re: &regex::Regex, max: usize, hits: &mut Vec<SymbolSite>) {
    if hits.len() >= max {
        return;
    }
    // Skip huge files (minified bundles, lockfiles).
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > 1_000_000
    {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let rel = path.to_string_lossy().to_string();
    for (idx, line) in content.lines().enumerate() {
        if hits.len() >= max {
            return;
        }
        if re.is_match(line) {
            hits.push(SymbolSite {
                file: rel.clone(),
                line: idx as u32 + 1,
                text: line.trim().chars().take(160).collect(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_symbol_no_hits() {
        let dir = std::env::temp_dir();
        assert!(find_symbol(&dir, "", 10).is_empty());
    }
}
