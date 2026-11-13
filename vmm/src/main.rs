mod debugger;
mod vmm;

use gdbstub::stub::GdbStub;
use gdbstub::stub::SingleThreadStopReason;
use gdbstub::stub::run_blocking;
use gdbstub::target::Target;
use kvm_bindings::KVM_GUESTDBG_ENABLE;
use kvm_bindings::KVM_GUESTDBG_USE_SW_BP;
use kvm_ioctls::Kvm;
use linux_loader::bootparam::boot_params;
use linux_loader::configurator::BootConfigurator;
use linux_loader::configurator::BootParams;
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::loader::KernelLoader;
use linux_loader::loader::KernelLoaderResult;
use vm_memory::GuestAddress;
use vm_memory::GuestMemoryBackend;

use std::collections::HashMap;
use std::io::Cursor;
use std::net::TcpListener;
use std::net::TcpStream;

use kvm_bindings::KVM_MEM_LOG_DIRTY_PAGES;
use kvm_bindings::kvm_userspace_memory_region;

use crate::vmm::Vm;

type GuestMemoryMmap = vm_memory::GuestMemoryMmap<()>;

const RAM_GUEST_ADDR: u64 = 0;
const KERNEL_IMAGE_MEMSIZE: usize = 128 * 1024 * 1024;

// Memory layout:
// ----------
// 0x1000: Boot params
// 0x2000: Page table
// 0x3000: Page table
// 0x4000: Page table

enum MyGdbBlockingEventLoop {}

impl run_blocking::BlockingEventLoop for MyGdbBlockingEventLoop {
    type Target = vmm::Vm;

    type Connection = TcpStream;

    type StopReason = SingleThreadStopReason<u64>;

    fn wait_for_stop_reason(
        target: &mut Self::Target,
        conn: &mut Self::Connection,
    ) -> Result<
        run_blocking::Event<Self::StopReason>,
        run_blocking::WaitForStopReasonError<
            <Self::Target as Target>::Error,
            <Self::Connection as gdbstub::conn::Connection>::Error,
        >,
    > {
        loop {
            let stop_reason = target.run();
            return Ok(run_blocking::Event::TargetStopped(stop_reason));
        }
    }

    fn on_interrupt(
        target: &mut Self::Target,
    ) -> Result<Option<Self::StopReason>, <Self::Target as Target>::Error> {
        todo!()
    }
}

fn wait_for_gdb_connection(port: u16) -> anyhow::Result<TcpStream> {
    let sockaddr = format!("localhost:{}", port);
    eprintln!("Waiting for a GDB connection on {:?}...", sockaddr);
    let sock = TcpListener::bind(sockaddr)?;
    let (stream, addr) = sock.accept()?;

    // Blocks until a GDB client connects via TCP.
    // i.e: Running `target remote localhost:<port>` from the GDB prompt.

    eprintln!("Debugger connected from {}", addr);
    Ok(stream) // `TcpStream` implements `gdbstub::Connection`
}

fn create_memory() -> (KernelLoaderResult, GuestMemoryMmap) {
    let guest_memory =
        GuestMemoryMmap::from_ranges(&[(GuestAddress(RAM_GUEST_ADDR), KERNEL_IMAGE_MEMSIZE)])
            .unwrap();
    let mut kernel_image = vec![];
    kernel_image.extend_from_slice(include_bytes!("../../vmlinuz-virt"));
    let result = linux_loader::loader::BzImage::load(
        &guest_memory,
        None,
        &mut Cursor::new(&kernel_image),
        None,
    )
    .unwrap();

    let mut params = boot_params::default();
    let setup_header = result.setup_header.as_ref().unwrap().clone();
    params.hdr = setup_header;

    let boot_params = BootParams::new(&params, GuestAddress(0x1000));
    LinuxBootConfigurator::write_bootparams::<GuestMemoryMmap>(&boot_params, &guest_memory)
        .unwrap();

    (result, guest_memory)
}

fn main() {
    let (kernel_loader_result, guest_memory) = create_memory();

    let kvm = Kvm::new().unwrap();
    let vm = kvm.create_vm().unwrap();

    let host_memory = guest_memory
        .get_host_address(GuestAddress(RAM_GUEST_ADDR))
        .unwrap();

    let mem_region = kvm_userspace_memory_region {
        slot: 0,
        flags: KVM_MEM_LOG_DIRTY_PAGES,
        guest_phys_addr: RAM_GUEST_ADDR,
        memory_size: KERNEL_IMAGE_MEMSIZE as u64,
        userspace_addr: host_memory as u64,
    };
    unsafe {
        vm.set_user_memory_region(mem_region).unwrap();
    }

    // 4. Create one vCPU.
    let mut vcpu_fd = vm.create_vcpu(0).unwrap();
    let cpuid = kvm
        .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
        .unwrap();
    vcpu_fd.set_cpuid2(&cpuid).unwrap();

    const PML4: u64 = 0x2000;
    const PDPT: u64 = 0x3000;
    const PD: u64 = 0x4000;

    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const HUGE_PAGE: u64 = 1 << 7;

    // Translate a guest physical address into an offset in our host allocation.
    let ram = unsafe { std::slice::from_raw_parts_mut(host_memory, KERNEL_IMAGE_MEMSIZE) };

    for gpa in [PML4, PDPT, PD] {
        let offset = (gpa - RAM_GUEST_ADDR) as usize;
        ram[offset..offset + 4096].fill(0);
    }

    let mut write_entry = |table_gpa: u64, index: usize, value: u64| {
        let offset = (table_gpa - RAM_GUEST_ADDR) as usize + index * 8;
        ram[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };

    // Each table's first entry covers the low-address range.
    write_entry(PML4, 0, PDPT | PRESENT | WRITABLE);
    write_entry(PDPT, 0, PD | PRESENT | WRITABLE);

    // A page-directory entry with bit 7 set maps a 2 MiB page.
    // Physical base = 0, so virtual address == physical address.
    for i in 0..128 {
        let physical_base = (i as u64) * 0x20_0000;
        write_entry(PD, i, physical_base | PRESENT | WRITABLE | HUGE_PAGE);
    }

    // 5. Initialize general purpose and special registers.
    // x86_64 specific registry setup.
    let mut sregs = vcpu_fd.get_sregs().unwrap();

    sregs.cr3 = PML4;
    sregs.cr4 = 1 << 5; // PAE
    sregs.cr0 = (1 << 0) | (1 << 4) | (1 << 5) | (1 << 31);
    sregs.efer = (1 << 8) | (1 << 10); // LME | LMA

    let code = kvm_bindings::kvm_segment {
        base: 0,
        limit: 0xffff_ffff,
        selector: 0x8,
        type_: 11,
        present: 1,
        dpl: 0,
        db: 0,
        s: 1,
        l: 1, // Decode instructions as 64-bit
        g: 1,
        ..Default::default()
    };

    let data = kvm_bindings::kvm_segment {
        selector: 0x10,
        type_: 3,
        l: 0,
        db: 1,
        ..code
    };

    sregs.cs = code;
    sregs.ds = data;
    sregs.es = data;
    sregs.fs = data;
    sregs.gs = data;
    sregs.ss = data;

    vcpu_fd.set_sregs(&sregs).unwrap();

    let mut regs = vcpu_fd.get_regs().unwrap();
    regs.rip = kernel_loader_result.kernel_load.0 + 0x200;
    regs.rsp = 0;
    regs.rsi = 0x1000;
    regs.rflags = 2;
    vcpu_fd.set_regs(&regs).unwrap();

    // let mut debugger = Debugger::initialize(&vcpu_fd).unwrap();
    // debugger.run(None, &vcpu_fd, &guest_memory).unwrap();

    vcpu_fd
        .set_guest_debug(&kvm_bindings::kvm_guest_debug {
            control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_SW_BP,
            ..Default::default()
        })
        .unwrap();

    let mut vm = Vm {
        vcpu_fd,
        guest_memory,
        breakpoints: HashMap::new(),
        stepping: false,
    };

    let stub = GdbStub::<Vm, _>::new(wait_for_gdb_connection(8080).unwrap());

    stub.run_blocking::<MyGdbBlockingEventLoop>(&mut vm)
        .unwrap();

    // 6. Run code on the vCPU.
}
