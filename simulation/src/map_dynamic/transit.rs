use std::collections::BTreeMap;

use geom::{Color, Vec2, Vec3};
use ordered_float::OrderedFloat;
use prototypes::{Money, Tick};
use serde::{Deserialize, Serialize};
use slotmapd::{new_key_type, HopSlotMap};

use crate::economy::{Budget, Government};
use crate::map::{LaneID, LaneKind, Map, ProjectFilter, ProjectKind};
use crate::map_dynamic::{Destination, Itinerary, Router, RoutingStep};
use crate::transportation::Location;
use crate::utils::resources::Resources;
use crate::world_command::{Rejection, WorldCommand};
use crate::{HumanEnt, Simulation, VehicleID, World};

pub(crate) const WAIT_NOT_STARTED: Tick = Tick(u64::MAX);

new_key_type! {
    pub struct StopID;
    pub struct LineID;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitStop {
    pub id: StopID,
    pub pos: Vec3,
    pub dir: Vec2,
    pub walk_pos: Vec3,
    pub berth: Vec3,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitLine {
    pub id: LineID,
    pub stops: Vec<StopID>,
    pub target_vehicles: u8,
    pub fare: Money,
    pub color: Color,
    pub suspended: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusRun {
    pub line: LineID,
    pub next_stop_idx: usize,
    /// Elapsed tick when dwelling ends.
    pub dwell_until: Option<Tick>,
    /// Elapsed tick when the current leg began.
    pub leg_started: Tick,
    pub failures: u8,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Transit {
    pub stops: HopSlotMap<StopID, TransitStop>,
    pub lines: HopSlotMap<LineID, TransitLine>,
    pub runs: BTreeMap<VehicleID, BusRun>,
    /// Increment when stop or line data changes, including while paused.
    pub revision: u64,
}

impl Transit {
    /// Estimated interval between buses, in elapsed ticks.
    pub fn headway(&self, _line: LineID) -> Option<u64> {
        None
    }
}

/// Clock for transit leg, dwell, waiting, and trip timestamps. SetGameTime only
/// shifts the calendar clock. This clock advances by one tick per simulation step.
pub fn elapsed_ticks(resources: &Resources) -> Tick {
    let calendar_tick = resources.tick();
    Tick(resources.read::<Budget>().elapsed_ticks(calendar_tick))
}

pub(crate) fn is_command(command: &WorldCommand) -> bool {
    use WorldCommand::*;

    matches!(
        command,
        TransitAddStop { .. }
            | TransitRemoveStop { .. }
            | TransitCreateLine { .. }
            | TransitEditLine { .. }
            | TransitRemoveLine { .. }
            | TransitSetVehicles { .. }
            | TransitSetFare { .. }
    )
}

pub fn validate(sim: &Simulation, command: &WorldCommand) -> Result<(), Rejection> {
    use WorldCommand::*;

    if !is_command(command) {
        return Ok(());
    }
    let transit = sim.read::<Transit>();
    match command {
        TransitAddStop { pos, dir } => {
            if !pos.is_finite() || !dir.mag2().is_finite() || dir.mag2() == 0.0 {
                return Err(Rejection::InvalidConnection);
            }
            resolve_stop(&sim.map(), *pos, *dir).ok_or(Rejection::MissingReference)?;
        }
        TransitRemoveStop { stop } if !transit.stops.contains_key(*stop) => {
            return Err(Rejection::MissingReference);
        }
        TransitEditLine { line, .. }
        | TransitRemoveLine { line }
        | TransitSetVehicles { line, .. }
        | TransitSetFare { line, .. }
            if !transit.lines.contains_key(*line) =>
        {
            return Err(Rejection::MissingReference);
        }
        TransitCreateLine { stops } | TransitEditLine { stops, .. } => {
            if !(2..=30).contains(&stops.len()) {
                return Err(Rejection::InvalidConnection);
            }
            let map = sim.map();
            let berths = stops
                .iter()
                .map(|stop| {
                    let stop = transit
                        .stops
                        .get(*stop)
                        .ok_or(Rejection::MissingReference)?;
                    resolve_stop(&map, stop.pos, stop.dir).ok_or(Rejection::InvalidConnection)
                })
                .collect::<Result<Vec<_>, _>>()?;
            let tick = elapsed_ticks(&sim.resources);
            for (from, to) in berths.iter().zip(berths.iter().cycle().skip(1)) {
                Itinerary::route_between_lanes(tick, &map, *from, *to)
                    .ok_or(Rejection::InvalidConnection)?;
            }
        }
        TransitSetVehicles { n, .. } if *n > 10 => return Err(Rejection::InvalidConnection),
        TransitSetFare { fare, .. } if !(Money::ZERO..=Money::new_bucks(10)).contains(fare) => {
            return Err(Rejection::InvalidConnection);
        }
        _ => {}
    }

    let cost = Government::checked_action_cost(command, sim).ok_or(Rejection::CostOverflow)?;
    let balance = sim.read::<Government>().money;
    if cost > Money::ZERO && balance < cost {
        return Err(Rejection::InsufficientFunds);
    }
    balance
        .inner()
        .checked_sub(cost.inner())
        .ok_or(Rejection::CostOverflow)?;
    Ok(())
}

fn resolve_stop(map: &Map, pos: Vec3, dir: Vec2) -> Option<(LaneID, f32)> {
    let dir = dir.try_normalize()?;
    map.spatial_map()
        .query_around(pos.xy(), 4.0, ProjectFilter::ROAD)
        .filter_map(|project| match project {
            ProjectKind::Road(id) => map.get(id),
            _ => None,
        })
        .flat_map(|road| road.lanes_iter())
        .filter(|&(_, kind)| kind == LaneKind::Driving)
        .filter_map(|(id, _)| {
            let lane = map.get(id)?;
            if lane.points.length() < 30.0 {
                return None;
            }
            let (berth, segment, lane_dir) = lane.points.project_segment_dir(pos);
            let distance = pos.distance2(berth);
            if distance > 4.0 * 4.0 || dir.dot(lane_dir.xy()) <= 0.0 {
                return None;
            }
            let prefix = lane
                .points
                .segments()
                .take(segment - 1)
                .map(|s| s.src.distance(s.dst))
                .sum::<f32>();
            let offset = prefix + lane.points[segment - 1].distance(berth);
            if !(15.0..=lane.points.length() - 15.0).contains(&offset) {
                return None;
            }
            let sidewalk = map.get(map.get(lane.parent)?.sidewalks(lane.src).outgoing?)?;
            if sidewalk.points.project_dist2(pos) > 10.0 * 10.0 {
                return None;
            }
            Some(((OrderedFloat(distance), id), (id, offset)))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, berth)| berth)
}

/// Mutation entry point for P3a. A rejection here leaves the command unrecorded and uncharged.
pub fn apply(_sim: &mut Simulation, _command: &WorldCommand) -> Result<(), Rejection> {
    Err(Rejection::TransitNotImplemented)
}

/// Returns whether transit took over cancellation. An onboard rider keeps the
/// ride and decision until alighting, even when the destination is demolished.
pub fn on_route_cancelled(human: &mut HumanEnt, resources: &Resources) -> bool {
    if matches!(human.location, Location::Transit(_)) {
        if let Some(Destination::Building(id)) = human.router.target_dest {
            if resources.read::<Map>().get(id).is_none() {
                human.router.defer_replan();
            }
        }
        return true;
    }
    if matches!(
        human.router.cur_step(),
        Some(RoutingStep::WaitAtStop { .. })
    ) && human.collider.is_none()
    {
        Router::force_detach(human, human.trans.pos, resources);
    }
    false
}

pub fn on_vehicle_dropped(_sim: &mut Simulation, _vehicle: VehicleID, _position: Vec3) {}

pub fn on_topology_changed(_sim: &mut Simulation) {}

pub fn transit_stop_update(_world: &mut World, _resources: &mut Resources) {}

pub fn transit_dispatch_update(_world: &mut World, _resources: &mut Resources) {}

pub fn transit_board_update(world: &mut World, resources: &mut Resources) {
    if resources.read::<Transit>().lines.is_empty() {
        return;
    }
    // Routing starts the wait after this system runs, so stamp it on the
    // first boarding pass that can see the passenger in the queue.
    let now = elapsed_ticks(resources);
    for human in world.humans.values_mut() {
        human.router.stamp_wait_start(now);
    }
}
