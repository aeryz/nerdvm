use kvm_bindings::KVM_GUESTDBG_ENABLE;
use kvm_bindings::KVM_GUESTDBG_SINGLESTEP;
use kvm_ioctls::Kvm;
use kvm_ioctls::VcpuExit;
use linux_loader::bootparam::boot_params;
use linux_loader::configurator::BootConfigurator;
use linux_loader::configurator::BootParams;
use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::loader::KernelLoader;
use linux_loader::loader::KernelLoaderResult;
use vm_memory::GuestAddress;
use vm_memory::GuestMemoryBackend;
use vm_memory::GuestRegionCollection;
use vm_memory::GuestRegionMmap;

use std::io::Cursor;

use kvm_bindings::KVM_MEM_LOG_DIRTY_PAGES;
use kvm_bindings::kvm_userspace_memory_region;

type GuestMemoryMmap = vm_memory::GuestMemoryMmap<()>;

const RAM_GUEST_ADDR: u64 = 0;
const KERNEL_IMAGE_MEMSIZE: usize = 128 * 1024 * 1024;

// Memory layout:
// ----------
// 0x1000: Boot params
// 0x2000: Page table
// 0x3000: Page table
// 0x4000: Page table

fn create_memory() -> (KernelLoaderResult, GuestRegionCollection<GuestRegionMmap>) {
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

    let debug = kvm_bindings::kvm_guest_debug {
        control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_SINGLESTEP,
        ..Default::default()
    };

    let mut continue_until = 0;

    // 6. Run code on the vCPU.
    loop {
        vcpu_fd.set_guest_debug(&debug).unwrap();
        match vcpu_fd.run().expect("run failed") {
            VcpuExit::IoIn(addr, _) => {
                println!("io in addr: 0x{addr:?}");
            }
            VcpuExit::IoOut(addr, data) => {
                println!("io out addr: 0x{:?}, data: {:?}", addr, data);
            }
            VcpuExit::Debug(_) => {
                let regs = vcpu_fd.get_regs().unwrap();
                if continue_until == 0 || (continue_until != 0 && continue_until == regs.rip) {
                    if continue_until != 0 {
                        continue_until = 0;
                    }
                    println!("rip: 0x{:x}, 0x{:x}", regs.rip, regs.rbx);
                    let mut s = String::new();
                    std::io::stdin().read_line(&mut s).unwrap();
                    if let Ok(addr) = u64::from_str_radix(&s.strip_suffix('\n').unwrap(), 16) {
                        println!("waiting until rip = 0x{addr:x}");
                        continue_until = addr;
                    }
                }
            }
            VcpuExit::MmioRead(addr, mut out) => {
                let regs = vcpu_fd.get_regs().unwrap();
                println!("mmio read happened at {addr}, {:x}", regs.rip);
            }
            VcpuExit::MmioWrite(addr, data) => {
                println!("mmio write happened boss");
            }
            VcpuExit::Hlt => {
                println!("halt??");
                break;
            }
            VcpuExit::InternalError => {
                unsafe {
                    eprintln!(
                        "internal: {:?}",
                        vcpu_fd.get_kvm_run().__bindgen_anon_1.internal
                    );
                }
                eprintln!("regs: {:#x?}", vcpu_fd.get_regs().unwrap());
                eprintln!("sregs: {:#x?}", vcpu_fd.get_sregs().unwrap());
                break;
            }
            r => panic!("Unexpected exit reason: {:?}", r),
        }
    }
}
