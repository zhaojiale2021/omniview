#!/bin/bash
# Set the GitHub repository description and topics.
# Requires the GitHub CLI (https://cli.github.com) and repository admin.
set -e

DESCRIPTION="360° panorama video player built with Rust (ffmpeg-next + wgpu + egui)"
TOPICS=(rust ffmpeg wgpu egui video-player panorama)

gh repo edit zhaojiale2021/omniview --description "$DESCRIPTION"

for topic in "${TOPICS[@]}"; do
    gh repo edit zhaojiale2021/omniview --add-topic "$topic" >/dev/null 2>&1 || true
done

echo "Repo description/topics updated."
