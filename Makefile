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

.PHONY: build
# Package a Windows test build and include local saves when world exists.
build:
	scripts/build-windows.sh "$(WINDOWS_OUT)" "$(if $(wildcard world/.),world)"

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
		input="$$EGR_OSM_CACHE/lesce-20261122-signals.overpass.json"; \
		scripts/osm/fetch.sh "$$input"; \
	fi; \
	cargo run --locked --release -p scenarios -- osm-city --input "$$input" --save-root "$$EGR_OSM_SAVE_ROOT" $$EGR_OSM_FLAGS

export EGR_CITYMAP_OUT ?= /var/tmp/egr/citymap-build
export EGR_CITYMAP_LESCE ?=
export EGR_CITYMAP_SAVA ?=
.PHONY: citymap-build
citymap-build:
	@set -eu; \
	set -- --out "$$EGR_CITYMAP_OUT"; \
	if [ -n "$$EGR_CITYMAP_LESCE" ]; then set -- "$$@" --input "$$EGR_CITYMAP_LESCE"; fi; \
	if [ -n "$$EGR_CITYMAP_SAVA" ]; then set -- "$$@" --sava-input "$$EGR_CITYMAP_SAVA"; fi; \
	cargo run --locked --release -p scenarios -- citymap-build "$$@"

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

# Curated P5 release screenshots. Demo and Lesce slots are generated into a
# temporary root and installed into the script directory as in osm-lesce-shots.
.PHONY: p5-shots
p5-shots:
	cargo build --locked --release -p native_app -p scenarios
	@set -efu; \
	if [ "$${EGR_OSM_INPUT+x}" ]; then \
		input="$$EGR_OSM_INPUT"; \
	else \
		input="$$EGR_OSM_CACHE/lesce-20261122-signals.overpass.json"; \
		scripts/osm/fetch.sh "$$input"; \
	fi; \
	root=$$(mktemp -d "$${TMPDIR:-/tmp}/egr-p5-shots.XXXXXX"); \
	"$(CARGO_TARGET_DIR)/release/scenarios" demo-cities --save-root "$$root/slots"; \
	"$(CARGO_TARGET_DIR)/release/scenarios" osm-city --input "$$input" --save-root "$$root/slots"; \
	timeout 900 xvfb-run -a -s "-screen 0 1600x900x24" env WGPU_BACKEND=vulkan \
		VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
		bash -c 'set -eu; \
			display=$${DISPLAY#:}; \
			read -r xpid < "/tmp/.X$${display%%.*}-lock"; \
			[ "$$(ps -p "$$xpid" -o comm=)" = Xvfb ]; \
			kill -STOP "$$xpid"; \
			trap "kill -CONT $$xpid" EXIT; \
			"$$1" --script scripts/shots/p5_release.json --exit-after-script --save-dir "$$2" & \
			pid=$$!; \
			trap "kill -CONT $$xpid; kill $$pid 2>/dev/null || true" EXIT; \
			while [ ! -f "$$2/.egregoria-run" ]; do kill -0 "$$pid"; sleep 0.05; done; \
			cp -- "$$3"/* "$$2/"; \
			kill -CONT "$$xpid"; \
			wait "$$pid"; \
			trap - EXIT' \
		p5-shots "$(CARGO_TARGET_DIR)/release/native_app" "$$root/shots" "$$root/slots"; \
	printf 'Screenshots in %s\n' "$$root/shots"

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
