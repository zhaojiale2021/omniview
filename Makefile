# Developer conveniences (WSL / Linux).
#
# Usage:
#   make build        # debug build
#   make run FILE=... # run the player with a file
#   make fixtures     # generate /tmp test media (requires system ffmpeg)
#   make test         # fixtures + cargo test
#   make clippy       # lints (must stay at 0 warnings)
#   make release      # optimized build
#   make win          # cross-compile the Windows exe (see build-win.sh)

.PHONY: build run test fixtures clippy release win

build:
	cargo build

run:
	cargo run -- $(FILE)

fixtures:
	bash scripts/gen-test-fixtures.sh

test: fixtures
	cargo test

clippy:
	cargo clippy --all-targets

release:
	cargo build --release

win:
	bash build-win.sh
