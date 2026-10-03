use std::io::{IsTerminal, Read};
use std::path::Path;

pub mod runner;

/// Resolve user input into raw error text.
///
/// Accepts: inline text, a file path, or `-` / absent (stdin).
pub fn resolve_input(input: Option<&str>) -> anyhow::Result<(String, String)> {
    match input {
        None => read_stdin().map(|t| (t, "stdin".to_string())),
        Some("-") => read_stdin().map(|t| (t, "stdin".to_string())),
        Some(s) => {
            let p = Path::new(s);
            // A path that exists and is a file wins; otherwise treat as text.
            if p.is_file() && looks_like_path(s) {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| anyhow::anyhow!("cannot read {s}: {e}"))?;
                Ok((text, format!("file:{}", p.display())))
            } else if s.contains('\n') || s.contains("error") || s.contains("Error") {
                Ok((s.to_string(), "arg".to_string()))
            } else if p.is_file() {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| anyhow::anyhow!("cannot read {s}: {e}"))?;
                Ok((text, format!("file:{}", p.display())))
            } else {
                Ok((s.to_string(), "arg".to_string()))
            }
        }
    }
}

fn looks_like_path(s: &str) -> bool {
    !s.contains('\n') && (s.contains('/') || s.ends_with(".log") || s.ends_with(".txt"))
}

fn read_stdin() -> anyhow::Result<String> {
    if std::io::stdin().is_terminal() {
        anyhow::bail!(
            "no error text given — pass it inline, as a file path, or pipe it in \
             (e.g. `cargo build 2>&1 | rootguard explain -`)"
        );
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .map_err(|e| anyhow::anyhow!("cannot read stdin: {e}"))?;
    if buf.trim().is_empty() {
        anyhow::bail!("stdin was empty");
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_text_resolves() {
        let (t, src) = resolve_input(Some("error[E0308]: mismatched types")).unwrap();
        assert!(t.contains("mismatched"));
        assert_eq!(src, "arg");
    }

    #[test]
    fn multiline_resolves_as_arg() {
        let (t, src) = resolve_input(Some("line1\nline2")).unwrap();
        assert_eq!(t, "line1\nline2");
        assert_eq!(src, "arg");
    }
}
