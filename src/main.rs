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
    ops::Range,
    path::Path,
    process,
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail};
use colored::Colorize as _;
use defmt_decoder::{DecodeError, Frame, Locations, StreamDecoder};
use object::{Object, ObjectSymbol};
use probe_rs::{
    config::MemoryRegion,
    flashing::{self, Format},
    rtt::{Rtt, ScanRegion, UpChannel},
    Core,
    DebugProbeError::ProbeSpecific,
    MemoryInterface as _, Permissions, Session,
};
use signal_hook::consts::signal;

use crate::{
    backtrace::Outcome,
    canary::Canary,
    elf::Elf,
    registers::{PC, SP},
    target_info::TargetInfo,
};

const TIMEOUT: Duration = Duration::from_secs(1);

/// The default TCP port for the GDB Server
///
/// Port 3333 is the OpenOCD default, so we use that.
///
/// We only bind to localhost for security reasons, and we force IPv4 because
/// using `localhost` might bind to the IPv6 address `::1`, which is non-obvious
/// and will cause issues if people use `target remote 127.0.0.1:3333` in GDB.
const GDB_SERVER_DEFAULT_PORT: &str = "127.0.0.1:3333";

fn main() -> anyhow::Result<()> {
    configure_terminal_colorization();

    #[allow(clippy::redundant_closure)]
    cli::handle_arguments().map(|code| process::exit(code))
}

fn run_target_program(elf_path: &Path, chip_name: &str, opts: &cli::Opts) -> anyhow::Result<i32> {
    if !elf_path.exists() {
        bail!(
            "can't find ELF file at `{}`; are you sure you got the right path?",
            elf_path.display()
        );
    }

    let elf_bytes = fs::read(elf_path)?;
    let elf = &Elf::parse(&elf_bytes, elf_path)?;

    if let Some(cdp) = &opts.chip_description_path {
        probe_rs::config::add_target_from_yaml(Path::new(cdp))?;
    }
    let target_info = TargetInfo::new(chip_name, elf)?;

    let probe = probe::open(opts)?;

    let probe_target = target_info.probe_target.clone();
    let permissions = match opts.erase_all {
        false => Permissions::new(),
        true => Permissions::new().allow_erase_all(),
    };
    let mut sess = if opts.connect_under_reset {
        probe.attach_under_reset(probe_target, permissions)?
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
        probe_attach?
    };
    log::debug!("started session");

    if opts.no_flash {
        log::info!("skipped flashing");
    } else {
        let fp = flashing_progress();

        if opts.erase_all {
            flashing::erase_all(&mut sess, Some(fp.clone()))?;
        }

        let mut options = flashing::DownloadOptions::default();
        options.dry_run = false;
        options.progress = Some(fp);
        options.disable_double_buffering = opts.disable_double_buffering;
        options.verify = opts.verify;

        flashing::download_file_with_options(&mut sess, elf_path, Format::Elf, options)?;
        log::info!("success!");
    }

    let (stack_start, reset_range) = get_stack_start_and_reset_handler(&mut sess, elf)?;

    let canary = Canary::install(&mut sess, &target_info, elf, opts.measure_stack)?;
    if opts.measure_stack && canary.is_none() {
        bail!("failed to set up stack measurement");
    }

    let sess = Arc::new(Mutex::new(sess));

    let gdb_port = opts.gdb_port.as_deref().unwrap_or(GDB_SERVER_DEFAULT_PORT);
    let startup_gdb_port = if opts.gdb_on_start {
        Some(gdb_port)
    } else {
        None
    };

    let _gdb_handle = start_program(sess.clone(), elf, startup_gdb_port)?;

    let current_dir = &env::current_dir()?;

    log::debug!("Getting memory map");

    let memory_map = sess.lock().unwrap().target().memory_map.clone();

    log::debug!("Getting logs");

    let halted_due_to_signal = extract_and_print_logs(
        elf,
        sess.clone(),
        &memory_map,
        opts,
        current_dir,
        opts.gdb_on_start,
    )?;

    log::debug!("Got logs, result = {}", halted_due_to_signal);

    print_separator()?;

    let mut locked_session = sess.lock().unwrap();
    let mut core = locked_session.core(0)?;
    let canary_touched = canary
        .map(|canary| canary.touched(&mut core, elf))
        .transpose()?
        .unwrap_or(false);

    let panic_present = canary_touched || halted_due_to_signal;

    let mut backtrace_settings = backtrace::Settings {
        current_dir,
        backtrace_limit: opts.backtrace_limit,
        backtrace: (&opts.backtrace).into(),
        panic_present,
        shorten_paths: opts.shorten_paths,
        include_addresses: opts.verbose > 0,
    };

    let mut outcome = backtrace::print(
        &mut core,
        elf,
        &target_info.active_ram_region,
        &mut backtrace_settings,
        stack_start,
        reset_range,
    )?;

    // if general outcome was OK but the user ctrl-c'ed, that overrides our outcome
    // (TODO refactor this to be less bumpy)
    if halted_due_to_signal && outcome == Outcome::Ok {
        outcome = Outcome::CtrlC
    }

    drop(core);
    drop(locked_session);

    if opts.gdb_on_fault && outcome == Outcome::HardFault {
        outcome.log();

        let exit = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal::SIGINT, Arc::clone(&exit))?;

        log::info!("Starting gdbserver on {:?}", gdb_port);
        let gdb_connection_string = gdb_port.to_owned();
        let session = sess.clone();

        let _gdb_thread_handle = Some(std::thread::spawn(move || {
            log::info!(
                "{} to {}",
                "Now connect GDB".green().bold(),
                gdb_connection_string
            );
            log::info!("Press Ctrl-C to exit...");

            let instances = {
                let session = session.lock().unwrap();
                probe_rs_gdb_server::GdbInstanceConfiguration::from_session(
                    &session,
                    Some(gdb_connection_string),
                )
            };

            if let Err(e) = probe_rs_gdb_server::run(&session, instances.iter()) {
                log::warn!("During the execution of GDB an error was encountered:");
                log::warn!("{:?}", e);
            }
        }));

        while !exit.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }

        // FIXME there's no clean way to terminate the `run` thread. in the current implementation,
        // the process will `exit` (see `main` function) and `Session`'s destructor will not run
    } else {
        let mut locked_session = sess.lock().unwrap();
        let mut core = locked_session.core(0)?;
        core.reset_and_halt(TIMEOUT)?;
        outcome.log();
    }

    Ok(outcome.into())
}

fn start_program(
    sess: Arc<Mutex<Session>>,
    elf: &Elf,
    gdbport: Option<&str>,
) -> anyhow::Result<Option<std::thread::JoinHandle<()>>> {
    {
        let mut locked_session = sess.lock().unwrap();
        let mut core = locked_session.core(0)?;

        log::debug!("starting device");
        if core.available_breakpoint_units()? == 0 {
            if elf.rtt_buffer_address().is_some() {
                bail!("RTT not supported on device without HW breakpoints");
            } else {
                log::warn!("device doesn't support HW breakpoints; HardFault will NOT make `probe-run` exit with an error code");
            }
        }

        if let Some(rtt_buffer_address) = elf.rtt_buffer_address() {
            set_rtt_to_blocking(&mut core, elf.main_fn_address(), rtt_buffer_address)?
        }

        core.set_hw_breakpoint(cortexm::clear_thumb_bit(elf.vector_table.hard_fault).into())?;
    }

    let mut gdb_thread_handle = None;

    if let Some(port) = gdbport {
        let gdb_connection_string = port.to_owned();
        let session = sess.clone();

        gdb_thread_handle = Some(std::thread::spawn(move || {
            log::info!(
                "{} to {}",
                "Now connect GDB".green().bold(),
                gdb_connection_string
            );

            let instances = {
                let session = session.lock().unwrap();
                probe_rs_gdb_server::GdbInstanceConfiguration::from_session(
                    &session,
                    Some(gdb_connection_string),
                )
            };

            if let Err(e) = probe_rs_gdb_server::run(&session, instances.iter()) {
                log::warn!("During the execution of GDB an error was encountered:");
                log::warn!("{:?}", e);
            }
        }));
    }

    if gdb_thread_handle.is_none() {
        // Only start core if there is no GDB server
        log::debug!("Starting core...");
        let mut locked_session = sess.lock().unwrap();
        let mut core = locked_session.core(0)?;
        core.run()?;
        log::info!("Core started...");
    }

    Ok(gdb_thread_handle)
}

/// Set rtt to blocking mode
fn set_rtt_to_blocking(
    core: &mut Core,
    main_fn_address: u32,
    rtt_buffer_address: u32,
) -> anyhow::Result<()> {
    // set and wait for a hardware breakpoint at the beginning of `fn main()`
    core.set_hw_breakpoint(main_fn_address.into())?;
    core.run()?;
    core.wait_for_core_halted(Duration::from_secs(5))?;

    // calculate address of up-channel-flags inside the rtt control block
    const OFFSET: u32 = 44;
    let rtt_buffer_address = rtt_buffer_address + OFFSET;

    // read flags
    let channel_flags = &mut [0];
    core.read_32(rtt_buffer_address.into(), channel_flags)?;
    // modify flags to blocking
    const MODE_MASK: u32 = 0b11;
    const MODE_BLOCK_IF_FULL: u32 = 0b10;
    let modified_channel_flags = (channel_flags[0] & !MODE_MASK) | MODE_BLOCK_IF_FULL;
    // write flags back
    core.write_word_32(rtt_buffer_address.into(), modified_channel_flags)?;

    // clear the breakpoint we set before
    core.clear_hw_breakpoint(main_fn_address.into())?;

    Ok(())
}

fn extract_and_print_logs(
    elf: &Elf,
    sess: Arc<Mutex<Session>>,
    memory_map: &[MemoryRegion],
    opts: &cli::Opts,
    current_dir: &Path,
    using_gdb: bool,
) -> anyhow::Result<bool> {
    let exit = Arc::new(AtomicBool::new(false));
    let sig_id = signal_hook::flag::register(signal::SIGINT, exit.clone())?;

    let mut locked_session = sess.lock().unwrap();
    let mut core = locked_session.core(0)?;

    let mut logging_channel = if let Some(address) = elf.rtt_buffer_address() {
        Some(setup_logging_channel(address, &mut core, memory_map)?)
    } else {
        eprintln!("RTT logs not available; blocking until the device halts..");
        None
    };
    drop(core);
    drop(locked_session);

    let use_defmt = logging_channel
        .as_ref()
        .map_or(false, |channel| channel.name() == Some("defmt"));

    if use_defmt && opts.no_flash {
        log::warn!(
            "You are using `--no-flash` and `defmt` logging -- this combination can lead to malformed defmt data!"
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
        if using_gdb {
            // Give GDB a chance. This is the same value used in probe-rs-cli.
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut locked_session = sess.lock().unwrap();
        let mut core = locked_session.core(0)?;

        if let Some(logging_channel) = &mut logging_channel {
            let num_bytes_read = match logging_channel.read(&mut core, &mut read_buf) {
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

        if is_halted && was_halted && !using_gdb {
            break;
        }
        was_halted = is_halted;
    }

    drop(stdout);

    signal_hook::low_level::unregister(sig_id);
    signal_hook::flag::register_conditional_default(signal::SIGINT, exit.clone())?;

    // TODO refactor: a printing fucntion shouldn't stop the MC as a side effect
    // Ctrl-C was pressed; stop the microcontroller.
    if exit.load(Ordering::Relaxed) {
        let mut locked_session = sess.lock().unwrap();
        let mut core = locked_session.core(0)?;
        core.halt(TIMEOUT)?;
    }

    let halted_due_to_signal = exit.load(Ordering::Relaxed);

    Ok(halted_due_to_signal)
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
        .map(|locations| &locations[&frame.index()])
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

fn setup_logging_channel(
    rtt_buffer_address: u32,
    core: &mut probe_rs::Core,
    memory_map: &[MemoryRegion],
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

            Err(probe_rs::rtt::Error::ControlBlockNotFound) => {
                log::trace!("Could not attach because the target's RTT control block isn't initialized (yet). retrying");
            }

            Err(e) => {
                return Err(anyhow!(e));
            }
        }
    }

    log::error!("Max number of RTT attach retries exceeded.");
    Err(anyhow!(probe_rs::rtt::Error::ControlBlockNotFound))
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

fn flashing_progress() -> flashing::FlashProgress {
    flashing::FlashProgress::new(|evt| {
        match evt {
            // The flash layout has been built and the flashing procedure was initialized.
            flashing::ProgressEvent::Initialized { flash_layout, .. } => {
                let pages = flash_layout.pages();
                let num_pages = pages.len();
                let num_bytes: u64 = pages.iter().map(|x| x.size() as u64).sum();
                log::info!(
                    "flashing program ({} pages / {:.02} KiB)",
                    num_pages,
                    num_bytes as f64 / 1024.0
                );
            }
            // A sector has been erased. Sectors (usually) contain multiple pages.
            flashing::ProgressEvent::SectorErased { size, time } => {
                log::debug!(
                    "Erased sector of size {} bytes in {} ms",
                    size,
                    time.as_millis()
                );
            }
            // A page has been programmed.
            flashing::ProgressEvent::PageProgrammed { size, time } => {
                log::debug!(
                    "Programmed page of size {} bytes in {} ms",
                    size,
                    time.as_millis()
                );
            }
            _ => {
                // Ignore other events
            }
        }
    })
}

fn get_stack_start_and_reset_handler(
    sess: &mut Session,
    elf: &Elf,
) -> anyhow::Result<(u32, Range<u32>)> {
    let mut core = sess.core(0)?;
    core.reset_and_halt(Duration::from_secs(5))?;
    let stack_start = core.read_core_reg::<u32>(SP)?;
    let reset_address = cortexm::set_thumb_bit(core.read_core_reg::<u32>(PC)?);
    core.reset()?;
    let mut reset_symbols = elf
        .elf
        .symbols()
        .filter(|symbol| symbol.address() as u32 == reset_address && symbol.size() != 0)
        .collect::<Vec<_>>();
    let reset_symbol = match reset_symbols.len() {
        1 => reset_symbols.remove(0),
        _ => bail!("unable to determine reset handler"),
    };
    let reset_size = reset_symbol.size() as u32;

    Ok((stack_start, reset_address..reset_address + reset_size))
}
