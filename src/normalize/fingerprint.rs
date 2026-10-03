use super::{ErrorKind, NormalizedError};

/// Class key: structural identity of a failure.
///
/// Same failure class ⇒ same fingerprint, regardless of line numbers,
/// absolute paths, timestamps or counters in the message.
pub fn fingerprint(err: &NormalizedError) -> String {
    let sym = err
        .symbol
        .as_deref()
        .map(normalize_symbol)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "nosym".to_string());
    let path = err
        .location
        .as_ref()
        .map(|l| normalize_path(&l.file))
        .unwrap_or_else(|| "nofile".to_string());
    format!("{}/{}/{}", err.kind.as_str(), sym, path)
}

/// Lowercase, strip non-alphanumerics, drop trailing digits
/// (`foo2` → `foo`, `TS2304` → `ts`).
pub fn normalize_symbol(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c == '_' || c == '.' || c == ':' {
                '_'
            } else {
                '\0'
            }
        })
        .filter(|c| *c != '\0')
        .collect();
    let trimmed = cleaned.trim_matches('_');
    trimmed
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_matches('_')
        .to_string()
}

/// Keep the last two path segments, drop absolute prefixes, digits and
/// temp directories: `/home/x/proj/src/main.rs` → `src_main_rs`.
pub fn normalize_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    let p = strip_temp_prefix(&p);
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let tail = if segs.len() > 2 {
        &segs[segs.len() - 2..]
    } else {
        &segs[..]
    };
    let joined = tail.join("_");
    let cleaned: String = joined
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_');
    trimmed
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_matches('_')
        .to_string()
}

fn strip_temp_prefix(p: &str) -> String {
    for prefix in ["/tmp/", "/private/tmp/", "C:/Users/", "/var/folders/"] {
        if let Some(rest) = p.strip_prefix(prefix) {
            // Keep the last few segments so distinct temp projects stay distinct.
            let segs: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
            let keep = segs.len().saturating_sub(3);
            return segs[keep..].join("/");
        }
    }
    p.to_string()
}

/// Fingerprint from parts, used when only partial info is available.
pub fn fingerprint_parts(kind: ErrorKind, symbol: &str, file: &str) -> String {
    let sym = normalize_symbol(symbol);
    let path = normalize_path(file);
    format!(
        "{}/{}/{}",
        kind.as_str(),
        if sym.is_empty() { "nosym" } else { &sym },
        if path.is_empty() { "nofile" } else { &path }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(msg: &str, sym: Option<&str>, file: &str) -> NormalizedError {
        NormalizedError {
            kind: ErrorKind::Compile,
            message: msg.to_string(),
            symbol: sym.map(str::to_string),
            location: Some(crate::normalize::Location {
                file: file.to_string(),
                line: Some(99),
                column: Some(1),
            }),
            frames: vec![],
            parse_confident: true,
        }
    }

    #[test]
    fn same_failure_diff_lines_same_fingerprint() {
        let a = err("cannot find `user_id`", Some("user_id"), "/repo/src/db.rs");
        let b = err("cannot find `user_id`", Some("user_id"), "src/db.rs");
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn different_symbol_differs() {
        let a = err("x", Some("alpha"), "src/a.rs");
        let b = err("x", Some("beta"), "src/a.rs");
        assert_ne!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn symbol_normalization_drops_digits() {
        assert_eq!(normalize_symbol("error_2"), "error");
        assert_eq!(normalize_symbol("TS2304"), "ts");
        assert_eq!(normalize_symbol("myFn"), "myfn");
    }

    #[test]
    fn path_normalization_keeps_tail() {
        assert_eq!(normalize_path("/home/x/proj/src/main.rs"), "src_main_rs");
        assert_eq!(normalize_path("src/main.rs"), "src_main_rs");
    }
}
