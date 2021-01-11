use std::{
    env, fs,
    io::{self, Read},
    path::PathBuf,
};

use anyhow::anyhow;
use structopt::StructOpt;

use probe_run::logger;

/// Prints defmt-encoded logs to stdout
#[derive(StructOpt)]
struct Opts {
    #[structopt(short, parse(from_os_str))]
    elf: PathBuf,
    // may want to add this later
    // #[structopt(short, long)]
    // verbose: bool,
    // TODO add file path argument; always use stdin for now
}

const READ_BUFFER_SIZE: usize = 1024;

fn main() -> anyhow::Result<()> {
    let opts: Opts = Opts::from_args();
    let verbose = false;
    logger::init(verbose);

    let bytes = fs::read(&opts.elf)?;

    let table = defmt_elf2table::parse(&bytes)?.ok_or_else(|| anyhow!(".defmt data not found"))?;
    let locs = defmt_elf2table::get_locations(&bytes, &table)?;

    let locs = if table.indices().all(|idx| locs.contains_key(&(idx as u64))) {
        Some(locs)
    } else {
        log::warn!("(BUG) location info is incomplete; it will be omitted from the output");
        None
    };

    let mut buf = [0; READ_BUFFER_SIZE];
    let mut frames = vec![];

    let current_dir = env::current_dir()?;
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    loop {
        let n = stdin.read(&mut buf)?;

        frames.extend_from_slice(&buf[..n]);

        probe_run::decode_loop(&mut frames, &table, &locs, &current_dir)?;
    }
}
