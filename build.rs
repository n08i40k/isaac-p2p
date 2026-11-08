// Generates the winmm forwarding stubs (from src/winmm_exports.txt) and the call-tracing thunks.
//
// Every winmm stub is a naked jump through a pointer that DllMain fills with the system winmm.dll's function of
// the same name, so the proxy works for any caller regardless of calling convention. Trace thunk i increments
// TRACE_COUNTS[i] and jumps to TRACE_TARGETS[i].

use std::fmt::Write;
use std::{env, fs, path::Path};

const TRACE_THUNKS: usize = 128;

fn main() {
    println!("cargo:rerun-if-changed=src/winmm_exports.txt");
    let out = env::var("OUT_DIR").unwrap();
    let names: Vec<String> = fs::read_to_string("src/winmm_exports.txt")
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    let windows = env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86");

    let mut exports = String::new();
    writeln!(exports, "pub const NAMES: [&str; {}] = [", names.len()).unwrap();
    for name in &names {
        writeln!(exports, "    \"{name}\\0\",").unwrap();
    }
    writeln!(exports, "];").unwrap();
    writeln!(exports, "pub static TARGETS: [AtomicUsize; {}] = [const {{ AtomicUsize::new(0) }}; {}];", names.len(), names.len()).unwrap();
    if windows {
        // Explicit export directives: the DLL exports exactly the winmm names, not every global symbol.
        write!(exports, "core::arch::global_asm!(\".pushsection .drectve,\\\"yn\\\"\", \".ascii \\\"").unwrap();
        for name in &names {
            write!(exports, " -export:{name}").unwrap();
        }
        writeln!(exports, "\\\"\", \".popsection\");").unwrap();
        for (i, name) in names.iter().enumerate() {
            writeln!(
                exports,
                "#[unsafe(naked)] #[unsafe(export_name = \"{name}\")] unsafe extern \"C\" fn stub_{i}() {{ \
                 core::arch::naked_asm!(\"jmp dword ptr [{{t}} + {}]\", t = sym TARGETS) }}",
                4 * i
            )
            .unwrap();
        }
    }
    fs::write(Path::new(&out).join("exports.rs"), exports).unwrap();

    let mut trace = String::new();
    writeln!(trace, "pub const TRACE_THUNKS: usize = {TRACE_THUNKS};").unwrap();
    writeln!(trace, "pub static TRACE_COUNTS: [AtomicI32; TRACE_THUNKS] = [const {{ AtomicI32::new(0) }}; TRACE_THUNKS];").unwrap();
    writeln!(trace, "pub static TRACE_TARGETS: [AtomicUsize; TRACE_THUNKS] = [const {{ AtomicUsize::new(0) }}; TRACE_THUNKS];").unwrap();
    if windows {
        for i in 0..TRACE_THUNKS {
            writeln!(
                trace,
                "#[unsafe(naked)] unsafe extern \"C\" fn thunk_{i}() {{ core::arch::naked_asm!(\
                 \"lock inc dword ptr [{{c}} + {o}]\", \"jmp dword ptr [{{t}} + {o}]\", \
                 c = sym TRACE_COUNTS, t = sym TRACE_TARGETS) }}",
                o = 4 * i
            )
            .unwrap();
        }
        writeln!(trace, "pub static TRACE_THUNK_FNS: [unsafe extern \"C\" fn(); TRACE_THUNKS] = [").unwrap();
        for i in 0..TRACE_THUNKS {
            writeln!(trace, "    thunk_{i},").unwrap();
        }
        writeln!(trace, "];").unwrap();
    }
    fs::write(Path::new(&out).join("trace_thunks.rs"), trace).unwrap();
}
