//! isaac_p2p.ini next to the DLL.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::win;

pub const ICE_ENABLE_RELAY: i32 = 1;
pub const ICE_ENABLE_PRIVATE: i32 = 2;
pub const ICE_ENABLE_PUBLIC: i32 = 4;

/// `[transport] host`: when this player runs the transport host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostMode {
    /// While this player owns the Steam lobby it is in.
    #[default]
    Auto,
    /// From start-up.
    Always,
    Never,
}

#[derive(Debug, Clone, Default)]
pub struct Settings {
    // [p2p]: Steam Networking Sockets config for sessions that stay on Steam.
    pub ice_flags: i32,
    pub ice_penalty: i32,
    pub sdr_penalty: i32,
    pub log_steam: bool,
    pub monitor_interval_ms: u32,
    pub ping_log_interval_s: u32,
    pub force_no_relay: bool,
    pub stun_servers: String,
    // [transport]: own UDP transport.
    pub console: bool,
    pub port: u16,
    pub host_mode: HostMode,
    pub auto_connect: String,
    pub password: String,
    pub auto_join: bool,
    pub addresses: String,
    // [overlay]
    pub hud: bool,
    pub drop_percent: u32,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();
static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();
/// `[p2p] enabled`; cleared when Steam lacks the config API.
pub static ENABLED: AtomicBool = AtomicBool::new(false);
/// `[p2p] monitor`; cleared when there is no networking object to monitor.
pub static MONITOR: AtomicBool = AtomicBool::new(false);

pub fn get() -> &'static Settings {
    SETTINGS.get_or_init(Settings::default)
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn monitor() -> bool {
    MONITOR.load(Ordering::Relaxed)
}

/// Folder of the DLL, where the ini and the log live.
pub fn base_dir() -> &'static Path {
    BASE_DIR.get().map(PathBuf::as_path).unwrap_or(Path::new(""))
}

pub fn set_base_dir(dir: PathBuf) {
    let _ = BASE_DIR.set(dir);
}

struct Ini(Vec<u16>);

impl Ini {
    fn int(&self, section: &str, key: &str, default: i32) -> i32 {
        unsafe { win::GetPrivateProfileIntW(win::wide(section).as_ptr(), win::wide(key).as_ptr(), default, self.0.as_ptr()) as i32 }
    }

    fn flag(&self, section: &str, key: &str, default: bool) -> bool {
        self.int(section, key, default as i32) != 0
    }

    fn string(&self, section: &str, key: &str) -> String {
        let mut out = [0u16; 1024];
        let len = unsafe {
            win::GetPrivateProfileStringW(
                win::wide(section).as_ptr(),
                win::wide(key).as_ptr(),
                win::wide("").as_ptr(),
                out.as_mut_ptr(),
                out.len() as u32,
                self.0.as_ptr(),
            )
        };
        String::from_utf16_lossy(&out[..len as usize])
    }
}

/// The shipped isaac_p2p.ini: every setting with its default and a comment.
const TEMPLATE: &str = include_str!("../isaac_p2p.ini");

/// Writes the default isaac_p2p.ini next to the DLL when there is none. Returns whether it did.
pub fn create_default() -> bool {
    let path = base_dir().join("isaac_p2p.ini");
    if path.exists() {
        return false;
    }
    std::fs::write(&path, TEMPLATE.replace("\r\n", "\n").replace('\n', "\r\n")).is_ok()
}

pub fn load() {
    let ini = Ini(win::wide_os(base_dir().join("isaac_p2p.ini").as_os_str()));
    let flags = (if ini.flag("p2p", "allow_public", true) { ICE_ENABLE_PUBLIC } else { 0 })
        | (if ini.flag("p2p", "allow_private", true) { ICE_ENABLE_PRIVATE } else { 0 })
        | (if ini.flag("p2p", "allow_turn", false) { ICE_ENABLE_RELAY } else { 0 });
    let settings = Settings {
        ice_flags: flags,
        ice_penalty: ini.int("p2p", "direct_penalty_ms", 0),
        sdr_penalty: ini.int("p2p", "relay_penalty_ms", 20),
        log_steam: ini.flag("p2p", "log", true),
        monitor_interval_ms: ini.int("p2p", "monitor_interval_ms", 2000).max(50) as u32,
        ping_log_interval_s: ini.int("p2p", "ping_log_interval_s", 30).max(0) as u32,
        force_no_relay: ini.flag("p2p", "force_no_relay", false),
        stun_servers: ini.string("p2p", "stun_servers"),
        console: ini.flag("transport", "console", true),
        port: u16::try_from(ini.int("transport", "port", 27777)).ok().filter(|&p| p != 0).unwrap_or(27777),
        host_mode: match ini.string("transport", "host").trim() {
            "1" => HostMode::Always,
            "0" => HostMode::Never,
            _ => HostMode::Auto,
        },
        auto_connect: ini.string("transport", "connect"),
        password: ini.string("transport", "password").trim().to_string(),
        auto_join: ini.flag("transport", "join", true),
        addresses: ini.string("transport", "addresses"),
        hud: ini.flag("overlay", "hud", true),
        drop_percent: ini.int("transport", "drop_percent", 0).clamp(0, 100) as u32,
    };
    ENABLED.store(ini.flag("p2p", "enabled", true), Ordering::Relaxed);
    MONITOR.store(ini.flag("p2p", "monitor", true), Ordering::Relaxed);
    let _ = SETTINGS.set(settings);
}
