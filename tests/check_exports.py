#!/usr/bin/env python3
"""Fails when a module in the game folder imports a winmm function that dist/winmm.dll does not export.

Windows refuses to start the game in that case ("The procedure entry point ... could not be located"); Wine only
logs it, so the Wine tests alone do not catch it.

    check_exports.py <winmm.dll> <game folder>
"""
import glob
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tools"))
import pe  # noqa: E402  (tools/pe.py)

exported = set(pe.exports(sys.argv[1]))
missing = 0
modules = sorted(glob.glob(os.path.join(sys.argv[2], "*.dll")) + glob.glob(os.path.join(sys.argv[2], "*.exe")))
for module in modules:
    if os.path.basename(module).lower() == "winmm.dll":
        continue
    for name in sorted(pe.imports(module)):
        if name not in exported:
            print(f"FAIL {os.path.basename(module)} imports {name}")
            missing += 1
print(f"exports: {len(exported)} winmm functions, {'all imports covered' if not missing else f'{missing} missing'}")
sys.exit(1 if missing else 0)
