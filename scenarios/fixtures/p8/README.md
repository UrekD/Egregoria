# P8 Wave 0 baseline

Wave 0 runs on the P6 floor recorded in [handoff.json](handoff.json). The two
slot traces are diagnostic windows over stock city activity.
[baseline.json](baseline.json) records the captured snapshots, inputs, counts
and representative events. The reduction
pins a single offered carless rider and a separate one-line rider. The later
release gates use predeclared cohorts, common setup and the acceptance rules
in [acceptance.json](acceptance.json).

Run from the repository root using the Wave 0 target directory. Run the
commands sequentially and retain stdout/stderr for the ignored tests.

```sh
export CARGO_TARGET_DIR=/home/durek/.cache/egr-target/slot4
export TMPDIR=/var/tmp/egr

cargo test --locked -p simulation freight2_
cargo test --locked -p simulation --test p8_transport p8_freight_physical_boundary_control -- --nocapture
cargo test --locked -p scenarios --test p8_connections p8_bus_stock_reduction_controls -- --nocapture
cargo test --locked -p scenarios --test p8_connections p8_stock_warehouse_policy_control
cargo test --locked -p simulation --test p8_transport -- --ignored --list
cargo test --locked -p scenarios --test p8_connections -- --ignored --list

# Future assertions, expected to fail on the P6 floor.
cargo test --locked -p simulation --test p8_transport -- --ignored --nocapture --test-threads=1
cargo test --locked -p scenarios --test p8_connections p8_bus_stock_reduction -- --ignored --nocapture

# Copy or generate the two named diagnostic slot families in this directory.
export P8_W0_SLOT_ROOT=/absolute/path/to/private/diagnostic-slots
cargo test --locked -p scenarios --test p8_connections p8_slot_stock_traces -- --ignored --nocapture

export PKG_CONFIG_PATH=/home/durek/.local/egr-sysroot/usr/lib/x86_64-linux-gnu/pkgconfig
export LIBRARY_PATH=/home/durek/.local/egr-sysroot/usr/lib/x86_64-linux-gnu
cargo check --locked --workspace --tests
```

The named-test output prefixes are `bus_offer ` and `slot_trace `. The
reduction records offer, board, alight and actual final-door arrival. The
ordered slot records carry elapsed ticks, stock bus residency transitions,
stock rail call stages and sampled 3D poses, road manifests, receiver stock,
receipts and fiscal deltas. The observer chooses a short recipe input at the initial
state when one exists, and otherwise follows a stocked comparison receiver. Initial sightings of already boarded riders are
explicitly flagged. A cross-line sighting with the same router target is
diagnostic until an authoritative journey and occurrence key exist. The
stock terminal import pool, physical road receipts and market ledger are
separate observations. Wave 1 needs an authoritative rail call event before
they can establish lineage.

The policy arm submits the existing warehouse command to the same terminal
in a private in-memory load. The paired comparison checks the stock outcome
and identical gameplay evidence; the positive warehouse test exercises an
eligible target. P8 will replace this probe with an accepted, independently
scored terminal release change. The fixture lookup is by slot name under
`P8_W0_SLOT_ROOT`; the manifest pins the bytes used for the captured run.
The slot families themselves are external inputs. The Showcase producer is
`scenarios demo-cities --save-root DIR`. The archived Lesce input came
from `scenarios osm-city --input EXTRACT --save-root DIR` at the producer
pin and extract digest in the baseline manifest.

Wave 1 needs S to integrate the P7 authority, routing and movement seams and
register the runtime APIs. R1 owns shared track claims and physical events
before F consumes them. The follow-on tests in the acceptance manifest
describe cases that need those interfaces and their owning merge windows.
