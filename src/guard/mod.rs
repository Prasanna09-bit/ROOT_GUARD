//! Regression-guard generation (V2).
//!
//! A guard is the cheapest deterministic check that fails while this failure
//! class is present. Generation is pure: kind × toolchain → a check command.
//! The command is only ever *executed* by the verification ladder (`--verify`),
//! and T3 refuses to call a guard confirmed unless it discriminates the
//! known-good revision from the current one.

use std::fmt;
use std::path::Path;

use crate::normalize::fingerprint::fingerprint;
use crate::normalize::{ErrorKind, NormalizedError};

/// A generated regression guard: one check command, tied to the failure's
/// class fingerprint and cited location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    /// Shell command that detects this failure class (cheapest that works).
    pub check: String,
    /// Class fingerprint the guard protects (`kind/symbol/path`).
    pub fingerprint: String,
    /// Location the guard is anchored to, or `class <kind>` when unknown.
    pub cite: String,
}

impl fmt::Display for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} | {} | {}", self.check, self.fingerprint, self.cite)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Rust,
    Ts,
    Node,
    Python,
    Unknown,
}

/// Generate at most one guard for this failure.
///
/// Returns an empty vector when no deterministic check can be guessed —
/// inventing a command we cannot justify would be a guard-shaped lie.
pub fn generate(err: &NormalizedError, root: &Path) -> Vec<Guard> {
    let Some(check) = check_for(err, root) else {
        return Vec::new();
    };
    vec![Guard {
        check,
        fingerprint: fingerprint(err),
        cite: err
            .location
            .as_ref()
            .map(|l| {
                // Same portability rule as the check: a cite a machine cannot
                // resolve outside this checkout is decoration, not evidence.
                let file = crate::codeintel::relativize(root, &l.file);
                match (l.line, l.column) {
                    (Some(n), Some(c)) => format!("{file}:{n}:{c}"),
                    (Some(n), None) => format!("{file}:{n}"),
                    _ => file,
                }
            })
            .unwrap_or_else(|| format!("class {}", err.kind)),
    }]
}

fn check_for(err: &NormalizedError, root: &Path) -> Option<String> {
    let tool = detect_tool(err, root);
    if tool == Tool::Unknown {
        return None;
    }
    // Relative paths keep the check portable: the T3 ladder re-runs it inside
    // a temporary worktree, where an absolute path would point back at the
    // original tree and silently test the wrong content.
    let file = err
        .location
        .as_ref()
        .map(|l| crate::codeintel::relativize(root, &l.file));
    let file = file.as_deref();
    let sym = err.symbol.as_deref().unwrap_or("");

    let check = match (err.kind, tool) {
        (ErrorKind::Test, Tool::Rust) => {
            if sym.is_empty() {
                "cargo test".to_string()
            } else {
                format!("cargo test {sym}")
            }
        }
        (ErrorKind::Test, Tool::Python) => match file {
            Some(f) => format!("python3 -m pytest {f}"),
            None => "python3 -m pytest".to_string(),
        },
        (ErrorKind::Test, Tool::Ts | Tool::Node) => "npm test".to_string(),

        (ErrorKind::Lint, Tool::Rust) => "cargo clippy --all-targets -- -D warnings".to_string(),
        (ErrorKind::Lint, Tool::Ts | Tool::Node) => match file {
            Some(f) => format!("npx eslint {f}"),
            None => "npx eslint .".to_string(),
        },
        (ErrorKind::Lint, Tool::Python) => match file {
            Some(f) => format!("python3 -m ruff check {f}"),
            None => "python3 -m ruff check .".to_string(),
        },

        (ErrorKind::Compile | ErrorKind::Type, Tool::Rust) => "cargo check --all-targets".into(),
        (ErrorKind::Compile | ErrorKind::Type, Tool::Ts) => "npx tsc --noEmit".into(),
        (ErrorKind::Compile | ErrorKind::Type, Tool::Node) => {
            file.map(|f| format!("node --check {f}"))?
        }
        (ErrorKind::Compile | ErrorKind::Type, Tool::Python) => {
            file.map(|f| format!("python3 -m py_compile {f}"))?
        }

        (ErrorKind::Build | ErrorKind::Ci, Tool::Rust) => "cargo build".into(),
        (ErrorKind::Build | ErrorKind::Ci, Tool::Node) => "npm run build".into(),
        (ErrorKind::Build | ErrorKind::Ci, Tool::Ts) => "npx tsc --noEmit".into(),
        (ErrorKind::Build | ErrorKind::Ci, Tool::Python) => {
            let dir = file.map(parent_dir).unwrap_or_else(|| ".".into());
            format!("python3 -m compileall -q {dir}")
        }

        (ErrorKind::Dep, Tool::Rust) => "cargo check --locked".into(),
        (ErrorKind::Dep, Tool::Ts | Tool::Node) => "npm ci".into(),
        (ErrorKind::Dep, Tool::Python) => "python3 -m pip check".into(),

        (ErrorKind::Runtime, Tool::Python) => file.map(|f| format!("python3 {f}"))?,
        (ErrorKind::Runtime, Tool::Rust) => "cargo run".into(),
        (ErrorKind::Runtime, Tool::Node) => file.map(|f| format!("node {f}"))?,
        (ErrorKind::Runtime, Tool::Ts) => return None,

        // Config/Db/Api/Unknown failures are not guessable beyond the
        // cheapest gate that proves the tree still parses and typechecks.
        (ErrorKind::Config | ErrorKind::Db | ErrorKind::Api | ErrorKind::Unknown, _) => {
            base_gate(tool, file)?
        }
        (
            ErrorKind::Test
            | ErrorKind::Lint
            | ErrorKind::Compile
            | ErrorKind::Type
            | ErrorKind::Build
            | ErrorKind::Ci
            | ErrorKind::Dep
            | ErrorKind::Runtime,
            Tool::Unknown,
        ) => {
            return None;
        }
    };
    Some(check)
}

fn base_gate(tool: Tool, file: Option<&str>) -> Option<String> {
    match tool {
        Tool::Rust => Some("cargo check --all-targets".into()),
        Tool::Ts => Some("npx tsc --noEmit".into()),
        Tool::Node => file.map(|f| format!("node --check {f}")),
        Tool::Python => file.map(|f| format!("python3 -m py_compile {f}")),
        Tool::Unknown => None,
    }
}

fn parent_dir(file: &str) -> String {
    match file.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_string(),
        _ => ".".to_string(),
    }
}

/// Toolchain of the failing unit: the failing file's extension wins, then
/// manifests at the repository root.
fn detect_tool(err: &NormalizedError, root: &Path) -> Tool {
    if let Some(loc) = &err.location {
        let f = loc.file.to_ascii_lowercase();
        if f.ends_with(".rs") {
            return Tool::Rust;
        }
        if f.ends_with(".ts") || f.ends_with(".tsx") {
            return Tool::Ts;
        }
        if f.ends_with(".js") || f.ends_with(".jsx") || f.ends_with(".mjs") || f.ends_with(".cjs") {
            return Tool::Node;
        }
        if f.ends_with(".py") {
            return Tool::Python;
        }
    }
    if root.join("Cargo.toml").exists() {
        Tool::Rust
    } else if root.join("tsconfig.json").exists() {
        Tool::Ts
    } else if root.join("package.json").exists() {
        Tool::Node
    } else if root.join("pyproject.toml").exists()
        || root.join("requirements.txt").exists()
        || root.join("setup.py").exists()
    {
        Tool::Python
    } else {
        Tool::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize::Location;

    fn err(kind: ErrorKind, file: Option<&str>, sym: Option<&str>) -> NormalizedError {
        NormalizedError {
            kind,
            message: "boom".into(),
            symbol: sym.map(str::to_string),
            location: file.map(|f| Location {
                file: f.into(),
                line: Some(2),
                column: None,
            }),
            frames: vec![],
            parse_confident: true,
        }
    }

    #[test]
    fn runtime_python_guard_runs_the_failing_file() {
        let dir = std::env::temp_dir();
        let g = generate(&err(ErrorKind::Runtime, Some("app.py"), Some("main")), &dir);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].check, "python3 app.py");
        assert!(g[0].fingerprint.starts_with("runtime/"));
    }

    #[test]
    fn rust_gates_come_from_cargo() {
        let dir = std::env::temp_dir();
        assert_eq!(
            generate(&err(ErrorKind::Compile, Some("src/db.rs"), None), &dir)[0].check,
            "cargo check --all-targets"
        );
        assert_eq!(
            generate(&err(ErrorKind::Lint, Some("src/db.rs"), None), &dir)[0].check,
            "cargo clippy --all-targets -- -D warnings"
        );
        assert_eq!(
            generate(
                &err(ErrorKind::Test, Some("src/db.rs"), Some("my_test")),
                &dir
            )[0]
            .check,
            "cargo test my_test"
        );
    }

    #[test]
    fn unknown_environment_gets_no_guard() {
        let dir = std::env::temp_dir();
        let g = generate(&err(ErrorKind::Runtime, Some("mystery.xyz"), None), &dir);
        assert!(g.is_empty(), "no manifest, unknown extension → no guard");
    }

    #[test]
    fn guard_display_is_pipe_separated() {
        let dir = std::env::temp_dir();
        let g = &generate(&err(ErrorKind::Runtime, Some("app.py"), None), &dir)[0];
        let s = g.to_string();
        assert_eq!(s.matches(" | ").count(), 2, "got: {s}");
        assert!(s.starts_with("python3 app.py | runtime/"));
    }

    #[test]
    fn manifest_detects_toolchain_without_location() {
        let tmp = std::env::temp_dir().join(format!("rootguard-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        let e = err(ErrorKind::Config, None, None);
        assert_eq!(detect_tool(&e, &tmp), Tool::Rust);
        assert_eq!(
            check_for(&e, &tmp).as_deref(),
            Some("cargo check --all-targets")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn abs_paths_inside_root_become_portable() {
        use crate::codeintel::relativize;

        let tmp = std::env::temp_dir().join(format!("rootguard-portable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let inside = tmp.join("app.py").to_string_lossy().into_owned();
        assert_eq!(relativize(&tmp, &inside), "app.py");
        assert_eq!(relativize(&tmp, "app.py"), "app.py");
        assert_eq!(
            relativize(&tmp, "/elsewhere/app.py"),
            "/elsewhere/app.py",
            "paths outside the root must stay untouched"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
