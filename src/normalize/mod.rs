pub mod fingerprint;
pub mod parsers;

use serde::{Deserialize, Serialize};

/// Broad failure taxonomy (V1 subset — extended in later versions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorKind {
    Compile,
    Runtime,
    Test,
    Lint,
    Type,
    Dep,
    Build,
    Ci,
    Config,
    Db,
    Api,
    Unknown,
}

impl ErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::Compile => "compile",
            ErrorKind::Runtime => "runtime",
            ErrorKind::Test => "test",
            ErrorKind::Lint => "lint",
            ErrorKind::Type => "type",
            ErrorKind::Dep => "dep",
            ErrorKind::Build => "build",
            ErrorKind::Ci => "ci",
            ErrorKind::Config => "config",
            ErrorKind::Db => "db",
            ErrorKind::Api => "api",
            ErrorKind::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

impl Location {
    /// `file:line:col` or `file` when line unknown.
    pub fn cite(&self) -> String {
        match (self.line, self.column) {
            (Some(l), Some(c)) => format!("{}:{}:{}", self.file, l, c),
            (Some(l), None) => format!("{}:{}", self.file, l),
            _ => self.file.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedError {
    pub kind: ErrorKind,
    pub message: String,
    pub symbol: Option<String>,
    pub location: Option<Location>,
    pub frames: Vec<Location>,
    /// Confidence that the parser understood the input at all.
    pub parse_confident: bool,
}

/// Parse arbitrary error text into a normalized form.
///
/// Order matters: most specific parser first, generic last.
pub fn parse(raw: &str) -> NormalizedError {
    if let Some(e) = parsers::rustc(raw) {
        return e;
    }
    if let Some(e) = parsers::test_panic(raw) {
        return e;
    }
    if let Some(e) = parsers::typescript(raw) {
        return e;
    }
    if let Some(e) = parsers::python(raw) {
        return e;
    }
    if let Some(e) = parsers::node(raw) {
        return e;
    }
    parsers::generic(raw)
}

/// Pull the most likely meaningful identifier out of a message.
///
/// Prefers backticks (rustc/clippy), then quoted names, then an
/// ALL_LOWER/CamelCase token that isn't a common word.
pub fn extract_symbol(message: &str) -> Option<String> {
    let from_backtick = first_between(message, '`', '`');
    if let Some(s) = from_backtick
        && plausible_symbol(&s)
    {
        return Some(s);
    }
    let from_single = first_between(message, '\'', '\'');
    if let Some(s) = from_single
        && plausible_symbol(&s)
    {
        return Some(s);
    }
    let from_double = first_between(message, '"', '"');
    if let Some(s) = from_double
        && plausible_symbol(&s)
    {
        return Some(s);
    }
    // Fall back to the first token that "looks like" an identifier of interest.
    for tok in message.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')) {
        if tok.len() < 3 || tok.len() > 48 {
            continue;
        }
        if !tok.chars().next().is_some_and(|c| c.is_alphabetic()) {
            continue;
        }
        if COMMON_WORDS.contains(&tok) {
            continue;
        }
        if tok.chars().any(|c| c.is_uppercase()) && tok.chars().any(|c| c.is_lowercase()) {
            return Some(tok.to_string());
        }
        if tok.contains('_') {
            return Some(tok.to_string());
        }
    }
    None
}

const COMMON_WORDS: &[&str] = &[
    "error",
    "warning",
    "failed",
    "failure",
    "cannot",
    "found",
    "expected",
    "invalid",
    "missing",
    "undefined",
    "null",
    "true",
    "false",
    "string",
    "number",
    "thread",
    "process",
    "value",
    "tests",
    "assert",
    "assertion",
    "import",
    "export",
    "module",
    "package",
    "compile",
    "cannot",
    "exception",
    "traceback",
    "assertionerror",
    "typeerror",
    "referenceerror",
    "rangeerror",
];

fn plausible_symbol(s: &str) -> bool {
    let s = s.trim();
    if s.len() < 2 || s.len() > 64 {
        return false;
    }
    if s.contains(char::is_whitespace) {
        return false;
    }
    // Sentence-like backticked phrases are messages, not symbols.
    if s.contains('.') && !s.contains("::") && s.split('.').count() > 3 {
        return false;
    }
    !COMMON_WORDS.contains(&s)
}

fn first_between(s: &str, open: char, close: char) -> Option<String> {
    let start = s.find(open)? + open.len_utf8();
    let rest = &s[start..];
    let end = rest.find(close)?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_from_backticks() {
        let e = parse("error[E0425]: unresolved name `loop_thing`");
        assert_eq!(e.symbol.as_deref(), Some("loop_thing"));
        assert_eq!(e.kind, ErrorKind::Compile);
    }

    #[test]
    fn symbol_from_single_quotes() {
        let e = parse("TypeError: Cannot read properties of undefined (reading 'name')");
        assert_eq!(e.symbol.as_deref(), Some("name"));
    }

    #[test]
    fn plain_message_parses_fallback() {
        let e = parse("something went sideways");
        assert!(e.message.contains("sideways"));
    }
}
