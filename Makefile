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
		input="$$EGR_OSM_CACHE/lesce.overpass.json"; \
		scripts/osm/fetch.sh "$$input"; \
	fi; \
	cargo run --locked --release -p scenarios -- osm-city --input "$$input" --save-root "$$EGR_OSM_SAVE_ROOT" $$EGR_OSM_FLAGS

# Native script mode requires a fresh save root. Stage the slot after its
# first screenshot, once the native startup check has accepted that root.
.PHONY: osm-lesce-shots
osm-lesce-shots:
	cargo build --locked --release -p native_app
	@set -eu; \
	shots_dir=$$(mktemp -d "$${TMPDIR:-/tmp}/egr-lesce-shots.XXXXXX"); \
	timeout 900 xvfb-run -a -s "-screen 0 1600x900x24" env WGPU_BACKEND=vulkan \
		VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json sh -eu -c '\
			"$$1" --script scripts/shots/osm_lesce.json --exit-after-script --save-dir "$$2" & \
			pid=$$!; trap "kill \$$pid 2>/dev/null || true" EXIT; \
			while [ ! -s "$$2/osm-ready.png" ]; do kill -0 "$$pid"; sleep 0.1; done; \
			cp "$$3/lesce_osm.zip" "$$3/lesce_osm.meta.json" "$$3/ATTRIBUTION-OSM.txt" "$$2/"; \
			wait "$$pid"; rm "$$2/osm-ready.png"; trap - EXIT' \
		sh "$(CARGO_TARGET_DIR)/release/native_app" "$$shots_dir" "$$EGR_OSM_SAVE_ROOT"; \
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
