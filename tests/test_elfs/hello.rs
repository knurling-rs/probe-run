#![no_main]
#![no_std]

use app as _;

#[cortex_m_rt::entry]
fn main() -> ! {
    defmt::println!("Hello, world!");

    app::exit()
}
