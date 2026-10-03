use super::{ErrorKind, Location, NormalizedError};

/// rustc / cargo output.
///
/// ```text
/// error[E0425]: unresolved name `foo`
///   --> src/main.rs:12:9
/// ```
pub fn rustc(raw: &str) -> Option<NormalizedError> {
    let mut kind = None;
    let mut message = None;
    let mut location = None;
    let mut frames = Vec::new();
    let mut confident = false;

    for line in raw.lines() {
        let t = line.trim_start();
        if message.is_none() {
            if let Some(rest) = t.strip_prefix("error[") {
                // error[E0425]: msg
                if let Some(idx) = rest.find(']') {
                    let msg = rest[idx + 1..].trim_start_matches(':').trim();
                    if !msg.is_empty() {
                        message = Some(msg.to_string());
                        kind = Some(ErrorKind::Compile);
                        confident = true;
                    }
                }
            } else if let Some(rest) = t.strip_prefix("error:") {
                let msg = rest.trim();
                if !msg.is_empty() {
                    message = Some(msg.to_string());
                    kind = Some(ErrorKind::Compile);
                    confident = true;
                }
            } else if let Some(rest) = t.strip_prefix("warning:") {
                let msg = rest.trim();
                if !msg.is_empty() {
                    message = Some(msg.to_string());
                    kind = Some(ErrorKind::Lint);
                    confident = true;
                }
            } else if let Some(rest) = t.strip_prefix("panicked at") {
                let msg = rest.trim();
                if !msg.is_empty() {
                    message = Some(t.to_string());
                    kind = Some(ErrorKind::Runtime);
                    confident = true;
                }
            }
        }
        if let Some(rest) = t.strip_prefix("--> ")
            && let Some(loc) = parse_location(rest)
        {
            if location.is_none() {
                location = Some(loc.clone());
            }
            frames.push(loc);
        }
    }

    let message = message?;
    let symbol = super::extract_symbol(&message);
    Some(NormalizedError {
        kind: kind?,
        message,
        symbol,
        location,
        frames,
        parse_confident: confident,
    })
}

/// Rust test-harness panic:
/// ```text
/// thread 'test_parse_header' panicked at src/parse.rs:88:5:
/// assertion `left == right` failed
///   left: 12
///  right: 13
/// ```
pub fn test_panic(raw: &str) -> Option<NormalizedError> {
    let idx = raw.find("panicked at ")?;
    let after = &raw[idx + "panicked at ".len()..];
    // Location runs to the end of that line: `src/parse.rs:88:5:`
    let loc_line = after.lines().next().unwrap_or("");
    let loc_str = loc_line.trim().trim_end_matches(':');
    let location = parse_location(loc_str);

    // Body = lines after the panic header, before any `note:` trailer.
    let body_start = raw[idx..]
        .find('\n')
        .map(|i| idx + i + 1)
        .unwrap_or(raw.len());
    let body: String = raw[body_start..]
        .lines()
        .take_while(|l| {
            let t = l.trim();
            !t.starts_with("note:") && !t.starts_with("stack backtrace")
        })
        .collect::<Vec<&str>>()
        .join("\n")
        .trim()
        .to_string();

    let message = if body.is_empty() {
        format!("panicked at {loc_str}")
    } else {
        body
    };
    let symbol = super::extract_symbol(&message);
    let frames = location.iter().cloned().collect();
    Some(NormalizedError {
        kind: ErrorKind::Test,
        message,
        symbol,
        location,
        frames,
        parse_confident: true,
    })
}

/// tsc output: `src/a.ts(12,5): error TS2304: Cannot find name 'foo'.`
pub fn typescript(raw: &str) -> Option<NormalizedError> {
    let mut message = None;
    let mut location = None;
    let mut confident = false;

    for line in raw.lines() {
        let t = line.trim();
        if t.contains("error TS") || t.contains(": error TS") {
            if let Some(loc) = parse_tsloc(t)
                && location.is_none()
            {
                location = Some(loc);
            }
            if let Some(idx) = t.find("error TS") {
                let rest = &t[idx..];
                // error TS2304: Cannot find name 'foo'.
                if let Some(colon) = rest.find(':') {
                    let msg = rest[colon + 1..].trim();
                    if !msg.is_empty() {
                        message = Some(msg.to_string());
                        confident = true;
                    }
                }
            }
        }
    }

    let message = message?;
    let symbol = super::extract_symbol(&message);
    Some(NormalizedError {
        kind: ErrorKind::Type,
        message,
        symbol,
        frames: location.iter().cloned().collect(),
        location,
        parse_confident: confident,
    })
}

/// Python traceback.
pub fn python(raw: &str) -> Option<NormalizedError> {
    if !raw.contains("Traceback (most recent call last)") && !raw.contains(".py\", line ") {
        return None;
    }
    let mut frames: Vec<Location> = Vec::new();
    let mut message = None;
    let mut kind = ErrorKind::Runtime;

    for line in raw.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("File \"")
            && let Some(end) = rest.find('"')
        {
            let file = &rest[..end];
            let after = &rest[end + 1..];
            let lineno = after
                .split("line ")
                .nth(1)
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse::<u32>().ok());
            frames.push(Location {
                file: file.to_string(),
                line: lineno,
                column: None,
            });
        }
        // Final exception line: `SomeError: message`
        if let Some((head, tail)) = t.split_once(": ")
            && (head.ends_with("Error")
                || head.ends_with("Exception")
                || head.ends_with("Warning")
                || head == "Exception")
            && !tail.trim().is_empty()
        {
            message = Some(format!("{head}: {tail}"));
            kind = if head == "AssertionError" {
                ErrorKind::Test
            } else {
                ErrorKind::Runtime
            };
        }
    }

    let message = message?;
    // Python errors carry the meaningful name inside the message:
    //   NameError: name 'a' is not defined
    //   ImportError: cannot import name 'helper' from 'mod'
    //   KeyError: 'user_id'
    let symbol = extract_python_symbol(&message).or_else(|| super::extract_symbol(&message));
    // Deepest frame is where it blew up; first is entry point.
    let location = frames.last().cloned();
    Some(NormalizedError {
        kind,
        message,
        symbol,
        location,
        frames,
        parse_confident: true,
    })
}

/// Node / V8 runtime error with a JS stack trace.
pub fn node(raw: &str) -> Option<NormalizedError> {
    let mut message = None;
    let mut kind = ErrorKind::Runtime;
    let mut frames: Vec<Location> = Vec::new();

    for line in raw.lines() {
        let t = line.trim();
        if message.is_none() {
            for prefix in [
                "TypeError: ",
                "ReferenceError: ",
                "RangeError: ",
                "SyntaxError: ",
                "Error: ",
            ] {
                if let Some(rest) = t.strip_prefix(prefix)
                    && !rest.is_empty()
                {
                    message = Some(rest.to_string());
                    kind = ErrorKind::Runtime;
                    break;
                }
            }
        }
        if let Some(rest) = t.strip_prefix("at ") {
            // at foo (/abs/path/file.js:10:5)  |  at /path/file.js:10:5
            let inner = rest
                .rsplit_once('(')
                .map(|(_, p)| p.trim_end_matches(')'))
                .unwrap_or(rest);
            if let Some(loc) = parse_location(inner) {
                frames.push(loc);
            }
        }
    }

    let message = message?;
    let symbol = super::extract_symbol(&message);
    let location = frames.first().cloned();
    Some(NormalizedError {
        kind,
        message,
        symbol,
        location,
        frames,
        parse_confident: true,
    })
}

/// Last resort: treat the first non-empty line as the message.
pub fn generic(raw: &str) -> NormalizedError {
    let first = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown failure")
        .to_string();
    let symbol = super::extract_symbol(&first);
    let kind = classify_generic(raw, &first);
    NormalizedError {
        kind,
        message: first,
        symbol,
        location: None,
        frames: Vec::new(),
        parse_confident: false,
    }
}

fn classify_generic(raw: &str, message: &str) -> ErrorKind {
    let m = raw.to_ascii_lowercase();
    if m.contains("test result: failed") || m.contains("failures:") || m.contains("assertionfailed")
    {
        return ErrorKind::Test;
    }
    if m.contains("npm err!") || m.contains("could not compile") || m.contains("build failed") {
        return ErrorKind::Build;
    }
    if m.contains("cannot find module")
        || m.contains("modulenotfounderror")
        || m.contains("unresolved dependency")
    {
        return ErrorKind::Dep;
    }
    if m.contains("permission denied") || m.contains("command not found") {
        return ErrorKind::Config;
    }
    let lm = message.to_ascii_lowercase();
    if lm.starts_with("test ") || lm.contains("expected") && lm.contains("to equal") {
        return ErrorKind::Test;
    }
    ErrorKind::Unknown
}

/// `src/main.rs:12:9` or `src/main.rs:12`
fn parse_location(s: &str) -> Option<Location> {
    let s = s.trim().trim_end_matches(':');
    if s.is_empty() || s.contains(' ') {
        return None;
    }
    // Windows drive letters: `C:\path\file.rs:12:3` — the leading `C:` would
    // otherwise split as its own segment. Only fires when a backslash follows.
    let normalized: String = if let Some((drive, rest)) = s.split_once(':') {
        if drive.len() == 1
            && drive
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && rest.starts_with('\\')
        {
            format!("{drive}:{}", rest.replace('\\', "/"))
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    };

    let parts: Vec<&str> = normalized.split(':').collect();
    match parts.len() {
        1 => Some(Location {
            file: parts[0].to_string(),
            line: None,
            column: None,
        }),
        2 => Some(Location {
            file: parts[0].to_string(),
            line: parts[1].parse().ok(),
            column: None,
        }),
        _ => Some(Location {
            file: parts[..parts.len() - 2].join(":"),
            line: parts[parts.len() - 2].parse().ok(),
            column: parts[parts.len() - 1].parse().ok(),
        }),
    }
}

/// Pull the name Python is complaining about.
/// `name 'a' is not defined` → `a`, `cannot import name 'x'` → `x`, `KeyError: 'k'` → `k`.
fn extract_python_symbol(message: &str) -> Option<String> {
    for marker in ["name '", "name \"", "KeyError: '", "KeyError: \""] {
        if let Some(idx) = message.find(marker) {
            let rest = &message[idx + marker.len()..];
            let quote = marker.chars().last()?;
            if let Some(end) = rest.find(quote) {
                let name = &rest[..end];
                if !name.is_empty() && name.len() <= 64 && !name.contains(char::is_whitespace) {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// `src/a.ts(12,5): error ...`
fn parse_tsloc(s: &str) -> Option<Location> {
    let open = s.find('(')?;
    let close = s.find(')')?;
    if close <= open {
        return None;
    }
    let file = &s[..open];
    let inner = &s[open + 1..close];
    let mut it = inner.split(',');
    let line = it.next()?.trim().parse().ok();
    let column = it.next().and_then(|c| c.trim().parse().ok());
    Some(Location {
        file: file.trim().to_string(),
        line,
        column,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustc_error_with_location() {
        let raw = "\
error[E0425]: unresolved name `foo`
  --> src/main.rs:12:9
   |
12 |     foo();
   |     ^^^ not found";
        let e = rustc(raw).expect("should parse");
        assert_eq!(e.kind, ErrorKind::Compile);
        assert_eq!(e.symbol.as_deref(), Some("foo"));
        let loc = e.location.expect("location");
        assert_eq!(loc.file, "src/main.rs");
        assert_eq!(loc.line, Some(12));
        assert_eq!(loc.column, Some(9));
    }

    #[test]
    fn tsc_error() {
        let raw = "src/api.ts(44,13): error TS2304: Cannot find name 'Response'.";
        let e = typescript(raw).expect("should parse");
        assert_eq!(e.kind, ErrorKind::Type);
        assert_eq!(e.symbol.as_deref(), Some("Response"));
        assert_eq!(e.location.as_ref().unwrap().line, Some(44));
    }

    #[test]
    fn python_traceback() {
        let raw = "\
Traceback (most recent call last):
  File \"/app/main.py\", line 10, in <module>
    run()
  File \"/app/svc.py\", line 33, in run
    raise ValueError(\"bad input\")
ValueError: bad input";
        let e = python(raw).expect("should parse");
        assert_eq!(e.kind, ErrorKind::Runtime);
        let loc = e.location.expect("loc");
        assert_eq!(loc.file, "/app/svc.py");
        assert_eq!(loc.line, Some(33));
        assert_eq!(e.frames.len(), 2);
    }

    #[test]
    fn node_stack() {
        let raw = "\
TypeError: Cannot read properties of undefined (reading 'id')
    at getUser (/app/src/user.js:21:18)
    at /app/src/index.js:5:1";
        let e = node(raw).expect("should parse");
        assert_eq!(
            e.message,
            "Cannot read properties of undefined (reading 'id')"
        );
        let loc = e.location.expect("loc");
        assert_eq!(loc.file, "/app/src/user.js");
        assert_eq!(loc.line, Some(21));
    }

    #[test]
    fn generic_fallback() {
        let e = generic("Kaboom: something exploded");
        assert_eq!(e.kind, ErrorKind::Unknown);
        assert!(!e.parse_confident);
    }
}
