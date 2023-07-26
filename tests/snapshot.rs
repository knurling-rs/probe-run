use std::{
    io::Read,
    process::{Child, Command, ExitStatus},
    thread,
    time::Duration,
};

use os_pipe::pipe;
use rstest::rstest;
use serial_test::serial;

struct RunResult {
    exit_status: ExitStatus,
    output: String,
}

/// Run `probe-run` with `args` and truncate the output.
///
/// If `terminate` is `true`, the command gets terminated after a short timeout.
fn run(args: &[&str], terminate: bool) -> RunResult {
    let (mut reader, mut handle) = run_command(args);

    if terminate {
        wait_and_terminate(&handle);
    }

    // retrieve output and clean up
    let mut probe_run_output = String::new();
    reader.read_to_string(&mut probe_run_output).unwrap();
    let exit_status = handle.wait().unwrap();

    // remove the lines printed during flashing, as they contain timing info that's not always the same
    let output = truncate_output(probe_run_output);

    RunResult {
        exit_status,
        output,
    }
}

fn wait_and_terminate(handle: &Child) {
    // sleep a bit so that child can process the input
    thread::sleep(Duration::from_secs(5));

    // send SIGINT to the child
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(handle.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .expect("cannot send ctrl-c");
}

fn run_command(args: &[&str]) -> (os_pipe::PipeReader, Child) {
    let mut cmd = vec!["run", "--", "--chip", "nRF52840_xxAA", "--shorten-paths"];
    cmd.extend(&args[1..]);

    let path = format!("tests/test_elfs/{}", args[0]);
    cmd.push(path.as_str());

    // capture stderr and stdout while preserving line order
    let (reader, writer) = pipe().unwrap();

    let handle = Command::new("cargo")
        .args(cmd)
        .stdout(writer.try_clone().unwrap())
        .stderr(writer)
        .spawn()
        .unwrap();
    (reader, handle)
}

// remove the lines printed during flashing, as they contain timing info that's not always the same
fn truncate_output(probe_run_output: String) -> String {
    probe_run_output
        .lines()
        .filter(|line| {
            !line.starts_with("    Finished")
                && !line.starts_with("     Running `")
                && !line.starts_with("    Blocking waiting for file lock ")
                && !line.starts_with("   Compiling probe-run v")
                && !line.starts_with("└─ ") // remove after https://github.com/knurling-rs/probe-run/issues/217 is resolved
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

#[rstest]
#[case::successful_run_has_no_backtrace("hello-rzcobs", true)]
#[case::raw_encoding("hello-raw", true)]
#[case::successful_run_can_enforce_backtrace("hello-rzcobs --backtrace=always", true)]
#[case::stack_overflow_is_reported_as_such("overflow-rzcobs", false)]
#[case::panic_is_reported_as_such("panic-rzcobs", false)]
#[should_panic] // FIXME: see https://github.com/knurling-rs/probe-run/issues/336
#[case::panic_verbose("panic-rzcobs --verbose", false)]
#[case::unsuccessful_run_can_suppress_backtrace("panic-rzcobs --backtrace=never", false)]
#[case::stack_overflow_can_suppress_backtrace("overflow-rzcobs --backtrace=never", false)]
#[case::canary("overflow-no-flip-link", false)]
#[serial]
#[ignore = "requires the target hardware to be present"]
fn snapshot_test(#[case] args: &str, #[case] success: bool) {
    // Arrange
    let args = args.split(' ').collect::<Vec<_>>();

    // Act
    let run_result = run(args.as_slice(), false);

    // Assert
    assert_eq!(success, run_result.exit_status.success());
    insta::assert_snapshot!(run_result.output);
}

#[test]
#[serial]
#[ignore = "requires the target hardware to be present"]
#[cfg(target_family = "unix")]
fn ctrl_c_by_user_is_reported_as_such() {
    // Arrange
    let args = &["silent-loop-rzcobs"];

    // Act
    let run_result = run(args, true);

    // Assert
    assert!(!run_result.exit_status.success());
    insta::assert_snapshot!(run_result.output);
}
