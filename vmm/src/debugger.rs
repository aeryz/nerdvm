use std::{
    collections::HashMap,
    io::Write,
    sync::atomic::{AtomicU64, Ordering},
};

use kvm_bindings::{
    KVM_GUESTDBG_ENABLE, KVM_GUESTDBG_SINGLESTEP, KVM_GUESTDBG_USE_SW_BP, kvm_debug_exit_arch,
};
use kvm_ioctls::VcpuFd;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub struct Debugger {
    breakpoints: HashMap<u64, u8>,
    current_bp_addr: u64,
}

impl Debugger {
    pub fn initialize(vcpu_fd: &VcpuFd) -> anyhow::Result<Self> {
        vcpu_fd.set_guest_debug(&kvm_bindings::kvm_guest_debug {
            control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_SW_BP,
            ..Default::default()
        })?;

        Ok(Self {
            breakpoints: Default::default(),
            current_bp_addr: 0,
        })
    }

    pub fn add_breakpoint(&mut self, addr: u64) {
        self.breakpoints.insert(addr, 0);
    }

    pub fn run(
        &mut self,
        exit: Option<kvm_debug_exit_arch>,
        vcpu_fd: &VcpuFd,
        guest_memory: &GuestMemoryMmap<()>,
    ) -> anyhow::Result<()> {
        let mut s = String::new();

        loop {
            if self.current_bp_addr != 0 {
                let orig_byte = self.breakpoints.get(&self.current_bp_addr).unwrap();
                guest_memory.write(&[*orig_byte], GuestAddress(self.current_bp_addr))?;
                self.current_bp_addr = 0;
                vcpu_fd.set_guest_debug(&kvm_bindings::kvm_guest_debug {
                    control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_SW_BP,
                    ..Default::default()
                })?;
                break;
            }

            if let Some(exit_reason) = exit {
                match exit_reason.exception {
                    3 => {
                        println!("hit a breakpoint at 0x{:x}", exit_reason.pc);

                        vcpu_fd.set_guest_debug(&kvm_bindings::kvm_guest_debug {
                            control: KVM_GUESTDBG_ENABLE
                                | KVM_GUESTDBG_SINGLESTEP
                                | KVM_GUESTDBG_USE_SW_BP,
                            ..Default::default()
                        })?;
                        self.current_bp_addr = exit_reason.pc;
                    }
                    _ => {}
                }
            }

            print!("=> ");
            std::io::stdout().flush().unwrap();
            s.clear();
            std::io::stdin().read_line(&mut s).unwrap();

            let mut tokens = s.trim_end().split(' ');
            match tokens.next() {
                Some("ni") => {
                    vcpu_fd.set_guest_debug(&kvm_bindings::kvm_guest_debug {
                        control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP,
                        ..Default::default()
                    })?;
                    break;
                }
                Some("bp") => {
                    let addr = tokens.next().unwrap();
                    let addr = match addr.strip_prefix("0x") {
                        Some(addr) => u64::from_str_radix(addr, 16),
                        None => u64::from_str_radix(addr, 10),
                    }?;
                    guest_memory.write(&[0xcc], GuestAddress(addr))?;

                    self.add_breakpoint(addr);
                    println!("Breakpoint placed at 0x{addr:x}");
                }
                Some("c") => {
                    break;
                }
                Some(unknown) => {
                    println!("Unknown command {unknown}");
                }
                _ => {}
            }
        }

        Ok(())
    }
}
