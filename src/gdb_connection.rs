use std::{sync::Mutex, thread::JoinHandle, net::TcpListener};
use probe_rs::Session;
use log::Level;
use gdbstub::target::{Target, ext::{base::{BaseOps, singlethread}, breakpoints::HwBreakpointOps}};
use gdbstub::arch::{RegId, Arch};
use gdbstub::arch::arm::reg::ArmCoreRegs;
use gdbstub::target::ext::base::singlethread::SingleThreadOps;

// todo handle
// - gdb client re-attach
// - persistence throughout several program runs (e.g. restarts by gdb– is that built-in?
//   but probe-run quits on program exit? )

enum ArchArm {}
/// 32-bit ARM core register identifier.
/// see https://developer.arm.com/documentation/100166/0001/Programmers-Model/Processor-core-register-summary
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
enum ArmCortexMRegId {
    /// General purpose registers (R0-R12)
    Gpr(u8),
    /// Stack Pointer (R13)
    Sp,
    /// Link Register (R14)
    Lr,
    /// Program Counter (R15)
    Pc,

    // TODO Not sure if this fully covers hard-float targets?
}

impl RegId for ArmCortexMRegId {
    fn from_raw_id(id: usize) -> Option<(Self, usize)> {
        let reg = match id {
            0..=12 => Self::Gpr(id as u8),
            13 => Self::Sp,
            14 => Self::Lr,
            15 => Self::Pc,
            _ => return None,
        };
        Some((reg, 4))
    }
}

impl Arch for ArchArm {
    // 32-bit processor / see
    // https://developer.arm.com/documentation/dui0491/i/C-and-C---Implementation-Details/Basic-data-types
    type Usize: = u32;

    // TODO ölet's seeif we can recycle these
    type Registers = ArmCoreRegs;

    type RegId = ArmCortexMRegId;
}
struct ArmCortexM();

// let's start with Cortex-M4
impl Target for ArmCortexM {
    type Arch = ArchArm; // TODO
    type Error = bool; // TODO
    fn hw_breakpoint(&mut self) -> Option<HwBreakpointOps<'_, Self>> {
          // mal gucken wo wir `core` her bekommen
          //core.set_hw_breakpoint();
          None
    }

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }
}

impl SingleThreadOps for ArmCortexM {
    fn resume(
        &mut self,
        action: gdbstub::target::ext::base::ResumeAction,
        check_gdb_interrupt: &mut dyn FnMut() -> bool,
    ) -> Result<singlethread::StopReason<<Self::Arch as Arch>::Usize>, Self::Error> {
        todo!()
    }

    fn read_registers(
        &mut self,
        regs: &mut <Self::Arch as Arch>::Registers,
    ) -> gdbstub::target::TargetResult<(), Self> {
        todo!()
    }

    fn write_registers(&mut self, regs: &<Self::Arch as Arch>::Registers)
        -> gdbstub::target::TargetResult<(), Self> {
        todo!()
    }

    fn read_addrs(
        &mut self,
        start_addr: <Self::Arch as Arch>::Usize,
        data: &mut [u8],
    ) -> gdbstub::target::TargetResult<(), Self> {
        todo!()
    }

    fn write_addrs(
        &mut self,
        start_addr: <Self::Arch as Arch>::Usize,
        data: &[u8],
    ) -> gdbstub::target::TargetResult<(), Self> {
        todo!()
    }
}

pub const DEFAULT_GDB_SERVER_ADDR: &str = "127.0.0.1:1337";

/// Spawns a thread that opens a GDB connection to the target and handles any communication
///
/// `server_address`   is the `ip:port` address under which the server will be reachable
pub fn spawn(server_address: &'static str, session: &Mutex<Session>) -> Option<JoinHandle<()>>{

    let gdb_thread = Some(std::thread::spawn(move || {
        log::info!("starting gdb server at {}", server_address);

        // TODO actually do the thing here

    }));

    gdb_thread
}
