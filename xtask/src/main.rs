//! # xtask
//!
//! cargo xtask build-tests

mod build_tests;

use clap::Parser;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
enum Command {
    /// Rebuild elfs for on-device testing
    BuildElfs,
}

fn main() {
    match Command::parse() {
        Command::BuildElfs => build_tests::run(),
    }
}
