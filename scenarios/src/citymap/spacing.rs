//! Controlled pitch, lane count and junction-control calibration.

use super::traffic::{
    scored_delay, CutoffQueues, LaneOffset, Occupancy, TrafficCollector, TripRecord, TripSpec,
    TripStatus,
};
use geom::{vec2, Vec2};
use prototypes::{TICKS_PER_DAY, TICKS_PER_HOUR, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};
use simulation::map::{
    Density, IntersectionID, LaneID, LaneKind, LanePatternBuilder, LightPolicy, Lot, LotLayout,
    Map, MapProject, RoadID, TrafficBehavior, TurnPolicy, ZoneCell, ZoneKind,
};
use simulation::souls::growth::check_growable_lot;
use simulation::transportation::VehicleKind;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::Simulation;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SEED: u64 = 6_042;
const STARTS: [u64; 2] = [TICKS_PER_HOUR, 9 * TICKS_PER_HOUR];
const PEAK_TICKS: u64 = 2 * TICKS_PER_HOUR;
const PORTS: [&str; 6] = ["west", "east", "north", "south", "west_south", "east_north"];
const OD: [(usize, usize); 12] = [
    (0, 1),
    (1, 0),
    (2, 3),
    (3, 2),
    (0, 2),
    (1, 3),
    (2, 0),
    (3, 1),
    (0, 3),
    (1, 2),
    (2, 1),
    (3, 0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum Topology {
    Orthogonal,
    StaggeredT,
    CurvedCollector,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum Control {
    Auto,
    StopSigns,
    Signals,
    Uncontrolled,
}

impl Control {
    const TESTED: [Self; 3] = [Self::Auto, Self::StopSigns, Self::Signals];

    fn policy(self) -> LightPolicy {
        match self {
            Self::Auto => LightPolicy::Auto,
            Self::StopSigns => LightPolicy::StopSigns,
            Self::Signals => LightPolicy::Lights,
            Self::Uncontrolled => LightPolicy::NoLights,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum RoadClass {
    Collector,
    Local,
    Arterial,
}

impl RoadClass {
    fn lane_floor(self) -> f32 {
        match self {
            // The CM1 limits already used by the authored graph.
            Self::Local => super::SpacingLimits::default().local_lane,
            Self::Collector => super::SpacingLimits::default().collector_lane,
            Self::Arterial => super::SpacingLimits::default().arterial_lane,
        }
    }

    fn speed(self) -> f32 {
        match self {
            Self::Local => 9.0,
            Self::Collector => 11.1,
            Self::Arterial => 13.9,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum Load {
    Nominal,
    Overload,
}

impl Load {
    fn trips_per_hour(self) -> u64 {
        match self {
            Self::Nominal => 120,
            Self::Overload => 150,
        }
    }

    fn cutoff_minutes(self) -> u64 {
        match self {
            Self::Nominal => 30,
            Self::Overload => 45,
        }
    }

    fn completion_percent(self) -> u32 {
        match self {
            Self::Nominal => 95,
            Self::Overload => 90,
        }
    }

    fn delay_minutes(self) -> u64 {
        match self {
            Self::Nominal => 20,
            Self::Overload => 35,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
enum DemandPattern {
    MatchedFourPorts,
    AllApproaches,
}

impl DemandPattern {
    fn port_count(self) -> usize {
        match self {
            Self::MatchedFourPorts => 4,
            Self::AllApproaches => 6,
        }
    }

    fn pairs(self) -> Vec<(usize, usize)> {
        let mut pairs = OD.to_vec();
        if self == Self::AllApproaches {
            pairs.extend((0..PORTS.len()).flat_map(|from| {
                (0..PORTS.len())
                    .filter(move |&to| from != to && (from >= 4 || to >= 4))
                    .map(move |to| (from, to))
            }));
        }
        pairs
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct Case {
    topology: Topology,
    demand_pattern: DemandPattern,
    pitch_m: u32,
    lanes_per_direction: u32,
    control: Control,
    road_class: RoadClass,
    load: Load,
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for load in [Load::Nominal, Load::Overload] {
        for topology in [
            Topology::Orthogonal,
            Topology::StaggeredT,
            Topology::CurvedCollector,
        ] {
            for pitch_m in [80, 100, 120, 140, 160] {
                for lanes_per_direction in [1, 2] {
                    for control in Control::TESTED {
                        cases.push(Case {
                            topology,
                            demand_pattern: DemandPattern::MatchedFourPorts,
                            pitch_m,
                            lanes_per_direction,
                            control,
                            road_class: RoadClass::Collector,
                            load,
                        });
                    }
                }
            }
        }
        // Supplement the factorial with class-specific speeds and parking.
        // Uncontrolled T junctions test the geometry option using stock rules.
        for pitch_m in [60, 80, 100, 120, 160] {
            for control in [Control::Auto, Control::StopSigns, Control::Signals] {
                cases.push(Case {
                    topology: Topology::Orthogonal,
                    demand_pattern: DemandPattern::MatchedFourPorts,
                    pitch_m,
                    lanes_per_direction: 1,
                    control,
                    road_class: RoadClass::Local,
                    load,
                });
            }
            cases.push(Case {
                topology: Topology::StaggeredT,
                demand_pattern: DemandPattern::MatchedFourPorts,
                pitch_m,
                lanes_per_direction: 1,
                control: Control::Auto,
                road_class: RoadClass::Local,
                load,
            });
        }
        for pitch_m in [120, 160, 200, 240] {
            cases.push(Case {
                topology: Topology::StaggeredT,
                demand_pattern: DemandPattern::MatchedFourPorts,
                pitch_m,
                lanes_per_direction: 1,
                control: Control::Uncontrolled,
                road_class: RoadClass::Collector,
                load,
            });
            for lanes_per_direction in [1, 2] {
                for control in Control::TESTED {
                    for road_class in [RoadClass::Collector, RoadClass::Arterial] {
                        // Collector 120/160 m already runs in the factorial.
                        if road_class == RoadClass::Collector && pitch_m < 200 {
                            continue;
                        }
                        cases.push(Case {
                            topology: Topology::CurvedCollector,
                            demand_pattern: DemandPattern::MatchedFourPorts,
                            pitch_m,
                            lanes_per_direction,
                            control,
                            road_class,
                            load,
                        });
                    }
                }
            }
        }
        for topology in [Topology::Orthogonal, Topology::CurvedCollector] {
            for pitch_m in [140, 160] {
                for lanes_per_direction in [1, 2] {
                    for control in Control::TESTED {
                        cases.push(Case {
                            topology,
                            demand_pattern: DemandPattern::AllApproaches,
                            pitch_m,
                            lanes_per_direction,
                            control,
                            road_class: RoadClass::Collector,
                            load,
                        });
                    }
                }
            }
        }
    }
    cases
}

struct Fixture {
    sim: Simulation,
    incoming: Vec<LaneID>,
    outgoing: Vec<LaneID>,
    geometry: Geometry,
}

#[derive(Serialize)]
struct LaneGeometry {
    id: LaneID,
    towards_east: bool,
    length_m: f32,
    entries: u32,
}

#[derive(Serialize)]
struct Geometry {
    actual_junction_chord_m: f32,
    centreline_arc_m: f32,
    road_width_m: f32,
    min_spine_lane_m: f32,
    spine_lanes: Vec<LaneGeometry>,
    frontage: Frontage,
    spine_frontage: Frontage,
    approach_controls: [ControlCounts; 2],
    approach_road_entries: [Vec<ApproachRoad>; 2],
}

const FRONTAGE_ZONE: ZoneCell = ZoneCell {
    kind: ZoneKind::Residential,
    density: Density::Low,
};

#[derive(Default, Serialize)]
struct Frontage {
    generated: usize,
    eligible: usize,
}

impl Frontage {
    fn count<'a>(map: &Map, lots: impl Iterator<Item = &'a Lot>) -> Self {
        let mut counts = Self::default();
        for lot in lots {
            counts.generated += 1;
            counts.eligible += usize::from(check_growable_lot(map, lot, FRONTAGE_ZONE).is_ok());
        }
        counts
    }
}

#[derive(Serialize)]
struct ApproachRoad {
    road: RoadID,
    #[serde(skip)]
    inbound: Vec<LaneID>,
    entries: u32,
}

#[derive(Default, Serialize)]
struct ControlCounts {
    always: usize,
    stops: usize,
    signals: usize,
    opposing_phases_valid: bool,
}

fn build_fixture(case: Case) -> Result<Fixture, String> {
    let mut sim = Simulation::new_authored_flat(3, SEED).map_err(|error| error.to_string())?;
    sim.write::<Map>().set_lot_layout(LotLayout::Uniform);
    let pitch = case.pitch_m as f32;
    let a = vec2(620.0, 740.0);
    // Hold the chord exact even with the diagonal displacement and elbow.
    let b = a + if case.topology == Topology::CurvedCollector {
        vec2(0.988_686, 0.15) * pitch
    } else {
        vec2(pitch, 0.0)
    };
    let elbow = (case.topology == Topology::CurvedCollector)
        .then_some((a + b) * 0.5 + vec2(0.0, pitch * 0.18));
    // Point indices 2–5 are the matched ports. Indices 6 and 7 also carry
    // demand in the all-approach comparison. The staggered T omits them.
    let points = [
        a,
        b,
        a - vec2(128.0, 0.0),
        b + vec2(128.0, 0.0),
        a + vec2(0.0, 128.0),
        b - vec2(0.0, 128.0),
        a - vec2(0.0, 128.0),
        b + vec2(0.0, 128.0),
    ];
    let links: &[(usize, usize)] = if case.topology == Topology::StaggeredT {
        &[(0, 1), (2, 0), (1, 3), (4, 0), (1, 5)]
    } else {
        &[(0, 1), (2, 0), (1, 3), (4, 0), (1, 5), (0, 6), (7, 1)]
    };
    let pattern = LanePatternBuilder::new()
        .n_lanes(case.lanes_per_direction)
        .speed_limit(case.road_class.speed())
        .sidewalks(true)
        .parking(case.road_class == RoadClass::Local)
        .build();
    let projects = {
        let map = sim.map();
        points
            .iter()
            .map(|&p| {
                map.environment()
                    .height(p)
                    .map(|height| MapProject::ground(p.z(height)))
                    .ok_or("fixture endpoint outside the terrain")
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let connections = links
        .iter()
        .enumerate()
        .map(|(i, &(a, b))| (a, b, if i == 0 { elbow } else { None }, pattern.clone()))
        .collect();
    sim.apply_commands([&WorldCommand::MapMakeMultipleConnections(
        projects,
        connections,
    )]);
    {
        let outcomes = sim.read::<CommandOutcomes>();
        if outcomes.accepted != [0]
            || outcomes
                .link_outcomes(0)
                .iter()
                .copied()
                .ne((0..links.len()).map(|i| (i, Ok(()))))
        {
            return Err(format!("fixture construction failed: {:?}", outcomes.last));
        }
    }
    let (centres, port_ids, spine) = {
        let map = sim.map();
        if map.roads().len() != links.len() || map.intersections().len() != links.len() + 1 {
            return Err("fixture changed shape during construction".into());
        }
        let id_at = |pos: Vec2| {
            map.intersections()
                .values()
                .find(|i| i.pos.xy().distance2(pos) < 0.01)
                .map(|i| i.id)
                .ok_or_else(|| format!("missing fixture intersection at {pos:?}"))
        };
        let centres = [id_at(a)?, id_at(b)?];
        let port_ids = points[2..2 + case.demand_pattern.port_count()]
            .iter()
            .copied()
            .map(id_at)
            .collect::<Result<Vec<_>, _>>()?;
        let spine = map
            .roads()
            .iter()
            .find(|(_, road)| {
                [road.src, road.dst].contains(&centres[0])
                    && [road.src, road.dst].contains(&centres[1])
            })
            .map(|(id, _)| id)
            .ok_or("missing fixture spine")?;
        (centres, port_ids, spine)
    };
    let policies = centres.map(|inter| WorldCommand::MapUpdateIntersectionPolicy {
        inter,
        turn: TurnPolicy::default(),
        light: case.control.policy(),
    });
    sim.apply_commands(&policies);
    if sim.read::<CommandOutcomes>().accepted != [0, 1] {
        return Err("fixture intersection policies were rejected".into());
    }
    let (incoming, outgoing, geometry) = {
        let map = sim.map();
        let select_lane = |port, enters: bool| {
            map.lanes()
                .values()
                .filter(|lane| {
                    lane.kind == LaneKind::Driving
                        && if enters {
                            lane.src == port
                        } else {
                            lane.dst == port
                        }
                })
                .map(|lane| lane.id)
                .min()
                .ok_or("missing directed port lane")
        };
        let mut incoming = Vec::with_capacity(port_ids.len());
        let mut outgoing = Vec::with_capacity(port_ids.len());
        for port in port_ids {
            incoming.push(select_lane(port, true)?);
            outgoing.push(select_lane(port, false)?);
        }
        let spine_lanes: Vec<_> = map
            .lanes()
            .values()
            .filter(|lane| lane.kind == LaneKind::Driving && lane.parent == spine)
            .map(|lane| LaneGeometry {
                id: lane.id,
                towards_east: lane.dst == centres[1],
                length_m: lane.points.length(),
                entries: 0,
            })
            .collect();
        if spine_lanes.len() != 2 * case.lanes_per_direction as usize {
            return Err("unexpected live spine lane count".into());
        }
        let min_spine_lane_m = spine_lanes
            .iter()
            .map(|lane| lane.length_m)
            .fold(f32::INFINITY, f32::min);
        let road = &map.roads()[spine];
        let chord = a.distance(b);
        if case.topology != Topology::CurvedCollector && (road.length() - chord).abs() > 0.01 {
            return Err(format!(
                "straight fixture spine is not level: arc {:.2} m, chord {chord:.2} m",
                road.length()
            ));
        }
        let geometry = Geometry {
            actual_junction_chord_m: chord,
            centreline_arc_m: road.length(),
            road_width_m: pattern.width(),
            min_spine_lane_m,
            spine_lanes,
            frontage: Frontage::count(&map, map.lots().values()),
            spine_frontage: Frontage::count(
                &map,
                map.lots().values().filter(|lot| lot.parent == spine),
            ),
            approach_controls: centres.map(|id| control_counts(&map, id, spine)),
            approach_road_entries: centres.map(|id| {
                let mut approaches = BTreeMap::<_, Vec<_>>::new();
                for lane in map
                    .lanes()
                    .values()
                    .filter(|l| l.kind == LaneKind::Driving && l.dst == id)
                {
                    approaches.entry(lane.parent).or_default().push(lane.id);
                }
                approaches
                    .into_iter()
                    .map(|(road, inbound)| ApproachRoad {
                        road,
                        inbound,
                        entries: 0,
                    })
                    .collect()
            }),
        };
        (incoming, outgoing, geometry)
    };
    Ok(Fixture {
        sim,
        incoming,
        outgoing,
        geometry,
    })
}

/// For stock lights, check that each incoming road shares its green phase
/// with exactly one geometrically opposing road.
fn control_counts(map: &Map, id: IntersectionID, spine: RoadID) -> ControlCounts {
    let mut counts = ControlCounts {
        opposing_phases_valid: true,
        ..ControlCounts::default()
    };
    let mut roads = Vec::new();
    for lane in map
        .lanes()
        .values()
        .filter(|l| l.kind == LaneKind::Driving && l.dst == id)
    {
        if lane.control.is_light() {
            counts.signals += 1;
            if !roads.iter().any(|&(road, _, _)| road == lane.parent) {
                roads.push((
                    lane.parent,
                    map.roads()[lane.parent].dir_from(id),
                    lane.control,
                ));
            }
        } else if lane.control.is_stop_sign() {
            counts.stops += 1;
        } else {
            counts.always += 1;
        }
    }
    if !roads.is_empty() {
        counts.opposing_phases_valid = roads.len() == 4 && roads.iter().any(|&(road, _, _)| road == spine)
            && roads.iter().enumerate().all(|(i, &(_, dir, control))| {
                let same_phase: Vec<_> = roads.iter().enumerate().filter(|&(j, &(_, _, other))| {
                    i != j && (0..14 * 60).any(|s| matches!(
                        (control.get_behavior(s), other.get_behavior(s)),
                        (TrafficBehavior::GREEN, TrafficBehavior::GREEN)
                    ))
                }).collect();
                matches!(same_phase.as_slice(), [(_, (_, other_dir, _))] if dir.dot(*other_dir) < -0.8)
            });
    }
    counts
}

#[derive(Clone, Copy)]
struct Release {
    tick: u64,
    peak: usize,
    od: usize,
    mode: usize,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OdDemand {
    from: String,
    to: String,
    district_trips: u32,
    gateway_trips: u32,
}

impl OdDemand {
    fn total(&self) -> u64 {
        u64::from(self.district_trips) + u64::from(self.gateway_trips)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DemandEnvelope {
    version: u32,
    revision: String,
    district_source: String,
    gateway_source: String,
    matched_four_ports: [Vec<OdDemand>; 2],
    all_approaches: [Vec<OdDemand>; 2],
}

impl DemandEnvelope {
    fn parse(reader: impl io::Read, nominal: bool) -> Result<Self, String> {
        let mut envelope: Self = serde_json::from_reader(reader).map_err(|err| err.to_string())?;
        if envelope.version != 1 {
            return Err(format!(
                "unsupported demand envelope version {}",
                envelope.version
            ));
        }
        if [
            &envelope.revision,
            &envelope.district_source,
            &envelope.gateway_source,
        ]
        .into_iter()
        .any(|value| value.trim().is_empty())
        {
            return Err("demand envelope requires revision and both source references".into());
        }
        for (pattern, peaks) in [
            (
                DemandPattern::MatchedFourPorts,
                &mut envelope.matched_four_ports,
            ),
            (DemandPattern::AllApproaches, &mut envelope.all_approaches),
        ] {
            let pairs = pattern.pairs();
            for (peak, rows) in peaks.iter_mut().enumerate() {
                let mut by_pair = BTreeMap::new();
                for row in std::mem::take(rows) {
                    let pair = (row.from.clone(), row.to.clone());
                    if by_pair.insert(pair, row).is_some() {
                        return Err(format!("duplicate OD in {pattern:?} peak {peak}"));
                    }
                }
                for &(from, to) in &pairs {
                    let name = (PORTS[from].to_owned(), PORTS[to].to_owned());
                    let row = by_pair.remove(&name).ok_or_else(|| {
                        format!(
                            "missing OD {} to {} in {pattern:?} peak {peak}",
                            name.0, name.1
                        )
                    })?;
                    rows.push(row);
                }
                if !by_pair.is_empty() {
                    return Err(format!("unexpected OD in {pattern:?} peak {peak}"));
                }
                if nominal
                    && (rows.iter().any(|row| row.total() == 0)
                        || rows.iter().map(OdDemand::total).sum::<u64>() > PEAK_TICKS * 4 / 5)
                {
                    return Err(format!("{pattern:?} peak {peak} requires positive demand on every OD and room for overload in the tick schedule"));
                }
            }
        }
        Ok(envelope)
    }

    fn peaks(&self, pattern: DemandPattern) -> &[Vec<OdDemand>; 2] {
        match pattern {
            DemandPattern::MatchedFourPorts => &self.matched_four_ports,
            DemandPattern::AllApproaches => &self.all_approaches,
        }
    }
}

#[derive(Default)]
struct DemandBasis {
    nominal: Option<DemandEnvelope>,
    observed: Option<DemandEnvelope>,
}

impl DemandBasis {
    fn observed_within_nominal(&self, pattern: DemandPattern) -> Option<[bool; 2]> {
        let nominal = self.nominal.as_ref()?.peaks(pattern);
        let observed = self.observed.as_ref()?.peaks(pattern);
        Some(std::array::from_fn(|peak| {
            nominal[peak]
                .iter()
                .zip(&observed[peak])
                .all(|(limit, actual)| {
                    actual.district_trips <= limit.district_trips
                        && actual.gateway_trips <= limit.gateway_trips
                })
        }))
    }
}

// Largest-remainder apportionment with stable ties in input order.
fn apportion(weights: &[u64], total: u64) -> Vec<u64> {
    let sum: u64 = weights.iter().sum();
    let mut counts: Vec<_> = weights.iter().map(|w| w * total / sum).collect();
    let mut order: Vec<_> = (0..weights.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(weights[index] * total % sum));
    let assigned: u64 = counts.iter().sum();
    for index in order.into_iter().take((total - assigned) as usize) {
        counts[index] += 1;
    }
    counts
}

fn demand(basis: &DemandBasis, pattern: DemandPattern, load: Load, start: u64) -> Vec<Release> {
    let mut releases = Vec::new();
    let pair_count = pattern.pairs().len();
    for (peak, first) in STARTS.into_iter().enumerate() {
        let counts = if let Some(envelope) = &basis.nominal {
            let nominal: Vec<_> = envelope.peaks(pattern)[peak]
                .iter()
                .map(OdDemand::total)
                .collect();
            if load == Load::Nominal {
                nominal
            } else {
                let total = nominal.iter().sum::<u64>();
                // Keep every nominal trip and apportion the rounded 25% increment.
                let extra = apportion(&nominal, (total + 2) / 4);
                nominal
                    .into_iter()
                    .zip(extra)
                    .map(|(base, extra)| base + extra)
                    .collect()
            }
        } else {
            vec![load.trips_per_hour() * 2 / pair_count as u64; pair_count]
        };
        let mut slots = Vec::new();
        for (od, count) in counts.into_iter().enumerate() {
            for ordinal in 0..count {
                // Interleave unequal cohorts evenly across the peak.
                slots.push(((2 * ordinal + 1) * PEAK_TICKS / (2 * count), od, ordinal));
            }
        }
        slots.sort_unstable();
        let total = slots.len() as u64;
        let target = apportion(&[80, 15, 5], total);
        let mut modes = [0; 3];
        let first_release = releases.len();
        for (index, (_, od, ordinal)) in slots.into_iter().enumerate() {
            let mix = (ordinal as usize + od * 7 + peak * 3) % 20;
            let mode = if mix < 16 {
                0
            } else if mix < 19 {
                1
            } else {
                2
            };
            modes[mode] += 1;
            releases.push(Release {
                tick: start + first + index as u64 * PEAK_TICKS / total,
                peak,
                od,
                mode,
            });
        }
        // Only replace assignments in surplus modes, working back from the last slot.
        for release in releases[first_release..].iter_mut().rev() {
            if modes[release.mode] > target[release.mode] {
                if let Some(missing) = (0..MODES.len()).find(|&m| modes[m] < target[m]) {
                    modes[release.mode] -= 1;
                    modes[missing] += 1;
                    release.mode = missing;
                }
            }
        }
    }
    releases
}

const MODES: [VehicleKind; 3] = [VehicleKind::Car, VehicleKind::Truck, VehicleKind::Bus];
const ORIGIN_M: f32 = 20.0;
const DESTINATION_M: f32 = 38.0;

#[derive(Default, Serialize)]
struct Counts {
    offered: u32,
    admitted: u32,
    completed: u32,
}

#[derive(Default, Serialize)]
struct Outcomes {
    planned: u32,
    waiting: u32,
    pending_active: u32,
    rejected: u32,
    lost: u32,
}

#[derive(Serialize)]
struct Cohort {
    from: &'static str,
    to: &'static str,
    peak: usize,
    by_mode: [Counts; 3],
    by_mode_outcomes: [Outcomes; 3],
    by_mode_completed_by_cutoff: [u32; 3],
    completed_by_cutoff: u32,
    p95_delay_ticks: u64,
    p95_trip_ticks: Option<u64>,
    p95_entry_wait_ticks: Option<u64>,
}

#[derive(Serialize)]
struct Peak {
    cutoff_tick: u64,
    cutoff_queues: CutoffQueues,
    offered: u32,
    completed_by_cutoff: u32,
    completion_pass: bool,
    all_directions_served: bool,
    od_delay_pass: bool,
}

#[derive(Serialize)]
struct Gates {
    traffic_pass: bool,
    storage_pass: bool,
    phase_pass: bool,
    bidirectional_spine_pass: Option<bool>,
    all_approaches_served: Option<bool>,
    eligible: bool,
    spillback_pass: bool,
    overload_queue_cleared: Option<bool>,
}

#[derive(Serialize)]
struct CaseReport {
    case: Case,
    start_tick: u64,
    totals: Counts,
    outcomes: Outcomes,
    completed_per_hour: f64,
    hourly: Vec<Counts>,
    peaks: Vec<Peak>,
    cohorts: Vec<Cohort>,
    first_failure: Option<&'static str>,
    directed_turns_used: usize,
    directed_turn_entries: u32,
    occupancy: Occupancy,
    geometry: Geometry,
    gates: Gates,
}

fn p95(samples: &mut [u64]) -> Option<u64> {
    samples.sort_unstable();
    samples
        .get((samples.len() * 95).div_ceil(100).saturating_sub(1))
        .copied()
}

fn tally(counts: &mut Counts, outcomes: &mut Outcomes, trip: &TripRecord) {
    counts.offered += u32::from(trip.status != TripStatus::Planned);
    counts.admitted += u32::from(trip.admitted_tick.is_some());
    counts.completed += u32::from(trip.arrival_tick.is_some());
    match trip.status {
        TripStatus::Planned => outcomes.planned += 1,
        TripStatus::Waiting => outcomes.waiting += 1,
        TripStatus::Active => outcomes.pending_active += 1,
        TripStatus::Rejected => outcomes.rejected += 1,
        TripStatus::Lost => outcomes.lost += 1,
        TripStatus::Arrived => {}
    }
}

fn run_case(
    case: Case,
    basis: &DemandBasis,
    instrument: bool,
) -> Result<(CaseReport, Duration, u64), String> {
    let Fixture {
        mut sim,
        incoming,
        outgoing,
        mut geometry,
    } = build_fixture(case)?;
    let start = sim.get_tick();
    let pairs = case.demand_pattern.pairs();
    let releases = demand(basis, case.demand_pattern, case.load, start);
    let mut collector = TrafficCollector::new(
        releases
            .iter()
            .enumerate()
            .map(|(key, release)| {
                let (from, to) = pairs[release.od];
                TripSpec {
                    key: key as u32,
                    departure_tick: release.tick,
                    vehicle_kind: MODES[release.mode],
                    origin_lane_offset: LaneOffset {
                        lane: incoming[from],
                        metres: ORIGIN_M,
                    },
                    destination_lane_offset: LaneOffset {
                        lane: outgoing[to],
                        metres: DESTINATION_M,
                    },
                }
            })
            .collect(),
        instrument,
    )?;
    let mut schedule = Simulation::schedule();
    let mut cutoff_queues = [None; 2];
    let cutoffs: [_; 2] = std::array::from_fn(|peak| {
        let last_release = releases
            .iter()
            .rfind(|release| release.peak == peak)
            .unwrap()
            .tick;
        last_release + case.load.cutoff_minutes() * TICKS_PER_MINUTE
    });
    let begin = Instant::now();
    for _ in 0..TICKS_PER_DAY {
        collector.release(&mut sim);
        sim.tick(&mut schedule, []);
        collector.observe(&mut sim);
        if let Some(peak) = cutoffs.iter().position(|&tick| tick == sim.get_tick()) {
            cutoff_queues[peak] = Some(collector.cutoff_queues(&sim));
        }
    }
    let elapsed = begin.elapsed();
    let (records, occupancy) = collector.finish();
    for lane in &mut geometry.spine_lanes {
        lane.entries = occupancy
            .directed_lane_entries
            .get(&lane.id)
            .copied()
            .unwrap_or_default();
    }
    for approach in geometry.approach_road_entries.iter_mut().flatten() {
        approach.entries = approach
            .inbound
            .iter()
            .filter_map(|lane| occupancy.directed_lane_entries.get(lane))
            .sum();
    }
    let mut totals = Counts::default();
    let mut outcomes = Outcomes::default();
    let mut hourly: Vec<_> = (0..24).map(|_| Counts::default()).collect();
    let last_hour = hourly.len() - 1;
    let hour = |tick| (((tick - start) / TICKS_PER_HOUR) as usize).min(last_hour);
    let mut trace = std::hash::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    for trip in &records {
        tally(&mut totals, &mut outcomes, trip);
        let offered_hour = hour(trip.spec.departure_tick);
        let admitted_hour = trip.admitted_tick.map(hour);
        let completed_hour = trip.arrival_tick.map(hour);
        hourly[offered_hour].offered += u32::from(trip.status != TripStatus::Planned);
        if let Some(hour) = admitted_hour {
            hourly[hour].admitted += 1;
        }
        if let Some(hour) = completed_hour {
            hourly[hour].completed += 1;
        }
        (
            trip.spec.key,
            trip.admitted_tick,
            trip.arrival_tick,
            trip.status as u8,
        )
            .hash(&mut trace);
    }
    let mut peaks = Vec::new();
    let mut cohorts = Vec::new();
    let limit = case.load.delay_minutes() * TICKS_PER_MINUTE;
    for (peak, cutoff) in cutoffs.into_iter().enumerate() {
        let mut result = Peak {
            cutoff_tick: cutoff,
            cutoff_queues: cutoff_queues[peak].ok_or("missed exact peak cutoff")?,
            offered: 0,
            completed_by_cutoff: 0,
            completion_pass: false,
            all_directions_served: true,
            od_delay_pass: true,
        };
        for (od, &(from, to)) in pairs.iter().enumerate() {
            let mut by_mode = std::array::from_fn(|_| Counts::default());
            let mut by_mode_outcomes = std::array::from_fn(|_| Outcomes::default());
            let mut by_mode_completed_by_cutoff = [0; 3];
            let mut delays = Vec::new();
            let mut trip_times = Vec::new();
            let mut waiting = Vec::new();
            for (&release, trip) in releases
                .iter()
                .zip(&records)
                .filter(|(r, _)| r.peak == peak && r.od == od)
            {
                tally(
                    &mut by_mode[release.mode],
                    &mut by_mode_outcomes[release.mode],
                    trip,
                );
                by_mode_completed_by_cutoff[release.mode] +=
                    u32::from(trip.arrival_tick.is_some_and(|t| t <= cutoff));
                delays.push(scored_delay(
                    release.tick,
                    trip.arrival_tick,
                    cutoff,
                    trip.free_flow_ticks.unwrap_or_default(),
                    limit,
                ));
                if let Some(tick) = trip.arrival_tick {
                    trip_times.push(tick - release.tick);
                }
                if let Some(tick) = trip.admitted_tick {
                    waiting.push(tick - release.tick);
                }
            }
            let completed_by_cutoff: u32 = by_mode_completed_by_cutoff.iter().sum();
            let p95_delay_ticks = p95(&mut delays).ok_or("missing OD demand")?;
            result.offered += by_mode.iter().map(|counts| counts.offered).sum::<u32>();
            result.completed_by_cutoff += completed_by_cutoff;
            result.all_directions_served &= completed_by_cutoff > 0;
            result.od_delay_pass &= p95_delay_ticks <= limit;
            cohorts.push(Cohort {
                from: PORTS[from],
                to: PORTS[to],
                peak,
                by_mode,
                by_mode_outcomes,
                by_mode_completed_by_cutoff,
                completed_by_cutoff,
                p95_delay_ticks,
                p95_trip_ticks: p95(&mut trip_times),
                p95_entry_wait_ticks: p95(&mut waiting),
            });
        }
        result.completion_pass =
            result.completed_by_cutoff * 100 >= result.offered * case.load.completion_percent();
        peaks.push(result);
    }
    let overload_queue_cleared = (case.load == Load::Overload)
        .then(|| peaks.iter().all(|peak| peak.cutoff_queues.is_clear()));
    let spillback_pass = overload_queue_cleared.unwrap_or(occupancy.blocking_20m_episodes == 0);
    let traffic_pass = peaks
        .iter()
        .all(|p| p.completion_pass && p.all_directions_served && p.od_delay_pass)
        && spillback_pass
        && outcomes.rejected == 0
        && outcomes.lost == 0;
    let storage_pass = geometry.min_spine_lane_m >= case.road_class.lane_floor();
    let phase_pass = geometry
        .approach_controls
        .iter()
        .all(|counts| counts.opposing_phases_valid);
    let bidirectional_spine_pass = instrument.then(|| {
        [false, true].into_iter().all(|east| {
            geometry
                .spine_lanes
                .iter()
                .any(|lane| lane.towards_east == east && lane.entries > 0)
        })
    });
    let all_approaches_served = (instrument && case.demand_pattern == DemandPattern::AllApproaches)
        .then(|| {
            geometry
                .approach_road_entries
                .iter()
                .all(|roads| roads.len() == 4 && roads.iter().all(|road| road.entries > 0))
        });
    let gates = Gates {
        traffic_pass,
        storage_pass,
        phase_pass,
        bidirectional_spine_pass,
        all_approaches_served,
        eligible: traffic_pass
            && storage_pass
            && phase_pass
            && bidirectional_spine_pass == Some(true)
            && all_approaches_served != Some(false),
        spillback_pass,
        overload_queue_cleared,
    };
    Ok((
        CaseReport {
            case,
            start_tick: start,
            completed_per_hour: f64::from(totals.completed) / 24.0,
            totals,
            outcomes,
            hourly,
            peaks,
            cohorts,
            first_failure: records.iter().find_map(|r| r.failure),
            directed_turns_used: occupancy.directed_turn_entries.len(),
            directed_turn_entries: occupancy.directed_turn_entries.values().sum(),
            occupancy,
            geometry,
            gates,
        },
        elapsed,
        trace.finish(),
    ))
}

#[derive(Serialize)]
struct Contract<'a> {
    demand_basis: &'static str,
    nominal_envelope: Option<&'a DemandEnvelope>,
    observed_envelope: Option<&'a DemandEnvelope>,
    observed_within_matched_four_ports: Option<[bool; 2]>,
    observed_within_all_approaches: Option<[bool; 2]>,
    synthetic_trips_per_peak_hour_nominal_overload: [u64; 2],
    frontage_zone: ZoneCell,
    completion_pct: [u32; 2],
    clearance_minutes: [u64; 2],
    max_od_p95_delay_minutes: [u64; 2],
    min_lane_storage_m_local_collector_arterial: [f32; 3],
    start_seed: u64,
    run_ticks: u64,
    peak_start_elapsed_ticks: [u64; 2],
    peak_duration_ticks: u64,
    origin_offset_m: f32,
    destination_offset_m: f32,
    mode_order: [&'static str; 3],
    port_order: [&'static str; 6],
}

impl<'a> Contract<'a> {
    fn new(basis: &'a DemandBasis) -> Self {
        let loads = [Load::Nominal, Load::Overload];
        Self {
            demand_basis: if basis.nominal.is_some() {
                "district_gateway_envelope"
            } else {
                "synthetic_baseline"
            },
            nominal_envelope: basis.nominal.as_ref(),
            observed_envelope: basis.observed.as_ref(),
            observed_within_matched_four_ports: basis
                .observed_within_nominal(DemandPattern::MatchedFourPorts),
            observed_within_all_approaches: basis
                .observed_within_nominal(DemandPattern::AllApproaches),
            synthetic_trips_per_peak_hour_nominal_overload: loads.map(Load::trips_per_hour),
            frontage_zone: FRONTAGE_ZONE,
            completion_pct: loads.map(Load::completion_percent),
            clearance_minutes: loads.map(Load::cutoff_minutes),
            max_od_p95_delay_minutes: loads.map(Load::delay_minutes),
            min_lane_storage_m_local_collector_arterial: [
                RoadClass::Local.lane_floor(),
                RoadClass::Collector.lane_floor(),
                RoadClass::Arterial.lane_floor(),
            ],
            start_seed: SEED,
            run_ticks: TICKS_PER_DAY,
            peak_start_elapsed_ticks: STARTS,
            peak_duration_ticks: PEAK_TICKS,
            origin_offset_m: ORIGIN_M,
            destination_offset_m: DESTINATION_M,
            mode_order: ["car", "truck", "bus"],
            port_order: PORTS,
        }
    }
}

#[derive(Serialize)]
struct PitchResult {
    road_class: RoadClass,
    topology: Topology,
    demand_pattern: DemandPattern,
    lanes_per_direction: u32,
    control: Control,
    minimum_passing_tested_pitch_m: Option<u32>,
}

#[derive(Serialize)]
struct MatrixReport<'a> {
    version: u32,
    contract: Contract<'a>,
    passing_pitches: Vec<PitchResult>,
    cases: Vec<CaseReport>,
}

fn passing_pitches(reports: &[CaseReport]) -> Vec<PitchResult> {
    let eligible: BTreeMap<_, _> = reports
        .iter()
        .map(|report| (report.case, report.gates.eligible))
        .collect();
    let mut groups = BTreeMap::<_, Option<u32>>::new();
    for report in reports
        .iter()
        .filter(|report| report.case.load == Load::Nominal)
    {
        let case = report.case;
        let key = (
            case.road_class,
            case.topology,
            case.demand_pattern,
            case.lanes_per_direction,
            case.control,
        );
        let minimum = groups.entry(key).or_default();
        if report.gates.eligible
            && eligible[&Case {
                load: Load::Overload,
                ..case
            }]
        {
            *minimum = Some(minimum.map_or(case.pitch_m, |prior| prior.min(case.pitch_m)));
        }
    }
    groups
        .into_iter()
        .map(
            |((road_class, topology, demand_pattern, lanes_per_direction, control), minimum)| {
                PitchResult {
                    road_class,
                    topology,
                    demand_pattern,
                    lanes_per_direction,
                    control,
                    minimum_passing_tested_pitch_m: minimum,
                }
            },
        )
        .collect()
}

fn run_matrix(jobs: usize, basis: &DemandBasis) -> Result<MatrixReport<'_>, String> {
    let cases = cases();
    let next = AtomicUsize::new(0);
    let mut reports = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs.min(cases.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut complete = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&case) = cases.get(index) else { break };
                        let (report, elapsed, _) = run_case(case, basis, true)
                            .map_err(|err| format!("{case:?}: {err}"))?;
                        eprintln!(
                            "CM0 {:>3}/{} {case:?}: {}/{} completed, {:.2}s",
                            index + 1,
                            cases.len(),
                            report.totals.completed,
                            report.totals.offered,
                            elapsed.as_secs_f64()
                        );
                        complete.push((index, report));
                    }
                    Ok::<_, String>(complete)
                })
            })
            .collect();
        let mut reports = Vec::new();
        for worker in workers {
            reports.extend(worker.join().map_err(|_| "CM0 worker panicked")??);
        }
        Ok::<_, String>(reports)
    })?;
    reports.sort_by_key(|&(index, _)| index);
    let cases: Vec<_> = reports.into_iter().map(|(_, report)| report).collect();
    Ok(MatrixReport {
        version: 2,
        contract: Contract::new(basis),
        passing_pitches: passing_pitches(&cases),
        cases,
    })
}

#[derive(Serialize)]
struct Timing {
    case: Case,
    off_ms: [f64; 3],
    on_ms: [f64; 3],
    median_overhead_pct: f64,
    matching_trip_trace: bool,
}

fn timing(basis: &DemandBasis) -> Result<Timing, String> {
    let case = Case {
        topology: Topology::CurvedCollector,
        demand_pattern: DemandPattern::MatchedFourPorts,
        pitch_m: 120,
        lanes_per_direction: 1,
        control: Control::Auto,
        road_class: RoadClass::Collector,
        load: Load::Nominal,
    };
    let mut off_ms = [0.0; 3];
    let mut on_ms = [0.0; 3];
    let mut previous = None;
    for i in 0..3 {
        let order = if i % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        };
        for instrument in order {
            let (report, elapsed, trace) = run_case(case, basis, instrument)?;
            if previous.replace(trace).is_some_and(|prior| prior != trace)
                || report.totals.completed == 0
            {
                return Err(
                    "occupancy timing runs changed the trip trace or never completed a trip".into(),
                );
            }
            let target = if instrument { &mut on_ms } else { &mut off_ms };
            target[i] = elapsed.as_secs_f64() * 1000.0;
        }
    }
    let mut overhead = std::array::from_fn::<_, 3, _>(|i| (on_ms[i] / off_ms[i] - 1.0) * 100.0);
    overhead.sort_by(f64::total_cmp);
    Ok(Timing {
        case,
        off_ms,
        on_ms,
        median_overhead_pct: overhead[1],
        matching_trip_trace: true,
    })
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);
    let format_error = |err: io::Error| format!("{}: {err}", path.display());
    let parent = path.parent().ok_or("output path has no parent")?;
    let (temp, file) = loop {
        let index = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".cm0-{}-{index}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => break (temp, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format_error(err)),
        }
    };
    let result = (|| {
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.map_err(format_error)
}

const USAGE: &str = "usage: scenarios citymap-spacing --json PATH [--jobs N] [--timing PATH] [--envelope PATH [--observed-envelope PATH]]";

pub fn cli(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    let mut json = None;
    let mut timings = None;
    let mut jobs = None;
    let mut envelope = None;
    let mut observed = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--help" | "-h") => {
                println!("{USAGE}");
                return Ok(());
            }
            Some("--json") if json.is_none() => {
                json = Some(absolute_path(args.next())?);
            }
            Some("--timing") if timings.is_none() => {
                timings = Some(absolute_path(args.next())?);
            }
            Some("--envelope") if envelope.is_none() => {
                envelope = Some(absolute_path(args.next())?);
            }
            Some("--observed-envelope") if observed.is_none() => {
                observed = Some(absolute_path(args.next())?);
            }
            Some("--jobs") if jobs.is_none() => {
                jobs = Some(
                    args.next()
                        .and_then(|a| a.into_string().ok())
                        .and_then(|value| value.parse::<usize>().ok())
                        .filter(|&n| n > 0)
                        .ok_or_else(|| USAGE.to_string())?,
                );
            }
            _ => return Err(USAGE.into()),
        }
    }
    if observed.is_some() && envelope.is_none() {
        return Err("--observed-envelope requires --envelope".into());
    }
    let json = output_destination(&json.ok_or(USAGE)?)?;
    let timings = timings.as_deref().map(output_destination).transpose()?;
    if let Some(path) = &timings {
        require_distinct_paths(&json, path)?;
    }
    let read_envelope = |input: PathBuf, nominal| {
        let path = fs::canonicalize(&input).map_err(|err| format!("{}: {err}", input.display()))?;
        require_distinct_paths(&json, &path)?;
        if let Some(path_out) = &timings {
            require_distinct_paths(path_out, &path)?;
        }
        File::open(&path)
            .map_err(|err| err.to_string())
            .and_then(|file| DemandEnvelope::parse(io::BufReader::new(file), nominal))
            .map_err(|err| format!("{}: {err}", path.display()))
    };
    let basis = DemandBasis {
        nominal: envelope.map(|path| read_envelope(path, true)).transpose()?,
        observed: observed
            .map(|path| read_envelope(path, false))
            .transpose()?,
    };
    crate::ensure_repo_root_cwd();
    simulation::init::init();
    let report = run_matrix(jobs.unwrap_or(1), &basis)?;
    write_json(&json, &report)?;
    println!(
        "Wrote {} CM0 cases to {}",
        report.cases.len(),
        json.display()
    );
    if let Some(path) = timings {
        eprintln!("CM0 measuring matched occupancy overhead");
        write_json(&path, &timing(&basis)?)?;
    }
    Ok(())
}

fn output_destination(path: &Path) -> Result<PathBuf, String> {
    let resolve = || {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a file path",
            ));
        };
        let path = parent.canonicalize()?.join(name);
        match path.symlink_metadata() {
            Ok(_) => {
                let target = path.canonicalize()?;
                if !target.metadata()?.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "expected a regular file",
                    ));
                }
                Ok(target)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(path),
            Err(err) => Err(err),
        }
    };
    resolve().map_err(|err| format!("{}: {err}", path.display()))
}

fn require_distinct_paths(a: &Path, b: &Path) -> Result<(), String> {
    #[cfg(unix)]
    let same_file = {
        use std::os::unix::fs::MetadataExt;
        match (a.metadata(), b.metadata()) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    };
    #[cfg(not(unix))]
    let same_file = false;
    if a == b || same_file {
        return Err(format!(
            "CM0 inputs and outputs require distinct paths: {} and {}",
            a.display(),
            b.display()
        ));
    }
    Ok(())
}

fn absolute_path(arg: Option<OsString>) -> Result<PathBuf, String> {
    let path = arg
        .filter(|p| !p.is_empty() && !p.as_encoded_bytes().starts_with(b"--"))
        .ok_or(USAGE)?;
    std::path::absolute(path).map_err(|err| err.to_string())
}

#[cfg(test)]
pub(super) fn cm0_boundary_fixture() -> (Simulation, LaneID, LaneID) {
    crate::ensure_repo_root_cwd();
    simulation::init::init();
    let fixture = build_fixture(Case {
        topology: Topology::Orthogonal,
        demand_pattern: DemandPattern::MatchedFourPorts,
        pitch_m: 100,
        lanes_per_direction: 2,
        control: Control::StopSigns,
        road_class: RoadClass::Collector,
        load: Load::Nominal,
    })
    .unwrap();
    (fixture.sim, fixture.incoming[0], fixture.outgoing[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_case() -> Case {
        Case {
            topology: Topology::StaggeredT,
            demand_pattern: DemandPattern::MatchedFourPorts,
            pitch_m: 100,
            lanes_per_direction: 2,
            control: Control::Auto,
            road_class: RoadClass::Collector,
            load: Load::Nominal,
        }
    }

    #[test]
    fn cm0_fixture_keeps_exact_same_lane_release_order_and_retires_arrivals() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let Fixture {
            mut sim,
            incoming,
            outgoing,
            ..
        } = build_fixture(fixture_case()).unwrap();
        let start = sim.get_tick();
        let spec = TripSpec {
            key: 10,
            departure_tick: start,
            vehicle_kind: VehicleKind::Car,
            origin_lane_offset: LaneOffset {
                lane: incoming[0],
                metres: ORIGIN_M,
            },
            destination_lane_offset: LaneOffset {
                lane: outgoing[1],
                metres: DESTINATION_M,
            },
        };
        let second = TripSpec { key: 11, ..spec };
        let invalid = TripSpec {
            key: 12,
            destination_lane_offset: LaneOffset {
                lane: outgoing[1],
                metres: f32::NAN,
            },
            ..spec
        };
        let mut collector = TrafficCollector::new(vec![second, invalid, spec], true).unwrap();
        let mut schedule = Simulation::schedule();
        for _ in 0..30 * TICKS_PER_MINUTE {
            collector.release(&mut sim);
            sim.tick(&mut schedule, []);
            collector.observe(&mut sim);
        }
        let (records, occupancy) = collector.finish();
        assert_eq!(records[0].admitted_tick, Some(start));
        assert!(records[1].admitted_tick.unwrap() > start);
        assert_eq!(records[0].status, TripStatus::Arrived);
        assert_eq!(records[1].status, TripStatus::Arrived);
        assert_eq!(records[2].status, TripStatus::Rejected);
        assert_eq!(occupancy.directed_lane_entries.get(&incoming[0]), Some(&2));
        assert!(occupancy.directed_turn_entries.len() >= 2);
        assert!(sim.world().vehicles.is_empty());
        let map = sim.map();
        let (pos, dir) = map.lanes()[incoming[0]].points.point_dir_along(ORIGIN_M);
        drop(map);
        let pose = geom::Transform::new_dir(pos, dir);
        assert!(simulation::transportation::spawn_driving_vehicle(
            &mut sim,
            VehicleKind::Car,
            pose
        )
        .is_some());
    }

    #[test]
    fn cm0_flat_geometry_uses_terrain_height_and_stock_trim() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let case = Case {
            topology: Topology::Orthogonal,
            ..fixture_case()
        };
        let geometry = build_fixture(case).unwrap().geometry;
        assert!((geometry.centreline_arc_m - 100.0).abs() < 0.01);
        assert!((geometry.min_spine_lane_m - 64.8).abs() < 0.1);
    }

    #[test]
    fn cm0_matrix_has_all_factorial_cells_and_matched_peak_demand() {
        let cases = cases();
        assert_eq!(
            cases
                .iter()
                .filter(|case| case.road_class == RoadClass::Collector
                    && case.demand_pattern == DemandPattern::MatchedFourPorts
                    && case.pitch_m <= 160
                    && case.control != Control::Uncontrolled)
                .count(),
            180
        );
        for load in [Load::Nominal, Load::Overload] {
            let releases = demand(
                &DemandBasis::default(),
                DemandPattern::MatchedFourPorts,
                load,
                42,
            );
            for peak in 0..2 {
                let mut modes = [0; 3];
                for release in releases.iter().filter(|release| release.peak == peak) {
                    modes[release.mode] += 1;
                }
                assert_eq!(
                    modes,
                    if load == Load::Nominal {
                        [192, 36, 12]
                    } else {
                        [240, 45, 15]
                    }
                );
                for od in 0..OD.len() {
                    assert_eq!(
                        releases
                            .iter()
                            .filter(|release| release.od == od && release.peak == peak)
                            .count(),
                        (load.trips_per_hour() * 2 / OD.len() as u64) as usize
                    );
                }
            }
            assert!(releases.windows(2).all(|pair| pair[0].tick < pair[1].tick));
        }
    }

    #[test]
    fn cm0_long_pitch_compares_both_lane_counts_at_each_class() {
        let cases = cases();
        for pitch_m in [120, 160, 200, 240] {
            for road_class in [RoadClass::Collector, RoadClass::Arterial] {
                for lanes_per_direction in [1, 2] {
                    for control in Control::TESTED {
                        for load in [Load::Nominal, Load::Overload] {
                            let case = Case {
                                topology: Topology::CurvedCollector,
                                demand_pattern: DemandPattern::MatchedFourPorts,
                                pitch_m,
                                lanes_per_direction,
                                control,
                                road_class,
                                load,
                            };
                            assert!(cases.contains(&case), "missing {case:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn cm0_output_paths_reject_normalized_and_link_aliases() {
        let dir = std::env::temp_dir().join(format!("cm0-paths-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let stats = dir.join("stats.json");
        std::fs::write(&stats, "old").unwrap();
        let mut aliases = vec![dir.join("sub/../stats.json")];
        #[cfg(unix)]
        {
            let hardlink = dir.join("hard.json");
            std::fs::hard_link(&stats, &hardlink).unwrap();
            aliases.push(hardlink);
            let symlink = dir.join("symbolic.json");
            std::os::unix::fs::symlink(&stats, &symlink).unwrap();
            aliases.push(symlink);
            let parent_alias = dir.join("parent-alias");
            std::os::unix::fs::symlink(&dir, &parent_alias).unwrap();
            aliases.push(parent_alias.join("stats.json"));
        }
        let results: Vec<_> = aliases
            .iter()
            .map(|path| {
                require_distinct_paths(
                    &output_destination(&stats).unwrap(),
                    &output_destination(path).unwrap(),
                )
            })
            .collect();
        assert!(require_distinct_paths(&stats, &dir.join("distinct.json")).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
        for (alias, result) in aliases.iter().zip(results) {
            assert!(result.is_err(), "accepted alias {}", alias.display());
        }
    }

    #[test]
    fn cm0_envelope_rejects_unknown_schema_version() {
        let dir = std::env::temp_dir().join(format!("cm0-envelope-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let envelope = dir.join("envelope.json");
        std::fs::write(&envelope, r#"{"version":999,"revision":"test","district_source":"test districts","gateway_source":"test gateways","matched_four_ports":[[],[]],"all_approaches":[[],[]]}"#).unwrap();
        let result = cli([
            OsString::from("--json"),
            dir.join("stats.json").into_os_string(),
            OsString::from("--envelope"),
            envelope.into_os_string(),
        ]
        .into_iter());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(result
            .unwrap_err()
            .contains("unsupported demand envelope version 999"));
    }

    #[test]
    fn cm0_envelope_uses_versioned_od_counts_and_compares_observations() {
        let rows = |pattern: DemandPattern, peak: usize| {
            pattern
                .pairs()
                .into_iter()
                .enumerate()
                .rev()
                .map(|(od, (from, to))| {
                    serde_json::json!({
                        "from": PORTS[from],
                        "to": PORTS[to],
                        "district_trips": if od == 0 { 7 + peak } else { 1 },
                        "gateway_trips": if from >= 4 { 2 } else { 0 }
                    })
                })
                .collect::<Vec<_>>()
        };
        let mut json = serde_json::json!({
            "version": 1,
            "revision": "fixture-forecast",
            "district_source": "commissioning fixture",
            "gateway_source": "gateway fixture",
            "matched_four_ports": std::array::from_fn::<_, 2, _>(|peak| rows(DemandPattern::MatchedFourPorts, peak)),
            "all_approaches": std::array::from_fn::<_, 2, _>(|peak| rows(DemandPattern::AllApproaches, peak)),
        });
        let nominal = DemandEnvelope::parse(json.to_string().as_bytes(), true).unwrap();
        json["revision"] = "fixture-observed".into();
        json["all_approaches"][1][0]["gateway_trips"] = 3.into();
        let observed = DemandEnvelope::parse(json.to_string().as_bytes(), false).unwrap();
        let basis = DemandBasis {
            nominal: Some(nominal),
            observed: Some(observed),
        };
        let pattern = DemandPattern::AllApproaches;
        assert_eq!(basis.observed_within_nominal(pattern), Some([true, false]));
        assert_eq!(
            basis.observed_within_nominal(DemandPattern::MatchedFourPorts),
            Some([true, true])
        );
        let nominal = demand(&basis, pattern, Load::Nominal, 42);
        let overload = demand(&basis, pattern, Load::Overload, 42);
        for (peak, offered) in [56, 57].into_iter().enumerate() {
            let rows = |releases: &[Release]| releases.iter().filter(|r| r.peak == peak).count();
            assert_eq!(rows(&nominal), offered);
            assert_eq!(rows(&overload), offered + (offered + 2) / 4);
            for od in 0..pattern.pairs().len() {
                let count = |releases: &[Release]| {
                    releases
                        .iter()
                        .filter(|r| r.od == od && r.peak == peak)
                        .count()
                };
                assert!(count(&overload) >= count(&nominal));
            }
        }
        assert_eq!(
            nominal.iter().filter(|r| r.peak == 1 && r.od == 0).count(),
            8
        );
        assert!(overload.windows(2).all(|rs| rs[0].tick < rs[1].tick));

        let mut invalid = json;
        invalid["all_approaches"][0].as_array_mut().unwrap().pop();
        let error = DemandEnvelope::parse(invalid.to_string().as_bytes(), true)
            .err()
            .unwrap();
        assert!(error.contains("missing OD west to east"));
    }

    #[test]
    fn cm0_all_approach_control_comparisons_cover_both_crosses() {
        let all_approaches = cases()
            .into_iter()
            .filter(|case| case.demand_pattern == DemandPattern::AllApproaches)
            .count();
        assert_eq!(all_approaches, 48);
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let case = Case {
            topology: Topology::Orthogonal,
            demand_pattern: DemandPattern::AllApproaches,
            control: Control::Signals,
            pitch_m: 140,
            ..fixture_case()
        };
        let (report, _, _) = run_case(case, &DemandBasis::default(), true).unwrap();
        assert_eq!(report.gates.all_approaches_served, Some(true));
        assert!(report.gates.phase_pass);
        assert_eq!(report.totals.completed, 480);
        for &(a, b) in &[(2, 4), (3, 5)] {
            for (from, to) in [(a, b), (b, a)] {
                let completed = report
                    .cohorts
                    .iter()
                    .filter(|r| r.from == PORTS[from] && r.to == PORTS[to])
                    .flat_map(|r| &r.by_mode)
                    .map(|mode| mode.completed)
                    .sum::<u32>();
                assert_eq!(completed, 16, "{} to {}", PORTS[from], PORTS[to]);
            }
        }
    }

    #[test]
    fn cm0_reports_generated_and_eligible_frontage_separately() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let fixture = build_fixture(fixture_case()).unwrap();
        let data = serde_json::to_value(&fixture.geometry).unwrap();
        let generated = data["frontage"]["generated"].as_u64().unwrap();
        let eligible = data["frontage"]["eligible"].as_u64().unwrap();
        assert_eq!(generated, fixture.sim.map().lots().len() as u64);
        assert!(eligible > 0 && eligible <= generated);
        let map = fixture.sim.map();
        let lot = map
            .lots()
            .values()
            .find(|lot| check_growable_lot(&map, lot, FRONTAGE_ZONE).is_ok())
            .unwrap();
        let mut too_small = lot.clone();
        too_small.shape = geom::OBB::new(lot.shape.center(), Vec2::X, 1.0, 1.0);
        let counts = Frontage::count(&map, [lot, &too_small].into_iter());
        assert_eq!((counts.generated, counts.eligible), (2, 1));
    }

    #[test]
    fn cm0_json_failure_preserves_previous_file() {
        struct Unserializable;
        impl Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("injected JSON error"))
            }
        }
        let path = std::env::temp_dir().join(format!("cm0-json-{}.json", std::process::id()));
        std::fs::write(&path, "old").unwrap();
        assert!(write_json(&path, &Unserializable).is_err());
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(bytes, b"old");
    }

    #[test]
    fn cm0_ordinary_signal_wait_clears_and_full_day_repeats() {
        crate::ensure_repo_root_cwd();
        simulation::init::init();
        let case = Case {
            topology: Topology::Orthogonal,
            control: Control::Signals,
            ..fixture_case()
        };
        let (a, _, trace_a) = run_case(case, &DemandBasis::default(), true).unwrap();
        let (b, _, trace_b) = run_case(case, &DemandBasis::default(), true).unwrap();
        assert_eq!(trace_a, trace_b);
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap()
        );
        assert!(a.gates.phase_pass);
        assert!(a.occupancy.max_queue_m > 0.0);
        assert_eq!(a.occupancy.blocking_20m_episodes, 0);
        assert_eq!(a.totals.completed, a.totals.offered);
    }
}
