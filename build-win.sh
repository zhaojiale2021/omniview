#!/bin/bash
# Cross-compile the Windows exe.
#
# Requires the prebuilt mingw FFmpeg libraries in third_party/ffmpeg-win64
# (headers + import libs, e.g. the MSYS2 mingw-w64-x86_64-ffmpeg package).
# The resulting exe needs the FFmpeg DLLs (avcodec-62.dll etc.) on PATH at
# runtime — any FFmpeg 8.x installation provides them.
set -e
cd "$(dirname "$0")"

FFMPEG_DIR="$PWD/third_party/ffmpeg-win64" \
BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I/usr/lib/gcc/x86_64-w64-mingw32/13-posix/include" \
cargo build --release --target x86_64-pc-windows-gnu

echo
echo "Built: target/x86_64-pc-windows-gnu/release/my-project.exe"
