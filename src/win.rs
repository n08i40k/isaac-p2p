//! The few Win32 functions the proxy calls directly, declared by hand.

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use core::ffi::c_void;

pub type HMODULE = *mut c_void;
pub type HANDLE = *mut c_void;
pub type BOOL = i32;

pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const GENERIC_WRITE: u32 = 0x4000_0000;
pub const FILE_SHARE_READ: u32 = 1;
pub const FILE_SHARE_WRITE: u32 = 2;
pub const OPEN_EXISTING: u32 = 3;
pub const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 2;
pub const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 4;

#[repr(C)]
#[derive(Default)]
pub struct SYSTEMTIME {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetSystemDirectoryW(buffer: *mut u16, size: u32) -> u32;
    pub fn LoadLibraryW(name: *const u16) -> HMODULE;
    pub fn GetProcAddress(module: HMODULE, name: *const u8) -> *mut c_void;
    pub fn GetModuleHandleA(name: *const u8) -> HMODULE;
    pub fn GetModuleHandleExA(flags: u32, name: *const u8, module: *mut HMODULE) -> BOOL;
    pub fn GetModuleFileNameW(module: HMODULE, buffer: *mut u16, size: u32) -> u32;
    pub fn DisableThreadLibraryCalls(module: HMODULE) -> BOOL;
    pub fn VirtualProtect(address: *mut c_void, size: usize, protect: u32, old: *mut u32) -> BOOL;
    pub fn FlushInstructionCache(process: HANDLE, address: *const c_void, size: usize) -> BOOL;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn GetPrivateProfileIntW(section: *const u16, key: *const u16, default: i32, file: *const u16) -> u32;
    pub fn GetPrivateProfileStringW(
        section: *const u16,
        key: *const u16,
        default: *const u16,
        out: *mut u16,
        size: u32,
        file: *const u16,
    ) -> u32;
    pub fn GetLocalTime(time: *mut SYSTEMTIME);
    pub fn AllocConsole() -> BOOL;
    pub fn SetConsoleTitleW(title: *const u16) -> BOOL;
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: HANDLE,
    ) -> HANDLE;
    pub fn ReadFile(file: HANDLE, buffer: *mut u8, size: u32, read: *mut u32, overlapped: *mut c_void) -> BOOL;
    pub fn WriteFile(file: HANDLE, buffer: *const u8, size: u32, written: *mut u32, overlapped: *mut c_void)
    -> BOOL;
}

#[repr(C)]
pub struct FdSet {
    pub count: u32,
    pub sockets: [usize; 64],
}

#[repr(C)]
pub struct Timeval {
    pub sec: i32,
    pub usec: i32,
}

pub const SOL_SOCKET: i32 = 0xffff;
pub const SO_SNDBUF: i32 = 0x1001;
pub const SO_RCVBUF: i32 = 0x1002;
pub const SIO_UDP_CONNRESET: u32 = 0x9800_000C;

#[link(name = "ws2_32")]
unsafe extern "system" {
    pub fn select(nfds: i32, read: *mut FdSet, write: *mut FdSet, except: *mut FdSet, timeout: *const Timeval)
    -> i32;
    pub fn gethostname(name: *mut u8, size: i32) -> i32;
    pub fn setsockopt(socket: usize, level: i32, name: i32, value: *const c_void, size: i32) -> i32;
    pub fn WSAIoctl(
        socket: usize,
        code: u32,
        input: *const c_void,
        input_size: u32,
        output: *mut c_void,
        output_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
        completion: *mut c_void,
    ) -> i32;
}

/// NUL-terminated UTF-16 copy of `text`.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

pub fn wide_os(text: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    text.encode_wide().chain(Some(0)).collect()
}

/// Address of a NUL-terminated byte string literal, for GetProcAddress and friends.
pub fn cstr(text: &'static str) -> *const u8 {
    debug_assert!(text.ends_with('\0'));
    text.as_ptr()
}

// ---------------------------------------------------------------------------------------------------------------
// Overlay: window, keyboard, clipboard, GDI text.

pub type HDC = *mut c_void;
pub type HWND = *mut c_void;
pub type HGDIOBJ = *mut c_void;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[repr(C)]
#[derive(Default)]
pub struct SIZE {
    pub cx: i32,
    pub cy: i32,
}

#[repr(C)]
pub struct BITMAPINFOHEADER {
    pub size: u32,
    pub width: i32,
    pub height: i32,
    pub planes: u16,
    pub bit_count: u16,
    pub compression: u32,
    pub size_image: u32,
    pub x_pels_per_meter: i32,
    pub y_pels_per_meter: i32,
    pub clr_used: u32,
    pub clr_important: u32,
}

#[repr(C)]
pub struct BITMAPINFO {
    pub header: BITMAPINFOHEADER,
    pub colors: [u32; 1],
}

pub const VK_F9: i32 = 0x78;
pub const VK_F10: i32 = 0x79;
pub const VK_F11: i32 = 0x7A;
pub const CF_UNICODETEXT: u32 = 13;
pub const GMEM_MOVEABLE: u32 = 2;
pub const TRANSPARENT: i32 = 1;
pub const FW_BOLD: i32 = 700;
pub const ANTIALIASED_QUALITY: u32 = 4;
pub const FIXED_PITCH_MODERN: u32 = 1 | (3 << 4);

#[link(name = "user32")]
unsafe extern "system" {
    pub fn WindowFromDC(dc: HDC) -> HWND;
    pub fn GetClientRect(window: HWND, rect: *mut RECT) -> BOOL;
    pub fn GetForegroundWindow() -> HWND;
    pub fn GetAsyncKeyState(key: i32) -> i16;
    pub fn OpenClipboard(window: HWND) -> BOOL;
    pub fn EmptyClipboard() -> BOOL;
    pub fn SetClipboardData(format: u32, data: HANDLE) -> HANDLE;
    pub fn CloseClipboard() -> BOOL;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GlobalAlloc(flags: u32, size: usize) -> HANDLE;
    pub fn GlobalLock(memory: HANDLE) -> *mut c_void;
    pub fn GlobalUnlock(memory: HANDLE) -> BOOL;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    pub fn CreateCompatibleDC(dc: HDC) -> HDC;
    pub fn DeleteDC(dc: HDC) -> BOOL;
    pub fn CreateDIBSection(
        dc: HDC,
        info: *const BITMAPINFO,
        usage: u32,
        bits: *mut *mut c_void,
        section: HANDLE,
        offset: u32,
    ) -> HGDIOBJ;
    pub fn SelectObject(dc: HDC, object: HGDIOBJ) -> HGDIOBJ;
    pub fn DeleteObject(object: HGDIOBJ) -> BOOL;
    pub fn CreateFontW(
        height: i32,
        width: i32,
        escapement: i32,
        orientation: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike_out: u32,
        charset: u32,
        out_precision: u32,
        clip_precision: u32,
        quality: u32,
        pitch_and_family: u32,
        face: *const u16,
    ) -> HGDIOBJ;
    pub fn SetBkMode(dc: HDC, mode: i32) -> i32;
    pub fn SetTextColor(dc: HDC, color: u32) -> u32;
    pub fn TextOutW(dc: HDC, x: i32, y: i32, text: *const u16, len: i32) -> BOOL;
    pub fn GetTextExtentPoint32W(dc: HDC, text: *const u16, len: i32, size: *mut SIZE) -> BOOL;
    pub fn GdiFlush() -> BOOL;
}
