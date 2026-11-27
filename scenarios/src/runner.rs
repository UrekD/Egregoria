use crate::builders::CommandSink;
use crate::MetricSample;
use geom::Vec3;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use simulation::economy::Government;
use simulation::map_dynamic::{Destination, ParkingManagement, RoutingStep};
use simulation::transportation::{Location, VehicleKind, VehicleState};
use simulation::utils::rand_provider::RandProvider;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::WorldCommand;
use simulation::{EntityID, Simulation, SimulationOptions};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::Hasher;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Once;
use std::time::Duration;

pub const MEDIAN_TICKS_PER_SEC: &str = "median_ticks_per_sec";
pub const P99_MS_PER_TICK: &str = "p99_ms_per_tick";
pub(crate) const P50_MS_PER_TICK: &str = "p50_ms_per_tick";
pub(crate) const P95_MS_PER_TICK: &str = "p95_ms_per_tick";

const CHECKPOINT_INTERVAL: u64 = 10_000;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ProjectionCheckpoint {
    tick: u64,
    projection: u64,
}

struct Checkpoint {
    projection: ProjectionCheckpoint,
    hashes: BTreeMap<String, u64>,
}

struct CheckpointDump {
    include_hashes: bool,
    first: Vec<Checkpoint>,
    second: Option<Vec<Checkpoint>>,
}

#[derive(Serialize)]
struct HashLine<'a> {
    scenario: &'a str,
    run: usize,
    #[serde(flatten)]
    projection: ProjectionCheckpoint,
    hashes: &'a BTreeMap<String, u64>,
}

#[derive(Serialize)]
pub struct PerfTimings {
    system_timings: BTreeMap<&'static str, SystemTiming>,
    max_ms_per_tick: f64,
    max_tick: u64,
}

#[derive(Serialize)]
struct SystemTiming {
    median_ms: f64,
    p99_ms: f32,
    max_ms: f32,
    max_tick: u64,
}

#[derive(Default)]
struct PerfTimingRecorder {
    systems: BTreeMap<&'static str, Vec<(u64, f32)>>,
    peak: Option<(u64, Duration)>,
    measured: Option<PerfTimings>,
}

impl PerfTimingRecorder {
    fn record_tick(
        &mut self,
        tick: u64,
        elapsed: Duration,
        system_samples: impl IntoIterator<Item = (&'static str, f32)>,
    ) {
        for (system, ms) in system_samples {
            self.systems.entry(system).or_default().push((tick, ms));
        }
        if self.peak.is_none_or(|(_, max)| elapsed >= max) {
            self.peak = Some((tick, elapsed));
        }
    }

    fn finish_phase(&mut self, label: &str) {
        if let Some((max_tick, elapsed)) = self.peak.take().filter(|_| label == "measured") {
            let system_timings = self
                .systems
                .iter_mut()
                .filter(|(_, samples)| !samples.is_empty())
                .map(|(&system, samples)| {
                    // Preserve tick order for equal durations so the last maximum wins.
                    samples.sort_by(|a, b| a.1.total_cmp(&b.1));
                    let n = samples.len();
                    let median_ms =
                        (f64::from(samples[(n - 1) / 2].1) + f64::from(samples[n / 2].1)) / 2.0;
                    let p99_ms = samples[(n * 99).div_ceil(100) - 1].1;
                    let (max_tick, max_ms) = samples[n - 1];
                    let timing = SystemTiming {
                        median_ms,
                        p99_ms,
                        max_ms,
                        max_tick,
                    };
                    (system, timing)
                })
                .collect();
            self.measured = Some(PerfTimings {
                system_timings,
                max_ms_per_tick: elapsed.as_secs_f64() * 1_000.0,
                max_tick,
            });
        }
        for samples in self.systems.values_mut() {
            samples.clear();
        }
    }
}

pub struct Scenario {
    pub name: &'static str,
    pub run: fn(&mut ScenarioCtx) -> Result<(), String>,
}

pub struct ScenarioCtx {
    pub sim: Simulation,
    sched: SeqSchedule,
    pub samples: Vec<MetricSample>,
    tick_times: Vec<Duration>,
    queued: Vec<WorldCommand>,
    executed_ticks: u64,
    checkpoints: Option<Vec<Checkpoint>>,
    hash_dump: Option<CheckpointDump>,
    perf_timings: Option<PerfTimingRecorder>,
    pub(crate) determinism_pass: Option<u8>,
}

/// Set the process working directory to the repository root for prototype loading.
pub fn ensure_repo_root_cwd() {
    std::env::set_current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
        .expect("could not set the working directory to the repository root");
}

impl ScenarioCtx {
    pub fn new(opts: SimulationOptions) -> Self {
        Self::initialize();
        Self::from_sim(Simulation::new_with_options(opts))
    }

    pub(crate) fn initialize() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let executable = std::env::current_exe().ok();
            let runtime = executable
                .as_deref()
                .and_then(Path::parent)
                .filter(|dir| dir.join("base_mod/data.lua").is_file());
            if let Some(runtime) = runtime {
                std::env::set_current_dir(runtime)
                    .expect("could not set the working directory to the executable directory");
            } else {
                ensure_repo_root_cwd();
            }
            simulation::init::init();
        });
    }

    pub(crate) fn from_sim(sim: Simulation) -> Self {
        Self {
            sim,
            sched: Simulation::schedule(),
            samples: Vec::new(),
            tick_times: Vec::new(),
            queued: Vec::new(),
            executed_ticks: 0,
            checkpoints: None,
            hash_dump: None,
            perf_timings: None,
            determinism_pass: None,
        }
    }

    /// Queue commands in order for the next tick.
    pub fn submit(&mut self, cmds: &[WorldCommand]) {
        self.queued.extend_from_slice(cmds);
    }

    pub fn run_ticks(&mut self, n: u64) {
        self.run_ticks_observed(n, |_| {});
    }

    pub(crate) fn run_ticks_observed(
        &mut self,
        n: u64,
        mut observe: impl FnMut(&Simulation),
    ) -> u64 {
        let before = self.executed_ticks;
        for _ in 0..n {
            let elapsed = self.sim.tick(&mut self.sched, &self.queued);
            self.tick_times.push(elapsed);
            self.queued.clear();
            self.executed_ticks += 1;

            if let Some(timings) = &mut self.perf_timings {
                timings.record_tick(self.sim.get_tick(), elapsed, self.sched.latest_samples());
            }
            if self.executed_ticks % CHECKPOINT_INTERVAL == 0 {
                let include_hashes = self.checkpoints.is_some()
                    || self
                        .hash_dump
                        .as_ref()
                        .is_some_and(|dump| dump.include_hashes);
                let checkpoints = self
                    .checkpoints
                    .as_mut()
                    .or_else(|| self.hash_dump.as_mut().map(|dump| &mut dump.first));
                if let Some(checkpoints) = checkpoints {
                    checkpoints.push(checkpoint(&self.sim, include_hashes));
                }
            }
            observe(&self.sim);
        }
        self.executed_ticks - before
    }

    /// Collect raw per-tick timing samples for the perf JSON report.
    pub fn enable_perf_timings(&mut self) {
        self.perf_timings = Some(PerfTimingRecorder::default());
    }

    pub fn take_measured_timings(&mut self) -> Option<PerfTimings> {
        self.perf_timings.as_mut()?.measured.take()
    }

    /// Capture current world counts and tick throughput since the previous sample.
    pub fn sample(&mut self, label: &str) -> MetricSample {
        self.sample_with_extra(label, BTreeMap::new())
    }

    pub(crate) fn sample_perf(&mut self, label: &str, houses: usize) -> MetricSample {
        self.sample_perf_with_extra(label, houses, BTreeMap::new(), false)
    }

    pub(crate) fn sample_perf_with_extra(
        &mut self,
        label: &str,
        houses: usize,
        mut extra: BTreeMap<String, f64>,
        include_p50: bool,
    ) -> MetricSample {
        let active_vehicles = self
            .sim
            .world()
            .vehicles
            .values()
            .filter(|vehicle| !vehicle.vehicle.state.is_parked())
            .count();
        extra.extend([
            ("houses".to_string(), houses as f64),
            ("active_vehicles".to_string(), active_vehicles as f64),
        ]);
        if !self.tick_times.is_empty() {
            let times = &mut self.tick_times;
            times.sort_unstable();
            let n = times.len();
            let median_seconds =
                (times[(n - 1) / 2].as_secs_f64() + times[n / 2].as_secs_f64()) / 2.0;
            if include_p50 {
                extra.insert(P50_MS_PER_TICK.to_string(), median_seconds * 1_000.0);
            }
            let median_ticks_per_sec = if median_seconds > 0.0 {
                median_seconds.recip()
            } else {
                0.0
            };
            // Use the nearest rank for tail latencies.
            let percentile_ms = |percent: usize| {
                let rank = (n * percent).div_ceil(100);
                times[rank - 1].as_secs_f64() * 1_000.0
            };
            extra.extend([
                (MEDIAN_TICKS_PER_SEC.to_string(), median_ticks_per_sec),
                (P95_MS_PER_TICK.to_string(), percentile_ms(95)),
                (P99_MS_PER_TICK.to_string(), percentile_ms(99)),
            ]);
        }
        self.sample_with_extra(label, extra)
    }

    fn sample_with_extra(&mut self, label: &str, mut extra: BTreeMap<String, f64>) -> MetricSample {
        let elapsed: f64 = self.tick_times.iter().map(Duration::as_secs_f64).sum();
        let ticks_per_sec = if elapsed > 0.0 {
            self.tick_times.len() as f64 / elapsed
        } else {
            0.0
        };
        self.tick_times.clear();
        if let Some(timings) = &mut self.perf_timings {
            timings.finish_phase(label);
        }

        let world = self.sim.world();
        let employed = world.humans.values().filter(|h| h.work.is_some()).count();
        extra.insert("employed".to_string(), employed as f64);
        let sample = MetricSample {
            tick: self.sim.get_tick(),
            label: label.to_string(),
            humans: world.humans.len(),
            vehicles: world.vehicles.len(),
            companies: world.companies.len(),
            trains: world.trains.len(),
            money_cents: self.sim.read::<Government>().money.cents(),
            ticks_per_sec,
            extra,
        };
        self.samples.push(sample.clone());
        sample
    }

    /// Run a scenario and emit its periodic and final checkpoints as JSON lines.
    pub fn run_with_hashes(
        &mut self,
        scenario: &str,
        out: &mut impl Write,
        run: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        self.run_with_checkpoints(scenario, Some(out), None, run)
    }

    /// Emit checkpoint dumps and/or compare each run against a projection baseline.
    pub fn run_with_checkpoints(
        &mut self,
        scenario: &str,
        mut out: Option<&mut dyn Write>,
        expected: Option<&[ProjectionCheckpoint]>,
        run: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.hash_dump.is_some() {
            return Err("hash dumps cannot be nested".to_string());
        }
        let include_hashes = out.is_some();
        self.hash_dump = Some(CheckpointDump {
            include_hashes,
            first: Vec::new(),
            second: None,
        });
        let result = run(self);
        let CheckpointDump {
            mut first, second, ..
        } = self.hash_dump.take().unwrap();
        result?;
        if second.is_none() {
            first.push(checkpoint(&self.sim, include_hashes));
        }
        for (index, checkpoints) in std::iter::once(first).chain(second).enumerate() {
            let run = index + 1;
            if let Some(expected) = expected {
                compare_projection(&checkpoints, expected)
                    .map_err(|err| format!("run {run}: {err}"))?;
            }
            if let Some(out) = out.as_deref_mut() {
                for checkpoint in &checkpoints {
                    let line = HashLine {
                        scenario,
                        run,
                        projection: checkpoint.projection,
                        hashes: &checkpoint.hashes,
                    };
                    serde_json::to_writer(&mut *out, &line).map_err(|err| err.to_string())?;
                    out.write_all(b"\n").map_err(|err| err.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// Run from this fresh context and another with identical options, retaining the first run.
    pub fn verify_twice(
        &mut self,
        mut run: impl FnMut(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let opts = *self.sim.read::<SimulationOptions>();
        self.determinism_pass = Some(1);
        let first = self.record_checkpoints(&mut run);
        self.determinism_pass = None;
        let first = first?;
        let mut second_ctx = Self::new(opts);
        second_ctx.determinism_pass = Some(2);
        let second = second_ctx.record_checkpoints(&mut run)?;

        for (first, second) in first.iter().zip(&second) {
            compare_checkpoint(first, second)?;
        }
        if first.len() != second.len() {
            return Err(format!(
                "determinism checkpoint counts differ ({} and {})",
                first.len(),
                second.len()
            ));
        }
        if let Some(dump) = &mut self.hash_dump {
            dump.first = first;
            dump.second = Some(second);
        }
        Ok(())
    }

    pub(crate) fn verify_resumed(
        &mut self,
        ticks: u64,
        run_for: impl Fn(&mut Self, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let opts = *self.sim.read::<SimulationOptions>();
        run_for(self, ticks)?;

        let midpoint = ticks / 2;
        let mut resumed = Self::new(opts);
        run_for(&mut resumed, midpoint)?;
        resumed.reload()?;
        resumed.run_ticks(ticks - midpoint);

        compare_checkpoint(
            &checkpoint(&self.sim, true),
            &checkpoint(&resumed.sim, true),
        )
    }

    pub(crate) fn reload(&mut self) -> Result<(), String> {
        self.sim = self.round_tripped()?;
        self.sched = Simulation::schedule();
        Ok(())
    }

    fn round_tripped(&self) -> Result<Simulation, String> {
        // Reserve a private directory so concurrent runs cannot share a midpoint save.
        static NEXT_SAVE: AtomicUsize = AtomicUsize::new(0);
        let save_dir = simulation::save_dir();
        fs::create_dir_all(save_dir)
            .map_err(|err| format!("could not create {}: {err}", save_dir.display()))?;
        let (directory, path) = loop {
            let id = NEXT_SAVE.fetch_add(1, Ordering::Relaxed);
            let directory = format!("scenarios-resume-{}-{id}", std::process::id());
            let path = save_dir.join(&directory);
            match fs::create_dir(&path) {
                Ok(()) => break (directory, path),
                Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("could not create {}: {err}", path.display())),
            }
        };

        let name = format!("{directory}/midpoint");
        self.sim.save_to_disk(&name);
        let sim = Simulation::load_from_disk(&name);
        fs::remove_dir_all(&path)
            .map_err(|err| format!("could not remove {}: {err}", path.display()))?;
        let sim = sim.map_err(|err| {
            format!(
                "could not round-trip save at tick {}: {err}",
                self.sim.get_tick()
            )
        })?;
        compare_checkpoint(&checkpoint(&self.sim, true), &checkpoint(&sim, true))?;
        Ok(sim)
    }

    fn record_checkpoints(
        &mut self,
        run: &mut impl FnMut(&mut Self) -> Result<(), String>,
    ) -> Result<Vec<Checkpoint>, String> {
        if self.checkpoints.is_some() {
            return Err("determinism comparisons cannot be nested".to_string());
        }
        self.checkpoints = Some(Vec::new());
        let result = run(self);
        let mut checkpoints = self.checkpoints.take().unwrap();
        result?;
        checkpoints.push(checkpoint(&self.sim, true));
        Ok(checkpoints)
    }
}

fn checkpoint(sim: &Simulation, include_hashes: bool) -> Checkpoint {
    Checkpoint {
        projection: ProjectionCheckpoint {
            tick: sim.get_tick(),
            projection: gameplay_projection(sim),
        },
        hashes: include_hashes.then(|| sim.hashes()).unwrap_or_default(),
    }
}

fn compare_projection(
    actual: &[Checkpoint],
    expected: &[ProjectionCheckpoint],
) -> Result<(), String> {
    for (actual, expected) in actual.iter().zip(expected) {
        let actual = actual.projection;
        if actual.tick != expected.tick {
            return Err(format!(
                "projection checkpoint tick mismatch: expected {}, got {}",
                expected.tick, actual.tick
            ));
        }
        if actual.projection != expected.projection {
            return Err(format!(
                "projection mismatch at tick {}: expected {}, got {}",
                actual.tick, expected.projection, actual.projection
            ));
        }
    }
    if actual.len() != expected.len() {
        return Err(format!(
            "projection checkpoint count mismatch: expected {}, got {}",
            expected.len(),
            actual.len()
        ));
    }
    Ok(())
}

// These named views read private fields without changing simulation APIs.
// Only the explicitly encoded gameplay values below enter the hash.
#[derive(Deserialize)]
struct RouterView {
    #[serde(deserialize_with = "Deserialize::deserialize")]
    cur_step: Option<RoutingStep>,
    steps: Vec<RoutingStep>,
}

#[derive(Deserialize)]
struct ParkingView {
    reserved_spots: Vec<SlotKey>,
}

#[derive(Deserialize)]
struct SlotKey {
    idx: u32,
    version: u32,
}

impl SlotKey {
    fn ffi(&self) -> u64 {
        (u64::from(self.version) << 32) | u64::from(self.idx)
    }
}

fn serde_view<T: DeserializeOwned>(value: &impl Serialize) -> T {
    serde_json::from_value(serde_json::to_value(value).expect("could not read gameplay state"))
        .expect("could not decode gameplay state")
}

fn entity_ffi(id: impl EntityID) -> u64 {
    id.data().as_ffi()
}

fn slot_ffi(id: &impl Serialize) -> u64 {
    serde_view::<SlotKey>(id).ffi()
}

fn write_u64_le(hash: &mut impl Hasher, value: u64) {
    hash.write(&value.to_le_bytes());
}

fn write_tagged_id(hash: &mut impl Hasher, tag: u8, id: u64) {
    hash.write_u8(tag);
    write_u64_le(hash, id);
}

fn write_position_cm(hash: &mut impl Hasher, pos: Vec3) {
    for coord in [pos.x, pos.y, pos.z] {
        let cm = (f64::from(coord) * 100.0).round() as i64;
        hash.write(&cm.to_le_bytes());
    }
}

fn write_route_position(hash: &mut impl Hasher, pos: Vec3) {
    for coord in [pos.x, pos.y, pos.z] {
        hash.write(&coord.to_bits().to_le_bytes());
    }
}

fn write_routing_step(hash: &mut impl Hasher, step: Option<&RoutingStep>) {
    match step {
        None => hash.write_u8(0),
        Some(RoutingStep::WalkTo(pos)) => {
            hash.write_u8(1);
            write_route_position(hash, *pos);
        }
        Some(RoutingStep::DriveTo(id, pos)) => {
            write_tagged_id(hash, 2, entity_ffi(*id));
            write_route_position(hash, *pos);
        }
        Some(RoutingStep::Park(id, spot)) => {
            write_tagged_id(hash, 3, entity_ffi(*id));
            match spot {
                None => hash.write_u8(0),
                Some(spot) => write_tagged_id(hash, 1, slot_ffi(spot)),
            }
        }
        Some(RoutingStep::Unpark(id)) => write_tagged_id(hash, 4, entity_ffi(*id)),
        Some(RoutingStep::GetInVehicle(id)) => write_tagged_id(hash, 5, entity_ffi(*id)),
        Some(RoutingStep::GetOutVehicle(id)) => write_tagged_id(hash, 6, entity_ffi(*id)),
        Some(RoutingStep::GetInBuilding(id)) => write_tagged_id(hash, 7, slot_ffi(id)),
        Some(RoutingStep::GetOutBuilding(id)) => write_tagged_id(hash, 8, slot_ffi(id)),
        Some(RoutingStep::WaitAtStop { stop, line, since }) => {
            write_tagged_id(hash, 9, slot_ffi(stop));
            write_u64_le(hash, slot_ffi(line));
            write_u64_le(hash, since.0);
        }
        Some(RoutingStep::RideTransit { line, alight }) => {
            write_tagged_id(hash, 10, slot_ffi(line));
            write_u64_le(hash, slot_ffi(alight));
        }
    }
}

/// Hash stable gameplay fields with fixed tags, length prefixes and little-endian primitives.
/// Entities are ordered by slot key, and their XYZ positions use signed centimetres.
/// Humans include location, the active routing step and its operands, and the requested
/// destination. Route coordinates retain their f32 bits. Vehicles include kind.
#[allow(deprecated)] // SipHasher exposes a specified SipHash-2-4 with fixed keys.
fn gameplay_projection(sim: &Simulation) -> u64 {
    let mut hash = std::hash::SipHasher::new_with_keys(0, 0);
    let world = sim.world();
    let mut humans: Vec<_> = world
        .humans
        .iter()
        .map(|(id, human)| (entity_ffi(id), human))
        .collect();
    humans.sort_unstable_by_key(|&(id, _)| id);
    let mut vehicles: Vec<_> = world
        .vehicles
        .iter()
        .map(|(id, vehicle)| (entity_ffi(id), vehicle))
        .collect();
    vehicles.sort_unstable_by_key(|&(id, _)| id);

    // The parking manager tracks reserved spots. Vehicles and pending Park steps
    // hold the reservation tokens and identify their owners. Include orphaned spots
    // with zero owners, since those spots still affect future parking searches.
    let parking: ParkingView = serde_view(&*sim.read::<ParkingManagement>());
    let mut reservations: BTreeMap<u64, BTreeSet<u64>> = parking
        .reserved_spots
        .iter()
        .map(|spot| (spot.ffi(), BTreeSet::new()))
        .collect();

    write_u64_le(&mut hash, humans.len() as u64);
    let mut employed = 0_u64;
    for (id, human) in humans {
        write_u64_le(&mut hash, id);
        write_position_cm(&mut hash, human.trans.pos);
        employed += u64::from(human.work.is_some());
        match human.location {
            Location::Outside => hash.write_u8(0),
            Location::Vehicle(id) => write_tagged_id(&mut hash, 1, entity_ffi(id)),
            Location::Building(id) => write_tagged_id(&mut hash, 2, slot_ffi(&id)),
            Location::Transit(id) => write_tagged_id(&mut hash, 3, entity_ffi(id)),
            Location::RailTransit(id) => write_tagged_id(&mut hash, 4, entity_ffi(id)),
            Location::TransitHeld => write_tagged_id(&mut hash, 5, 0),
        }

        let router: RouterView = serde_view(&human.router);
        write_routing_step(&mut hash, router.cur_step.as_ref());
        match human.router.target_dest {
            None => hash.write_u8(0),
            Some(Destination::Outside(pos)) => {
                hash.write_u8(1);
                write_route_position(&mut hash, pos);
            }
            Some(Destination::Building(id)) => write_tagged_id(&mut hash, 2, slot_ffi(&id)),
        }
        for step in router.cur_step.iter().chain(&router.steps) {
            if let RoutingStep::Park(vehicle, Some(spot)) = step {
                reservations
                    .entry(slot_ffi(spot))
                    .or_default()
                    .insert(entity_ffi(*vehicle));
            }
        }
    }
    write_u64_le(&mut hash, employed);

    write_u64_le(&mut hash, vehicles.len() as u64);
    for (id, vehicle) in vehicles {
        write_u64_le(&mut hash, id);
        write_position_cm(&mut hash, vehicle.trans.pos);
        hash.write_u8(match vehicle.vehicle.kind {
            VehicleKind::Car => 0,
            VehicleKind::Truck => 1,
            VehicleKind::Bus => 2,
        });
        match &vehicle.vehicle.state {
            VehicleState::Parked(spot)
            | VehicleState::RoadToPark(_, _, spot)
            | VehicleState::ReturningToDepot { spot, .. } => {
                reservations.entry(slot_ffi(spot)).or_default().insert(id);
            }
            VehicleState::Driving | VehicleState::Panicking(_) | VehicleState::ParkedAtDepot => {}
        }
    }
    write_u64_le(&mut hash, reservations.len() as u64);
    for (spot, owners) in reservations {
        write_u64_le(&mut hash, spot);
        write_u64_le(&mut hash, owners.len() as u64);
        for owner in owners {
            write_u64_le(&mut hash, owner);
        }
    }

    // Four successive outputs identify the current four-word xorshift state.
    // Advance a copy to keep the live RNG and simulation untouched.
    let mut rng: RandProvider = serde_view(&*sim.read::<RandProvider>());
    for _ in 0..4 {
        hash.write(&rng.next_u32().to_le_bytes());
    }
    hash.write(&sim.read::<Government>().money.inner().to_le_bytes());
    hash.finish()
}

fn compare_checkpoint(first: &Checkpoint, second: &Checkpoint) -> Result<(), String> {
    let tick = first.projection.tick;
    let other_tick = second.projection.tick;
    let hashes = &first.hashes;
    let other_hashes = &second.hashes;
    if tick != other_tick {
        return Err(format!(
            "determinism checkpoint ticks differ ({tick} and {other_tick})"
        ));
    }
    if hashes != other_hashes {
        let changed: BTreeSet<_> = hashes
            .keys()
            .chain(other_hashes.keys())
            .filter(|key| hashes.get(*key) != other_hashes.get(*key))
            .map(String::as_str)
            .collect();
        let changed: Vec<_> = changed.into_iter().collect();
        return Err(format!(
            "determinism mismatch at tick {tick} ({})",
            changed.join(", ")
        ));
    }
    if first.projection != second.projection {
        return Err(format!("determinism projection mismatch at tick {tick}"));
    }
    Ok(())
}

impl CommandSink for ScenarioCtx {
    fn sim(&self) -> &Simulation {
        &self.sim
    }

    fn submit(&mut self, commands: &[WorldCommand]) {
        ScenarioCtx::submit(self, commands);
    }

    fn has_pending_commands(&self) -> bool {
        !self.queued.is_empty()
    }

    fn flush(&mut self) {
        self.run_ticks(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Color;
    use serde_json::{json, Value};
    use simulation::map::LaneKind;
    use simulation::map_dynamic::{Transit, TransitLine, TransitStop};

    #[test]
    fn p3b_no_transit_matches_frozen_projections() {
        let baseline: BTreeMap<String, Vec<ProjectionCheckpoint>> =
            serde_json::from_str(include_str!("../baselines/p2_projection.json")).unwrap();
        for name in ["smoke", "growth_basic"] {
            let (scenario, options) = crate::builtin::SCENARIOS
                .iter()
                .find(|(scenario, _)| scenario.name == name)
                .unwrap();
            let checkpoints = &baseline[name];
            let mut ctx = ScenarioCtx::new(*options);
            ctx.run_with_checkpoints(scenario.name, None, Some(checkpoints), scenario.run)
                .unwrap();
            if scenario.name == "growth_basic" {
                check_zero_fleet_projection(ctx, *checkpoints.last().unwrap());
            }
        }
    }

    fn check_zero_fleet_projection(legacy: ScenarioCtx, expected: ProjectionCheckpoint) {
        let fleetless = legacy.round_tripped().unwrap();
        let mut legacy = ScenarioCtx::from_sim(legacy.round_tripped().unwrap());
        {
            let map = fleetless.map();
            let mut transit = fleetless.write::<Transit>();
            let (lane, sidewalk) = map
                .lanes()
                .values()
                .filter(|lane| lane.kind == LaneKind::Driving && lane.points.length() >= 60.0)
                .find_map(|lane| {
                    map.lanes()
                        .values()
                        .filter(|walk| walk.kind == LaneKind::Walking && walk.parent == lane.parent)
                        .min_by(|a, b| {
                            let middle = lane.points.middle();
                            a.points.middle().distance2(middle)
                                .total_cmp(&b.points.middle().distance2(middle))
                        })
                        .map(|walk| (lane, walk))
                })
                .expect("growth_basic must have a road with a sidewalk");
            let stops = [0.3, 0.7]
                .map(|fraction| {
                    let (pos, dir) = lane.points.point_dir_along(lane.points.length() * fraction);
                    let walk_pos = sidewalk.points.project(pos);
                    transit.stops.insert_with_key(|id| TransitStop {
                        id,
                        pos,
                        dir: dir.xy(),
                        walk_pos,
                        berth: pos,
                    })
                })
                .to_vec();
            // Set up the line directly so the two branches start with identical treasuries.
            transit.lines.insert_with_key(|id| TransitLine {
                id,
                stops,
                target_vehicles: 0,
                fare: prototypes::Money::ZERO,
                color: Color::WHITE,
                suspended: false,
            });
        }

        assert_eq!(checkpoint(&fleetless, false).projection, expected);
        let mut fleetless = ScenarioCtx::from_sim(fleetless);
        for _ in 0..3 {
            legacy.run_ticks(CHECKPOINT_INTERVAL);
            fleetless.run_ticks(CHECKPOINT_INTERVAL);
            {
                let transit = fleetless.sim.read::<Transit>();
                assert_eq!(transit.lines.len(), 1);
                assert!(transit.runs.is_empty());
            }
            assert_eq!(
                gameplay_projection(&legacy.sim),
                gameplay_projection(&fleetless.sim),
            );
        }
    }

    #[test]
    fn hashes_dump_includes_checkpoints_and_final_for_each_run() {
        let options = SimulationOptions {
            terrain_size: 0,
            save_replay: false,
            ..SimulationOptions::LEGACY
        };
        let run = |ctx: &mut ScenarioCtx| {
            ctx.run_ticks(CHECKPOINT_INTERVAL + 1);
            Ok(())
        };

        for twice in [false, true] {
            let mut ctx = ScenarioCtx::new(options);
            let start = ctx.sim.get_tick();
            let mut dump = Vec::new();
            ctx.run_with_hashes("test", &mut dump, |ctx| {
                if twice {
                    ctx.verify_twice(run)
                } else {
                    run(ctx)
                }
            })
            .unwrap();

            let lines: Vec<Value> = std::str::from_utf8(&dump)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(lines.len(), if twice { 4 } else { 2 });
            for (index, pair) in lines.chunks_exact(2).enumerate() {
                for (offset, line) in pair.iter().enumerate() {
                    assert_eq!(line["scenario"], "test");
                    assert_eq!(line["run"], index + 1);
                    assert_eq!(line["tick"], start + CHECKPOINT_INTERVAL + offset as u64);
                    assert!(line["hashes"]["world"].is_u64());
                    assert!(line["hashes"]["government"].is_u64());
                    assert!(line["projection"].is_u64());
                    assert_eq!(line["hashes"], lines[offset]["hashes"]);
                    assert_eq!(line["projection"], lines[offset]["projection"]);
                }
            }
            assert_eq!(lines[1]["hashes"], json!(ctx.sim.hashes()));
            assert_eq!(lines[1]["projection"], json!(gameplay_projection(&ctx.sim)));
        }
    }

    #[test]
    fn perf_timing_uses_all_raw_samples_in_the_measured_phase() {
        let mut timings = PerfTimingRecorder::default();
        timings.record_tick(1, Duration::from_millis(500), [("periodic", 200.0)]);
        timings.finish_phase("warmup");

        timings.record_tick(2, Duration::from_millis(25), [("periodic", 20.0)]);
        for tick in 3..103 {
            let ms = if tick == 3 { 40 } else { 2 };
            timings.record_tick(tick, Duration::from_millis(ms), [("periodic", 1.0)]);
        }
        timings.record_tick(103, Duration::from_millis(5), [("periodic", 4.0)]);
        timings.finish_phase("measured");

        assert_eq!(
            json!(timings.measured.unwrap()),
            json!({
                "system_timings": {
                    "periodic": {"median_ms": 1.0, "p99_ms": 4.0, "max_ms": 20.0, "max_tick": 2}
                },
                "max_ms_per_tick": 40.0,
                "max_tick": 3,
            })
        );
    }
}
