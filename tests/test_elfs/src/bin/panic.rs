#![no_main]
#![no_std]

use test_elfs as _; // global logger + panicking-behavior + memory layout

#[cortex_m_rt::entry]
fn main() -> ! {
    defmt::panic!()
}
