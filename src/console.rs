//! Console window: mirrors the log and takes transport commands.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::{log, settings, transport, win};

static OUTPUT: AtomicUsize = AtomicUsize::new(usize::MAX);
static INPUT: AtomicUsize = AtomicUsize::new(usize::MAX);

fn open_device(name: &str) -> usize {
    unsafe {
        win::CreateFileW(
            win::wide(name).as_ptr(),
            win::GENERIC_READ | win::GENERIC_WRITE,
            win::FILE_SHARE_READ | win::FILE_SHARE_WRITE,
            core::ptr::null_mut(),
            win::OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        ) as usize
    }
}

pub fn open() {
    if unsafe { win::AllocConsole() } == 0 {
        return;
    }
    unsafe { win::SetConsoleTitleW(win::wide("isaac-p2p").as_ptr()) };
    OUTPUT.store(open_device("CONOUT$"), Ordering::Release);
    INPUT.store(open_device("CONIN$"), Ordering::Release);
}

pub fn write(line: &str) {
    let output = OUTPUT.load(Ordering::Acquire) as win::HANDLE;
    if output == win::INVALID_HANDLE_VALUE {
        return;
    }
    let mut written = 0;
    let text = format!("{line}\r\n");
    unsafe { win::WriteFile(output, text.as_ptr(), text.len() as u32, &mut written, core::ptr::null_mut()) };
}

fn help() {
    log!("commands:");
    log!("  host [port] [password]          listen for players on a UDP port (default {})", settings::get().port);
    log!("  connect <ip[:port]> [password]  connect to a host; the game then joins the host's lobby");
    log!("  join [password]                 connect to the host of the Steam lobby you are in (no IP needed);");
    log!("                                  when connected: ask the game to join the host's lobby again");
    log!("  disconnect                      leave the transport session");
    log!("  status                          show peers, RTT and the lobby");
    log!("without a password (password= in isaac_p2p.ini), players are verified through Steam");
}

fn run_command(line: &str) {
    let mut words = line.split_whitespace();
    let Some(command) = words.next() else { return };
    let argument = words.next();
    let password = words.next();
    match (command, argument) {
        ("host", port) => transport::host(port.and_then(|p| p.parse().ok()).unwrap_or(0), password),
        ("connect", Some(address)) => transport::connect(address, password),
        ("join", password) => transport::join(password),
        ("disconnect", _) => transport::disconnect(),
        ("status", _) => transport::status(),
        _ => help(),
    }
}

pub fn start_input() {
    let input = INPUT.load(Ordering::Acquire) as win::HANDLE;
    if input == win::INVALID_HANDLE_VALUE {
        return;
    }
    let input = input as usize;
    std::thread::spawn(move || {
        help();
        let mut buffer = [0u8; 512];
        loop {
            let mut read = 0;
            let ok = unsafe {
                win::ReadFile(input as win::HANDLE, buffer.as_mut_ptr(), buffer.len() as u32, &mut read, core::ptr::null_mut())
            };
            if ok == 0 || read == 0 {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            run_command(&String::from_utf8_lossy(&buffer[..read as usize]));
        }
    });
}
