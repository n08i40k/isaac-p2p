//! isaac_p2p.log next to the DLL, mirrored to the console window.

use std::fmt::Arguments;
use std::fs::File;
use std::io::Write;
use std::sync::Mutex;

use crate::{console, overlay, win};

static FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn open(file: Option<File>) {
    *FILE.lock().unwrap_or_else(|e| e.into_inner()) = file;
}

pub fn line(args: Arguments) {
    let mut now = win::SYSTEMTIME::default();
    unsafe { win::GetLocalTime(&mut now) };
    let message = args.to_string();
    let text = format!("[{:02}:{:02}:{:02}.{:03}] {}", now.hour, now.minute, now.second, now.milliseconds, message);
    let mut file = FILE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = file.as_mut() {
        let _ = writeln!(file, "{text}");
        let _ = file.flush();
    }
    console::write(&text);
    drop(file);
    overlay::event(&message);
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::line(format_args!($($arg)*)) };
}
