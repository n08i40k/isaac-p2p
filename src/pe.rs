//! Reading loaded PE images: sections, import tables, own image range.

use core::ffi::c_void;
use core::ptr;

use crate::win;

const SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const ORDINAL_FLAG: u32 = 0x8000_0000;

unsafe fn read32(base: *const u8, offset: usize) -> u32 {
    unsafe { ptr::read_unaligned(base.add(offset).cast::<u32>()) }
}

unsafe fn read16(base: *const u8, offset: usize) -> u16 {
    unsafe { ptr::read_unaligned(base.add(offset).cast::<u16>()) }
}

unsafe fn nt_headers(base: *const u8) -> *const u8 {
    unsafe { base.add(read32(base, 0x3C) as usize) }
}

/// Size of the image in memory.
pub unsafe fn image_size(base: *const u8) -> usize {
    unsafe { read32(nt_headers(base), 24 + 56) as usize }
}

/// Executable sections as byte slices.
pub unsafe fn code_sections(base: *const u8) -> Vec<&'static [u8]> {
    unsafe {
        let nt = nt_headers(base);
        let count = read16(nt, 4 + 2) as usize;
        let optional_size = read16(nt, 4 + 16) as usize;
        let first = nt.add(24 + optional_size);
        (0..count)
            .map(|i| first.add(40 * i))
            .filter(|s| read32(*s, 36) & SCN_MEM_EXECUTE != 0)
            .map(|s| core::slice::from_raw_parts(base.add(read32(s, 12) as usize), read32(s, 8) as usize))
            .collect()
    }
}

unsafe fn c_str(p: *const u8) -> &'static [u8] {
    unsafe { core::ffi::CStr::from_ptr(p.cast()).to_bytes() }
}

/// Replaces the module's import of `dll!name` with `hook`; returns the previous target.
pub unsafe fn hook_import(module: win::HMODULE, dll: &str, name: &str, hook: usize) -> Option<usize> {
    unsafe {
        let base = module as *const u8;
        let nt = nt_headers(base);
        let directory = read32(nt, 24 + 104) as usize;
        if directory == 0 {
            return None;
        }
        let mut import = base.add(directory);
        while read32(import, 12) != 0 {
            if c_str(base.add(read32(import, 12) as usize)).eq_ignore_ascii_case(dll.as_bytes()) {
                let mut names = base.add(read32(import, 0) as usize).cast::<u32>();
                let mut addresses = base.add(read32(import, 16) as usize) as *mut u32;
                while *names != 0 {
                    let entry = *names;
                    if entry & ORDINAL_FLAG == 0 && c_str(base.add(entry as usize + 2)) == name.as_bytes() {
                        let mut old = 0;
                        if win::VirtualProtect(addresses.cast::<c_void>(), 4, win::PAGE_READWRITE, &mut old) == 0 {
                            return None;
                        }
                        let original = *addresses as usize;
                        *addresses = hook as u32;
                        win::VirtualProtect(addresses.cast::<c_void>(), 4, old, &mut old);
                        return Some(original);
                    }
                    names = names.add(1);
                    addresses = addresses.add(1);
                }
            }
            import = import.add(20);
        }
        None
    }
}

/// Points `vtable[slot]` at `hook`; returns the previous entry, or `None` when the page can't be made writable.
pub unsafe fn patch_slot(vtable: *mut usize, slot: usize, hook: usize) -> Option<usize> {
    unsafe {
        let entry = vtable.add(slot);
        let mut old = 0;
        if win::VirtualProtect(entry.cast(), 4, win::PAGE_EXECUTE_READWRITE, &mut old) == 0 {
            return None;
        }
        let previous = *entry;
        *entry = hook;
        win::VirtualProtect(entry.cast(), 4, old, &mut old);
        win::FlushInstructionCache(win::GetCurrentProcess(), entry.cast(), 4);
        Some(previous)
    }
}

/// Start and end address of this DLL's image.
pub fn own_image() -> (usize, usize) {
    static RANGE: std::sync::OnceLock<(usize, usize)> = std::sync::OnceLock::new();
    *RANGE.get_or_init(|| unsafe {
        let mut module = ptr::null_mut();
        win::GetModuleHandleExA(
            win::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | win::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            own_image as *const u8,
            &mut module,
        );
        let start = module as usize;
        (start, start + image_size(module as *const u8))
    })
}
