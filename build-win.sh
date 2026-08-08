#!/bin/bash
# Cross-compile the Windows exe and stage the runtime DLLs next to it.
#
# Requirements:
#  - The BtbN n8.1 gpl-shared win64 build in third_party/btbn/<build>/.
#    It provides BOTH the import libs for LINKING (lib/) and the headers
#    (include/), AND the self-contained runtime DLLs (bin/).  Download
#    ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip and unzip it there.
set -e
cd "$(dirname "$0")"

# The BtbN build directory (headers + import libs for linking).
FFBUILD="$(find third_party/btbn -maxdepth 1 -type d -name 'ffmpeg-*' | head -1)"
if [ -z "$FFBUILD" ]; then
    echo "ERROR: no BtbN build in third_party/btbn (unzip ffmpeg-n8.1-latest-win64-gpl-shared-8.1.zip there)." >&2
    exit 1
fi
BIN="$FFBUILD/bin"

# Link against the BtbN mingw FFmpeg import libs.
FFMPEG_DIR="$PWD/$FFBUILD" \
BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I/usr/lib/gcc/x86_64-w64-mingw32/13-posix/include" \
cargo build --release --target x86_64-pc-windows-gnu

REL="target/x86_64-pc-windows-gnu/release"

# Stage the runtime FFmpeg DLLs (self-contained build) next to the exe.
# Audio is decoded in-process now, so ffmpeg.exe/ffprobe.exe are no longer
# needed at runtime.
cp "$BIN"/*.dll "$REL"/
echo "Staged $(ls "$BIN"/*.dll | wc -l) runtime FFmpeg DLLs from $BIN"

echo
echo "Built: $REL/omniview.exe ($(du -h "$REL/omniview.exe" | cut -f1))"

