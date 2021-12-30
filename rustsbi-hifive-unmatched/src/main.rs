#![no_std]
#![no_main]
#![feature(naked_functions, asm_const, asm_sym)]
#![feature(generator_trait)]
#![feature(default_alloc_error_handler)]
#![feature(ptr_metadata)]

extern crate alloc;

mod console;
mod device_tree;
mod early_trap;
mod execute;
mod feature;
mod hart_csr_utils;
mod peripheral;
mod runtime;
mod util;

use console::{eprintln, println};
use core::panic::PanicInfo;

#[panic_handler]
fn on_panic(info: &PanicInfo) -> ! {
    let hart_id = riscv::register::mhartid::read();
    eprintln!("[rustsbi-panic] hart {} {}", hart_id, info); // [rustsbi-panic] hart 0 panicked at xxx
    loop {}
}

static DEVICE_TREE: &'static [u8] = include_bytes!("hifive-unmatched-a00.dtb");

// ref: https://github.com/riscv-software-src/opensbi/blob/master/include/sbi/fw_dynamic.h
#[repr(C)]
#[derive(Debug)]
struct FwDynamicInfo {
    magic: usize,
    version: usize,
    next_addr: usize,
    next_mode: usize,
    options: usize,
    boot_hart: usize,
}
const FW_DYNAMIC_INFO_MAGIC_VALUE: usize = 0x4942534f;

fn rust_main(hart_id: usize, opaque: usize, fw_dynamic_info: *const FwDynamicInfo) {
    let clint = peripheral::Clint::new(0x2000000 as *mut u8);
    let fw_dynamic_info = unsafe { &*fw_dynamic_info };
    let boot_hart = fw_dynamic_info.boot_hart;

    if hart_id == boot_hart {
        init_bss();
        let uart = unsafe { peripheral::Uart::preloaded_uart0() };
        crate::console::init_stdout(uart);
        for target_hart_id in 1..=4 {
            if target_hart_id != boot_hart {
                clint.send_soft(target_hart_id);
            }
        }
    } else {
        pause(clint);
    }
    let opaque = if opaque == 0 {
        // 如果上一级没有填写设备树文件，这一级填写
        DEVICE_TREE.as_ptr() as usize
    } else {
        opaque
    };
    early_trap::init(hart_id);
    if hart_id == boot_hart {
        init_heap(); // 必须先加载堆内存，才能使用rustsbi框架
        let uart = unsafe { peripheral::Uart::preloaded_uart0() };
        init_rustsbi_stdio(uart);
        init_rustsbi_clint(clint);
        println!("[rustsbi] RustSBI version {}", rustsbi::VERSION);
        // println!("{}", rustsbi::LOGO);
        println!(
            "[rustsbi] Implementation: RustSBI-HiFive-Unleashed Version {}",
            env!("CARGO_PKG_VERSION")
        );
        if let Err(e) = unsafe { device_tree::parse_device_tree(opaque) } {
            println!("[rustsbi] warning: choose from device tree error, {}", e);
        }
        delegate_interrupt_exception();
        hart_csr_utils::print_hartn_csrs();
        println!(
            "[rustsbi] enter supervisor, opaque register {:#x}, fw_dynamic_info {:?}",
            opaque,
            &fw_dynamic_info
        );
        for target_hart_id in 1..=4 {
            if target_hart_id != boot_hart {
                clint.send_soft(target_hart_id);
            }
        }
    } else {
        // 不是初始化核，先暂停
        if hart_id != 0 {
            delegate_interrupt_exception(); // 第0个核不能委托中断（@dram）
        }
        pause(clint);
    }
    runtime::init();
    execute::execute_supervisor(fw_dynamic_info.next_addr, hart_id, opaque);
}

fn init_bss() {
    extern "C" {
        static mut ebss: u32;
        static mut sbss: u32;
        static mut edata: u32;
        static mut sdata: u32;
        static sidata: u32;
    }
    unsafe {
        r0::zero_bss(&mut sbss, &mut ebss);
        r0::init_data(&mut sdata, &mut edata, &sidata);
    }
}

fn init_rustsbi_stdio(uart: peripheral::Uart) {
    use rustsbi::legacy_stdio::init_legacy_stdio_embedded_hal;
    init_legacy_stdio_embedded_hal(uart);
}

fn init_rustsbi_clint(clint: peripheral::Clint) {
    rustsbi::init_ipi(clint);
    rustsbi::init_timer(clint);
}

fn delegate_interrupt_exception() {
    use riscv::register::{medeleg, mideleg, mie};
    unsafe {
        mideleg::set_sext();
        mideleg::set_stimer();
        mideleg::set_ssoft();
        mideleg::set_uext();
        mideleg::set_utimer();
        mideleg::set_usoft();
        medeleg::set_instruction_misaligned();
        medeleg::set_breakpoint();
        medeleg::set_user_env_call();
        medeleg::set_instruction_page_fault();
        medeleg::set_load_page_fault();
        medeleg::set_store_page_fault();
        medeleg::set_instruction_fault();
        medeleg::set_load_fault();
        medeleg::set_store_fault();
        mie::set_mext();
        // 不打开mie::set_mtimer
        mie::set_msoft();
    }
}

pub fn pause(clint: peripheral::Clint) {
    use riscv::asm::wfi;
    use riscv::register::{mhartid, mie, mip};
    unsafe {
        let hartid = mhartid::read();
        clint.clear_soft(hartid); // Clear IPI
        mip::clear_msoft(); // clear machine software interrupt flag
        let prev_msoft = mie::read().msoft();
        mie::set_msoft(); // Start listening for software interrupts
        loop {
            wfi();
            if mip::read().msoft() {
                break;
            }
        }
        if !prev_msoft {
            mie::clear_msoft(); // Stop listening for software interrupts
        }
        clint.clear_soft(hartid); // Clear IPI
    }
}

const SBI_HEAP_SIZE: usize = 64 * 1024; // 64KiB
#[link_section = ".bss.uninit"]
static mut HEAP_SPACE: [u8; SBI_HEAP_SIZE] = [0; SBI_HEAP_SIZE];

use buddy_system_allocator::LockedHeap;

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap<32> = LockedHeap::<32>::empty();

#[inline]
fn init_heap() {
    unsafe {
        HEAP_ALLOCATOR
            .lock()
            .init(HEAP_SPACE.as_ptr() as usize, SBI_HEAP_SIZE);
    }
}

const PER_HART_STACK_SIZE: usize = 4 * 4096; // 16KiB
const SBI_STACK_SIZE: usize = 5 * PER_HART_STACK_SIZE; // 5 harts
#[link_section = ".bss.uninit"]
static mut SBI_STACK: [u8; SBI_STACK_SIZE] = [0; SBI_STACK_SIZE];

#[naked]
#[link_section = ".text.entry"]
#[export_name = "_start"]
unsafe extern "C" fn entry() -> ! {
    core::arch::asm!(
    // 1. clear all registers
    "li x1, 0
    li x2, 0
    li x3, 0
    li x4, 0
    li x5, 0
    li x6, 0
    li x7, 0
    li x8, 0
    li x9, 0",
    // no x10, x11 and x12: x10 is a0, x11 is a1 and x12 is a2, they are passed to
    // main function as arguments
    "li x13, 0
    li x14, 0
    li x15, 0
    li x16, 0
    li x17, 0
    li x18, 0
    li x19, 0
    li x20, 0
    li x21, 0
    li x22, 0
    li x23, 0
    li x24, 0
    li x25, 0
    li x26, 0
    li x27, 0
    li x28, 0
    li x29, 0
    li x30, 0
    li x31, 0",
    // 2. set sp
    // sp = bootstack + (hart_id + 1) * HART_STACK_SIZE
    "
    la      sp, {stack}
    li      t0, {per_hart_stack_size}
    csrr    t1, mhartid
    addi    t2, t1, 1
1:  add     sp, sp, t0
    addi    t2, t2, -1
    bnez    t2, 1b
    ",
    // 3. jump to main function (absolute address)
    "call   {rust_main}",
    // 4. after main function return, invoke CEASE instruction
    ".word 0x30500073", // cease
    per_hart_stack_size = const PER_HART_STACK_SIZE,
    stack = sym SBI_STACK,
    rust_main = sym rust_main,
    options(noreturn))
}
