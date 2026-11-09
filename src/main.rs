use kvm_bindings::KVM_XEN_ATTR_TYPE_LONG_MODE;
use kvm_ioctls::Kvm;
use kvm_ioctls::VcpuExit;

use std::io::Write;
use std::ptr::null_mut;
use std::slice;

use kvm_bindings::KVM_MEM_LOG_DIRTY_PAGES;
use kvm_bindings::kvm_userspace_memory_region;

fn main() {
    let mem_size = 0x4000;
    let guest_addr = 0x1000;
    let asm_code = include_bytes!("guest").as_slice();

    // 1. Instantiate KVM.
    let kvm = Kvm::new().unwrap();

    // 2. Create a VM.
    let vm = kvm.create_vm().unwrap();

    // 3. Initialize Guest Memory.
    let load_addr: *mut u8 = unsafe {
        libc::mmap(
            null_mut(),
            mem_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_SHARED | libc::MAP_NORESERVE,
            -1,
            0,
        ) as *mut u8
    };

    let slot = 0;
    // When initializing the guest memory slot specify the
    // `KVM_MEM_LOG_DIRTY_PAGES` to enable the dirty log.
    let mem_region = kvm_userspace_memory_region {
        slot,
        guest_phys_addr: guest_addr,
        memory_size: mem_size as u64,
        userspace_addr: load_addr as u64,
        flags: KVM_MEM_LOG_DIRTY_PAGES,
    };
    unsafe { vm.set_user_memory_region(mem_region).unwrap() };

    // Write the code in the guest memory. This will generate a dirty page.
    unsafe {
        let mut slice = slice::from_raw_parts_mut(load_addr, mem_size);
        slice.write(&asm_code).unwrap();
    }

    // 4. Create one vCPU.
    let mut vcpu_fd = vm.create_vcpu(0).unwrap();

    const PML4: u64 = 0x2000;
    const PDPT: u64 = 0x3000;
    const PD: u64 = 0x4000;

    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const HUGE_PAGE: u64 = 1 << 7;

    // Translate a guest physical address into an offset in our host allocation.
    let ram = unsafe { std::slice::from_raw_parts_mut(load_addr, mem_size) };

    for gpa in [PML4, PDPT, PD] {
        let offset = (gpa - guest_addr) as usize;
        ram[offset..offset + 4096].fill(0);
    }

    let mut write_entry = |table_gpa: u64, index: usize, value: u64| {
        let offset = (table_gpa - guest_addr) as usize + index * 8;
        ram[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    };

    // Each table's first entry covers the low-address range.
    write_entry(PML4, 0, PDPT | PRESENT | WRITABLE);
    write_entry(PDPT, 0, PD | PRESENT | WRITABLE);

    // A page-directory entry with bit 7 set maps a 2 MiB page.
    // Physical base = 0, so virtual address == physical address.
    write_entry(PD, 0, PRESENT | WRITABLE | HUGE_PAGE);

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
    regs.rip = guest_addr;
    regs.rflags = 2;
    vcpu_fd.set_regs(&regs).unwrap();

    let mut device_0x8000: u64 = 0;

    // 6. Run code on the vCPU.
    loop {
        match vcpu_fd.run().expect("run failed") {
            VcpuExit::IoIn(_, _) => {}
            VcpuExit::IoOut(addr, data) => {
                if addr == 0x3f8 {
                    // println!("raw data: {data:?}");
                    print!("{}", String::from_utf8_lossy(data));
                }
            }
            VcpuExit::MmioRead(addr, mut out) => {
                if addr == 0x8000 {
                    out.write(device_0x8000.to_le_bytes().as_slice()).unwrap();
                }
            }
            VcpuExit::MmioWrite(addr, data) => {
                if addr == 0x8000 {
                    device_0x8000 = u64::from_le_bytes(data.try_into().unwrap());
                }
            }
            VcpuExit::Hlt => {
                break;
            }
            r => panic!("Unexpected exit reason: {:?}", r),
        }
    }
}
