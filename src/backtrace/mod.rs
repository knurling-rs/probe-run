use std::path::PathBuf;

use probe_rs::Core;
use signal_hook::consts::signal;

use crate::{cli::Opts, elf::Elf, target_info::TargetInfo};

mod pp;
mod symbolicate;
mod unwind;

#[derive(PartialEq, Eq)]
pub enum BacktraceOptions {
    Auto,
    Never,
    Always,
}

impl From<&String> for BacktraceOptions {
    fn from(item: &String) -> Self {
        match item.as_str() {
            "auto" | "Auto" => BacktraceOptions::Auto,
            "never" | "Never" => BacktraceOptions::Never,
            "always" | "Always" => BacktraceOptions::Always,
            _ => panic!("options for `--backtrace` are `auto`, `never`, `always`."),
        }
    }
}

pub struct Settings {
    pub backtrace_limit: u32,
    pub backtrace: BacktraceOptions,
    pub current_dir: PathBuf,
    pub halted_due_to_signal: bool,
    pub include_addresses: bool,
    pub shorten_paths: bool,
    pub stack_overflow: bool,
}

impl Settings {
    pub fn new(
        current_dir: PathBuf,
        halted_due_to_signal: bool,
        opts: &Opts,
        stack_overflow: bool,
    ) -> Self {
        Self {
            backtrace_limit: opts.backtrace_limit,
            backtrace: (&opts.backtrace).into(),
            current_dir,
            halted_due_to_signal,
            include_addresses: opts.verbose > 0,
            shorten_paths: opts.shorten_paths,
            stack_overflow,
        }
    }

    fn panic_present(&self) -> bool {
        self.stack_overflow || self.halted_due_to_signal
    }
}

/// (virtually) unwinds the target's program and prints its backtrace
pub fn print(
    core: &mut Core,
    elf: &Elf,
    target_info: &TargetInfo,
    settings: &mut Settings,
) -> anyhow::Result<Outcome> {
    let mut unwind = unwind::target(core, elf, target_info);
    let frames = symbolicate::frames(&unwind.raw_frames, &settings.current_dir, elf);

    let contains_exception = unwind
        .raw_frames
        .iter()
        .any(|raw_frame| raw_frame.is_exception());

    let print_backtrace = match settings.backtrace {
        BacktraceOptions::Never => false,
        BacktraceOptions::Always => true,
        BacktraceOptions::Auto => {
            settings.panic_present()
                || unwind.outcome == Outcome::StackOverflow
                || unwind.corrupted
                || contains_exception
        }
    };

    // `0` disables the limit and we want to show _all_ frames
    if settings.backtrace_limit == 0 {
        settings.backtrace_limit = frames.len() as u32;
    }

    if print_backtrace && settings.backtrace_limit > 0 {
        pp::backtrace(&frames, settings)?;

        if unwind.corrupted {
            log::warn!("call stack was corrupted; unwinding could not be completed");
        }
        if let Some(err) = unwind.processing_error {
            log::error!(
                "error occurred during backtrace creation: {err:?}\n               \
                         the backtrace may be incomplete.",
            );
        }
    }

    // if general outcome was OK but the user ctrl-c'ed, that overrides our outcome
    if settings.halted_due_to_signal && unwind.outcome == Outcome::Ok {
        unwind.outcome = Outcome::CtrlC
    }

    Ok(unwind.outcome)
}

/// Target program outcome
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    HardFault,
    Ok,
    StackOverflow,
    /// Control-C was pressed
    CtrlC,
}

impl Outcome {
    pub fn log(&self) {
        match self {
            Outcome::StackOverflow => log::error!("the program has overflowed its stack"),
            Outcome::HardFault => log::error!("the program panicked"),
            Outcome::Ok => log::info!("device halted without error"),
            Outcome::CtrlC => log::info!("interrupted by user"),
        }
    }
}

// Convert `Outcome` to an exit code.
impl From<Outcome> for i32 {
    fn from(outcome: Outcome) -> i32 {
        match outcome {
            Outcome::HardFault | Outcome::StackOverflow => signal::SIGABRT,
            Outcome::CtrlC => signal::SIGINT,
            Outcome::Ok => 0,
        }
    }
}
