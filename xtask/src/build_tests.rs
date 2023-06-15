use std::{env, process::Command};

const PATH: &str = "tests/test_elfs";

/// Build the various test elfs and copy them to the cache
pub fn run() {
    all_rzcobs();
    hello_raw();
    overflow_no_flip_link();
}

fn all_rzcobs() {
    cargo_build("--bins", false);

    for name in ["hello", "overflow", "panic", "silent-loop"] {
        copy(name, &format!("{name}-rzcobs"))
    }
}

fn hello_raw() {
    // activate feature `encoding-raw` of `defmt`
    run_cmd("cargo add defmt --features encoding-raw", "");

    cargo_build("--bin hello", false);

    // deactivate feature `encoding-raw` of `defmt`
    run_cmd("git checkout HEAD -- Cargo.toml", "");

    copy("hello", "hello-raw");
}

fn overflow_no_flip_link() {
    cargo_build("--bin overflow", true);

    copy("overflow", "overflow-no-flip-link");
}

fn cargo_build(target: &str, no_flip_link: bool) {
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

    // set flip-link as linker, e
    if !no_flip_link {
        rustflags += " -C linker=flip-link";
    }

    run_cmd(&args, &rustflags);
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

fn copy(old_name: &str, new_name: &str) {
    run_cmd(
        &format!("cp ../../target/thumbv7em-none-eabihf/release/{old_name} cache/{new_name}"),
        "",
    );
}
