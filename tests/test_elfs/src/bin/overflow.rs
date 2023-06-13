#![no_main]
#![no_std]

use test_elfs as _; // global logger + panicking-behavior + memory layout

#[cortex_m_rt::entry]
fn main() -> ! {
    ack(10, 10);
    test_elfs::exit()
}

fn ack(m: u32, n: u32) -> u32 {
    // waste stack space to trigger a stack overflow
    let mut buffer = [0u8; 16 * 1024];
    // estimate of the Stack Pointer register
    let sp = buffer.as_mut_ptr();
    defmt::println!("ack(m={=u32}, n={=u32}, SP={:x})", m, n, sp);

    if m == 0 {
        n + 1
    } else {
        if n == 0 {
            ack(m - 1, 1)
        } else {
            ack(m - 1, ack(m, n - 1))
        }
    }
}
