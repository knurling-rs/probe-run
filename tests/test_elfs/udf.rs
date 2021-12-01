#![no_main]
#![no_std]

use udf_app as _; // global logger + panicking-behavior + memory layout

#[cortex_m_rt::entry]
fn main() -> ! {
    cortex_m::asm::udf()
}
