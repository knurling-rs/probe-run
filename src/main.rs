mod backtrace;
mod canary;
mod cli;
mod cortexm;
mod dep;
mod elf;
mod probe;
mod registers;
mod stacked;
mod target_info;

use std::{
    env, fs,
    io::{self, Write as _},
    path::Path,
    process,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail};
use colored::Colorize as _;
use defmt_decoder::{DecodeError, Frame, Locations, StreamDecoder};
use probe_rs::{
    config::MemoryRegion,
    flashing::{self, Format},
    rtt::{Rtt, ScanRegion, UpChannel},
    Core,
    DebugProbeError::ProbeSpecific,
    MemoryInterface as _, Permissions, Session,
};
use signal_hook::consts::signal;

use crate::{canary::Canary, elf::Elf, target_info::TargetInfo};

const TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> anyhow::Result<()> {
    configure_terminal_colorization();

    #[allow(clippy::redundant_closure)]
    cli::handle_arguments().map(|code| process::exit(code))
}

fn run_target_program(elf_path: &Path, chip_name: &str, opts: &cli::Opts) -> anyhow::Result<i32> {
    // connect to probe and flash firmware
    let probe_target = lookup_probe_target(elf_path, chip_name, opts)?;
    let mut sess = attach_to_probe(probe_target.clone(), opts)?;
    if opts.reset {
        flash(&mut sess, elf_path, opts)?;
    }

    // attach to core
    let memory_map = sess.target().memory_map.clone();
    let core = &mut sess.core(0)?;

    // reset if requested, and then halt the core; this is necessary for analyzing
    // the vector table and painting the stack
    if opts.reset {
        core.reset_and_halt(TIMEOUT)?;
    } else {
        core.halt(TIMEOUT)?;
    }

    // gather information
    let elf_bytes = fs::read(elf_path)?;
    let elf = &Elf::parse(&elf_bytes, elf_path)?;
    let target_info = TargetInfo::new(elf, memory_map, probe_target)?;

    let canary;
    if opts.reset {
        // install stack canary
        canary = Canary::install(core, &target_info, elf, opts.measure_stack)?;
        if opts.measure_stack && canary.is_none() {
            bail!("failed to set up stack measurement");
        }
    } else {
        // cannot safely touch the stack of a running application
        canary = None;
        log::warn!("You are using `--no-reset` -- stack overflow checks will not be done")
    }

    // run program and print logs until there is an exception
    attach_to_program(core, elf)?;
    let current_dir = &env::current_dir()?;
    let halted_due_to_signal = print_logs(core, current_dir, elf, &target_info.memory_map, opts)?; // blocks until exception
    print_separator()?;

    // analyze stack canary
    let canary_touched = canary
        .map(|canary| canary.touched(core, elf))
        .transpose()?
        .unwrap_or(false);

    // print the backtrace
    let mut backtrace_settings =
        backtrace::Settings::new(canary_touched, current_dir, halted_due_to_signal, opts);
    let outcome = backtrace::print(core, elf, &target_info, &mut backtrace_settings)?;

    if opts.reset {
        // reset the target
        core.reset_and_halt(TIMEOUT)?;
    } else {
        // remove our instrumentation and restart the target
        detach_from_program(core, elf)?;
    }

    outcome.log();
    Ok(outcome.into())
}

fn lookup_probe_target(
    elf_path: &Path,
    chip_name: &str,
    opts: &cli::Opts,
) -> anyhow::Result<probe_rs::Target> {
    if !elf_path.exists() {
        bail!(
            "can't find ELF file at `{}`; are you sure you got the right path?",
            elf_path.display()
        );
    }

    // register chip description
    if let Some(cdp) = &opts.chip_description_path {
        probe_rs::config::add_target_from_yaml(fs::File::open(cdp)?)?;
    }

    // look up target and check combat
    let probe_target = probe_rs::config::get_target_by_name(chip_name)?;
    target_info::check_processor_target_compatability(&probe_target.cores[0], elf_path);

    Ok(probe_target)
}

fn attach_to_probe(probe_target: probe_rs::Target, opts: &cli::Opts) -> anyhow::Result<Session> {
    let permissions = match opts.erase_all {
        false => Permissions::new(),
        true => Permissions::new().allow_erase_all(),
    };
    let probe = probe::open(opts)?;
    let sess = if opts.connect_under_reset {
        probe.attach_under_reset(probe_target, permissions)
    } else {
        let probe_attach = probe.attach(probe_target, permissions);
        if let Err(probe_rs::Error::Probe(ProbeSpecific(e))) = &probe_attach {
            // FIXME Using `to_string().contains(...)` is a workaround as the concrete type
            // of `e` is not public and therefore does not allow downcasting.
            if e.to_string().contains("JtagNoDeviceConnected") {
                eprintln!("Info: Jtag cannot find a connected device.");
                eprintln!("Help:");
                eprintln!("    Check that the debugger is connected to the chip, if so");
                eprintln!("    try using probe-run with option `--connect-under-reset`");
                eprintln!("    or, if using cargo:");
                eprintln!("        cargo run -- --connect-under-reset");
                eprintln!("    If using this flag fixed your issue, this error might");
                eprintln!("    come from the program currently in the chip and using");
                eprintln!("    `--connect-under-reset` is only a workaround.\n");
            }
        }
        probe_attach
    }?;
    log::debug!("started session");
    Ok(sess)
}

fn flash(sess: &mut Session, elf_path: &Path, opts: &cli::Opts) -> anyhow::Result<()> {
    if opts.no_flash {
        log::info!("skipped flashing");
    } else {
        let fp = Some(flashing_progress());

        if opts.erase_all {
            flashing::erase_all(sess, fp.clone())?;
        }

        let mut options = flashing::DownloadOptions::default();
        options.dry_run = false;
        options.progress = fp;
        options.disable_double_buffering = opts.disable_double_buffering;
        options.verify = opts.verify;

        flashing::download_file_with_options(sess, elf_path, Format::Elf, options)?;
        log::info!("success!");
    }
    Ok(())
}

fn flashing_progress() -> flashing::FlashProgress {
    flashing::FlashProgress::new(|evt| {
        match evt {
            // The flash layout has been built and the flashing procedure was initialized.
            flashing::ProgressEvent::Initialized { flash_layout, .. } => {
                let pages = flash_layout.pages();
                let num_pages = pages.len();
                let num_kb = pages.iter().map(|x| x.size() as f64).sum::<f64>() / 1024.0;
                log::info!("flashing program ({num_pages} pages / {num_kb:.02} KiB)",);
            }
            // A sector has been erased. Sectors (usually) contain multiple pages.
            flashing::ProgressEvent::SectorErased { size, time } => log::debug!(
                "Erased sector of size {size} bytes in {} ms",
                time.as_millis()
            ),
            // A page has been programmed.
            flashing::ProgressEvent::PageProgrammed { size, time } => log::debug!(
                "Programmed page of size {size} bytes in {} ms",
                time.as_millis()
            ),
            _ => { /* Ignore other events */ }
        }
    })
}

fn attach_to_program(core: &mut Core, elf: &Elf) -> anyhow::Result<()> {
    log::debug!("attaching to program");

    match (core.available_breakpoint_units()?, elf.rtt_buffer_address()) {
        (0, Some(_)) => bail!("RTT not supported on device without HW breakpoints"),
        (0, None) => log::warn!("device doesn't support HW breakpoints; HardFault will NOT make `probe-run` exit with an error code"),
        (_, Some(rtt_buffer_address)) =>
            set_rtt_blocking_mode(core, rtt_buffer_address, true)?,
        (_, None) => {}
    }

    core.set_hw_breakpoint(cortexm::clear_thumb_bit(elf.vector_table.hard_fault).into())?;
    core.run()?;

    Ok(())
}

fn detach_from_program(core: &mut Core, elf: &Elf) -> anyhow::Result<()> {
    log::debug!("detaching from program");

    if let Some(rtt_buffer_address) = elf.rtt_buffer_address() {
        set_rtt_blocking_mode(core, rtt_buffer_address, false)?;
    }

    core.clear_hw_breakpoint(cortexm::clear_thumb_bit(elf.vector_table.hard_fault).into())?;
    core.run()?;

    Ok(())
}

/// Set rtt to blocking mode
fn set_rtt_blocking_mode(
    core: &mut Core,
    rtt_buffer_address: u32,
    is_blocking: bool,
) -> anyhow::Result<()> {
    log::debug!(
        "setting RTT mode to {}",
        if is_blocking {
            "blocking"
        } else {
            "nonblocking"
        }
    );

    // calculate address of up-channel-flags inside the rtt control block
    const OFFSET: u32 = 44;
    let rtt_buffer_address = rtt_buffer_address + OFFSET;

    // read flags
    let channel_flags = &mut [0];
    core.read_32(rtt_buffer_address.into(), channel_flags)?;
    // modify flags to blocking
    const MODE_MASK: u32 = 0b11;
    const MODE_NON_BLOCKING_TRIM: u32 = 0b01;
    const MODE_BLOCK_IF_FULL: u32 = 0b10;
    let modified_channel_flags = (channel_flags[0] & !MODE_MASK)
        | if is_blocking {
            MODE_BLOCK_IF_FULL
        } else {
            MODE_NON_BLOCKING_TRIM
        };
    // write flags back
    core.write_word_32(rtt_buffer_address.into(), modified_channel_flags)?;

    Ok(())
}

fn print_logs(
    core: &mut Core,
    current_dir: &Path,
    elf: &Elf,
    memory_map: &[MemoryRegion],
    opts: &cli::Opts,
) -> anyhow::Result<bool> {
    let exit = Arc::new(AtomicBool::new(false));
    let sig_id = signal_hook::flag::register(signal::SIGINT, exit.clone())?;

    let mut logging_channel = if let Some(address) = elf.rtt_buffer_address() {
        Some(setup_logging_channel(core, memory_map, address)?)
    } else {
        eprintln!("RTT logs not available; blocking until the device halts...");
        None
    };

    let use_defmt = logging_channel
        .as_ref()
        .map_or(false, |channel| channel.name() == Some("defmt"));

    if use_defmt && (!opts.reset || opts.no_flash) {
        log::warn!(
            "You are using `{}` and `defmt` logging -- this combination can lead to malformed defmt data!",
            if !opts.reset { "--no-reset" } else { "--no-flash" }
        );
    } else if use_defmt && elf.defmt_table.is_none() {
        bail!("\"defmt\" RTT channel is in use, but the firmware binary contains no defmt data");
    }

    let mut decoder_and_encoding = if use_defmt {
        elf.defmt_table
            .as_ref()
            .map(|table| (table.new_stream_decoder(), table.encoding()))
    } else {
        None
    };

    print_separator()?;

    let mut stdout = io::stdout().lock();
    let mut read_buf = [0; 1024];
    let mut was_halted = false;
    while !exit.load(Ordering::Relaxed) {
        if let Some(logging_channel) = &mut logging_channel {
            let num_bytes_read = match logging_channel.read(core, &mut read_buf) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("RTT error: {e}");
                    break;
                }
            };

            if num_bytes_read != 0 {
                match decoder_and_encoding.as_mut() {
                    Some((stream_decoder, encoding)) => {
                        stream_decoder.received(&read_buf[..num_bytes_read]);

                        decode_and_print_defmt_logs(
                            &mut **stream_decoder,
                            elf.defmt_locations.as_ref(),
                            current_dir,
                            opts.shorten_paths,
                            encoding.can_recover(),
                        )?;
                    }

                    _ => {
                        stdout.write_all(&read_buf[..num_bytes_read])?;
                        stdout.flush()?;
                    }
                }
            }
        }

        let is_halted = core.core_halted()?;

        if is_halted && was_halted {
            break;
        }
        was_halted = is_halted;
    }

    drop(stdout);

    signal_hook::low_level::unregister(sig_id);
    signal_hook::flag::register_conditional_default(signal::SIGINT, exit.clone())?;

    // Ctrl-C was pressed; stop the microcontroller.
    // TODO refactor: a printing function shouldn't stop the MC as a side effect
    if exit.load(Ordering::Relaxed) {
        core.halt(TIMEOUT)?;
    }

    let halted_due_to_signal = exit.load(Ordering::Relaxed);

    Ok(halted_due_to_signal)
}

fn setup_logging_channel(
    core: &mut Core,
    memory_map: &[MemoryRegion],
    rtt_buffer_address: u32,
) -> anyhow::Result<UpChannel> {
    const NUM_RETRIES: usize = 10; // picked at random, increase if necessary

    let scan_region = ScanRegion::Exact(rtt_buffer_address);
    for _ in 0..NUM_RETRIES {
        match Rtt::attach_region(core, memory_map, &scan_region) {
            Ok(mut rtt) => {
                log::debug!("Successfully attached RTT");
                let channel = rtt
                    .up_channels()
                    .take(0)
                    .ok_or_else(|| anyhow!("RTT up channel 0 not found"))?;
                return Ok(channel);
            }
            Err(probe_rs::rtt::Error::ControlBlockNotFound) => log::trace!(
                "Couldn't attach because the target's RTT control block isn't initialized (yet). retrying"
            ),
            Err(e) => return Err(anyhow!(e)),
        }
    }

    log::error!("Max number of RTT attach retries exceeded.");
    Err(anyhow!(probe_rs::rtt::Error::ControlBlockNotFound))
}

fn decode_and_print_defmt_logs(
    stream_decoder: &mut dyn StreamDecoder,
    locations: Option<&Locations>,
    current_dir: &Path,
    shorten_paths: bool,
    encoding_can_recover: bool,
) -> anyhow::Result<()> {
    loop {
        match stream_decoder.decode() {
            Ok(frame) => forward_to_logger(&frame, locations, current_dir, shorten_paths),
            Err(DecodeError::UnexpectedEof) => break,
            Err(DecodeError::Malformed) => match encoding_can_recover {
                // if recovery is impossible, abort
                false => return Err(DecodeError::Malformed.into()),
                // if recovery is possible, skip the current frame and continue with new data
                true => continue,
            },
        }
    }

    Ok(())
}

fn forward_to_logger(
    frame: &Frame,
    locations: Option<&Locations>,
    current_dir: &Path,
    shorten_paths: bool,
) {
    let (file, line, mod_path) = location_info(frame, locations, current_dir, shorten_paths);
    defmt_decoder::log::log_defmt(frame, file.as_deref(), line, mod_path.as_deref());
}

fn location_info(
    frame: &Frame,
    locations: Option<&Locations>,
    current_dir: &Path,
    shorten_paths: bool,
) -> (Option<String>, Option<u32>, Option<String>) {
    locations
        .and_then(|locations| locations.get(&frame.index()))
        .map(|location| {
            let path = if let Ok(relpath) = location.file.strip_prefix(current_dir) {
                relpath.display().to_string()
            } else {
                let dep_path = dep::Path::from_std_path(&location.file);
                match shorten_paths {
                    true => dep_path.format_short(),
                    false => dep_path.format_highlight(),
                }
            };
            (
                Some(path),
                Some(location.line as u32),
                Some(location.module.clone()),
            )
        })
        .unwrap_or((None, None, None))
}

/// Print a line to separate different execution stages.
fn print_separator() -> io::Result<()> {
    writeln!(io::stderr(), "{}", "─".repeat(80).dimmed())
}

fn configure_terminal_colorization() {
    // ! This should be detected by `colored`, but currently is not.
    // See https://github.com/mackwic/colored/issues/108 and https://github.com/knurling-rs/probe-run/pull/318.

    if let Ok("dumb") = env::var("TERM").as_deref() {
        colored::control::set_override(false)
    }
}
