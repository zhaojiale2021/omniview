#!/bin/bash
# Cross-compile the Windows exe and stage the runtime DLLs next to it.
#
# Requirements:
#  - Prebuilt mingw FFmpeg libraries for LINKING in third_party/ffmpeg-win64
#    (headers + import libs, e.g. the MSYS2 mingw-w64-x86_64-ffmpeg package).
#    Set via FFMPEG_DIR below.
#  - A self-contained runtime FFmpeg DLL set in third_party/btbn/<build>/bin
#    (the BtbN n8.1 gpl-shared build), copied next to the exe so it runs
#    without needing ffmpeg installed on the target machine.
set -e
cd "$(dirname "$0")"

# Link against the prebuilt mingw FFmpeg import libs.
FFMPEG_DIR="$PWD/third_party/ffmpeg-win64" \
BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I/usr/lib/gcc/x86_64-w64-mingw32/13-posix/include" \
cargo build --release --target x86_64-pc-windows-gnu

REL="target/x86_64-pc-windows-gnu/release"

# Stage the runtime FFmpeg DLLs (self-contained build) + the CLI tools the
# audio path uses.
BIN="$(find third_party/btbn -type d -name bin 2>/dev/null | head -1)"
if [ -n "$BIN" ]; then
    cp "$BIN"/*.dll "$BIN"/ffmpeg.exe "$BIN"/ffprobe.exe "$REL"/ 2>/dev/null || true
    echo "Staged runtime FFmpeg DLLs + ffmpeg.exe/ffprobe.exe from $BIN"
else
    echo "WARNING: no runtime DLLs found in third_party/btbn — the exe will need"
    echo "         FFmpeg DLLs on PATH to run."
fi

echo
echo "Built: $REL/my-project.exe"
