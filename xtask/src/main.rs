//! # xtask
//!
//! cargo xtask build-tests

use clap::Parser;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
enum Command {
    /// Rebuild elfs for on-device testing
    BuildElfs,
}

fn main() {
    match Command::parse() {
        Command::BuildElfs => todo!(),
    }
}
