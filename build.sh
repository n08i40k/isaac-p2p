#!/bin/sh
# Builds dist/winmm.dll (32-bit Windows). Needs rustup's i686-pc-windows-gnu target, zig and cargo-zigbuild.
set -eu
cd "$(dirname "$0")"
cargo zigbuild --release --target i686-pc-windows-gnu
mkdir -p dist
cp target/i686-pc-windows-gnu/release/winmm.dll isaac_p2p.ini dist/
