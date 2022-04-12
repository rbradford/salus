// Copyright (c) 2021 by Rivos Inc.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

#![no_main]
#![no_std]
#![feature(panic_info_message, allocator_api, alloc_error_handler, lang_items)]

use core::alloc::{GlobalAlloc, Layout};
use core::slice;

extern crate alloc;

mod abort;
mod asm;
mod data_measure;
mod print_util;
mod test_measure;
mod vm;
mod vm_pages;

use abort::abort;
use device_tree::Fdt;
use print_util::*;
use riscv_page_tables::page_tracking::HypMemoryPages;
use riscv_page_tables::*;
use riscv_pages::*;
use test_measure::TestMeasure;
use vm::Host;
use vm_pages::HostRootBuilder;

const RAM_BASE: u64 = 0x8000_0000;

extern "C" {
    static _stack_end: u8;
}

// Dummy global allocator - panic if anything tries to do an allocation.
struct GeneralGlobalAlloc;

unsafe impl GlobalAlloc for GeneralGlobalAlloc {
    unsafe fn alloc(&self, _layout: Layout) -> *mut u8 {
        abort()
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        abort()
    }
}

#[global_allocator]
static GENERAL_ALLOCATOR: GeneralGlobalAlloc = GeneralGlobalAlloc;

#[alloc_error_handler]
pub fn alloc_error(_layout: Layout) -> ! {
    abort()
}

/// Powers off this machine.
pub fn poweroff() -> ! {
    // Safety: on this platform, a write of 0x5555 to 0x100000 will trigger the platform to
    // poweroff, which is defined behavior.
    unsafe {
        core::ptr::write_volatile(0x10_0000 as *mut u32, 0x5555);
    }
    abort()
}

/// Adds a device tree to host memory at the host offset given.
/// Returns the size of the host's device tree.
/// # Safety:
///     All memory past the end of hypervisor memory must be unused and available for writing. (this
///     will all be assigned to the host).
///     `host_dt_addr` must poing to memory that is safe for writing `host_ram_size` bytes to.
unsafe fn pass_device_tree(hyp_fdt: &Fdt, host_dt_addr: u64, host_ram_size: u64) -> u64 {
    // Create a slice of the fdt passed from firmware
    let dt_size = hyp_fdt.size();

    // Update memory size - TODO - other modifications
    let host_slice = slice::from_raw_parts_mut(host_dt_addr as *mut u8, dt_size);
    hyp_fdt.write_with_updated_memory_size(host_slice, host_ram_size);

    // Make sure the new FDT parses and the memory was updated as expected.
    let host_fdt = match Fdt::new_from_raw_pointer(host_dt_addr as *const u8) {
	Ok(fdt) => fdt,
	Err(e) => panic!("Failed to parse host FDT: {}", e),
    };
    let (_, host_fdt_ram_size) = host_fdt.get_mem_info();
    assert!(host_ram_size == host_fdt_ram_size);

    dt_size as u64
}

// Basic configuration of and running the test VM.
fn test_boot_vm(hart_id: u64, fdt_addr: u64) {
    // put the host DT somewhere the host can read it.
    const HOST_DT_OFFSET: u64 = 0x220_0000;

    // Safe because we trust that the firmware passed a valid FDT.
    let hyp_fdt = match unsafe { Fdt::new_from_raw_pointer(fdt_addr as *const u8) } {
	Ok(fdt) => fdt,
	Err(e) => panic!("Failed to read FDT: {}", e),
    };
    let hyp_fdt_end = fdt_addr.checked_add(hyp_fdt.size().try_into().unwrap()).unwrap();
    let (ram_base, ram_size) = hyp_fdt.get_mem_info();

    // Safe because we trust the linker placed _stack_end correctly.
    let hyp_stack_end = unsafe { core::ptr::addr_of!(_stack_end) as u64 };

    // We assume that the FDT is placed after the hypervisor image and that everything up until
    // the end of the FDT is unusable
    assert!(hyp_stack_end <= fdt_addr);
    let ram_start_page = PageAddr4k::new(PhysAddr::new(ram_base)).unwrap();
    let usable_start_page = PageAddr4k::with_round_up(PhysAddr::new(hyp_fdt_end));
    let hw_map = unsafe { HwMemMap::new(ram_start_page, ram_size, usable_start_page) };
    let mut hyp_mem = HypMemoryPages::new(hw_map);

    let host_guests_pages =
        match SequentialPages::<PageSize4k>::from_pages(hyp_mem.by_ref().take(2)) {
            Ok(s) => s,
            Err(_) => unreachable!(),
        };

    let (mut host_pages, host_root_builder) = HostRootBuilder::from_hyp_mem(hyp_mem);

    let host_base = host_pages.next_addr().bits();
    let host_size = host_pages.remaining_size();

    // This is not safe, assumes that the host kernel is loaded at 0xc020_0000 by qemu, that it
    // doesn't overlap with hypervisor memory, and that it is less than 0x200_0000 long.
    // TODO - find a better way to locate the host payload
    let dt_len = unsafe {
        // Not safe!
        let kern_addr = 0xc020_0000 as *mut u8;
        let kern_size = 0x200_0000;
        core::ptr::copy(kern_addr, (host_base + 0x20_0000) as *mut u8, kern_size);
        // zero out the data from qemu now that it's been copied to the destination pages.
        core::ptr::write_bytes(kern_addr as *mut u8, 0, kern_size);

        pass_device_tree(&hyp_fdt, host_base + HOST_DT_OFFSET, host_size)
    };

    let data_page_count = (HOST_DT_OFFSET + dt_len) / PageSize4k::SIZE_BYTES + 1;
    let first_zero_addr = ram_start_page.checked_add_pages(data_page_count).unwrap();

    let host_root_pages = host_root_builder
        .add_4k_data_pages(
            ram_start_page,
            host_pages.by_ref().take(data_page_count as usize),
        )
        .add_4k_pages(first_zero_addr, host_pages)
        .create_host();

    let mut host: Host<Sv48x4, TestMeasure> = Host::new(host_root_pages, host_guests_pages);
    host.add_device_tree(RAM_BASE + HOST_DT_OFFSET);

    // TODO return host and let main run it.
    let _ = host.run(hart_id);
}

/// The entry point of the Rust part of the kernel.
#[no_mangle]
extern "C" fn kernel_init(hart_id: u64, fdt_addr: u64) {
    if hart_id != 0 {
        // TODO handle more than 1 cpu
        abort();
    }

    // Safety: This is the very beginning of the kernel, there are no other users of the UART or the
    // CONSOLE_DRIVER global.
    unsafe { UartDriver::new(0x1000_0000).use_as_console() }
    println!("Salus: Boot test VM");

    test_boot_vm(hart_id, fdt_addr);

    println!("Salus: Host exited");

    poweroff();
}
