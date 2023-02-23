#![no_main]
#![no_std]

use app as _;

#[cortex_m_rt::entry]
fn main() -> ! {
    defmt::flush(); // BUG: panics without `defmt::flush` or `defmt::println
    loop {}
}
