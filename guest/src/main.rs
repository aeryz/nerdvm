#![no_std]
#![no_main]

use core::panic::PanicInfo;

fn print(s: &[u8]) {
    for b in s {
        unsafe {
            core::ptr::write_volatile(0x8000 as *mut u8, *b);
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    print(b"hello world!");

    loop {}
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
