#!/bin/bash
# Generate the /tmp test media fixtures required by `cargo test`.
#
# The unit tests in src/media/ (audio.rs, demux.rs, playback.rs) open these
# files directly by absolute path:
#   /tmp/test_v.mp4     3s video-only   (probe test)
#   /tmp/test_av.mp4    3s A/V          (full pipeline, sync, seek, pause)
#   /tmp/test_av20.mp4  20s A/V 48kHz   (atempo rate tests)
#
# Requires a system `ffmpeg` binary (Ubuntu: `sudo apt install ffmpeg`).
set -e

OUT=/tmp
DUR_AV=3
DUR_AV20=20

echo "Generating $OUT/test_v.mp4 (video only, ${DUR_AV}s)..."
ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=640x360:rate=30:duration=${DUR_AV}" \
    -c:v libx264 -pix_fmt yuv420p -an \
    "$OUT/test_v.mp4"

echo "Generating $OUT/test_av.mp4 (A/V, ${DUR_AV}s, 48kHz stereo aac)..."
ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=640x360:rate=30:duration=${DUR_AV}" \
    -f lavfi -i "sine=frequency=440:duration=${DUR_AV}" \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -b:a 128k -shortest \
    "$OUT/test_av.mp4"

echo "Generating $OUT/test_av20.mp4 (A/V, ${DUR_AV20}s, 48kHz stereo aac)..."
ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=640x360:rate=30:duration=${DUR_AV20}" \
    -f lavfi -i "sine=frequency=440:duration=${DUR_AV20}" \
    -c:v libx264 -pix_fmt yuv420p -c:a aac -b:a 128k -shortest \
    "$OUT/test_av20.mp4"

echo "Done. Fixtures ready for 'cargo test':"
ls -la "$OUT"/test_*.mp4
