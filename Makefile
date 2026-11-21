SYSROOT_LIB ?= $(HOME)/.local/egr-sysroot/usr/lib/x86_64-linux-gnu
export PKG_CONFIG_PATH ?= $(SYSROOT_LIB)/pkgconfig
export LIBRARY_PATH ?= $(SYSROOT_LIB)
CARGO_TARGET_DIR ?= target
SHOTS_DIR ?= /tmp/egr/shots/$(shell date +%Y%m%d-%H%M%S)

.PHONY: check-sim check test test-sim scenarios determinism perf shots clippy-sim fmt-check

check-sim:
	cargo check --locked --workspace --tests --exclude native_app --exclude engine --exclude engine_demo --exclude assets_gui

check:
	cargo check --locked --workspace --tests --examples --features multiplayer

test:
	cargo test --locked --workspace --features multiplayer -- --test-threads=1

test-sim:
	cd simulation && cargo test --locked -- --test-threads=1

scenarios:
	cargo run --locked -p scenarios --release -- all

determinism:
	cargo run --locked -p scenarios --release -- determinism --twice

perf:
	cargo run --locked -p scenarios --release -- perf --json $(CARGO_TARGET_DIR)/perf.json

shots:
	cargo build --locked -p native_app --release
	mkdir -p $(SHOTS_DIR)
	timeout 900 xvfb-run -a -s "-screen 0 1600x900x24" env WGPU_BACKEND=vulkan \
		VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
		$(CARGO_TARGET_DIR)/release/native_app --script scripts/shots/smoke.json \
		--exit-after-script --save-dir $(SHOTS_DIR)
	@echo "screenshots in $(SHOTS_DIR)"

clippy-sim:
	cargo clippy --locked -p simulation -- -D clippy::iter_over_hash_type

fmt-check:
	cargo fmt --all -- --check
