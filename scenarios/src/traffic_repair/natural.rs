//! Stock-schedule observations for a verified release or a detached live capture.
use super::comparison::{check_cancelled, load_snapshot, snapshot_bytes};
use super::staging::load_staged_slot;
use super::{ComparisonError, ComparisonReceipt, FixtureFingerprint};
use geom::Vec3;
use prototypes::{game_secs_to_ticks, Tick, TICKS_PER_HOUR, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};
use simulation::diagnostics::elapsed_tick;
use simulation::economy::{DailyEconomyMetrics, TripModeStats, TripPurpose, TripStats};
use simulation::map::{
    IntersectionID, LaneID, LaneKind, RoadID, TrafficBehavior, TraverseKind, TurnID,
};
use simulation::world_command::WorldCommand;
use simulation::{
    ReplayAdvance, ReplayBudget, Simulation, SimulationOptions, SimulationReplayLoader,
    SlotCapture, SnapshotRevision, VehicleID,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::AtomicBool;

const SAMPLE_TICKS: u64 = game_secs_to_ticks(30);
const STATIONARY_TICKS: u64 = 5 * TICKS_PER_MINUTE;
const MAX_WINDOW: u64 = 72 * TICKS_PER_HOUR;
const LIVE_WINDOW: u64 = 10_600;

#[derive(Deserialize)]
struct PinnedSchedule {
    opening_tick: u64,
    seed: u64,
    workers: u16,
    lead_in_ticks: u64,
    scored_ticks: u64,
    corridors: Vec<PinnedCorridor>,
    #[serde(default)]
    gates: SoakGates,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) struct SoakGates {
    max_spillback_ticks: u64,
    max_unresolved_standstill_ticks: u64,
}

impl Default for SoakGates {
    fn default() -> Self {
        Self {
            max_spillback_ticks: 20 * TICKS_PER_MINUTE,
            max_unresolved_standstill_ticks: 30 * TICKS_PER_MINUTE,
        }
    }
}

impl SoakGates {
    pub fn passes(self, report: &NaturalTrafficReport) -> bool {
        report.longest_stationary_ticks < self.max_unresolved_standstill_ticks
            && report.longest_sampled_spillback_ticks < self.max_spillback_ticks
            && report.journeys_by_mode["car"].completed > 0
            && report
                .journeys_by_purpose
                .get(&TripPurpose::Work)
                .is_some_and(|trips| trips.completed > 0)
    }
}

#[derive(Deserialize)]
struct PinnedCorridor {
    key: String,
    road: RoadID,
    point_xy_m: [f32; 2],
    directions: Vec<PinnedDirection>,
}

#[derive(Deserialize)]
struct PinnedDirection {
    from: IntersectionID,
    lane: LaneID,
    to: IntersectionID,
}

pub(super) struct StagedTraffic {
    pub fingerprint: FixtureFingerprint,
    pub gates: SoakGates,
    bytes: Vec<u8>,
    lead_in: u64,
    window: u64,
    lanes: BTreeMap<LaneID, String>,
}

impl StagedTraffic {
    pub fn open(root: &Path, id: &str) -> Result<Self, String> {
        let (fixture, inputs) = load_staged_slot(root, id)?;
        let schedule: PinnedSchedule =
            serde_json::from_slice(&inputs.schedule).map_err(|error| error.to_string())?;
        let sim = load_snapshot(&inputs.snapshot).map_err(|error| error.to_string())?;
        if schedule.opening_tick != sim.get_tick()
            || schedule.seed != sim.read::<SimulationOptions>().seed
            || schedule.workers != 1
            || schedule.lead_in_ticks > MAX_WINDOW
            || !(1..=MAX_WINDOW).contains(&schedule.scored_ticks)
            || schedule.gates.max_spillback_ticks == 0
            || schedule.gates.max_unresolved_standstill_ticks == 0
        {
            return Err(
                "staged traffic schedule disagrees with its slot or exceeds the supported window"
                    .into(),
            );
        }
        let map = sim.map();
        let mut lanes = BTreeMap::new();
        let mut keys = BTreeSet::new();
        for corridor in schedule.corridors {
            let road = map
                .get(corridor.road)
                .ok_or("missing pinned corridor road")?;
            let anchor = geom::Vec2::from(corridor.point_xy_m);
            if !anchor.is_finite()
                || !road
                    .points()
                    .project_segment_2d(anchor)
                    .0
                    .xy()
                    .is_close(anchor, road.width)
                || corridor.key.is_empty()
                || !keys.insert(corridor.key.clone())
                || corridor.directions.is_empty()
            {
                return Err(format!("invalid pinned corridor {}", corridor.key));
            }
            for direction in corridor.directions {
                if map.get(direction.lane).is_none_or(|lane| {
                    lane.parent != road.id
                        || lane.kind != LaneKind::Driving
                        || lane.src != direction.from
                        || lane.dst != direction.to
                }) || lanes
                    .insert(
                        direction.lane,
                        format!("{}/{:?}/{:?}", corridor.key, direction.from, direction.lane),
                    )
                    .is_some()
                {
                    return Err(format!("invalid directed lane on {}", corridor.key));
                }
            }
        }
        Ok(Self {
            fingerprint: fixture.fingerprint,
            gates: schedule.gates,
            bytes: inputs.snapshot,
            lead_in: schedule.lead_in_ticks,
            window: schedule.scored_ticks,
            lanes,
        })
    }

    pub fn soak(
        &self,
        days: u64,
        resume: bool,
        cancelled: &AtomicBool,
    ) -> Result<NaturalTrafficReport, ComparisonError> {
        self.run_soak(days, resume, false, cancelled)
    }

    pub fn soak_replayed(
        &self,
        days: u64,
        resume: bool,
        cancelled: &AtomicBool,
    ) -> Result<NaturalTrafficReport, ComparisonError> {
        self.run_soak(days, resume, true, cancelled)
    }

    fn run_soak(
        &self,
        days: u64,
        resume: bool,
        replay: bool,
        cancelled: &AtomicBool,
    ) -> Result<NaturalTrafficReport, ComparisonError> {
        if days.checked_mul(24 * TICKS_PER_HOUR) != Some(self.window) {
            return Err(ComparisonError::InvalidSchedule(
                "soak duration differs from the staged pin",
            ));
        }
        run_arm(
            &self.bytes,
            self.lead_in,
            self.window,
            &self.lanes,
            None,
            resume,
            replay,
            cancelled,
        )
        .map(|(report, _)| report)
    }

    pub fn compare(
        &self,
        edit: &WorldCommand,
        resume: bool,
        cancelled: &AtomicBool,
    ) -> Result<NaturalRepairComparison, ComparisonError> {
        compare(
            &self.bytes,
            self.lead_in,
            self.window,
            &self.lanes,
            edit,
            resume,
            false,
            cancelled,
        )
    }

    pub fn compare_replayed(
        &self,
        edit: &WorldCommand,
        cancelled: &AtomicBool,
    ) -> Result<NaturalRepairComparison, ComparisonError> {
        compare(
            &self.bytes,
            self.lead_in,
            self.window,
            &self.lanes,
            edit,
            false,
            true,
            cancelled,
        )
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct NaturalRepairComparison {
    pub control: NaturalTrafficReport,
    pub treatment: NaturalTrafficReport,
    pub receipt: ComparisonReceipt,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct NaturalTrafficReport {
    pub start_elapsed_tick: u64,
    pub end_elapsed_tick: u64,
    pub peak_stationary: usize,
    pub cutoff_stationary: usize,
    pub longest_stationary_ticks: u64,
    pub longest_sampled_spillback_ticks: u64,
    pub journeys_by_mode: BTreeMap<&'static str, TripMeasurement>,
    pub journeys_by_purpose: BTreeMap<TripPurpose, TripMeasurement>,
    pub directed_lane_entries_sampled: BTreeMap<String, u64>,
    pub terminal_hashes: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct TripMeasurement {
    pub completed: u64,
    pub duration_ticks: u64,
}

impl TripMeasurement {
    fn add(&mut self, stats: &TripModeStats) {
        self.completed += stats.completed;
        self.duration_ticks += stats.duration_ticks;
    }

    fn since(self, previous: Self) -> Self {
        Self {
            completed: self.completed.saturating_sub(previous.completed),
            duration_ticks: self.duration_ticks.saturating_sub(previous.duration_ticks),
        }
    }
}

fn by_mode(sim: &Simulation) -> BTreeMap<&'static str, TripMeasurement> {
    let stats = sim.read::<TripStats>();
    [
        ("walk", &stats.walk),
        ("car", &stats.car),
        ("transit", &stats.transit),
    ]
    .into_iter()
    .map(|(mode, stats)| {
        let mut total = TripMeasurement::default();
        total.add(stats);
        (mode, total)
    })
    .collect()
}

fn by_purpose(sim: &Simulation) -> BTreeMap<TripPurpose, TripMeasurement> {
    let mut totals = BTreeMap::<_, TripMeasurement>::new();
    for day in sim.read::<DailyEconomyMetrics>().days.values() {
        for (&purpose, stats) in &day.trips_by_purpose {
            totals.entry(purpose).or_default().add(stats);
        }
    }
    totals
}

#[derive(Default)]
struct TrafficSamples {
    still: BTreeMap<VehicleID, (u64, Vec3)>,
    lanes: BTreeMap<VehicleID, LaneID>,
    peak: usize,
    cutoff: usize,
    longest: u64,
    spillback: BTreeMap<TurnID, (u64, bool)>,
    longest_spillback: u64,
}

impl TrafficSamples {
    fn sample(
        &mut self,
        sim: &Simulation,
        tick: u64,
        watched: &BTreeMap<LaneID, String>,
        entries: &mut BTreeMap<String, u64>,
    ) {
        let vehicles = &sim.world().vehicles;
        self.still.retain(|id, _| vehicles.contains_key(*id));
        self.lanes.retain(|id, _| vehicles.contains_key(*id));
        self.cutoff = 0;
        for (id, vehicle) in vehicles.iter() {
            let active = vehicle.collider.is_some() && !vehicle.vehicle.state.is_parked();
            if !active
                || vehicle.speed.0.abs() > 0.1
                || self
                    .still
                    .get(&id)
                    .is_some_and(|(_, pos)| pos.distance(vehicle.trans.pos) > 0.5)
            {
                self.still.remove(&id);
            } else {
                let (since, _) = self.still.entry(id).or_insert((tick, vehicle.trans.pos));
                let duration = tick - *since;
                self.longest = self.longest.max(duration);
                self.cutoff += usize::from(duration >= STATIONARY_TICKS);
            }
            let lane = match vehicle.it.get_travers().map(|path| path.kind) {
                Some(TraverseKind::Lane(lane)) if active && watched.contains_key(&lane) => lane,
                _ => {
                    self.lanes.remove(&id);
                    continue;
                }
            };
            if self.lanes.insert(id, lane) != Some(lane) {
                *entries.get_mut(&watched[&lane]).unwrap() += 1;
            }
        }
        self.peak = self.peak.max(self.cutoff);
    }

    fn sample_spillback(&mut self, sim: &Simulation, tick: u64) {
        let map = sim.map();
        let seconds = sim.read::<prototypes::GameTime>().seconds;
        let mut entry_gaps = BTreeMap::<LaneID, f32>::new();
        let mut occupied_turns = Vec::new();
        for vehicle in sim.world().vehicles.values() {
            if vehicle.collider.is_none()
                || vehicle.vehicle.state.is_parked()
                || vehicle.speed.0.abs() > 0.1
            {
                continue;
            }
            let Some(path) = vehicle.it.get_travers() else {
                continue;
            };
            let pos = vehicle.trans.pos;
            let radius = vehicle.vehicle.kind.width() * 0.5;
            match path.kind {
                TraverseKind::Turn(turn) => occupied_turns.push((turn, radius)),
                TraverseKind::Lane(lane) => {
                    let Some(lane) = map.get(lane) else { continue };
                    if lane.points.first().is_close(pos, 24.0 + radius)
                        && lane.points.project_dist2(pos) <= (lane.kind.width() * 0.5).powi(2)
                    {
                        let gap = (lane.points.first().distance(pos) - radius).max(0.0);
                        entry_gaps
                            .entry(lane.id)
                            .and_modify(|nearest| *nearest = nearest.min(gap))
                            .or_insert(gap);
                    }
                }
            }
        }
        let mut current = BTreeMap::new();
        // A stop-line queue is a diagnostic candidate. Charge the soak gate
        // only while a turn is occupied and the receiving lane cannot fit it.
        for (turn, radius) in occupied_turns {
            if !entry_gaps
                .get(&turn.dst)
                .is_some_and(|&gap| gap < 2.0 * radius + 1.0)
            {
                continue;
            }
            let Some(source) = map.get(turn.src) else {
                continue;
            };
            let (since, permissive) = current
                .entry(turn)
                .or_insert_with(|| self.spillback.get(&turn).copied().unwrap_or((tick, false)));
            *permissive |= matches!(
                source.control.get_behavior(seconds),
                TrafficBehavior::GREEN | TrafficBehavior::STOP
            );
            if *permissive {
                self.longest_spillback = self.longest_spillback.max(tick - *since);
            }
        }
        self.spillback = current;
    }
}

/// Compare an inspector quote on a bounded capture at the simulation worker's boundary.
pub fn compare_captured_city(
    capture: SlotCapture,
    edit: &WorldCommand,
    cancelled: &AtomicBool,
) -> Result<NaturalRepairComparison, ComparisonError> {
    check_cancelled(cancelled)?;
    let bytes = capture
        .encode_with_limit(super::comparison::SNAPSHOT_LIMIT)
        .map_err(|error| ComparisonError::Snapshot(error.to_string()))?
        .into_snapshot_bytes();
    let sim = load_snapshot(&bytes)?;
    let map = sim.map();
    let mut lanes = BTreeMap::new();
    for (id, lane) in map.lanes().iter() {
        if lane.kind == LaneKind::Driving
            && match edit {
                WorldCommand::MapUpgradeRoad { road, .. } => lane.parent == *road,
                WorldCommand::MapSetJunctionRules { junction, .. } => lane.dst == *junction,
                _ => false,
            }
        {
            lanes.insert(id, format!("{id:?}"));
        }
    }
    drop(map);
    drop(sim);
    compare(
        &bytes,
        0,
        LIVE_WINDOW,
        &lanes,
        edit,
        false,
        false,
        cancelled,
    )
}

fn compare(
    bytes: &[u8],
    lead_in: u64,
    window: u64,
    lanes: &BTreeMap<LaneID, String>,
    edit: &WorldCommand,
    resume: bool,
    replay: bool,
    cancelled: &AtomicBool,
) -> Result<NaturalRepairComparison, ComparisonError> {
    if !matches!(
        edit,
        WorldCommand::MapUpgradeRoad { .. } | WorldCommand::MapSetJunctionRules { .. }
    ) {
        return Err(ComparisonError::InvalidSchedule(
            "a quoted road or junction edit is required",
        ));
    }
    let (control, _) = run_arm(
        bytes, lead_in, window, lanes, None, resume, replay, cancelled,
    )?;
    let (treatment, receipt) = run_arm(
        bytes,
        lead_in,
        window,
        lanes,
        Some(edit),
        resume,
        replay,
        cancelled,
    )?;
    Ok(NaturalRepairComparison {
        control,
        treatment,
        receipt: receipt
            .ok_or_else(|| ComparisonError::RepairDenied("missing edit receipt".into()))?,
    })
}

fn run_arm(
    bytes: &[u8],
    lead_in: u64,
    window: u64,
    watched: &BTreeMap<LaneID, String>,
    edit: Option<&WorldCommand>,
    resume: bool,
    replay: bool,
    cancelled: &AtomicBool,
) -> Result<(NaturalTrafficReport, Option<ComparisonReceipt>), ComparisonError> {
    check_cancelled(cancelled)?;
    let mut sim = load_snapshot(bytes)?;
    let mut schedule = Simulation::schedule();
    for _ in 0..lead_in {
        check_cancelled(cancelled)?;
        sim.tick(&mut schedule, &[]);
    }
    let start = elapsed_tick(&sim).0;
    let mut playback = replay
        .then(|| {
            SimulationReplayLoader::checkpoint_playback(
                Tick(sim.get_tick()),
                SnapshotRevision::new(1),
                window,
                edit.map(|command| (0, true, std::slice::from_ref(command))),
            )
            .map_err(|error| ComparisonError::Setup(error.to_string()))
        })
        .transpose()?;
    let mut budget = ReplayBudget::new(window as usize);
    let modes = by_mode(&sim);
    let purposes = by_purpose(&sim);
    let mut receipt = None;
    let mut samples = TrafficSamples::default();
    let mut entries = watched.values().map(|key| (key.clone(), 0)).collect();
    for step in 1..=window {
        check_cancelled(cancelled)?;
        let command = edit.filter(|_| step == 1);
        if let Some(playback) = &mut playback {
            if !matches!(
                playback.advance_one(&mut sim, &mut schedule, &mut budget),
                ReplayAdvance::Operation { advanced: true, .. }
            ) {
                return Err(ComparisonError::InvalidSchedule(
                    "unexpected replay operation",
                ));
            }
        } else {
            sim.tick(&mut schedule, command);
        }
        if let Some(edit) = command {
            receipt = Some(ComparisonReceipt::read(&sim, edit, 0)?);
        }
        if step % SAMPLE_TICKS == 0 || step == window {
            let tick = elapsed_tick(&sim).0;
            samples.sample(&sim, tick, watched, &mut entries);
            samples.sample_spillback(&sim, tick);
        }
        if resume && step == window / 2 {
            sim = load_snapshot(&snapshot_bytes(&sim)?)?;
        }
    }
    if let Some(playback) = &mut playback {
        if !matches!(
            playback.advance_one(&mut sim, &mut schedule, &mut budget),
            ReplayAdvance::Settled { complete: true }
        ) {
            return Err(ComparisonError::InvalidSchedule(
                "replay did not reach the scored cutoff",
            ));
        }
    }
    let report = NaturalTrafficReport {
        start_elapsed_tick: start,
        end_elapsed_tick: elapsed_tick(&sim).0,
        peak_stationary: samples.peak,
        cutoff_stationary: samples.cutoff,
        longest_stationary_ticks: samples.longest,
        longest_sampled_spillback_ticks: samples.longest_spillback,
        journeys_by_mode: by_mode(&sim)
            .into_iter()
            .map(|(mode, total)| (mode, total.since(modes[mode])))
            .collect(),
        journeys_by_purpose: by_purpose(&sim)
            .into_iter()
            .map(|(purpose, total)| {
                (
                    purpose,
                    total.since(purposes.get(&purpose).copied().unwrap_or_default()),
                )
            })
            .collect(),
        directed_lane_entries_sampled: entries,
        terminal_hashes: sim.hashes(),
    };
    Ok((report, receipt))
}

#[cfg(test)]
#[test]
fn p7_comparison_natural_city_runs_stock_schedule_and_replays() {
    use crate::builtin::traffic_repair::{prepare_repair_fixture, FixtureArm};

    let fixture = prepare_repair_fixture(FixtureArm::NoLoss).unwrap();
    assert!(!fixture.simulation.world().humans.is_empty());
    assert!(!fixture.simulation.world().companies.is_empty());
    let bytes = snapshot_bytes(&fixture.simulation).unwrap();
    let cancelled = AtomicBool::new(false);
    let watched = BTreeMap::new();
    let stock = compare(
        &bytes,
        0,
        250,
        &watched,
        &fixture.repair,
        false,
        false,
        &cancelled,
    )
    .unwrap();
    let replayed = compare(
        &bytes,
        0,
        250,
        &watched,
        &fixture.repair,
        true,
        true,
        &cancelled,
    )
    .unwrap();
    assert_eq!(stock, replayed);
    let mut sim = load_snapshot(&bytes).unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..250 {
        sim.tick(&mut schedule, &[]);
    }
    assert_eq!(stock.control.terminal_hashes, sim.hashes());
}
