SYSROOT_LIB ?= $(HOME)/.local/egr-sysroot/usr/lib/x86_64-linux-gnu
export PKG_CONFIG_PATH ?= $(SYSROOT_LIB)/pkgconfig
export LIBRARY_PATH ?= $(SYSROOT_LIB)
CARGO_TARGET_DIR ?= target

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

# Windows zip (cross-compiled; see scripts/build-windows.sh for prerequisites).
# WINDOWS_SLOTS=<dir> bundles save slots, e.g. WINDOWS_SLOTS=world after make demo-cities osm-lesce.
WINDOWS_OUT ?= build
WINDOWS_SLOTS ?=
.PHONY: windows
windows:
	scripts/build-windows.sh "$(WINDOWS_OUT)" "$(WINDOWS_SLOTS)"

.PHONY: demo-cities
demo-cities:
	cargo run --locked --release -p scenarios -- demo-cities --save-root world

export EGR_OSM_CACHE ?= target/osm-cache
export EGR_OSM_SAVE_ROOT ?= world
export EGR_OSM_FLAGS ?=
ifneq ($(origin EGR_OSM_INPUT),undefined)
export EGR_OSM_INPUT
endif

.PHONY: osm-lesce
osm-lesce:
	@set -efu; \
	if [ "$${EGR_OSM_INPUT+x}" ]; then \
		input="$$EGR_OSM_INPUT"; \
	else \
		input="$$EGR_OSM_CACHE/lesce-20261122-wood.overpass.json"; \
		scripts/osm/fetch.sh "$$input"; \
	fi; \
	cargo run --locked --release -p scenarios -- osm-city --input "$$input" --save-root "$$EGR_OSM_SAVE_ROOT" $$EGR_OSM_FLAGS

# Install the fixture after native accepts the empty directory and before the
# private Xvfb display allows the script to enter its render loop.
.PHONY: osm-lesce-shots
osm-lesce-shots:
	cargo build --locked --release -p native_app
	@set -eu; \
	shots_dir=$$(mktemp -d "$${TMPDIR:-/tmp}/egr-lesce-shots.XXXXXX"); \
	timeout 900 xvfb-run -a -s "-screen 0 1600x900x24" env WGPU_BACKEND=vulkan \
		VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
		scripts/osm/shots.sh "$(CARGO_TARGET_DIR)/release/native_app" "$$shots_dir" "$$EGR_OSM_SAVE_ROOT"; \
	printf 'Screenshots in %s\n' "$$shots_dir"

determinism:
	cargo run --locked -p scenarios --release -- determinism --twice

perf:
	cargo run --locked -p scenarios --release -- perf --json "$(CARGO_TARGET_DIR)/perf.json"

.PHONY: perf-10k
perf-10k:
	cargo run --locked -p scenarios --release -- perf_10k --json "$(CARGO_TARGET_DIR)/perf_10k.json"

shots:
	cargo build --locked -p native_app --release
	@mkdir -p /tmp/egr/shots && \
	shots_dir=$$(mktemp -d /tmp/egr/shots/XXXXXX) && \
	timeout 900 xvfb-run -a -s "-screen 0 1600x900x24" env WGPU_BACKEND=vulkan \
		VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
		"$(CARGO_TARGET_DIR)/release/native_app" --script scripts/shots/smoke.json \
		--exit-after-script --save-dir "$$shots_dir" && \
	echo "screenshots in $$shots_dir"

clippy-sim:
	cargo clippy --locked -p simulation -- -D clippy::iter_over_hash_type

fmt-check:
	cargo fmt --all -- --check
