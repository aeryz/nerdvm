use std::collections::HashMap;

use gdbstub::{
    stub::BaseStopReason,
    target::{
        Target,
        ext::{
            base::{
                BaseOps,
                singlethread::{SingleThreadBase, SingleThreadResume, SingleThreadSingleStep},
            },
            breakpoints::{Breakpoints, BreakpointsOps, SwBreakpoint},
        },
    },
};
use kvm_bindings::{KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_SW_BP};
use kvm_ioctls::{VcpuExit, VcpuFd};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub struct Vm {
    pub vcpu_fd: VcpuFd,
    pub guest_memory: GuestMemoryMmap<()>,
    pub breakpoints: HashMap<u64, u8>,
    pub stepping: bool,
}

impl Target for Vm {
    type Arch = gdbstub_arch::x86::X86_64_SSE;

    type Error = ();

    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }

    fn support_breakpoints(
        &mut self,
    ) -> Option<gdbstub::target::ext::breakpoints::BreakpointsOps<'_, Self>> {
        Some(self)
    }
}

impl Breakpoints for Vm {
    fn support_sw_breakpoint(
        &mut self,
    ) -> Option<gdbstub::target::ext::breakpoints::SwBreakpointOps<'_, Self>> {
        Some(self)
    }

    fn support_hw_breakpoint(
        &mut self,
    ) -> Option<gdbstub::target::ext::breakpoints::HwBreakpointOps<'_, Self>> {
        None
    }

    fn support_hw_watchpoint(
        &mut self,
    ) -> Option<gdbstub::target::ext::breakpoints::HwWatchpointOps<'_, Self>> {
        None
    }
}

impl SwBreakpoint for Vm {
    fn add_sw_breakpoint(
        &mut self,
        addr: <Self::Arch as gdbstub::arch::Arch>::Usize,
        _kind: <Self::Arch as gdbstub::arch::Arch>::BreakpointKind,
    ) -> gdbstub::target::TargetResult<bool, Self> {
        println!("add bp");
        let mut byte = [0u8];
        let _ = self
            .guest_memory
            .read(&mut byte, GuestAddress(addr))
            .unwrap();

        self.guest_memory
            .write(&[0xcc], GuestAddress(addr))
            .unwrap();

        self.add_breakpoint(addr, byte[0]);

        Ok(true)
    }

    fn remove_sw_breakpoint(
        &mut self,
        addr: <Self::Arch as gdbstub::arch::Arch>::Usize,
        _kind: <Self::Arch as gdbstub::arch::Arch>::BreakpointKind,
    ) -> gdbstub::target::TargetResult<bool, Self> {
        println!("remove bp");
        let byte = self.breakpoints.remove(&addr).unwrap();
        self.guest_memory.write(&[byte], GuestAddress(addr));
        Ok(true)
    }
}

impl SingleThreadBase for Vm {
    fn read_registers(
        &mut self,
        regs: &mut <Self::Arch as gdbstub::arch::Arch>::Registers,
    ) -> gdbstub::target::TargetResult<(), Self> {
        let registers = self.vcpu_fd.get_regs().unwrap();
        // RAX, RBX, RCX, RDX, RSI, RDI, RBP, RSP, r8-r15
        regs.regs = [
            registers.rax,
            registers.rbx,
            registers.rcx,
            registers.rdx,
            registers.rsi,
            registers.rdi,
            registers.rbp,
            registers.rsp,
            registers.r8,
            registers.r9,
            registers.r10,
            registers.r11,
            registers.r12,
            registers.r13,
            registers.r14,
            registers.r15,
        ];
        regs.rip = registers.rip;
        // XXX: is this the same thing??
        regs.eflags = registers.rflags as u32;

        let sregs = self.vcpu_fd.get_sregs().unwrap();
        regs.segments.cs = sregs.cs.selector as u32;
        regs.segments.ss = sregs.ss.selector as u32;
        regs.segments.ds = sregs.ds.selector as u32;
        regs.segments.fs = sregs.fs.selector as u32;
        regs.segments.gs = sregs.gs.selector as u32;

        Ok(())
    }

    fn write_registers(
        &mut self,
        regs: &<Self::Arch as gdbstub::arch::Arch>::Registers,
    ) -> gdbstub::target::TargetResult<(), Self> {
        todo!()
    }

    fn read_addrs(
        &mut self,
        start_addr: <Self::Arch as gdbstub::arch::Arch>::Usize,
        data: &mut [u8],
    ) -> gdbstub::target::TargetResult<usize, Self> {
        println!(
            "[GDB] reading addrs from {start_addr} for {} bytes",
            data.len()
        );
        self.guest_memory
            .read_slice(data, GuestAddress(start_addr))
            .unwrap();
        Ok(data.len())
    }

    fn write_addrs(
        &mut self,
        start_addr: <Self::Arch as gdbstub::arch::Arch>::Usize,
        data: &[u8],
    ) -> gdbstub::target::TargetResult<(), Self> {
        todo!()
    }

    fn support_resume(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadResumeOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadResume for Vm {
    fn resume(&mut self, _signal: Option<gdbstub::common::Signal>) -> Result<(), Self::Error> {
        Ok(())
    }

    fn support_single_step(
        &mut self,
    ) -> Option<gdbstub::target::ext::base::singlethread::SingleThreadSingleStepOps<'_, Self>> {
        Some(self)
    }
}

impl SingleThreadSingleStep for Vm {
    fn step(&mut self, _signal: Option<gdbstub::common::Signal>) -> Result<(), Self::Error> {
        println!("stepping boss");
        self.vcpu_fd
            .set_guest_debug(&kvm_bindings::kvm_guest_debug {
                control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP | KVM_GUESTDBG_USE_SW_BP,
                ..Default::default()
            })
            .unwrap();
        self.stepping = true;
        Ok(())
    }
}

impl Vm {
    pub fn run(&mut self) -> BaseStopReason<(), u64> {
        loop {
            match self.vcpu_fd.run().expect("run failed") {
                VcpuExit::IoIn(addr, _) => {
                    println!("io in addr: 0x{addr:?}");
                }
                VcpuExit::IoOut(addr, data) => {
                    println!("io out addr: 0x{:?}, data: {:?}", addr, data);
                }
                VcpuExit::Debug(exit) => {
                    if exit.exception == 3 {
                        return BaseStopReason::SwBreak(());
                    }

                    if self.stepping {
                        return BaseStopReason::DoneStep;
                    }
                }
                VcpuExit::MmioRead(addr, mut out) => {
                    let regs = self.vcpu_fd.get_regs().unwrap();
                    println!("mmio read happened at {addr}, {:x}", regs.rip);
                }
                VcpuExit::MmioWrite(addr, data) => {
                    println!("mmio write happened boss");
                }
                VcpuExit::Hlt => {
                    panic!("halt??");
                }
                VcpuExit::InternalError => {
                    unsafe {
                        eprintln!(
                            "internal: {:?}",
                            self.vcpu_fd.get_kvm_run().__bindgen_anon_1.internal
                        );
                    }
                    eprintln!("regs: {:#x?}", self.vcpu_fd.get_regs().unwrap());
                    eprintln!("sregs: {:#x?}", self.vcpu_fd.get_sregs().unwrap());
                    panic!("");
                }
                r => panic!("Unexpected exit reason: {:?}", r),
            }
        }
    }

    pub fn add_breakpoint(&mut self, addr: u64, byte: u8) {
        self.breakpoints.insert(addr, byte);
    }
}
