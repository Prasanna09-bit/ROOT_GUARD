use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use rootguard::ingest::resolve_input;
use rootguard::ingest::runner;

#[derive(Parser, Debug)]
#[command(
    name = "rootguard",
    version,
    about = "Failure intelligence: root cause → fix → prevention",
    long_about = "RootGuard turns a single failure into a cited root-cause chain, \
                  finds the commit that introduced it, and shows every site in the \
                  codebase that shares the failure class.\n\n\
                  `explain --verify` runs the T1/T2/T3 verification ladder: reproduce \
                  the instance, resolve every citation, and mutation-check the \
                  generated guard against a known-good revision.\n\n\
                  Deterministic only — no AI, no network."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Analyze a pasted/captured error into a 5-why chain with evidence
    Explain {
        /// Error text, a file path, or '-' for stdin
        input: Option<String>,
        /// Repository root used for blame and symbol search
        #[arg(long, short = 'r', default_value = ".")]
        root: PathBuf,
        /// Output format
        #[arg(long, short = 'f', default_value_t = Format::Text)]
        format: Format,
        /// Run the T1/T2/T3 verification ladder (executes check commands locally)
        #[arg(long)]
        verify: bool,
        /// Command that reproduces the failure — enables T1 and T3 (implies --verify)
        #[arg(long)]
        test: Option<String>,
        /// Known-good ref for T3's mutation check (auto-detected when omitted; implies --verify)
        #[arg(long)]
        good: Option<String>,
        /// Record a triage verdict for this failure (calibration input)
        #[arg(long, value_enum)]
        triage: Option<Triage>,
        /// Skip the local failure memory entirely (no recall, no recording)
        #[arg(long)]
        no_memory: bool,
    },

    /// Run a command; on failure, analyze the captured error automatically
    Watch {
        /// Command and arguments (everything after --)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        /// Repository root used for blame and symbol search
        #[arg(long, short = 'r', default_value = ".")]
        root: PathBuf,
        /// Output format
        #[arg(long, short = 'f', default_value_t = Format::Text)]
        format: Format,
        /// Skip the local failure memory entirely (no recall, no recording)
        #[arg(long)]
        no_memory: bool,
    },

    /// Report how well verification tiers predict human triage verdicts
    Calibrate {
        /// Repository root whose failure memory to read
        #[arg(long, short = 'r', default_value = ".")]
        root: PathBuf,
        /// Output format
        #[arg(long, short = 'f', default_value_t = Format::Text)]
        format: Format,
    },

    /// Find the first commit where a test command fails (git bisect, automated)
    Bisect {
        /// Shell command that fails on the broken revision (e.g. `cargo test foo`)
        #[arg(long)]
        test: String,
        /// Known-good ref (default: merge-base with origin/main or main)
        #[arg(long)]
        good: Option<String>,
        /// Known-bad ref (default: HEAD)
        #[arg(long)]
        bad: Option<String>,
        /// Repository root
        #[arg(long, short = 'r', default_value = ".")]
        root: PathBuf,
        /// Output format
        #[arg(long, short = 'f', default_value_t = Format::Text)]
        format: Format,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Yaml,
}

/// Human verdict on whether the analysis was right (`explain --triage`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Triage {
    Correct,
    Wrong,
}

impl From<Triage> for rootguard::memory::Verdict {
    fn from(t: Triage) -> Self {
        match t {
            Triage::Correct => rootguard::memory::Verdict::Correct,
            Triage::Wrong => rootguard::memory::Verdict::Wrong,
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Format::Text => write!(f, "text"),
            Format::Yaml => write!(f, "yaml"),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rootguard: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.cmd {
        Cmd::Explain {
            input,
            root,
            format,
            verify,
            test,
            good,
            triage,
            no_memory,
        } => {
            let (raw, source) = resolve_input(input.as_deref())?;
            if no_memory && triage.is_some() {
                anyhow::bail!("--triage records into the failure memory; drop --no-memory");
            }
            let do_verify = verify || test.is_some() || good.is_some();
            let mut report = if do_verify {
                let opts = rootguard::verify::Options {
                    test_cmd: test.as_deref(),
                    good: good.as_deref(),
                };
                rootguard::analyze_verified(&raw, &root, &source, None, &opts)
            } else {
                rootguard::analyze(&raw, &root, &source, None)
            };
            if !no_memory {
                // After the ladder: memory writes must never influence (or be
                // cleaned up by) the verification runs.
                match rootguard::memory::remember(&root, &report, triage.map(Into::into)) {
                    Ok(recall) => report.memory = Some(recall),
                    Err(e) => eprintln!("rootguard: failure memory: {e:#}"),
                }
            }
            emit(&report, format);
            Ok(ExitCode::SUCCESS)
        }

        Cmd::Watch {
            command,
            root,
            format,
            no_memory,
        } => {
            let result = runner::run_capture(&command, &root)?;
            if result.exit_code == 0 {
                eprintln!("rootguard: command succeeded (exit 0) — nothing to analyze");
                return Ok(ExitCode::SUCCESS);
            }
            let mut raw = result.stderr.clone();
            if raw.trim().is_empty() {
                raw = result.stdout_tail.clone();
            }
            if raw.trim().is_empty() {
                anyhow::bail!(
                    "command failed with exit {} but produced no stderr/stdout to analyze",
                    result.exit_code
                );
            }
            // Keep the source label single-line and short (it lands in YAML).
            let label = {
                let joined = command.join(" ");
                let flat: String = joined.split_whitespace().collect::<Vec<_>>().join(" ");
                if flat.chars().count() > 60 {
                    let cut: String = flat.chars().take(57).collect();
                    format!("{cut}...")
                } else {
                    flat
                }
            };
            let source = format!("watch:{label}");
            let mut report = rootguard::analyze(&raw, &root, &source, Some(result.exit_code));
            if !no_memory {
                match rootguard::memory::remember(&root, &report, None) {
                    Ok(recall) => report.memory = Some(recall),
                    Err(e) => eprintln!("rootguard: failure memory: {e:#}"),
                }
            }
            emit(&report, format);
            Ok(ExitCode::from(result.exit_code.clamp(1, 255) as u8))
        }

        Cmd::Calibrate { root, format } => {
            let cal = rootguard::memory::calibrate(&root);
            match format {
                Format::Text => println!("{}", cal.to_text()),
                Format::Yaml => println!(
                    "{}",
                    serde_yaml::to_string(&cal)
                        .unwrap_or_else(|e| format!("# serialize error: {e}"))
                ),
            }
            Ok(ExitCode::SUCCESS)
        }

        Cmd::Bisect {
            test,
            good,
            bad,
            root,
            format,
        } => {
            let res = rootguard::gitintel::bisect(&root, good.as_deref(), bad.as_deref(), &test)?;
            match format {
                Format::Yaml => {
                    let culprit = res.culprit.as_ref().expect("bisect guarantees Some");
                    let yaml = format!(
                        "rootguard: {}\ncommand: {}\ngood: {}\nbad: {}\ntested: {}\nculprit:\n  \
                         sha: {}\n  short: {}\n  author: {}\n  summary: {}\n",
                        rootguard::report::SCHEMA,
                        test,
                        res.auto_good
                            .as_deref()
                            .unwrap_or(good.as_deref().unwrap_or("HEAD")),
                        res.auto_bad
                            .as_deref()
                            .unwrap_or(bad.as_deref().unwrap_or("HEAD")),
                        res.tested,
                        culprit.sha,
                        culprit.short,
                        culprit.author,
                        culprit.summary,
                    );
                    println!("{yaml}");
                }
                Format::Text => {
                    let c = res.culprit.as_ref().expect("bisect guarantees Some");
                    println!("RootGuard bisect");
                    println!("{}", "=".repeat(72));
                    println!("test command : {test}");
                    println!(
                        "range        : {}..{}",
                        res.auto_good
                            .as_deref()
                            .unwrap_or(good.as_deref().unwrap_or("HEAD")),
                        res.auto_bad
                            .as_deref()
                            .unwrap_or(bad.as_deref().unwrap_or("HEAD"))
                    );
                    println!("revisions tested: {}", res.tested);
                    println!();
                    println!("FIRST BAD COMMIT");
                    println!("  sha     : {}", c.sha);
                    println!("  author  : {}", c.author);
                    println!("  summary : {}", c.summary);
                    if let Some(w) = &c.when {
                        println!("  when    : {w}");
                    }
                    println!();
                    println!("Next: run `rootguard explain` on the failure, or inspect with");
                    println!("      git show {}", c.short);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn emit(report: &rootguard::report::Report, format: Format) {
    match format {
        Format::Text => println!("{}", report.to_text()),
        Format::Yaml => println!("{}", report.to_yaml()),
    }
}
