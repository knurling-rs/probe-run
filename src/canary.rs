use std::time::Instant;

use probe_rs::{Core, MemoryInterface, RegisterId};

use crate::{
    registers::PC,
    target_info::{StackInfo, TargetInfo},
    Elf, TIMEOUT,
};

/// Canary value
const CANARY_U8: u8 = 0xAA;
/// Canary value
const CANARY_U32: u32 = u32::from_le_bytes([CANARY_U8, CANARY_U8, CANARY_U8, CANARY_U8]);

/// (Location of) the stack canary
///
/// The stack canary is used to detect *potential* stack overflows
///
/// The canary is placed in memory as shown in the diagram below:
///
/// ``` text
/// +--------+ -> initial_stack_pointer / stack_range.end()
/// |        |
/// | stack  | (grows downwards)
/// |        |
/// +--------+
/// |        |
/// |        |
/// +--------+
/// | canary |
/// +--------+ -> stack_range.start()
/// |        |
/// | static | (variables, fixed size)
/// |        |
/// +--------+ -> lowest RAM address
/// ```
///
/// The whole canary is initialized to `CANARY_U8` before the target program is started.
/// The canary size is 10% of the available stack space or 1 KiB, whichever is smallest.
///
/// When the programs ends (due to panic or breakpoint) the integrity of the canary is checked. If it was
/// "touched" (any of its bytes != `CANARY_U8`) then that is considered to be a *potential* stack
/// overflow.
#[derive(Clone, Copy)]
pub struct Canary {
    address: u32,
    size: u32,
    data_below_stack: bool,
}

impl Canary {
    /// Decide if and where to place the stack canary.
    ///
    /// Assumes that the target was reset-halted.
    pub fn install(
        core: &mut Core,
        target_info: &TargetInfo,
        elf: &Elf,
    ) -> Result<Option<Self>, anyhow::Error> {
        let canary = match Self::prepare(&target_info.stack_info, elf) {
            Some(canary) => canary,
            None => return Ok(None),
        };

        let size_kb = canary.size_kb();

        // Painting 100KB or more takes a few seconds, so provide user feedback.
        log::info!("painting {size_kb:.2} KiB of RAM for stack usage estimation");

        let start = Instant::now();
        paint_subroutine::execute(core, canary.address, canary.size)?;
        let seconds = start.elapsed().as_secs_f64();
        log::trace!(
            "setting up canary took {seconds:.3}s ({:.2} KiB/s)",
            size_kb / seconds
        );

        Ok(Some(canary))
    }

    /// Detect if the stack canary was touched.
    pub fn touched(self, core: &mut Core, elf: &Elf) -> anyhow::Result<bool> {
        let size_kb = self.size_kb();
        log::info!("reading {size_kb:.2} KiB of RAM for stack usage estimation",);
        let start = Instant::now();

        let touched_address = measure_subroutine::execute(core, self.address, self.size)?;

        let seconds = start.elapsed().as_secs_f64();
        log::trace!(
            "reading canary took {seconds:.3}s ({:.2} KiB/s)",
            size_kb / seconds
        );

        let min_stack_usage = touched_address.map(|touched_address| {
            log::debug!("canary was touched at {touched_address:#010X}");
            elf.vector_table.initial_stack_pointer - touched_address
        });

        let min_stack_usage = min_stack_usage.unwrap_or(0);
        let used_kb = min_stack_usage as f64 / 1024.0;
        let pct = used_kb / size_kb * 100.0;
        log::info!(
            "program has used at least {used_kb:.2}/{size_kb:.2} KiB ({pct:.1}%) of stack space"
        );

        if pct > 90.0 && self.data_below_stack {
            log::warn!("data segments might be corrupted due to stack overflow");
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Prepare, but not place the canary.
    ///
    /// If this succeeds, we have all the information we need in order to place the canary.
    fn prepare(stack_info: &Option<StackInfo>, elf: &Elf) -> Option<Self> {
        let stack_info = match stack_info {
            Some(stack_info) => stack_info,
            None => {
                log::debug!("couldn't find valid stack range, not placing stack canary");
                return None;
            }
        };

        if elf.program_uses_heap() {
            log::debug!("heap in use, not placing stack canary");
            return None;
        }

        let stack_start = *stack_info.range.start();
        let size = *stack_info.range.end() - stack_start;

        log::debug!(
            "{size} bytes of stack available ({:#010X} ..= {:#010X})",
            stack_info.range.start(),
            stack_info.range.end(),
        );

        Some(Canary {
            address: stack_start,
            size,
            data_below_stack: stack_info.data_below_stack,
        })
    }

    fn size_kb(self) -> f64 {
        self.size as f64 / 1024.0
    }
}

/// Assert 4-byte-alignment and that subroutine fits inside stack.
macro_rules! assert_subroutine {
    ($low_addr:expr, $stack_size:expr, $subroutine_size:expr) => {
        assert_eq!($low_addr % 4, 0, "low_addr needs to be 4-byte-aligned");
        assert_eq!($stack_size % 4, 0, "stack_size needs to be 4-byte-aligned");
        assert_eq!(
            $subroutine_size % 4,
            0,
            "subroutine needs to be 4-byte-aligned"
        );
        assert!(
            $subroutine_size < $stack_size,
            "subroutine does not fit inside stack"
        );
    };
}

/// Paint-stack subroutine.
///
/// # Rust
///
/// Corresponds to following rust code:
///
/// ```rust
/// unsafe fn paint(low_addr: u32, high_addr: u32, pattern: u32) {
///     while low_addr <= high_addr {
///         (low_addr as *mut u32).write(pattern);
///         low_addr += 4;
///     }
/// }
/// ```  
///
/// # Assembly
///
/// The assembly is generated from the Rust function `fn paint()` above, using the
/// jorge-hack.
///
/// ```armasm
/// 000200ec <paint>:
///    200ec:    4288    cmp      r0, r1
///    200ee:    d801    bhi.n    #6 <paint+0x8>
///    200f0:    c004    stmia    r0!, {r2}
///    200f2:    e7fb    b.n      #-6 <paint>
///
/// 000200f4 <paint+0x8>:
///    200f4:    be00    bkpt     0x0000
/// ```
mod paint_subroutine {
    use super::*;

    /// Write the carnary value to the stack.
    ///
    /// # Safety
    ///
    /// - Expects the [`Core`] to be halted and will leave it halted when the function
    /// returns.
    /// - `low_addr` and `size` need to be 4-byte-aligned.
    ///
    /// # How?
    ///
    /// We place the subroutine inside the memory we want to paint. The subroutine
    /// paints the whole memory, except of itself. After the subroutine finishes
    /// executing we overwrite the subroutine using the probe.
    pub fn execute(core: &mut Core, low_addr: u32, stack_size: u32) -> Result<(), probe_rs::Error> {
        assert_subroutine!(low_addr, stack_size, self::SUBROUTINE.len() as u32);
        super::execute_subroutine(core, low_addr, stack_size, self::SUBROUTINE)?;
        self::overwrite_subroutine(core, low_addr)?;
        Ok(())
    }

    /// Overwrite the subroutine with the canary value.
    ///
    /// Happens after the subroutine finishes.
    fn overwrite_subroutine(core: &mut Core, low_addr: u32) -> Result<(), probe_rs::Error> {
        core.write_8(low_addr as u64, &[CANARY_U8; self::SUBROUTINE.len()])
    }

    const SUBROUTINE: [u8; 12] = [
        0x88, 0x42, // cmp      r0, r1
        0x01, 0xd8, // bhi.n    #6 <paint+0x8>
        0x04, 0xc0, // stmia    r0!, {r2}
        0xfb, 0xe7, // b.n      #-6 <paint>
        0x00, 0xbe, // bkpt     0x0000
        0x00, 0xbe, // bkpt     0x0000 (padding instruction)
    ];
}

/// Measure-stack subroutine.
///
/// # Rust
///
/// Corresponds to following rust code;
///
/// ```rust
/// #[export_name = "measure"]
/// unsafe fn measure(mut low_addr: u32, high_addr: u32, pattern: u32) -> u32 {
///     let mut result = 0;
///
///     while low_addr < high_addr {
///         if (low_addr as *const u32).read() != pattern {
///             result = low_addr;
///             break;
///         } else {
///             low_addr += 4;
///         }
///     }
///
///     result
/// }
/// ```
///
/// # Assembly
///
/// The assembly is generated from the Rust function `fn measure()` above, using the
/// jorge-hack.
///
/// ```armasm
/// 000200ec <measure>:
///     200ec:    4288    cmp      r0, r1
///     200ee:    d204    bcs.n    #0xc <measure+0xe>
///     200f0:    6803    ldr      r3, [r0, #0]
///     200f2:    4293    cmp      r3, r2
///     200f4:    d102    bne.n    #8 <measure+0x10>
///     200f6:    1d00    adds     r0, r0, #4
///     200f8:    e7f8    b.n      #-8 <measure>
///
/// 000200fa <measure+0xe>:
///     200fa:    2000    movs     r0, #0
///
/// 000200fc <measure+0x10>:
///     200fc:    be00    bkpt     0x0000
/// //                    ^^^^ this was `bx lr`
/// ```
mod measure_subroutine {
    use super::*;

    /// Search for lowest touched byte in memory.
    ///
    /// The returned `Option<u32>` is `None`, if the memory is untouched. Otherwise it
    /// gives the position of the lowest byte which isn't equal to the pattern anymore.
    ///
    /// # Safety
    ///
    /// - Expects the [`Core`] to be halted and will leave it halted when the function
    /// returns.
    /// - `low_addr` and `size` need to be 4-byte-aligned.
    ///
    /// # How?
    ///
    /// Before we place the subroutine in the memory, we search through the memory we
    /// want to place the subroutine to check if the stack usage got that far. If we
    /// find a touched byte we return it. Otherwise we place the subroutine in this
    /// memory region and execute it. After the subroutine finishes we read out the
    /// address of the lowest touched 4-byte-word from the register r0. If r0 is `0`
    /// we return `None`. Otherwise we process it to get the address of the lowest
    /// byte, not only 4-byte-word.
    pub fn execute(
        core: &mut Core,
        low_addr: u32,
        stack_size: u32,
    ) -> Result<Option<u32>, probe_rs::Error> {
        assert_subroutine!(low_addr, stack_size, self::SUBROUTINE.len() as u32);

        // use probe to search through the memory the subroutine will be written to
        match self::search_with_probe(core, low_addr)? {
            addr @ Some(_) => return Ok(addr), // if we find a touched value, return early ...
            None => {}                         // ... otherwise we continue
        }

        super::execute_subroutine(core, low_addr, stack_size, self::SUBROUTINE)?;
        self::get_result(core)
    }

    /// Searches though memory byte by byte using the SWD/JTAG probe.
    ///
    /// Happens before we place the subroutine in memory.
    fn search_with_probe(core: &mut Core, low_addr: u32) -> Result<Option<u32>, probe_rs::Error> {
        let mut buf = [0; self::SUBROUTINE.len()];
        core.read_8(low_addr as u64, &mut buf)?;
        match buf.into_iter().position(|b| b != CANARY_U8) {
            Some(pos) => Ok(Some(low_addr + pos as u32)),
            None => Ok(None),
        }
    }

    /// Read out result from register r0 and process it to get lowest touched byte.
    ///
    /// Happens after the subroutine finishes.
    fn get_result(core: &mut Core) -> Result<Option<u32>, probe_rs::Error> {
        // get the address of the lowest touched 4-byte-word
        let word_addr = match core.read_core_reg(RegisterId(0))? {
            0 => return Ok(None),
            n => n,
        };

        // take a closer look at word, to get address of lowest touched byte
        let offset = core
            .read_word_32(word_addr as u64)?
            .to_le_bytes()
            .into_iter()
            .position(|b| b != CANARY_U8)
            .unwrap();

        Ok(Some(word_addr + offset as u32))
    }

    const SUBROUTINE: [u8; 20] = [
        0x88, 0x42, // cmp      r0, r1
        0x04, 0xd2, // bcs.n    #0xc <measure+0xe>
        0x03, 0x68, // ldr      r3, [r0, #0]
        0x93, 0x42, // cmp      r3, r2
        0x02, 0xd1, // bne.n    #8 <measure+0x10>
        0x00, 0x1d, // adds     r0, r0, #4
        0xf8, 0xe7, // b.n      #-8 <measure>
        0x00, 0x20, // movs     r0, #0
        0x00, 0xbe, // bkpt     0x0000
        0x00, 0xbe, // bkpt     0x0000 (padding instruction)
    ];
}

/// Prepare and run subroutine. Also clean up afterwards.
///
/// # How?
///
/// We place the parameters in the registers (see table below), place the subroutien
/// in memory, set the program counter to the beginning of the subroutine, execute
/// the subroutine and reset the program counter afterwards.
///
/// ## Register-parameter-mapping
///
/// | register | paramter                  |
/// | :------: | :------------------------ |
/// | `r0`     | `low_addr` + return value |
/// | `r1`     | `high_addr`               |
/// | `r2`     | `pattern`                 |
fn execute_subroutine<const N: usize>(
    core: &mut Core,
    low_addr: u32,
    stack_size: u32,
    subroutine: [u8; N],
) -> Result<(), probe_rs::Error> {
    let subroutine_size = N as u32;
    let high_addr = low_addr + stack_size;

    // set the registers
    // NOTE: add `subroutine_size` to `low_addr`, to avoid the subroutine overwriting itself
    core.write_core_reg(RegisterId(0), low_addr + subroutine_size)?;
    core.write_core_reg(RegisterId(1), high_addr)?;
    core.write_core_reg(RegisterId(2), CANARY_U32)?;

    // write subroutine to stack
    core.write_8(low_addr as u64, &subroutine)?;

    // store current PC and set PC to beginning of subroutine
    let previous_pc = core.read_core_reg(PC)?;
    core.write_core_reg(PC, low_addr)?;

    // execute the subroutine and wait for it to finish
    core.run()?;
    core.wait_for_core_halted(TIMEOUT)?;

    // reset PC to where it was before
    core.write_core_reg::<u32>(PC, previous_pc)?;

    Ok(())
}
