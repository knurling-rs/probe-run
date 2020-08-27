//! Control a `probe-run` process on the host computer from an embedded app.

#![no_std]

use cortex_m::asm;

/// Exits the `probe-run` host process with a success status.
pub fn exit() -> ! {
    loop {
        asm::bkpt();
    }
}

/// Exits the `probe-run` host process with an error status.
pub fn abort() -> ! {
    #[inline(never)] // ensure that this always has its own stack frame
    #[no_mangle]
    fn __probe_run_internal_abort() -> ! {
        loop {
            asm::bkpt();
        }
    }

    __probe_run_internal_abort();
}
