//! Stock-schedule observations for a verified release or a detached live capture.
use super::comparison::{check_cancelled, load_snapshot, snapshot_bytes};
use super::staging::load_staged_slot;
use super::{ComparisonError, ComparisonReceipt, FixtureFingerprint};
use crate::citymap::LoopEvidence;
use geom::Vec3;
use prototypes::{game_secs_to_ticks, ItemID, ServiceKind, Tick, TICKS_PER_HOUR, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};
use simulation::diagnostics::elapsed_tick;
use simulation::economy::{
    cargo::CargoBook, DailyEconomyMetrics, TransitStats, TripModeStats, TripPurpose, TripStats,
    TICKS_PER_BUDGET_DAY,
};
use simulation::map::{
    BuildingID, IntersectionID, LaneID, LaneKind, RoadID, TrafficBehavior, TraverseDirection,
    TraverseKind, TurnID,
};
use simulation::map_dynamic::{
    BuildingInfos, CoverageGrid, ElectricityFlow, LineID, RoutingStep, Transit, UtilityFlow,
    UtilityStatus,
};
use simulation::souls::residence::Residences;
use simulation::transportation::road::movement_cycles_at_cutoff;
use simulation::transportation::Location;
use simulation::world_command::WorldCommand;
use simulation::{
    ReplayAdvance, ReplayBudget, Simulation, SimulationOptions, SimulationReplayLoader,
    SlotCapture, SnapshotRevision, SoulID, VehicleEnt, VehicleID,
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
    freight_receivers: Vec<PinnedReceiver>,
    #[serde(default)]
    bus_lines: BTreeMap<String, LineID>,
    #[serde(default)]
    services: BTreeSet<ServiceKind>,
    #[serde(default)]
    check_utilities: bool,
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

#[derive(Deserialize)]
struct PinnedReceiver {
    key: String,
    building: BuildingID,
    item: String,
    min_road_units: u64,
}

struct FreightReceiver {
    pin: PinnedReceiver,
    owner: SoulID,
    item: ItemID,
}

#[derive(Default)]
struct Observations {
    lanes: BTreeMap<LaneID, String>,
    corridor_directions: BTreeMap<String, Vec<String>>,
    freight_receivers: BTreeMap<String, FreightReceiver>,
    bus_lines: BTreeMap<String, LineID>,
    services: BTreeSet<ServiceKind>,
    check_utilities: bool,
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
    observe: Observations,
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
        if !map.uses_stock_traffic() {
            return Err("release traffic fixtures require stock junction controls".into());
        }
        let mut observe = Observations {
            bus_lines: schedule.bus_lines,
            services: schedule.services,
            check_utilities: schedule.check_utilities,
            ..Observations::default()
        };
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
                let key = format!("{}/{:?}/{:?}", corridor.key, direction.from, direction.lane);
                if map.get(direction.lane).is_none_or(|lane| {
                    lane.parent != road.id
                        || lane.kind != LaneKind::Driving
                        || lane.src != direction.from
                        || lane.dst != direction.to
                }) || observe.lanes.insert(direction.lane, key.clone()).is_some()
                {
                    return Err(format!("invalid directed lane on {}", corridor.key));
                }
                observe
                    .corridor_directions
                    .entry(format!("{}/{:?}", corridor.key, direction.from))
                    .or_default()
                    .push(key);
            }
        }
        let owners = sim.read::<BuildingInfos>();
        for pin in schedule.freight_receivers {
            let item = ItemID::new(&pin.item);
            let owner = owners
                .owner(pin.building)
                .ok_or_else(|| format!("missing freight receiver {}", pin.key))?;
            if map.get(pin.building).is_none()
                || prototypes::try_prototype(item).is_none()
                || pin.key.is_empty()
                || pin.min_road_units == 0
                || observe
                    .freight_receivers
                    .insert(pin.key.clone(), FreightReceiver { pin, item, owner })
                    .is_some()
            {
                return Err("invalid pinned freight receiver".into());
            }
        }
        let transit = sim.read::<Transit>();
        for (key, id) in &observe.bus_lines {
            if transit
                .lines
                .get(*id)
                .is_none_or(|line| line.stops.len() < 2 || line.suspended)
            {
                return Err(format!("missing or inactive pinned bus line {key}"));
            }
        }
        Ok(Self {
            fingerprint: fixture.fingerprint,
            gates: schedule.gates,
            bytes: inputs.snapshot,
            lead_in: schedule.lead_in_ticks,
            window: schedule.scored_ticks,
            observe,
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
        cancelled: &AtomicBool,
    ) -> Result<NaturalTrafficReport, ComparisonError> {
        self.run_soak(days, false, true, cancelled)
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
            &self.observe,
            None,
            resume,
            replay,
            cancelled,
        )
        .map(|(report, _)| report)
    }

    pub fn release_failures(&self, report: &NaturalTrafficReport) -> Vec<String> {
        let mut failures = Vec::new();
        for (failed, reason) in [
            (
                report.longest_stationary_ticks >= self.gates.max_unresolved_standstill_ticks,
                "standstill limit reached",
            ),
            (
                report.longest_sampled_spillback_ticks >= self.gates.max_spillback_ticks,
                "spillback limit reached",
            ),
            (
                report.cutoff_stationary > 0,
                "vehicles still stationary at cutoff",
            ),
            (
                !report.unresolved_movement_cycles.is_empty(),
                "unresolved vehicle movement cycles at cutoff",
            ),
            (
                !report.stranded_routed_drivers.is_empty(),
                "routed drivers stranded in parked vehicles at cutoff",
            ),
            (
                report.journeys_by_mode["car"].completed == 0,
                "no completed driving journey",
            ),
            (
                report
                    .journeys_by_purpose
                    .get(&TripPurpose::Work)
                    .is_none_or(|trips| trips.completed == 0),
                "no completed commute",
            ),
            (
                self.observe.freight_receivers.is_empty(),
                "no named freight receipt pin",
            ),
            (
                report.utility_failure_samples > 0,
                "occupied homes lost utility service",
            ),
        ] {
            if failed {
                failures.push(reason.into());
            }
        }
        for (key, lanes) in &self.observe.corridor_directions {
            if !lanes
                .iter()
                .any(|lane| report.directed_lane_crossings[lane] > 0)
            {
                failures.push(format!("no completed crossing on {key}"));
            }
        }
        for (key, receiver) in &self.observe.freight_receivers {
            if report.road_unloads_by_receiver[key].road_units < receiver.pin.min_road_units {
                failures.push(format!("no qualifying road unload at {key}"));
            }
        }
        for (key, activity) in &report.bus_activity {
            if activity.boardings == 0 || activity.ordered_loops == 0 {
                failures.push(format!("no boardings or ordered loop on bus line {key}"));
            }
        }
        for (service, &percent) in &report.minimum_service_coverage_percent {
            if percent < 90 {
                failures.push(format!("{service:?} coverage below 90 percent"));
            }
        }
        failures
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
            &self.observe,
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
            &self.observe,
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
    pub directed_lane_crossings: BTreeMap<String, u64>,
    pub stranded_routed_drivers: Vec<simulation::HumanID>,
    pub unresolved_movement_cycles: Vec<Vec<VehicleID>>,
    pub road_freight_unloaded_units: u64,
    pub road_unloads_by_receiver: BTreeMap<String, RoadUnloadMeasurement>,
    pub bus_activity: BTreeMap<String, BusActivity>,
    pub utility_samples: u64,
    pub utility_failure_samples: u64,
    pub minimum_service_coverage_percent: BTreeMap<ServiceKind, u64>,
    pub terminal_hashes: BTreeMap<String, u64>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RoadUnloadMeasurement {
    pub building: BuildingID,
    pub item: String,
    pub road_units: u64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct BusActivity {
    pub boardings: u64,
    pub ordered_loops: u64,
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

fn purpose_on_day(
    ledger: &DailyEconomyMetrics,
    day: u64,
) -> BTreeMap<TripPurpose, TripMeasurement> {
    ledger
        .days
        .get(&day)
        .into_iter()
        .flat_map(|day| day.trips_by_purpose.iter())
        .map(|(&purpose, stats)| {
            (
                purpose,
                TripMeasurement {
                    completed: stats.completed,
                    duration_ticks: stats.duration_ticks,
                },
            )
        })
        .collect()
}

fn journeys_by_purpose(
    ledger: &DailyEconomyMetrics,
    start_day: u64,
    end_day: u64,
    baseline: &BTreeMap<TripPurpose, TripMeasurement>,
) -> BTreeMap<TripPurpose, TripMeasurement> {
    let mut journeys = BTreeMap::<_, TripMeasurement>::new();
    for day in start_day..=end_day {
        for (purpose, total) in purpose_on_day(ledger, day) {
            let delta = if day == start_day {
                total.since(baseline.get(&purpose).copied().unwrap_or_default())
            } else {
                total
            };
            let score = journeys.entry(purpose).or_default();
            score.completed += delta.completed;
            score.duration_ticks += delta.duration_ticks;
        }
    }
    journeys
}

fn road_unloads(
    book: &CargoBook,
    observe: &Observations,
    day: u64,
) -> (u64, BTreeMap<String, u64>) {
    let report = book.freight_day(day);
    let named = observe
        .freight_receivers
        .iter()
        .map(|(key, receiver)| {
            let units = report.map_or(0, |day| {
                day.receipt(receiver.owner, receiver.item).road_units
            });
            (key.clone(), units)
        })
        .collect();
    (report.map_or(0, |day| day.delivered_units), named)
}

fn stranded_routed_drivers(sim: &Simulation) -> Vec<simulation::HumanID> {
    let world = sim.world();
    let mut stranded: Vec<_> = world.humans.iter().filter_map(|(id, human)| {
        let Location::Vehicle(car) = human.location else { return None };
        (matches!(human.router.cur_step(), Some(RoutingStep::DriveTo(target, _)) if *target == car)
            && world.vehicles.get(car).is_some_and(|vehicle| vehicle.vehicle.state.is_parked()))
            .then_some(id)
    }).collect();
    stranded.sort_unstable();
    stranded
}

#[derive(Default)]
struct CoverageSamples {
    utility_samples: u64,
    utility_failure_samples: u64,
    minimum_service_coverage_percent: BTreeMap<ServiceKind, u64>,
}

impl CoverageSamples {
    fn sample(&mut self, sim: &Simulation, observe: &Observations) {
        if !observe.check_utilities && observe.services.is_empty() {
            return;
        }
        let mut homes = BTreeMap::<_, u64>::new();
        for human in sim.world().humans.values() {
            *homes.entry(human.home.house).or_default() += 1;
        }
        let coverage = sim.read::<CoverageGrid>();
        let population: u64 = homes.values().sum();
        for &kind in &observe.services {
            let covered: u64 = homes
                .iter()
                .filter(|(house, _)| coverage.coverage(**house, kind) >= 0.5)
                .map(|(_, people)| people)
                .sum();
            let percent = (100 * covered).checked_div(population).unwrap_or(0);
            let minimum = self
                .minimum_service_coverage_percent
                .entry(kind)
                .or_insert(percent);
            *minimum = (*minimum).min(percent);
        }
        if observe.check_utilities {
            let map = sim.map();
            let utilities = sim.read::<UtilityFlow>();
            let electricity = sim.read::<ElectricityFlow>();
            let residences = sim.read::<Residences>();
            self.utility_samples += 1;
            let failed = homes.is_empty()
                || homes.keys().any(|&house| {
                    let power = map.electricity.net_id(house).is_some_and(|network| {
                        let flow = electricity.network_stats(network);
                        flow.produced_power.0 > 0 && !flow.blackout
                    });
                    !power
                        || utilities
                            .status(&map, &residences, house)
                            .is_none_or(|status| {
                                status.water != UtilityStatus::Served
                                    || status.sewage != UtilityStatus::Served
                            })
                });
            self.utility_failure_samples += u64::from(failed);
        }
    }
}

#[derive(Default)]
struct CorridorCrossings {
    probes: BTreeMap<VehicleID, (LaneID, bool)>,
    completed: BTreeMap<String, u64>,
}

impl CorridorCrossings {
    fn watch(&mut self, sim: &Simulation, lanes: &BTreeMap<LaneID, String>) {
        if lanes.is_empty() {
            return;
        }
        let map = sim.map();
        for (id, vehicle) in sim.world().vehicles.iter() {
            if self.probes.contains_key(&id) || vehicle.vehicle.state.is_parked() {
                continue;
            }
            if let Some(lane_id) = approaching_lane(vehicle, |id| {
                lanes.contains_key(&id)
                    && map
                        .get(id)
                        .is_some_and(|lane| lane.points.first().is_close(vehicle.trans.pos, 80.0))
            }) {
                self.probes.insert(id, (lane_id, false));
            }
        }
    }

    fn observe(&mut self, sim: &Simulation, lanes: &BTreeMap<LaneID, String>) {
        if self.probes.is_empty() {
            return;
        }
        let map = sim.map();
        self.probes.retain(|id, (lane_id, entered)| {
            let Some(vehicle) = sim.world().vehicles.get(*id) else {
                return false;
            };
            let Some(lane) = map.get(*lane_id) else {
                return false;
            };
            if vehicle.vehicle.state.is_parked() {
                return false;
            }
            let on_lane = vehicle.it.get_travers().is_some_and(|step| {
                step.dir == TraverseDirection::Forward && step.kind == TraverseKind::Lane(*lane_id)
            });
            if !*entered {
                *entered = on_lane && lane.points.first().is_close(vehicle.trans.pos, 1.5);
                return *entered || approaching_lane(vehicle, |id| id == *lane_id).is_some();
            }
            if on_lane && lane.points.last().is_close(vehicle.trans.pos, 1.5) {
                *self.completed.get_mut(&lanes[lane_id]).unwrap() += 1;
                return false;
            }
            on_lane
        });
    }
}

fn approaching_lane(
    vehicle: &VehicleEnt,
    mut watches: impl FnMut(LaneID) -> bool,
) -> Option<LaneID> {
    let route = vehicle.it.get_route()?;
    std::iter::once(&route.cur)
        .chain(route.reversed_route.iter().rev().take(2))
        .find_map(|step| match step.kind {
            TraverseKind::Lane(id) if step.dir == TraverseDirection::Forward && watches(id) => {
                Some(id)
            }
            _ => None,
        })
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
    let mut observe = Observations::default();
    for (id, lane) in map.lanes().iter() {
        if lane.kind == LaneKind::Driving
            && match edit {
                WorldCommand::MapUpgradeRoad { road, .. } => lane.parent == *road,
                WorldCommand::MapSetJunctionRules { junction, .. } => lane.dst == *junction,
                _ => false,
            }
        {
            observe.lanes.insert(id, format!("{id:?}"));
        }
    }
    drop(map);
    drop(sim);
    compare(
        &bytes,
        0,
        LIVE_WINDOW,
        &observe,
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
    observe: &Observations,
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
        bytes, lead_in, window, observe, None, resume, replay, cancelled,
    )?;
    let (treatment, receipt) = run_arm(
        bytes,
        lead_in,
        window,
        observe,
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
    observe: &Observations,
    edit: Option<&WorldCommand>,
    resume: bool,
    replay: bool,
    cancelled: &AtomicBool,
) -> Result<(NaturalTrafficReport, Option<ComparisonReceipt>), ComparisonError> {
    check_cancelled(cancelled)?;
    if resume && window < 2 {
        return Err(ComparisonError::InvalidSchedule(
            "checkpoint reload requires at least two scored ticks",
        ));
    }
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
    let start_day = start / TICKS_PER_BUDGET_DAY;
    let purposes = purpose_on_day(&sim.read(), start_day);
    let (initial_unloaded, initial_receipts) = road_unloads(&sim.read(), observe, start_day);
    let mut receipt = None;
    let mut samples = TrafficSamples::default();
    let lanes = &observe.lanes;
    let mut entries = lanes.values().map(|key| (key.clone(), 0)).collect();
    let mut corridors = CorridorCrossings {
        completed: lanes.values().map(|key| (key.clone(), 0)).collect(),
        ..CorridorCrossings::default()
    };
    corridors.watch(&sim, lanes);
    let mut loops = LoopEvidence::at_boundary(&sim.read::<Transit>());
    let boardings = sim.read::<TransitStats>().boardings.clone();
    let mut coverage = CoverageSamples::default();
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
            samples.sample(&sim, tick, lanes, &mut entries);
            samples.sample_spillback(&sim, tick);
            corridors.watch(&sim, lanes);
        }
        corridors.observe(&sim, lanes);
        if !observe.bus_lines.is_empty() {
            loops.observe(&sim.read::<Transit>());
        }
        if step % (20 * TICKS_PER_MINUTE) == 0 || step == window {
            coverage.sample(&sim, observe);
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
    let end = elapsed_tick(&sim).0;
    let end_day = end / TICKS_PER_BUDGET_DAY;
    let mut unloaded = 0;
    let mut receipts: BTreeMap<_, _> = observe
        .freight_receivers
        .keys()
        .map(|key| (key.clone(), 0))
        .collect();
    {
        let book = sim.read::<CargoBook>();
        for day in start_day..=end_day {
            let (units, named) = road_unloads(&book, observe, day);
            unloaded += units;
            for (key, units) in named {
                *receipts.get_mut(&key).unwrap() += units;
            }
        }
    }
    let road_unloads_by_receiver = observe
        .freight_receivers
        .iter()
        .map(|(key, receiver)| {
            (
                key.clone(),
                RoadUnloadMeasurement {
                    building: receiver.pin.building,
                    item: receiver.pin.item.clone(),
                    road_units: receipts[key].saturating_sub(initial_receipts[key]),
                },
            )
        })
        .collect();
    let transit = sim.read::<TransitStats>();
    let bus_activity = observe
        .bus_lines
        .iter()
        .map(|(key, id)| {
            (
                key.clone(),
                BusActivity {
                    boardings: transit
                        .boardings
                        .get(id)
                        .copied()
                        .unwrap_or_default()
                        .saturating_sub(boardings.get(id).copied().unwrap_or_default()),
                    ordered_loops: loops.loops.get(id).copied().unwrap_or_default(),
                },
            )
        })
        .collect();
    let report = NaturalTrafficReport {
        start_elapsed_tick: start,
        end_elapsed_tick: end,
        peak_stationary: samples.peak,
        cutoff_stationary: samples.cutoff,
        longest_stationary_ticks: samples.longest,
        longest_sampled_spillback_ticks: samples.longest_spillback,
        journeys_by_mode: by_mode(&sim)
            .into_iter()
            .map(|(mode, total)| (mode, total.since(modes[mode])))
            .collect(),
        journeys_by_purpose: journeys_by_purpose(&sim.read(), start_day, end_day, &purposes),
        directed_lane_entries_sampled: entries,
        directed_lane_crossings: corridors.completed,
        stranded_routed_drivers: stranded_routed_drivers(&sim),
        unresolved_movement_cycles: movement_cycles_at_cutoff(&sim),
        road_freight_unloaded_units: unloaded.saturating_sub(initial_unloaded),
        road_unloads_by_receiver,
        bus_activity,
        utility_samples: coverage.utility_samples,
        utility_failure_samples: coverage.utility_failure_samples,
        minimum_service_coverage_percent: coverage.minimum_service_coverage_percent,
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
    let observe = Observations::default();
    let stock = compare(
        &bytes,
        0,
        250,
        &observe,
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
        &observe,
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

#[cfg(test)]
#[test]
fn p7_comparison_resume_requires_a_tick_after_checkpoint() {
    use crate::builtin::traffic_repair::{prepare_repair_fixture, FixtureArm};

    let fixture = prepare_repair_fixture(FixtureArm::NoLoss).unwrap();
    let bytes = snapshot_bytes(&fixture.simulation).unwrap();
    let observe = Observations::default();
    let cancelled = AtomicBool::new(false);
    let run = |window, resume| {
        compare(
            &bytes,
            0,
            window,
            &observe,
            &fixture.repair,
            resume,
            false,
            &cancelled,
        )
    };
    assert!(run(1, false).is_ok());
    assert!(matches!(
        run(1, true),
        Err(ComparisonError::InvalidSchedule(_))
    ));
    assert_eq!(run(2, false).unwrap(), run(2, true).unwrap());
}

#[cfg(test)]
#[test]
fn p7_comparison_purpose_rollover_excludes_expired_history() {
    let mut ledger = DailyEconomyMetrics::default();
    let purpose = TripPurpose::Work;
    for _ in 0..5 {
        ledger.record_arrival(0, purpose, 80);
    }
    ledger.record_arrival(14, purpose, 100);
    let before = purpose_on_day(&ledger, 14);
    ledger.record_arrival(14, purpose, 40);
    ledger.retain_recent(15);
    ledger.record_arrival(15, purpose, 90);
    let delta = journeys_by_purpose(&ledger, 14, 15, &before)[&purpose];
    assert_eq!(
        delta,
        TripMeasurement {
            completed: 2,
            duration_ticks: 130
        }
    );
}
