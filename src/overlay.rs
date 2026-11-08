//! On-screen HUD: connection state, route and ping of every player, recent connection events.
//!
//! The game presents its OpenGL frames with GDI32!SwapBuffers, which `hooks` redirects here. Right before the
//! frame goes out, the HUD text is drawn with GDI into a 32-bit bitmap (rebuilt a few times a second), uploaded
//! as a texture and drawn as one quad in the top-right corner. All OpenGL state the drawing touches is saved
//! and restored, so the game's next frame starts from the state it left.
//!
//! Keys, read only while the game window has focus: F9 cycles compact / detailed / hidden, F10 copies the
//! transport password to the clipboard, F11 starts or stops hosting.

use core::ffi::c_void;
use core::ptr;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crate::{log, monitor, settings, transport, win};

const REBUILD_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_LIFETIME: Duration = Duration::from_secs(8);
const MAX_EVENTS: usize = 4;
const MARGIN: i32 = 12;
const PANEL_ALPHA: u32 = 0xB0;

const WHITE: u32 = 0xFFFFFF;
const GRAY: u32 = 0xA8A8A8;
const GREEN: u32 = 0x60E070;
const YELLOW: u32 = 0x40D0F0; // GDI colors are 0x00BBGGRR
const RED: u32 = 0x5060F0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Compact = 0,
    Detailed = 1,
    Hidden = 2,
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Compact as u8);
static EVENTS: Mutex<VecDeque<(Instant, String)>> = Mutex::new(VecDeque::new());

fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        0 => Mode::Compact,
        1 => Mode::Detailed,
        _ => Mode::Hidden,
    }
}

/// Called for every log line; transport events show up on the HUD for a few seconds.
pub fn event(message: &str) {
    let Some(text) = message.strip_prefix("transport: ") else { return };
    if text.starts_with("password ") {
        return; // shown on demand with F10, not on screen
    }
    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    if events.len() >= MAX_EVENTS {
        events.pop_front();
    }
    events.push_back((Instant::now(), text.to_string()));
}

// ---------------------------------------------------------------------------------------------------------------
// HUD text.

struct Line {
    text: String,
    color: u32,
}

fn ping_color(ms: f32) -> u32 {
    if ms < 80.0 {
        GREEN
    } else if ms < 150.0 {
        YELLOW
    } else {
        RED
    }
}

fn lines(names: &mut Names) -> Vec<Line> {
    let detailed = mode() == Mode::Detailed;
    let status = transport::hud_status();
    let mut out = Vec::new();
    let header = match status.role {
        "hosting" => format!(
            "p2p  hosting on :{}{}",
            status.port,
            if status.offered { "  (offered in lobby)" } else { "" }
        ),
        "client" if status.reconnecting => "p2p  reconnecting to host...".to_string(),
        "client" => "p2p  connected to host".to_string(),
        _ => "p2p  idle (Steam networking)".to_string(),
    };
    out.push(Line { text: header, color: WHITE });

    let mut shown = Vec::new();
    for link in &status.links {
        shown.push(link.steam_id);
        let name = names.get(link.steam_id);
        if !link.connected {
            out.push(Line { text: format!("{name}  connecting..."), color: GRAY });
            continue;
        }
        let mut text = format!("{name}  direct  {:.0} ms", link.rtt_ms);
        if link.resend_percent >= 0.5 {
            text.push_str(&format!("  resend {:.0}%", link.resend_percent));
        }
        if detailed {
            text.push_str(&format!("  ±{:.0}  cwnd {:.0}  {}", link.rttvar_ms, link.cwnd, link.addr));
        }
        out.push(Line { text, color: ping_color(link.rtt_ms) });
    }
    for route in monitor::steam_routes() {
        if shown.contains(&route.steam_id) || route.route == "no session" {
            continue;
        }
        let name = names.get(route.steam_id);
        let (text, color) = match route.ping_ms {
            Some(ping) => (format!("{name}  {}  {ping} ms", route.route), ping_color(ping as f32)),
            None => (format!("{name}  {}", route.route), GRAY),
        };
        out.push(Line { text, color });
    }

    let mut events = EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    while events.front().is_some_and(|(at, _)| at.elapsed() > EVENT_LIFETIME) {
        events.pop_front();
    }
    for (_, text) in events.iter() {
        out.push(Line { text: text.clone(), color: GRAY });
    }
    drop(events);
    if detailed {
        out.push(Line { text: "F9 compact/detailed/hidden   F10 copy password   F11 host on/off".to_string(), color: GRAY });
    }
    out
}

/// Steam persona names, looked up once per player.
struct Names {
    friends: Option<(usize, unsafe extern "C" fn(*mut c_void, u64) -> *const core::ffi::c_char)>,
    known: Vec<(u64, String)>,
}

impl Names {
    fn new() -> Names {
        let friends = unsafe {
            let steam = win::GetModuleHandleA(win::cstr("steam_api.dll\0"));
            let get = crate::steam::proc::<crate::steam::InterfaceFn>(steam, "SteamAPI_SteamFriends_v017\0");
            let name = crate::steam::proc(steam, "SteamAPI_ISteamFriends_GetFriendPersonaName\0");
            match (steam.is_null(), get, name) {
                (false, Some(get), Some(name)) => Some((get() as usize, name)).filter(|(object, _)| *object != 0),
                _ => None,
            }
        };
        Names { friends, known: Vec::new() }
    }

    fn get(&mut self, steam_id: u64) -> String {
        if let Some((_, name)) = self.known.iter().find(|(id, _)| *id == steam_id) {
            return name.clone();
        }
        let looked_up = self.friends.and_then(|(object, get)| {
            let name = unsafe { get(object as *mut c_void, steam_id) };
            let name = (!name.is_null()).then(|| unsafe { core::ffi::CStr::from_ptr(name) }.to_string_lossy())?;
            (!name.is_empty() && name != "[unknown]").then(|| name.chars().take(20).collect::<String>())
        });
        match looked_up {
            Some(name) => {
                self.known.push((steam_id, name.clone()));
                name
            }
            // Steam may not know the name yet; ask again next time.
            None => format!("…{}", steam_id % 100000),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// GDI: HUD text into a BGRA bitmap.

struct Bitmap {
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

fn render(lines: &[Line], font_px: i32) -> Option<Bitmap> {
    unsafe {
        let dc = win::CreateCompatibleDC(ptr::null_mut());
        if dc.is_null() {
            return None;
        }
        let font = win::CreateFontW(
            -font_px,
            0,
            0,
            0,
            win::FW_BOLD,
            0,
            0,
            0,
            1, // DEFAULT_CHARSET
            0,
            0,
            win::ANTIALIASED_QUALITY,
            win::FIXED_PITCH_MODERN,
            win::wide("Consolas").as_ptr(),
        );
        let old_font = win::SelectObject(dc, font);
        let texts: Vec<Vec<u16>> = lines.iter().map(|l| l.text.encode_utf16().collect()).collect();
        let mut width = 0;
        let mut line_height = font_px + 2;
        for text in &texts {
            let mut size = win::SIZE::default();
            win::GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size);
            width = width.max(size.cx);
            line_height = line_height.max(size.cy);
        }
        let pad = font_px / 2;
        let (w, h) = (width + 2 * pad, line_height * texts.len() as i32 + 2 * pad);
        let info = win::BITMAPINFO {
            header: win::BITMAPINFOHEADER {
                size: size_of::<win::BITMAPINFOHEADER>() as u32,
                width: w,
                height: -h, // top-down
                planes: 1,
                bit_count: 32,
                compression: 0,
                size_image: 0,
                x_pels_per_meter: 0,
                y_pels_per_meter: 0,
                clr_used: 0,
                clr_important: 0,
            },
            colors: [0],
        };
        let mut bits: *mut c_void = ptr::null_mut();
        let dib = win::CreateDIBSection(dc, &info, 0, &mut bits, ptr::null_mut(), 0);
        let result = if dib.is_null() || bits.is_null() {
            None
        } else {
            let old_bitmap = win::SelectObject(dc, dib);
            let pixels = core::slice::from_raw_parts_mut(bits.cast::<u32>(), (w * h) as usize);
            pixels.fill(PANEL_ALPHA << 24);
            win::SetBkMode(dc, win::TRANSPARENT);
            for (i, (text, line)) in texts.iter().zip(lines).enumerate() {
                win::SetTextColor(dc, line.color);
                win::TextOutW(dc, pad, pad + i as i32 * line_height, text.as_ptr(), text.len() as i32);
            }
            win::GdiFlush();
            // GDI clears the alpha byte of every pixel it draws: those are text.
            for pixel in pixels.iter_mut() {
                if *pixel >> 24 == 0 {
                    *pixel |= 0xFF00_0000;
                }
            }
            let bytes = core::slice::from_raw_parts(bits.cast::<u8>(), (w * h * 4) as usize).to_vec();
            win::SelectObject(dc, old_bitmap);
            win::DeleteObject(dib);
            Some(Bitmap { width: w, height: h, pixels: bytes })
        };
        win::SelectObject(dc, old_font);
        win::DeleteObject(font);
        win::DeleteDC(dc);
        result
    }
}

// ---------------------------------------------------------------------------------------------------------------
// OpenGL.

mod gl {
    pub const BLEND: u32 = 0x0BE2;
    pub const DEPTH_TEST: u32 = 0x0B71;
    pub const SCISSOR_TEST: u32 = 0x0C11;
    pub const CULL_FACE: u32 = 0x0B44;
    pub const STENCIL_TEST: u32 = 0x0B90;
    pub const FRAMEBUFFER_SRGB: u32 = 0x8DB9;
    pub const VIEWPORT: u32 = 0x0BA2;
    pub const COLOR_WRITEMASK: u32 = 0x0C23;
    pub const CURRENT_PROGRAM: u32 = 0x8B8D;
    pub const VERTEX_ARRAY_BINDING: u32 = 0x85B5;
    pub const ARRAY_BUFFER_BINDING: u32 = 0x8894;
    pub const ACTIVE_TEXTURE: u32 = 0x84E0;
    pub const TEXTURE_BINDING_2D: u32 = 0x8069;
    pub const SAMPLER_BINDING: u32 = 0x8919;
    pub const DRAW_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
    pub const DRAW_FRAMEBUFFER: u32 = 0x8CA9;
    pub const PIXEL_UNPACK_BUFFER_BINDING: u32 = 0x88EF;
    pub const PIXEL_UNPACK_BUFFER: u32 = 0x88EC;
    pub const UNPACK_ALIGNMENT: u32 = 0x0CF5;
    pub const UNPACK_ROW_LENGTH: u32 = 0x0CF2;
    pub const BLEND_SRC_RGB: u32 = 0x80C9;
    pub const BLEND_DST_RGB: u32 = 0x80C8;
    pub const BLEND_SRC_ALPHA: u32 = 0x80CB;
    pub const BLEND_DST_ALPHA: u32 = 0x80CA;
    pub const BLEND_EQUATION_RGB: u32 = 0x8009;
    pub const BLEND_EQUATION_ALPHA: u32 = 0x883D;
    pub const FUNC_ADD: u32 = 0x8006;
    pub const SRC_ALPHA: u32 = 0x0302;
    pub const ONE_MINUS_SRC_ALPHA: u32 = 0x0303;
    pub const ONE: u32 = 1;
    pub const TEXTURE_2D: u32 = 0x0DE1;
    pub const TEXTURE0: u32 = 0x84C0;
    pub const TEXTURE_MIN_FILTER: u32 = 0x2801;
    pub const TEXTURE_MAG_FILTER: u32 = 0x2800;
    pub const TEXTURE_WRAP_S: u32 = 0x2802;
    pub const TEXTURE_WRAP_T: u32 = 0x2803;
    pub const NEAREST: i32 = 0x2600;
    pub const CLAMP_TO_EDGE: i32 = 0x812F;
    pub const RGBA8: i32 = 0x8058;
    pub const RGBA: u32 = 0x1908;
    pub const UNSIGNED_BYTE: u32 = 0x1401;
    pub const FLOAT: u32 = 0x1406;
    pub const TRIANGLES: u32 = 0x0004;
    pub const ARRAY_BUFFER: u32 = 0x8892;
    pub const STREAM_DRAW: u32 = 0x88E0;
    pub const VERTEX_SHADER: u32 = 0x8B31;
    pub const FRAGMENT_SHADER: u32 = 0x8B30;
    pub const COMPILE_STATUS: u32 = 0x8B81;
    pub const LINK_STATUS: u32 = 0x8B82;
    pub const VERSION: u32 = 0x1F02;
}

macro_rules! gl_api {
    ($($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        #[allow(non_snake_case)]
        struct Gl { $($name: unsafe extern "system" fn($($arg),*) $(-> $ret)?,)* }

        impl Gl {
            /// Every function from opengl32.dll (1.1) or wglGetProcAddress (newer); `None` when one is missing.
            unsafe fn load() -> Option<Gl> {
                let opengl = unsafe { win::GetModuleHandleA(win::cstr("opengl32.dll\0")) };
                if opengl.is_null() {
                    return None;
                }
                let get_proc: unsafe extern "system" fn(*const u8) -> *mut c_void =
                    unsafe { core::mem::transmute(win::GetProcAddress(opengl, win::cstr("wglGetProcAddress\0"))) };
                Some(Gl { $($name: unsafe {
                    let name = concat!(stringify!($name), "\0");
                    let mut address = win::GetProcAddress(opengl, name.as_ptr()) as usize;
                    if address == 0 {
                        address = get_proc(name.as_ptr()) as usize;
                    }
                    if address <= 3 || address == usize::MAX {
                        log!("overlay: OpenGL lacks {}; HUD off", stringify!($name));
                        return None;
                    }
                    core::mem::transmute::<usize, unsafe extern "system" fn($($arg),*) $(-> $ret)?>(address)
                },)* })
            }
        }
    };
}

gl_api! {
    glGetString: fn(u32) -> *const u8;
    glGetIntegerv: fn(u32, *mut i32);
    glGetBooleanv: fn(u32, *mut u8);
    glIsEnabled: fn(u32) -> u8;
    glEnable: fn(u32);
    glDisable: fn(u32);
    glViewport: fn(i32, i32, i32, i32);
    glColorMask: fn(u8, u8, u8, u8);
    glBlendFuncSeparate: fn(u32, u32, u32, u32);
    glBlendEquationSeparate: fn(u32, u32);
    glPixelStorei: fn(u32, i32);
    glGenTextures: fn(i32, *mut u32);
    glBindTexture: fn(u32, u32);
    glActiveTexture: fn(u32);
    glTexParameteri: fn(u32, u32, i32);
    glTexImage2D: fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
    glDrawArrays: fn(u32, i32, i32);
    glCreateShader: fn(u32) -> u32;
    glShaderSource: fn(u32, i32, *const *const u8, *const i32);
    glCompileShader: fn(u32);
    glGetShaderiv: fn(u32, u32, *mut i32);
    glCreateProgram: fn() -> u32;
    glAttachShader: fn(u32, u32);
    glBindAttribLocation: fn(u32, u32, *const u8);
    glLinkProgram: fn(u32);
    glGetProgramiv: fn(u32, u32, *mut i32);
    glUseProgram: fn(u32);
    glGetUniformLocation: fn(u32, *const u8) -> i32;
    glUniform1i: fn(i32, i32);
    glUniform2f: fn(i32, f32, f32);
    glGenBuffers: fn(i32, *mut u32);
    glBindBuffer: fn(u32, u32);
    glBufferData: fn(u32, isize, *const c_void, u32);
    glGenVertexArrays: fn(i32, *mut u32);
    glBindVertexArray: fn(u32);
    glVertexAttribPointer: fn(u32, i32, u32, u8, i32, *const c_void);
    glEnableVertexAttribArray: fn(u32);
    glBindFramebuffer: fn(u32, u32);
    glBindSampler: fn(u32, u32);
}

const VERTEX_SHADER: &str = "
in vec2 pos;
in vec2 uv;
out vec2 v_uv;
uniform vec2 screen;
void main() {
    v_uv = uv;
    gl_Position = vec4(pos.x / screen.x * 2.0 - 1.0, 1.0 - pos.y / screen.y * 2.0, 0.0, 1.0);
}
";

const FRAGMENT_SHADER: &str = "
in vec2 v_uv;
out vec4 color;
uniform sampler2D tex;
void main() {
    color = texture(tex, v_uv).bgra;
}
";

struct Renderer {
    gl: Gl,
    program: u32,
    screen: i32,
    sampler: i32,
    vao: u32,
    vbo: u32,
    texture: u32,
    size: (i32, i32),
}

/// OpenGL state the HUD changes, as the game left it.
struct Saved {
    program: i32,
    vao: i32,
    array_buffer: i32,
    active_texture: i32,
    texture: i32,
    sampler: i32,
    framebuffer: i32,
    unpack_buffer: i32,
    unpack_alignment: i32,
    unpack_row_length: i32,
    viewport: [i32; 4],
    color_mask: [u8; 4],
    blend: [i32; 6],
    enabled: [(u32, u8); 6],
}

impl Renderer {
    unsafe fn new() -> Option<Renderer> {
        let gl = unsafe { Gl::load()? };
        unsafe {
            let version = core::ffi::CStr::from_ptr((gl.glGetString)(gl::VERSION).cast()).to_string_lossy().into_owned();
            let major_minor: Vec<u32> =
                version.split(|c: char| !c.is_ascii_digit()).filter_map(|n| n.parse().ok()).take(2).collect();
            let at_least_32 = major_minor.as_slice() >= [3, 2].as_slice();
            let header = if at_least_32 { "#version 150\n" } else { "#version 130\n" };
            let compile = |kind, body: &str| {
                let source = format!("{header}{body}\0");
                let shader = (gl.glCreateShader)(kind);
                let text = source.as_ptr();
                (gl.glShaderSource)(shader, 1, &text, ptr::null());
                (gl.glCompileShader)(shader);
                let mut ok = 0;
                (gl.glGetShaderiv)(shader, gl::COMPILE_STATUS, &mut ok);
                (ok != 0).then_some(shader)
            };
            let (Some(vs), Some(fs)) = (compile(gl::VERTEX_SHADER, VERTEX_SHADER), compile(gl::FRAGMENT_SHADER, FRAGMENT_SHADER))
            else {
                log!("overlay: shaders do not compile on OpenGL {}; HUD off", version);
                return None;
            };
            let program = (gl.glCreateProgram)();
            (gl.glAttachShader)(program, vs);
            (gl.glAttachShader)(program, fs);
            (gl.glBindAttribLocation)(program, 0, c"pos".as_ptr().cast());
            (gl.glBindAttribLocation)(program, 1, c"uv".as_ptr().cast());
            (gl.glLinkProgram)(program);
            let mut ok = 0;
            (gl.glGetProgramiv)(program, gl::LINK_STATUS, &mut ok);
            if ok == 0 {
                log!("overlay: shader program does not link on OpenGL {}; HUD off", version);
                return None;
            }
            let screen = (gl.glGetUniformLocation)(program, c"screen".as_ptr().cast());
            let sampler = (gl.glGetUniformLocation)(program, c"tex".as_ptr().cast());
            let mut renderer = Renderer { gl, program, screen, sampler, vao: 0, vbo: 0, texture: 0, size: (0, 0) };
            let saved = renderer.save();
            let gl = &renderer.gl;
            (gl.glGenVertexArrays)(1, &mut renderer.vao);
            (gl.glGenBuffers)(1, &mut renderer.vbo);
            (gl.glGenTextures)(1, &mut renderer.texture);
            (gl.glBindVertexArray)(renderer.vao);
            (gl.glBindBuffer)(gl::ARRAY_BUFFER, renderer.vbo);
            (gl.glEnableVertexAttribArray)(0);
            (gl.glEnableVertexAttribArray)(1);
            (gl.glVertexAttribPointer)(0, 2, gl::FLOAT, 0, 16, ptr::null());
            (gl.glVertexAttribPointer)(1, 2, gl::FLOAT, 0, 16, 8 as *const c_void);
            (gl.glActiveTexture)(gl::TEXTURE0);
            (gl.glBindTexture)(gl::TEXTURE_2D, renderer.texture);
            for (name, value) in [
                (gl::TEXTURE_MIN_FILTER, gl::NEAREST),
                (gl::TEXTURE_MAG_FILTER, gl::NEAREST),
                (gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE),
                (gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE),
            ] {
                (gl.glTexParameteri)(gl::TEXTURE_2D, name, value);
            }
            renderer.restore(&saved);
            log!("overlay: HUD on (OpenGL {}); F9 cycles it, F10 copies the password", version);
            Some(renderer)
        }
    }

    unsafe fn save(&self) -> Saved {
        let gl = &self.gl;
        let int = |name| {
            let mut v = 0;
            unsafe { (gl.glGetIntegerv)(name, &mut v) };
            v
        };
        let mut s = Saved {
            program: int(gl::CURRENT_PROGRAM),
            vao: int(gl::VERTEX_ARRAY_BINDING),
            array_buffer: int(gl::ARRAY_BUFFER_BINDING),
            active_texture: int(gl::ACTIVE_TEXTURE),
            texture: 0,
            sampler: 0,
            framebuffer: int(gl::DRAW_FRAMEBUFFER_BINDING),
            unpack_buffer: int(gl::PIXEL_UNPACK_BUFFER_BINDING),
            unpack_alignment: int(gl::UNPACK_ALIGNMENT),
            unpack_row_length: int(gl::UNPACK_ROW_LENGTH),
            viewport: [0; 4],
            color_mask: [0; 4],
            blend: [
                int(gl::BLEND_SRC_RGB),
                int(gl::BLEND_DST_RGB),
                int(gl::BLEND_SRC_ALPHA),
                int(gl::BLEND_DST_ALPHA),
                int(gl::BLEND_EQUATION_RGB),
                int(gl::BLEND_EQUATION_ALPHA),
            ],
            enabled: [gl::BLEND, gl::DEPTH_TEST, gl::SCISSOR_TEST, gl::CULL_FACE, gl::STENCIL_TEST, gl::FRAMEBUFFER_SRGB]
                .map(|cap| (cap, unsafe { (gl.glIsEnabled)(cap) })),
        };
        unsafe {
            (gl.glGetIntegerv)(gl::VIEWPORT, s.viewport.as_mut_ptr());
            (gl.glGetBooleanv)(gl::COLOR_WRITEMASK, s.color_mask.as_mut_ptr());
            (gl.glActiveTexture)(gl::TEXTURE0);
        }
        s.texture = int(gl::TEXTURE_BINDING_2D);
        s.sampler = int(gl::SAMPLER_BINDING);
        s
    }

    unsafe fn restore(&self, s: &Saved) {
        let gl = &self.gl;
        unsafe {
            (gl.glBindSampler)(0, s.sampler as u32);
            (gl.glBindTexture)(gl::TEXTURE_2D, s.texture as u32);
            (gl.glActiveTexture)(s.active_texture as u32);
            (gl.glUseProgram)(s.program as u32);
            (gl.glBindVertexArray)(s.vao as u32);
            (gl.glBindBuffer)(gl::ARRAY_BUFFER, s.array_buffer as u32);
            (gl.glBindBuffer)(gl::PIXEL_UNPACK_BUFFER, s.unpack_buffer as u32);
            (gl.glPixelStorei)(gl::UNPACK_ALIGNMENT, s.unpack_alignment);
            (gl.glPixelStorei)(gl::UNPACK_ROW_LENGTH, s.unpack_row_length);
            (gl.glBindFramebuffer)(gl::DRAW_FRAMEBUFFER, s.framebuffer as u32);
            (gl.glViewport)(s.viewport[0], s.viewport[1], s.viewport[2], s.viewport[3]);
            (gl.glColorMask)(s.color_mask[0], s.color_mask[1], s.color_mask[2], s.color_mask[3]);
            (gl.glBlendFuncSeparate)(s.blend[0] as u32, s.blend[1] as u32, s.blend[2] as u32, s.blend[3] as u32);
            (gl.glBlendEquationSeparate)(s.blend[4] as u32, s.blend[5] as u32);
            for (cap, on) in s.enabled {
                if on != 0 { (gl.glEnable)(cap) } else { (gl.glDisable)(cap) }
            }
        }
    }

    unsafe fn upload(&mut self, bitmap: &Bitmap) {
        let gl = &self.gl;
        unsafe {
            (gl.glBindBuffer)(gl::PIXEL_UNPACK_BUFFER, 0);
            (gl.glPixelStorei)(gl::UNPACK_ALIGNMENT, 4);
            (gl.glPixelStorei)(gl::UNPACK_ROW_LENGTH, 0);
            (gl.glBindTexture)(gl::TEXTURE_2D, self.texture);
            // BGRA bytes uploaded as RGBA; the shader swizzles them back (.bgra), which avoids GL_BGRA formats.
            (gl.glTexImage2D)(
                gl::TEXTURE_2D,
                0,
                gl::RGBA8,
                bitmap.width,
                bitmap.height,
                0,
                gl::RGBA,
                gl::UNSIGNED_BYTE,
                bitmap.pixels.as_ptr().cast(),
            );
        }
        self.size = (bitmap.width, bitmap.height);
    }

    unsafe fn draw(&self, screen_w: i32, screen_h: i32) {
        let gl = &self.gl;
        let (w, h) = (self.size.0 as f32, self.size.1 as f32);
        let x0 = (screen_w - self.size.0 - MARGIN) as f32;
        let y0 = MARGIN as f32;
        let (x1, y1) = (x0 + w, y0 + h);
        let quad: [f32; 24] = [
            x0, y0, 0.0, 0.0, x1, y0, 1.0, 0.0, x1, y1, 1.0, 1.0, //
            x0, y0, 0.0, 0.0, x1, y1, 1.0, 1.0, x0, y1, 0.0, 1.0,
        ];
        unsafe {
            (gl.glBindFramebuffer)(gl::DRAW_FRAMEBUFFER, 0);
            (gl.glViewport)(0, 0, screen_w, screen_h);
            for cap in [gl::DEPTH_TEST, gl::SCISSOR_TEST, gl::CULL_FACE, gl::STENCIL_TEST, gl::FRAMEBUFFER_SRGB] {
                (gl.glDisable)(cap);
            }
            (gl.glEnable)(gl::BLEND);
            (gl.glBlendEquationSeparate)(gl::FUNC_ADD, gl::FUNC_ADD);
            (gl.glBlendFuncSeparate)(gl::SRC_ALPHA, gl::ONE_MINUS_SRC_ALPHA, gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
            (gl.glColorMask)(1, 1, 1, 1);
            (gl.glUseProgram)(self.program);
            (gl.glUniform2f)(self.screen, screen_w as f32, screen_h as f32);
            (gl.glUniform1i)(self.sampler, 0);
            (gl.glBindSampler)(0, 0);
            (gl.glBindTexture)(gl::TEXTURE_2D, self.texture);
            (gl.glBindVertexArray)(self.vao);
            (gl.glBindBuffer)(gl::ARRAY_BUFFER, self.vbo);
            (gl.glBufferData)(gl::ARRAY_BUFFER, size_of_val(&quad) as isize, quad.as_ptr().cast(), gl::STREAM_DRAW);
            (gl.glDrawArrays)(gl::TRIANGLES, 0, 6);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Per frame.

struct Overlay {
    renderer: Option<Renderer>,
    failed: bool,
    names: Option<Names>,
    last_build: Option<Instant>,
    font_px: i32,
    keys: [bool; 3],
}

static OVERLAY: Mutex<Overlay> = Mutex::new(Overlay {
    renderer: None,
    failed: false,
    names: None,
    last_build: None,
    font_px: 0,
    keys: [false; 3],
});

fn copy_to_clipboard(window: win::HWND, text: &str) -> bool {
    let wide = win::wide(text);
    unsafe {
        if win::OpenClipboard(window) == 0 {
            return false;
        }
        win::EmptyClipboard();
        let memory = win::GlobalAlloc(win::GMEM_MOVEABLE, wide.len() * 2);
        let ok = !memory.is_null() && {
            let target = win::GlobalLock(memory).cast::<u16>();
            if !target.is_null() {
                ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
            }
            win::GlobalUnlock(memory);
            !win::SetClipboardData(win::CF_UNICODETEXT, memory).is_null()
        };
        win::CloseClipboard();
        ok
    }
}

fn handle_keys(overlay: &mut Overlay, window: win::HWND) {
    if unsafe { win::GetForegroundWindow() } != window {
        overlay.keys = [false; 3];
        return;
    }
    let pressed = [win::VK_F9, win::VK_F10, win::VK_F11]
        .map(|key| unsafe { win::GetAsyncKeyState(key) } as u16 & 0x8000 != 0);
    if pressed[0] && !overlay.keys[0] {
        MODE.store((MODE.load(Ordering::Relaxed) + 1) % 3, Ordering::Relaxed);
        overlay.last_build = None;
    }
    if pressed[1] && !overlay.keys[1] {
        let password = transport::hud_status().password;
        let message = if password.is_empty() {
            "transport: no password: players are verified through Steam".to_string()
        } else if copy_to_clipboard(window, &password) {
            "transport: password copied to the clipboard".to_string()
        } else {
            "transport: could not open the clipboard".to_string()
        };
        event(&message);
        overlay.last_build = None;
    }
    if pressed[2] && !overlay.keys[2] {
        // On a thread of its own: starting a host binds a socket and logs, which takes the transport lock.
        std::thread::spawn(transport::toggle_hosting);
        overlay.last_build = None;
    }
    overlay.keys = pressed;
}

/// Draws the HUD over the frame that `dc` is about to present. Game render thread.
pub fn on_swap(dc: win::HDC) {
    if !settings::get().hud {
        return;
    }
    let mut overlay = OVERLAY.lock().unwrap_or_else(|e| e.into_inner());
    let window = unsafe { win::WindowFromDC(dc) };
    if window.is_null() {
        return;
    }
    handle_keys(&mut overlay, window);
    if mode() == Mode::Hidden || overlay.failed {
        return;
    }
    let mut client = win::RECT::default();
    unsafe { win::GetClientRect(window, &mut client) };
    let (screen_w, screen_h) = (client.right - client.left, client.bottom - client.top);
    if screen_w <= 0 || screen_h <= 0 {
        return;
    }
    if overlay.renderer.is_none() {
        overlay.renderer = unsafe { Renderer::new() };
        if overlay.renderer.is_none() {
            overlay.failed = true;
            return;
        }
        overlay.names = Some(Names::new());
    }
    let font_px = (screen_h / 48).clamp(11, 26);
    let due = overlay.last_build.is_none_or(|t| t.elapsed() >= REBUILD_INTERVAL) || font_px != overlay.font_px;
    let Overlay { renderer, names, last_build, font_px: built_px, .. } = &mut *overlay;
    let renderer = renderer.as_mut().unwrap();
    unsafe {
        let saved = renderer.save();
        if due {
            *last_build = Some(Instant::now());
            *built_px = font_px;
            if let Some(bitmap) = render(&lines(names.as_mut().unwrap()), font_px) {
                renderer.upload(&bitmap);
            }
        }
        if renderer.size.0 > 0 {
            renderer.draw(screen_w, screen_h);
        }
        renderer.restore(&saved);
    }
}
