use std::process::Command;

const PATH: &str = "tests/test_elfs";

pub fn run() {
    // configurations
    //   1. all with rzcobs
    //   2. hello-raw
    //   3. overflow

    cargo_build("--bins", false, false);
    cargo_build("--bin hello", false, true);
    cargo_build("--bin overflow", true, false);

    // TODO: copy binaries to cache
}

fn cargo_build(target: &str, no_flip_link: bool, raw_encoding: bool) {
    if raw_encoding {
        // activate feature `encoding-raw` of `defmt`
        run_cmd("cargo add defmt --features encoding-raw", "");
    }

    abc(no_flip_link, target);

    if raw_encoding {
        // deactivate feature `encoding-raw` of `defmt`
        run_cmd("git checkout HEAD -- Cargo.toml", "");
    }
}

fn abc(no_flip_link: bool, target: &str) {
    let mut args = "cargo build --release --target thumbv7em-none-eabihf ".to_string();
    args.push_str(target);

    let mut rustflags =
        "-C link-arg=-Tlink.x -C link-arg=-Tdefmt.x -C link-arg=--nmagic".to_string();
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
