use std::path::PathBuf;

use clap::{ArgAction, Parser};
use defmt_decoder::DEFMT_VERSIONS;
use git_version::git_version;
use log::Level;
use probe_rs::Probe;

use crate::probe;

/// Successfull termination of process.
const EXIT_SUCCESS: i32 = 0;

/// A Cargo runner for microcontrollers.
#[derive(Parser)]
#[command()]
pub struct Opts {
    /// Disable or enable backtrace (auto in case of panic or stack overflow).
    #[arg(long, default_value = "auto")]
    pub backtrace: String,

    /// Configure the number of lines to print before a backtrace gets cut off.
    #[arg(long, default_value = "50")]
    pub backtrace_limit: u32,

    /// The chip to program.
    #[arg(long, required = true, conflicts_with_all = HELPER_CMDS, env = "PROBE_RUN_CHIP")]
    chip: Option<String>,

    /// Path to chip description file, in YAML format.
    #[arg(long)]
    pub chip_description_path: Option<PathBuf>,

    /// Connect to device when NRST is pressed.
    #[arg(long)]
    pub connect_under_reset: bool,

    /// Disable use of double buffering while downloading flash.
    #[arg(long)]
    pub disable_double_buffering: bool,

    /// Path to an ELF firmware file.
    #[arg(required = true, conflicts_with_all = HELPER_CMDS)]
    elf: Option<PathBuf>,

    /// Mass-erase all nonvolatile memory before downloading flash.
    #[arg(long)]
    pub erase_all: bool,

    /// Output logs a structured json.
    #[arg(long)]
    pub json: bool,

    /// List supported chips and exit.
    #[arg(long)]
    list_chips: bool,

    /// Lists all the connected probes and exit.
    #[arg(long)]
    list_probes: bool,

    /// Applies the given format to the log output.
    ///
    /// The arguments between curly braces are placeholders for log metadata.
    /// The following arguments are supported:
    /// - {f} : file name (e.g. "main.rs")
    /// - {F} : file path (e.g. "src/bin/main.rs")
    /// - {l} : line number
    /// - {L} : log level (e.g. "INFO", "DEBUG", etc)
    /// - {m} : module path (e.g. "foo::bar::some_function")
    /// - {s} : the actual log
    /// - {t} : log timestamp
    ///
    /// For example, with the format "{t} [{L}] Location<{f}:{l}> {s}"
    /// a log would look like this:
    /// "23124 [INFO ] Location<main.rs:23> Hello, world!"
    #[arg(long, verbatim_doc_comment)]
    pub log_format: Option<String>,

    /// Applies the given format to the host log output. (see --log-format)
    #[arg(long)]
    pub host_log_format: Option<String>,

    /// Whether to measure the program's stack consumption.
    #[arg(long)]
    pub measure_stack: bool,

    /// Skip writing the application binary to flash.
    #[arg(
        long,
        conflicts_with = "disable_double_buffering",
        conflicts_with = "verify"
    )]
    pub no_flash: bool,

    /// The probe to use (eg. `VID:PID`, `VID:PID:Serial`, or just `Serial`).
    #[arg(long, env = "PROBE_RUN_PROBE")]
    pub probe: Option<String>,

    /// Whether to shorten paths (e.g. to crates.io dependencies) in backtraces and defmt logs
    #[arg(long)]
    pub shorten_paths: bool,

    /// The probe clock frequency in kHz
    #[arg(long, env = "PROBE_RUN_SPEED")]
    pub speed: Option<u32>,

    /// Enable more verbose output.
    #[arg(short, long, action = ArgAction::Count)]
    pub verbose: u8,

    /// Verifies the written program.
    #[arg(long)]
    pub verify: bool,

    /// Prints version information
    #[arg(short = 'V', long)]
    version: bool,

    /// Arguments passed after the ELF file path are discarded
    #[arg(allow_hyphen_values = true, hide = true, trailing_var_arg = true)]
    _rest: Vec<String>,
}

/// Helper commands, which will not execute probe-run normally.
const HELPER_CMDS: [&str; 3] = ["list_chips", "list_probes", "version"];

pub fn handle_arguments() -> anyhow::Result<i32> {
    let opts = Opts::parse();
    let verbose = opts.verbose;
    let mut log_format = opts.log_format.as_deref();
    let mut host_log_format = opts.host_log_format.as_deref();

    const DEFAULT_LOG_FORMAT: &str = "{L} {s}\n└─ {m} @ {F}:{l}";
    const DEFAULT_HOST_LOG_FORMAT: &str = "(HOST) {L} {s}";
    const DEFAULT_VERBOSE_HOST_LOG_FORMAT: &str = "(HOST) {L} {s}\n└─ {m} @ {F}:{l}";

    if log_format.is_none() {
        log_format = Some(DEFAULT_LOG_FORMAT);
    }

    if host_log_format.is_none() {
        if verbose == 0 {
            host_log_format = Some(DEFAULT_HOST_LOG_FORMAT);
        } else {
            host_log_format = Some(DEFAULT_VERBOSE_HOST_LOG_FORMAT);
        }
    }

    let logger_info =
        defmt_decoder::log::init_logger(log_format, host_log_format, opts.json, move |metadata| {
            if defmt_decoder::log::is_defmt_frame(metadata) {
                true // We want to display *all* defmt frames.
            } else {
                // Log depending on how often the `--verbose` (`-v`) cli-param is supplied:
                //   * 0: log everything from probe-run, with level "info" or higher
                //   * 1: log everything from probe-run
                //   * 2 or more: log everything
                match verbose {
                    0 => {
                        metadata.target().starts_with("probe_run")
                            && metadata.level() <= Level::Info
                    }
                    1 => metadata.target().starts_with("probe_run"),
                    _ => true,
                }
            }
        });

    if opts.measure_stack {
        log::warn!("use of deprecated option `--measure-stack`: Has no effect and will vanish on next breaking release")
    }

    if opts.version {
        print_version();
        Ok(EXIT_SUCCESS)
    } else if opts.list_probes {
        probe::print(&Probe::list_all());
        Ok(EXIT_SUCCESS)
    } else if opts.list_chips {
        print_chips();
        Ok(EXIT_SUCCESS)
    } else if let (Some(elf), Some(chip)) = (opts.elf.as_deref(), opts.chip.as_deref()) {
        crate::run_target_program(elf, chip, &opts, logger_info)
    } else {
        unreachable!("due to `StructOpt` constraints")
    }
}

fn print_chips() {
    let registry = probe_rs::config::families().expect("Could not retrieve chip family registry");
    for chip_family in registry {
        println!("{}\n    Variants:", chip_family.name);
        for variant in chip_family.variants.iter() {
            println!("        {}", variant.name);
        }
    }
}

/// The string reported by the `--version` flag
fn print_version() {
    /// Version from `Cargo.toml` e.g. `"0.1.4"`
    const VERSION: &str = env!("CARGO_PKG_VERSION");

    /// `""` OR git hash e.g. `"34019f8"`
    ///
    /// `git describe`-docs:
    /// > The command finds the most recent tag that is reachable from a commit. (...)
    /// It suffixes the tag name with the number of additional commits on top of the tagged object
    /// and the abbreviated object name of the most recent commit.
    //
    // The `fallback` is `"--"`, cause this will result in "" after `fn extract_git_hash`.
    const GIT_DESCRIBE: &str = git_version!(fallback = "--", args = ["--long"]);
    // Extract the "abbreviated object name"
    let hash = extract_git_hash(GIT_DESCRIBE);

    println!(
        "{VERSION} {hash}\nsupported defmt versions: {}",
        DEFMT_VERSIONS.join(", ")
    );
}

/// Extract git hash from a `git describe` statement
fn extract_git_hash(git_describe: &str) -> &str {
    git_describe.split('-').nth(2).unwrap()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::normal("v0.2.3-12-g25c50d2", "g25c50d2")]
    #[case::modified("v0.2.3-12-g25c50d2-modified", "g25c50d2")]
    #[case::fallback("--", "")]
    fn should_extract_hash_from_description(#[case] description: &str, #[case] expected: &str) {
        let hash = extract_git_hash(description);
        assert_eq!(hash, expected)
    }
}
