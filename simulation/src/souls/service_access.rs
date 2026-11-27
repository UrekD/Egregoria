use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use geom::{Intersect, PolyLine3, Segment3, Shape, Spline3, Transform, Vec3, OBB};
use ordered_float::OrderedFloat;
use prototypes::{try_prototype, ServiceID, ServiceKind};
use serde::{Deserialize, Serialize};

use crate::map::{
    BuildingID, BuildingKind, Lane, LaneID, LaneKind, Map, ParkingSpotID, PreparedEffects,
    ProjectFilter, RoadID, Traversable, TraverseDirection, TraverseKind, TurnID, TurnKind,
    ROAD_Z_OFFSET,
};
use crate::map_dynamic::ParkingManagement;
use crate::transportation::{TransportGrid, VehicleKind};
use crate::Simulation;

const STEPS: usize = 16;
const CLEARANCE: f32 = 0.5;
const MAX_LOCAL_GRADE: f32 = 0.6;
pub(crate) const APRON_WORK: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessError {
    MissingStation,
    MissingFrontage,
    MissingSidewalk,
    NoTruckFit,
    NoReturnLoop,
    OccupiedCurb,
    WorkLimit,
    AlreadyCommissioned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectedLaneAnchor {
    pub lane: LaneID,
    pub offset_cm: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FireApronPreview {
    pub road: RoadID,
    pub arrival: DirectedLaneAnchor,
    pub departure: DirectedLaneAnchor,
    pub bay: Transform,
    pub pocket: Vec3,
    pub(crate) door: Vec3,
    pub(crate) envelope: OBB,
    #[serde(deserialize_with = "crate::world_command::bounded_command_vec::<_, _, 256>")]
    pub parking_displacements: Vec<ParkingSpotID>,
    pub return_drive_ms: u64,
    pub arrival_sweep: [Vec3; STEPS + 1],
    pub departure_sweep: [Vec3; STEPS + 1],
}

/// Covers the two pocket offsets and the paths converging on the door.
pub(crate) fn walking_corridor(bay: Transform, pocket: Vec3, door: Vec3) -> OBB {
    let direction = (door - pocket).xy().try_normalize().unwrap_or(bay.dir.xy());
    OBB::new((pocket.xy() + door.xy()) * 0.5, direction, pocket.xy().distance(door.xy()) + 1.8, 1.8)
}

pub(crate) fn corridor_clear(map: &Map, site: Option<BuildingID>, corridor: OBB) -> bool {
    !map.spatial_map().query(corridor, ProjectFilter::BUILDING).any(|object| {
        matches!(object, crate::map::ProjectKind::Building(id) if Some(id) != site)
    })
}

pub(crate) fn check_truck_envelope(
    map: &Map,
    road: RoadID,
    envelope: OBB,
    work: &mut AccessBudget,
) -> Result<(), AccessError> {
    map.spatial_map()
        .reserve_apron_query(envelope.bbox(), &mut work.0)
        .map_err(|_| AccessError::WorkLimit)?;
    if map
        .spatial_map()
        .query(
            envelope,
            ProjectFilter::ROAD | ProjectFilter::INTER | ProjectFilter::BUILDING,
        )
        .any(|object| object != road.into())
        || map
            .authorize_land_effects(PreparedEffects::Footprint(&envelope.corners))
            .is_err()
    {
        return Err(AccessError::NoTruckFit);
    }
    Ok(())
}

pub(crate) struct AccessBudget(pub(crate) usize);

impl AccessBudget {
    fn charge(&mut self, work: usize) -> Result<(), AccessError> {
        self.0 = self.0.checked_sub(work).ok_or(AccessError::WorkLimit)?;
        Ok(())
    }
}

/// Checks a candidate apron against the live frontage. The preview holds no reservation.
pub fn preview_fire_apron(
    sim: &Simulation,
    site: BuildingID,
) -> Result<FireApronPreview, AccessError> {
    if let Some(apron) = super::service_dispatch::commissioned_apron(sim, site) {
        return Ok(apron);
    }
    let map = sim.map();
    let station = map
        .get(site)
        .filter(|building| is_fire_station(building.kind))
        .ok_or(AccessError::MissingStation)?;
    check_apron(
        sim,
        station.connected_road,
        station.door_pos,
        None,
        &mut AccessBudget(APRON_WORK),
    )
}

pub(crate) fn is_fire_station(kind: BuildingKind) -> bool {
    kind.as_goods_company()
        .and_then(|id| try_prototype(ServiceID::from(id)))
        .is_some_and(|proto| proto.service_kind == ServiceKind::Fire)
}

pub(crate) fn check_apron(
    sim: &Simulation,
    road: Option<RoadID>,
    door: Vec3,
    proposed_building: Option<OBB>,
    work: &mut AccessBudget,
) -> Result<FireApronPreview, AccessError> {
    check_apron_with_return(sim, road, door, proposed_building, work, true)
}

pub(crate) fn check_scene_apron(
    sim: &Simulation,
    road: Option<RoadID>,
    door: Vec3,
    work: &mut AccessBudget,
) -> Result<FireApronPreview, AccessError> {
    check_apron_with_return(sim, road, door, None, work, false)
}

fn check_apron_with_return(
    sim: &Simulation,
    road: Option<RoadID>,
    door: Vec3,
    proposed_building: Option<OBB>,
    work: &mut AccessBudget,
    prove_return: bool,
) -> Result<FireApronPreview, AccessError> {
    let map = sim.map();
    let road = road
        .and_then(|road| map.get(road))
        .ok_or(AccessError::MissingFrontage)?;
    let (_, road_offset) = project_offset(road.points(), door, work)?;
    let (road_pos, road_direction) = road.points().point_dir_along(road_offset);
    let outward = road_direction.xy().perpendicular();
    let outward = outward * (door.xy() - road_pos.xy()).dot(outward).signum();
    if outward.mag2() == 0.0 {
        return Err(AccessError::NoTruckFit);
    }

    let mut drive = None;
    let mut walk = None;
    for (id, kind) in road.lanes_iter() {
        work.charge(1)?;
        let candidates = match kind {
            LaneKind::Driving => &mut drive,
            LaneKind::Walking => &mut walk,
            _ => continue,
        };
        let lane = map.get(id).ok_or(AccessError::MissingFrontage)?;
        let (nearest, offset) = project_offset(&lane.points, door, work)?;
        if kind == LaneKind::Walking && (nearest.xy() - road_pos.xy()).dot(outward) <= 0.0 {
            continue;
        }
        let rank = (OrderedFloat(nearest.distance2(door)), id);
        if candidates.as_ref().is_none_or(|(best, _, _)| rank < *best) {
            *candidates = Some((rank, lane, offset));
        }
    }
    let (_, driving, offset) = drive.ok_or(AccessError::NoTruckFit)?;
    let (_, sidewalk, _) = walk.ok_or(AccessError::MissingSidewalk)?;
    let frontage = Frontage {
        road: road.id,
        width: road.width,
        driving,
        sidewalk,
        door,
        outward,
    };
    let parking = sim.read::<ParkingManagement>();
    let traffic = sim.read::<TransportGrid>();
    let accessed_site = match map.project(door, 0.0, ProjectFilter::BUILDING).kind {
        crate::map::ProjectKind::Building(site) if map.get(site).is_some_and(|site| site.door_pos == door) => Some(site),
        _ => None,
    };
    let mut failure = AccessError::NoTruckFit;
    for shift in [0.0, -16.0, 16.0, -32.0, 32.0, -43.0, 43.0] {
        match frontage.preview(&map, &parking, &traffic, offset + shift, work, prove_return) {
            Ok(apron)
                if proposed_building.is_none_or(|shape| !shape.intersects(&apron.envelope))
                    && !map.service_apron_overlaps(&apron.envelope) =>
            {
                let corridor = walking_corridor(apron.bay, apron.pocket, apron.door);
                map.spatial_map()
                    .reserve_apron_query(corridor.bbox(), &mut work.0)
                    .map_err(|_| AccessError::WorkLimit)?;
                if corridor_clear(&map, accessed_site, corridor)
                    && !map.service_apron_overlaps(&corridor)
                    && map.authorize_land_effects(PreparedEffects::Footprint(&corridor.corners)).is_ok()
                {
                    return Ok(apron);
                }
                failure = AccessError::MissingSidewalk;
            }
            Ok(_) => failure = AccessError::OccupiedCurb,
            Err(AccessError::WorkLimit) => return Err(AccessError::WorkLimit),
            Err(reason) => failure = reason,
        }
    }
    Err(failure)
}

struct Frontage<'a> {
    road: RoadID,
    width: f32,
    driving: &'a Lane,
    sidewalk: &'a Lane,
    door: Vec3,
    outward: geom::Vec2,
}

impl Frontage<'_> {
    fn preview(
        &self,
        map: &Map,
        parking: &ParkingManagement,
        traffic: &TransportGrid,
        offset: f32,
        work: &mut AccessBudget,
        prove_return: bool,
    ) -> Result<FireApronPreview, AccessError> {
        let lane = self.driving;
        let truck = VehicleKind::Truck;
        let radius = truck.width() * 0.5 + CLEARANCE;
        let apron_length = 4.0 * truck.min_turning_radius();
        let transition = 24.0;
        let entry = offset - transition;
        let exit = offset + transition;
        if entry < radius || exit + radius > lane.points.length() {
            return Err(AccessError::NoTruckFit);
        }
        let (enter_pos, enter_dir) = lane.points.point_dir_along(entry);
        let (leave_pos, leave_dir) = lane.points.point_dir_along(exit);
        let (drive_pos, bay_dir) = lane.points.point_dir_along(offset);
        if enter_dir.dot(bay_dir) < 0.995
            || leave_dir.dot(bay_dir) < 0.995
            || (enter_pos.z - leave_pos.z).abs() > MAX_LOCAL_GRADE
        {
            return Err(AccessError::NoTruckFit);
        }
        let road = map.get(self.road).ok_or(AccessError::MissingFrontage)?;
        let (road_pos, _) = project_offset(road.points(), drive_pos, work)?;
        let bay_pos = road_pos + (self.outward * (self.width * 0.5 + radius)).z0();
        let bay = Transform::new_dir(bay_pos, bay_dir);
        let pocket = bay_pos + (self.outward * (radius + 1.0)).z0();
        let (walk_pos, _) = project_offset(&self.sidewalk.points, pocket, work)?;
        if (self.door.z - pocket.z).abs() > 1.25
            || (walk_pos.z - pocket.z).abs() > MAX_LOCAL_GRADE
            || walk_pos.distance2(pocket) > 100.0
            || self.door.distance2(pocket) > 2500.0
        {
            return Err(AccessError::MissingSidewalk);
        }

        let arrival_sweep = sweep(Transform::new_dir(enter_pos, enter_dir), bay);
        let departure_sweep = sweep(bay, Transform::new_dir(leave_pos, leave_dir));
        let pad = OBB::new(bay_pos.xy(), bay_dir.xy(), apron_length, radius * 2.0);
        let envelope = OBB::new(
            (drive_pos.xy() + pocket.xy()) * 0.5,
            bay_dir.xy(),
            2.0 * (transition + radius),
            drive_pos.xy().distance(pocket.xy()) + 2.0 * radius,
        );
        check_truck_envelope(map, self.road, envelope, work)?;
        let environment = map.environment();
        let dry = |point: Vec3| {
            !environment.is_water(point.xy())
                && environment
                    .height(point.xy())
                    .is_some_and(|z| (z + ROAD_Z_OFFSET - point.z).abs() <= MAX_LOCAL_GRADE)
        };
        for point in pad
            .corners
            .map(|p| p.z(bay_pos.z))
            .into_iter()
            .chain(arrival_sweep)
            .chain(departure_sweep)
            .chain([pocket, walk_pos])
        {
            work.charge(1)?;
            if !dry(point) {
                return Err(AccessError::NoTruckFit);
            }
        }

        for index in 0..=STEPS {
            work.charge(1)?;
            let point = pocket.lerp(self.door, index as f32 / STEPS as f32);
            if !dry(point) {
                return Err(AccessError::MissingSidewalk);
            }
        }
        let intersects_sweep = |pos: Vec3, clearance: f32| {
            [&arrival_sweep, &departure_sweep].into_iter().any(|sweep| {
                sweep.windows(2).any(|pair| {
                    let nearest = Segment3::new(pair[0], pair[1]).project(pos);
                    nearest.distance2(pos) < clearance * clearance
                })
            })
        };
        let bounds = envelope.bbox().expand(4.5);
        for (id, pos) in traffic.query_aabb(bounds.ll, bounds.ur) {
            work.charge(STEPS * 2)?;
            if let Some((_, actor)) = traffic.get(id) {
                if (actor.height - bay_pos.z).abs() <= 1.25
                    && intersects_sweep(pos.z(actor.height), radius + actor.radius)
                {
                    return Err(AccessError::OccupiedCurb);
                }
            }
        }
        let mut parking_displacements = Vec::new();
        if let Some(curb) = road.parking_next_to(lane) {
            for spot in map.apron_spots(curb) {
                work.charge(1)?;
                let Some(geometry) = map.parking.get(spot) else {
                    continue;
                };
                if !bounds.contains(geometry.trans.pos.xy()) {
                    continue;
                }
                work.charge(STEPS * 2)?;
                if intersects_sweep(geometry.trans.pos, radius + VehicleKind::Car.width() * 0.5) {
                    if !parking.is_spot_free(spot) {
                        return Err(AccessError::OccupiedCurb);
                    }
                    parking_displacements.push(spot);
                }
            }
        }

        let anchor = |position: f32| DirectedLaneAnchor {
            lane: lane.id,
            offset_cm: (position * 100.0).round() as u32,
        };
        let arrival = anchor(entry);
        let departure = anchor(exit);
        let return_drive_ms = if prove_return {
            drive_route(map, departure, arrival, work)?.0
        } else {
            0
        };
        Ok(FireApronPreview {
            road: self.road,
            arrival,
            departure,
            bay,
            pocket,
            door: self.door,
            envelope,
            parking_displacements,
            return_drive_ms,
            arrival_sweep,
            departure_sweep,
        })
    }
}

fn project_offset(
    line: &PolyLine3,
    position: Vec3,
    work: &mut AccessBudget,
) -> Result<(Vec3, f32), AccessError> {
    if line.len() < 2 {
        return Err(AccessError::NoTruckFit);
    }
    work.charge(line.len().saturating_mul(2))?;
    let (point, segment) = line.project_segment_2d(position.xy());
    let Some(start) = segment.checked_sub(1) else {
        return Err(AccessError::NoTruckFit);
    };
    let offset = line
        .segments()
        .take(start)
        .map(|edge| edge.src.distance(edge.dst))
        .sum::<f32>()
        + point.distance(line[start]);
    Ok((point, offset))
}

fn sweep(from: Transform, to: Transform) -> [Vec3; STEPS + 1] {
    let tangent = 0.9 * from.pos.distance(to.pos);
    let spline = Spline3 {
        from: from.pos,
        to: to.pos,
        from_derivative: from.dir * tangent,
        to_derivative: to.dir * tangent,
    };
    std::array::from_fn(|i| spline.get(i as f32 / STEPS as f32))
}

pub(crate) fn drive_route(
    map: &Map,
    from: DirectedLaneAnchor,
    to: DirectedLaneAnchor,
    work: &mut AccessBudget,
) -> Result<(u64, Vec<Traversable>), AccessError> {
    DriveSearch::new(from, to).advance(map, work)
}

type SearchNode = (LaneID, bool);
type Predecessor = (u64, SearchNode, TurnID);
type SearchEntry = Reverse<(u64, SearchNode)>;

pub(crate) const MAX_FIRE_ROUTE: usize = 65_536;

#[derive(Serialize, Deserialize)]
pub(crate) struct DriveSearch {
    from: DirectedLaneAnchor,
    to: DirectedLaneAnchor,
    frontier: BinaryHeap<SearchEntry>,
    costs: BTreeMap<LaneID, Predecessor>,
    scan: Option<(u64, SearchNode, usize)>,
    route: Option<(u64, LaneID, Vec<Traversable>)>,
    started: bool,
}

impl DriveSearch {
    pub(crate) fn new(from: DirectedLaneAnchor, to: DirectedLaneAnchor) -> Self {
        Self {
            from,
            to,
            frontier: BinaryHeap::new(),
            costs: BTreeMap::new(),
            scan: None,
            route: None,
            started: false,
        }
    }

    pub(crate) fn departure(&self) -> DirectedLaneAnchor {
        self.from
    }

    pub(crate) fn matches(&self, from: DirectedLaneAnchor, to: DirectedLaneAnchor) -> bool {
        self.from == from && self.to == to
    }

    pub(crate) fn advance(
        &mut self,
        map: &Map,
        work: &mut AccessBudget,
    ) -> Result<(u64, Vec<Traversable>), AccessError> {
        let start = map.get(self.from.lane).ok_or(AccessError::NoReturnLoop)?;
        let end = map.get(self.to.lane).ok_or(AccessError::NoReturnLoop)?;
        let from_m = self.from.offset_cm as f32 / 100.0;
        let to_m = self.to.offset_cm as f32 / 100.0;
        if from_m > start.points.length()
            || to_m > end.points.length()
            || start.kind != LaneKind::Driving
            || end.kind != LaneKind::Driving
        {
            return Err(AccessError::NoReturnLoop);
        }
        let speed = |lane: &Lane| lane.speed_limit * VehicleKind::Truck.speed_factor();
        let lane_ms = |meters: f32, speed: f32| (1000.0 * meters.max(0.0) / speed).ceil() as u64;
        let driving = |kind| Traversable::new(kind, TraverseDirection::Forward);
        if self.from.lane == self.to.lane && from_m <= to_m {
            return Ok((lane_ms(to_m - from_m, speed(start)), vec![]));
        }
        let initial = (self.from.lane, true);
        if !self.started {
            let tail = lane_ms(start.points.length() - from_m, speed(start));
            self.frontier.push(Reverse((tail, initial)));
            self.started = true;
        }
        loop {
            if let Some((ms, cursor, route)) = &mut self.route {
                work.charge(1)?;
                if route.len() >= MAX_FIRE_ROUTE {
                    return Err(AccessError::NoReturnLoop);
                }
                let &(_, previous, turn) = self.costs.get(cursor).ok_or(AccessError::NoReturnLoop)?;
                route.push(driving(TraverseKind::Lane(*cursor)));
                route.push(driving(TraverseKind::Turn(turn)));
                if previous == initial {
                    return Ok((*ms, std::mem::take(route)));
                }
                *cursor = previous.0;
                continue;
            }
            if let Some((ms, (id, first), index)) = self.scan {
                let lane = map.get(id).ok_or(AccessError::NoReturnLoop)?;
                let junction = map.get(lane.dst).ok_or(AccessError::NoReturnLoop)?;
                let Some(turn) = junction.turns().nth(index) else {
                    self.scan = None;
                    continue;
                };
                work.charge(1)?;
                self.scan = Some((ms, (id, first), index + 1));
                if turn.id.src != id || turn.id.bidirectional || turn.kind != TurnKind::Driving {
                    continue;
                }
                let Some(next) = map.get(turn.id.dst).filter(|next| next.kind == LaneKind::Driving)
                else {
                    continue;
                };
                let distance = if next.id == self.to.lane {
                    to_m
                } else {
                    next.points.length()
                };
                let cost = ms
                    .saturating_add(lane_ms(turn.points.length(), speed(lane).min(speed(next))))
                    .saturating_add(lane_ms(distance, speed(next)));
                if self.costs.get(&next.id).is_none_or(|&(old, _, _)| cost < old) {
                    self.costs.insert(next.id, (cost, (id, first), turn.id));
                    self.frontier.push(Reverse((cost, (next.id, false))));
                }
                continue;
            }

            work.charge(1)?;
            let Reverse((ms, (id, first))) = self.frontier.pop().ok_or(AccessError::NoReturnLoop)?;
            if !first && self.costs.get(&id).is_none_or(|&(best, _, _)| best != ms) {
                continue;
            }
            if !first && id == self.to.lane {
                self.route = Some((ms, id, Vec::new()));
            } else {
                self.scan = Some((ms, (id, first), 0));
            }
        }
    }
}

pub(crate) fn current_anchor(
    map: &Map,
    lane: LaneID,
    position: Vec3,
) -> Option<DirectedLaneAnchor> {
    let lane = map.get(lane)?;
    let (nearest, offset) =
        project_offset(&lane.points, position, &mut AccessBudget(APRON_WORK)).ok()?;
    (nearest.distance2(position) <= 2.25).then_some(DirectedLaneAnchor {
        lane: lane.id,
        offset_cm: (offset * 100.0).round() as u32,
    })
}
