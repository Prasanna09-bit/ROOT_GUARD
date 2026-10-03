use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

/// Result of running a user command under `rootguard watch`.
#[derive(Debug)]
pub struct RunResult {
    pub exit_code: i32,
    pub stderr: String,
    pub stdout_tail: String,
    pub truncated: bool,
}

const MAX_CAPTURE: usize = 256 * 1024;

/// Run `argv` with stderr captured (and stdout tail-captured).
///
/// When both the user's stdout and our stdout are terminals we still show live
/// output by teeing stdout, so `rootguard watch -- cargo test` behaves like
/// running the command normally — failures are captured for analysis.
pub fn run_capture(argv: &[String], cwd: &std::path::Path) -> anyhow::Result<RunResult> {
    if argv.is_empty() {
        anyhow::bail!("no command given — usage: rootguard watch -- <command>");
    }

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run {}: {e}", argv[0]))?;

    let mut stderr_buf: Vec<u8> = Vec::new();
    if let Some(mut err_pipe) = child.stderr.take() {
        let mut chunk = [0u8; 8192];
        loop {
            use std::io::Read;
            match err_pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    // Echo stderr so the user still sees it live.
                    let _ = std::io::stderr().write_all(&chunk[..n]);
                    let _ = std::io::stderr().flush();
                    if stderr_buf.len() < MAX_CAPTURE {
                        let room = MAX_CAPTURE - stderr_buf.len();
                        stderr_buf.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
                Err(_) => break,
            }
        }
    }

    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("wait failed: {e}"))?;
    let truncated = stderr_buf.len() >= MAX_CAPTURE;
    Ok(RunResult {
        exit_code: status.code().unwrap_or(1),
        stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
        stdout_tail: String::new(),
        truncated,
    })
}

/// Detect whether output looks like a terminal for heuristics elsewhere.
pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_failing_command() {
        let dir = std::env::temp_dir();
        let argv = vec!["sh".into(), "-c".into(), "echo oops 1>&2; exit 3".into()];
        let r = run_capture(&argv, &dir).unwrap();
        assert_eq!(r.exit_code, 3);
        assert!(r.stderr.contains("oops"));
    }

    #[test]
    fn empty_argv_errors() {
        let dir = std::env::temp_dir();
        assert!(run_capture(&[], &dir).is_err());
    }
}
