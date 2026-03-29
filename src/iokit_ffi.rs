// Shared IOKit FFI declarations used across multiple modules.
use core_foundation_sys::base::CFAllocatorRef;
use core_foundation_sys::dictionary::CFMutableDictionaryRef;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    pub fn IOServiceMatching(name: *const i8) -> *mut libc::c_void;
    pub fn IOServiceGetMatchingService(main_port: u32, matching: *const libc::c_void) -> u32;
    pub fn IOServiceGetMatchingServices(
        main_port: u32,
        matching: *const libc::c_void,
        existing: *mut u32,
    ) -> i32;
    pub fn IOIteratorNext(iterator: u32) -> u32;
    pub fn IORegistryEntryGetName(entry: u32, name: *mut i8) -> i32;
    pub fn IORegistryEntryCreateCFProperties(
        entry: u32,
        properties: *mut CFMutableDictionaryRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> i32;
    pub fn IOServiceOpen(service: u32, owning_task: u32, conn_type: u32, conn: *mut u32) -> i32;
    pub fn IOServiceClose(conn: u32) -> i32;
    pub fn IOConnectCallStructMethod(
        conn: u32,
        selector: u32,
        input: *const u8,
        input_size: usize,
        output: *mut u8,
        output_size: *mut usize,
    ) -> i32;
    pub fn IOObjectRelease(obj: u32) -> u32;
    pub fn IOPMCopyAssertionsByProcess(
        assertions_by_pid: *mut core_foundation_sys::dictionary::CFDictionaryRef,
    ) -> i32;
}

extern "C" {
    pub fn mach_task_self() -> u32;
}
