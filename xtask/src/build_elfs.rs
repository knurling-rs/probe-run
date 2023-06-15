use std::{env, fs, process::Command};

const PATH: &str = "tests/test_elfs";

/// Build the various test elfs and copy them to the cache
pub fn run() {
    all_bins_rzcobs();
    hello_raw();
    overflow_no_flip_link();
}

fn all_bins_rzcobs() {
    cargo_build("--bins", true);

    for name in ["hello", "overflow", "panic", "silent-loop"] {
        copy(name, &format!("{name}-rzcobs"))
    }
}

fn hello_raw() {
    // save state of Cargo.toml, so we can restore it later
    let cargo_toml = format!("{PATH}/Cargo.toml");
    let before = fs::read(&cargo_toml).unwrap();

    // activate feature `encoding-raw` of `defmt`
    run_cmd("cargo add defmt --features encoding-raw", "");

    // build the binary
    cargo_build("--bin hello", true);

    // restore Cargo.toml
    fs::write(cargo_toml, before).unwrap();

    copy("hello", "hello-raw");
}

fn overflow_no_flip_link() {
    cargo_build("--bin overflow", false);

    copy("overflow", "overflow-no-flip-link");
}

fn cargo_build(target: &str, flip_link: bool) {
    let mut args = "cargo build --release --target thumbv7em-none-eabihf ".to_string();
    args.push_str(target);

    let mut rustflags =
        "-C link-arg=-Tlink.x -C link-arg=-Tdefmt.x -C link-arg=--nmagic".to_string();

    // remap the path
    let repo_dir = env::current_dir().unwrap();
    rustflags += &format!(
        " --remap-path-prefix {}/{PATH}=/test_elfs",
        repo_dir.display()
    );

    if flip_link {
        // set flip-link as linker
        rustflags += " -C linker=flip-link";
    }

    run_cmd(&args, &rustflags);
}

/// Copy the binary from the `target/` dir to the cache
fn copy(old_name: &str, new_name: &str) {
    run_cmd(
        &format!("cp ../../target/thumbv7em-none-eabihf/release/{old_name} cache/{new_name}"),
        "",
    );
}

fn run_cmd(command: &str, rustflags: &str) {
    println!("RUN: {command}");
    let mut cmd = command.split(' ');
    let success = Command::new(cmd.next().unwrap())
        .args(cmd)
        .current_dir(PATH)
        .env("RUSTFLAGS", rustflags)
        .status()
        .unwrap()
        .success();
    if !success {
        panic!("command failed: {command}");
    }
}
