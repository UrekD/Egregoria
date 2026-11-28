use crate::map::{
    Map, RoundaboutGeometry, TrafficBehavior, Traversable, TraverseDirection, TraverseKind,
};
use crate::map_dynamic::{Itinerary, Transit, OBJECTIVE_OK_DIST};
use crate::transportation::junction_admission::{self, segments_close, GAP, VEHICLE_BODY_RADIUS};
use crate::transportation::queue_diagnostics::{QueueDiagnostics, QueueHoldReason, QueueRevision};
use crate::transportation::{
    ColliderOwnerLookup, Speed, TransportGrid, TransportState, TransportationGroup, Transporter,
    TraversalOwner,
};
use crate::transportation::{Vehicle, VehicleState, TIME_TO_PARK};
use crate::utils::resources::Resources;
use crate::world::{VehicleEnt, VehicleID};
use crate::ParCommandBuffer;
use crate::World;
use flat_spatial::grid::GridHandle;
use geom::{angle_lerpxy, Ray, Segment3, Transform, Vec2, Vec3};
use prototypes::{GameTime, DELTA, TICKS_PER_REALTIME_SECOND};
use slotmapd::Key;
use std::collections::BTreeMap;

const STOPPED_SPEED: f32 = 0.2;
const STOPPED_BLOCK_DISTANCE: f32 = 1.5;

/// Describe the hold on one stopped vehicle, reusing a current traffic-repair
/// observation when available. The fallback reads only its immediate surroundings.
pub fn vehicle_hold_reason(
    sim: &crate::Simulation,
    map: &Map,
    id: VehicleID,
) -> Option<QueueHoldReason> {
    let vehicle = sim.get(id)?;
    if vehicle.speed.0.abs() > STOPPED_SPEED {
        return None;
    }
    let traversal = vehicle.it.get_travers().copied();
    if let Some(traversal) = traversal {
        let tick = crate::diagnostics::elapsed_tick(sim).0;
        if let Some(reason) = sim.read::<QueueDiagnostics>().recent_hold(
            id,
            traversal,
            QueueRevision::current(map),
            tick,
        ).filter(|reason| *reason != QueueHoldReason::Unknown) {
            return Some(reason);
        }
    }

    vehicle.it.get_point()?;
    if junction_admission::controls_road_decision(map, &vehicle.it, vehicle.trans.pos) {
        // Configured admission owns this decision. Nearby vehicles and the lane
        // sign cannot distinguish a conflict, dwell, or blocked receiving lane.
        return Some(QueueHoldReason::Unknown);
    }
    let grid = sim.read::<TransportGrid>();
    let collider = vehicle.collider?.0;
    let (_, own_state) = grid.get(collider)?;
    let neighbours = grid.query_around(vehicle.trans.pos.xy(), 12.0)
        .filter(|&(handle, _)| handle != collider)
        .filter_map(|(handle, pos)| Some((Some(handle), pos, grid.get(handle)?.1)));
    let (distance, _, reason) = calc_front_dist(
        &vehicle.vehicle,
        map,
        &vehicle.trans,
        own_state,
        &vehicle.it,
        RoundaboutArea::at(map, &vehicle.it, vehicle.trans.pos),
        neighbours,
        0.8,
    );
    if distance < STOPPED_BLOCK_DISTANCE && reason != QueueHoldReason::Unknown {
        return Some(reason);
    }
    let TraverseKind::Lane(lane) = traversal?.kind else {
        return None;
    };
    let lane = map.get(lane)?;
    let distance = lane.control_point().distance(vehicle.trans.pos);
    match lane.control.get_behavior(sim.read::<GameTime>().seconds) {
        TrafficBehavior::RED | TrafficBehavior::ORANGE
            if distance <= OBJECTIVE_OK_DIST * 1.05 + 2.0
                + (vehicle.vehicle.kind.width() * 0.5 - OBJECTIVE_OK_DIST).max(0.0) =>
        {
            Some(QueueHoldReason::Signal)
        }
        TrafficBehavior::STOP if (1.0..=5.0).contains(&distance) => Some(QueueHoldReason::StopDwell),
        _ => None,
    }
}

/// Finds physical wait cycles among vehicles that are still stopped at the cutoff.
/// Each cycle is ordered by its blocker links and starts at its lowest vehicle ID.
pub fn movement_cycles_at_cutoff(sim: &crate::Simulation) -> Vec<Vec<VehicleID>> {
    let world = sim.world();
    let map = sim.map();
    // The simulation grid was last synchronized before itinerary movement.
    let mut grid = sim.read::<TransportGrid>().clone();
    grid.refresh_admission_colliders(world);
    let transit = sim.read::<Transit>();
    let cutoff = crate::diagnostics::elapsed_tick(sim);
    let mut blockers = BTreeMap::new();
    for (id, vehicle) in world.vehicles.iter() {
        if vehicle.speed.0.abs() >= STOPPED_SPEED
            || !matches!(
                vehicle.vehicle.state,
                VehicleState::Driving
                    | VehicleState::Panicking(_)
                    | VehicleState::ReturningToDepot { .. }
            )
            || vehicle.it.get_point().is_none()
            || transit
                .runs
                .get(&id)
                .and_then(|run| run.dwell_until)
                .is_some_and(|until| until > cutoff)
        {
            continue;
        }
        let Some(Transporter(collider)) = vehicle.collider else {
            continue;
        };
        let Some((position, state)) = grid.get(collider) else {
            continue;
        };
        let stop_dist = state.speed.powi(2) / (2.0 * vehicle.vehicle.kind.deceleration());
        let neighbours = grid
            .query_around(position, 12.0 + stop_dist)
            .filter(|&(handle, _)| handle != collider)
            .map(|(handle, pos)| (Some(handle), pos, grid.get(handle).unwrap().1));
        let (distance, _, reason) = calc_front_dist(
            &vehicle.vehicle,
            &map,
            &vehicle.trans,
            state,
            &vehicle.it,
            RoundaboutArea::at(&map, &vehicle.it, vehicle.trans.pos),
            neighbours,
            (0.8 + stop_dist).min(STOPPED_BLOCK_DISTANCE),
        );
        if distance >= STOPPED_BLOCK_DISTANCE {
            continue;
        }
        let handle = match reason {
            QueueHoldReason::Leader(handle) | QueueHoldReason::Conflict(Some(handle)) => handle,
            _ => continue,
        };
        if let ColliderOwnerLookup::Owned(TraversalOwner::Vehicle(blocker)) =
            grid.collider_owner(world, handle)
        {
            blockers.insert(id, blocker);
        }
    }

    let mut cycles = Vec::new();
    while let Some((&start, _)) = blockers.first_key_value() {
        let mut path = Vec::new();
        let mut current = start;
        while let Some(blocker) = blockers.remove(&current) {
            path.push(current);
            current = blocker;
        }
        if let Some(entry) = path.iter().position(|&id| id == current) {
            let mut cycle = path.split_off(entry);
            let (first, _) = cycle.iter().enumerate().min_by_key(|(_, id)| *id).unwrap();
            cycle.rotate_left(first);
            cycles.push(cycle);
        }
    }
    cycles.sort_unstable();
    cycles
}

fn staging_clearance(
    it: &Itinerary,
    trans: &Transform,
    half: f32,
    ahead: &Itinerary,
    lead: &Transform,
    lead_to_port: f32,
    limit: f32,
) -> f32 {
    let mut lead_path = Vec::new();
    let mut from = lead.pos;
    let mut reach = lead_to_port + half;
    for &to in ahead.local_path().iter().rev().take(64) {
        let offset = to - from;
        lead_path.push(geom::Segment::new(
            from.xy(),
            (from + offset.cap_magnitude(reach)).xy(),
        ));
        reach -= offset.mag();
        if reach <= 0.0 {
            break;
        }
        from = to;
    }
    let mut distance: f32 = 0.0;
    let mut from = trans.pos;
    for &to in it.local_path().iter().rev().take(64) {
        let offset = to - from;
        let Some(direction) = offset.try_normalize() else {
            continue;
        };
        let nose = direction * (half - VEHICLE_BODY_RADIUS);
        let overlaps = |position: Vec3| {
            let front = (position + nose).xy();
            lead_path.iter().any(|line| {
                line.project(front)
                    .is_close(front, VEHICLE_BODY_RADIUS * 2.0 + GAP)
            })
        };
        if overlaps(to) {
            if overlaps(from) {
                return (distance - 0.1).max(0.0);
            }
            let mut open = 0.0;
            let mut closed = 1.0;
            for _ in 0..7 {
                let middle = (open + closed) * 0.5;
                if overlaps(from + offset * middle) {
                    closed = middle;
                } else {
                    open = middle;
                }
            }
            return (distance + offset.mag() * open - 0.1).max(0.0);
        }
        distance += offset.mag();
        if distance >= limit {
            break;
        }
        from = to;
    }
    distance
}

fn roundabout_merge_limits(
    world: &World,
    map: &Map,
    grid: &TransportGrid,
) -> BTreeMap<VehicleID, (f32, GridHandle)> {
    let entering = map
        .configured_junctions()
        .filter_map(|junction| {
            if !map.is_yield_roundabout(junction) {
                return None;
            }
            let intersection = map.get(junction)?;
            Some((junction, intersection.pos.xy(), intersection.radius + 36.0))
        })
        .flat_map(|(junction, center, reach)| {
            grid.query_around(center, reach)
                .filter_map(move |(handle, _)| {
                    let ColliderOwnerLookup::Owned(TraversalOwner::Vehicle(id)) =
                        grid.collider_owner(world, handle)
                    else {
                        return None;
                    };
                    let vehicle = world.vehicles.get(id)?;
                    let TraverseKind::Turn(turn) = vehicle.it.get_travers()?.kind else {
                        return None;
                    };
                    (turn.parent == junction).then_some((id, vehicle, turn))
                })
        });
    let mut limits = BTreeMap::new();
    for (id, vehicle, turn) in entering {
        let half = vehicle.vehicle.kind.width() * 0.5;
        let braking = vehicle.speed.0.powi(2) / (2.0 * vehicle.vehicle.kind.deceleration());
        let staging_space = 2.0 * half + GAP;
        let Some(entry) = RingEntry::approaching(
            map,
            &vehicle.it,
            &vehicle.trans,
            half - VEHICLE_BODY_RADIUS,
            braking + staging_space + 1.0,
        ) else {
            continue;
        };
        let port = entry.center.xy() + entry.port;
        let mut closest = None;
        for (handle, _) in grid.query_around(port, 35.0) {
            let ColliderOwnerLookup::Owned(TraversalOwner::Vehicle(other)) =
                grid.collider_owner(world, handle)
            else {
                continue;
            };
            if other == id {
                continue;
            }
            let actor = &world.vehicles[other];
            let Some(other_step) = actor.it.get_travers() else {
                continue;
            };
            if !matches!(other_step.kind, TraverseKind::Turn(other_turn) if other_turn.parent == turn.parent)
            {
                continue;
            }
            let other_half = actor.vehicle.kind.width() * 0.5;
            if let Some(other_entry) = RingEntry::approaching(
                map,
                &actor.it,
                &actor.trans,
                other_half - VEHICLE_BODY_RADIUS,
                f32::INFINITY,
            ) {
                // Only one driver from the converging entry lanes can use the port.
                if matches!(other_step.kind, TraverseKind::Turn(other_turn) if other_turn.src == turn.src)
                    || !other_entry.port.is_close(entry.port, 2.8)
                    || (
                        ordered_float::OrderedFloat(other_entry.distance_to_port),
                        other,
                    ) >= (ordered_float::OrderedFloat(entry.distance_to_port), id)
                {
                    continue;
                }
                let distance = entry.distance_to_hold.min(staging_clearance(
                    &vehicle.it,
                    &vehicle.trans,
                    half,
                    &actor.it,
                    &actor.trans,
                    other_entry.distance_to_port,
                    staging_space,
                ));
                if closest.is_none_or(|(previous, _)| distance < previous) {
                    closest = Some((distance, handle));
                }
                continue;
            }
            let mut from = actor.trans.pos;
            let outward = from.xy() - entry.center.xy();
            if actor.speed.0 > 2.0
                && (outward.mag() - entry.port.mag()).abs() < 0.3
                && actor.trans.dir.xy().dot(-outward.perpendicular()) > 0.7 * outward.mag()
            {
                let signed_distance = outward.angle(entry.port) * entry.port.mag();
                let accel = vehicle.vehicle.kind.acceleration();
                let speed = vehicle.speed.0;
                let arrival = 2.0 * entry.distance_to_port
                    / (speed + (speed * speed + 2.0 * accel * entry.distance_to_port).sqrt());
                if signed_distance < 0.0
                    && -signed_distance + actor.speed.0 * (arrival - 0.35).max(0.0)
                        > half + other_half + GAP
                {
                    continue;
                }
            }
            let mut reach = (actor.speed.0 * 1.05).max(4.0) + other_half;
            let zone = half + 2.0;
            let mut occupied = port.is_close(from.xy(), zone);
            for &to in actor.it.local_path().iter().rev().take(256) {
                let offset = to - from;
                let end = from + offset.cap_magnitude(reach);
                if Segment3::new(from, end)
                    .flatten()
                    .project(port)
                    .is_close(port, zone)
                {
                    occupied = true;
                    break;
                }
                reach -= offset.mag();
                if reach <= 0.0 {
                    break;
                }
                from = to;
            }
            if occupied && closest.is_none_or(|(previous, _)| entry.distance_to_hold < previous) {
                closest = Some((entry.distance_to_hold, handle));
            }
        }
        if let Some(limit) = closest {
            limits.insert(id, limit);
        }
    }
    limits
}

pub fn vehicle_decision_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("transportation::vehicle_decision_system");
    let map = &*resources.read::<Map>();
    let time = &*resources.read::<GameTime>();
    let grid = &*resources.read::<TransportGrid>();
    let mut diagnostics = resources.write::<QueueDiagnostics>();
    let observing = diagnostics.observing();
    let tick = if observing {
        let tick = crate::map_dynamic::transit::elapsed_ticks(resources).0;
        diagnostics.begin_step(map, tick);
        tick
    } else {
        0
    };

    let merge_limits = roundabout_merge_limits(world, map, grid);
    for (id, entity) in world.vehicles.iter_mut() {
        let Some(collider) = entity.collider else {
            continue;
        };
        let traversal = observing
            .then(|| entity.it.get_travers().copied())
            .flatten();
        let watched = diagnostics.watches(traversal);
        let mut hold = if watched {
            vehicle_decision_traced::<true>(
                map,
                time,
                grid,
                id,
                &mut entity.it,
                &mut entity.trans,
                &mut entity.speed,
                &mut entity.vehicle,
                &collider,
            )
        } else {
            vehicle_decision(
                map,
                time,
                grid,
                id,
                &mut entity.it,
                &mut entity.trans,
                &mut entity.speed,
                &mut entity.vehicle,
                &collider,
            );
            None
        };
        if let Some(&(distance, blocker)) = merge_limits.get(&id) {
            let braking = (2.0 * entity.vehicle.kind.deceleration() * distance).sqrt();
            let cap = braking.min(distance / DELTA);
            if entity.speed.0 > cap {
                entity.speed.0 = cap;
                hold = Some(QueueHoldReason::Conflict(Some(blocker)));
            }
        }
        if let Some(reason) = hold.filter(|_| watched) {
            diagnostics.record_hold(id, traversal, tick, reason);
        }
    }
}

pub fn vehicle_decision(
    map: &Map,
    time: &GameTime,
    cow: &TransportGrid,
    me: VehicleID,
    it: &mut Itinerary,
    trans: &mut Transform,
    kin: &mut Speed,
    vehicle: &mut Vehicle,
    collider: &Transporter,
) {
    vehicle_decision_traced::<false>(map, time, cow, me, it, trans, kin, vehicle, collider);
}

fn vehicle_decision_traced<const OBSERVE: bool>(
    map: &Map,
    time: &GameTime,
    grid: &TransportGrid,
    id: VehicleID,
    it: &Itinerary,
    trans: &mut Transform,
    speed: &mut Speed,
    vehicle: &mut Vehicle,
    collider: &Transporter,
) -> Option<QueueHoldReason> {
    let (_, own_state) = grid.get(collider.0).expect("Handle not in transport grid");
    let ((desired_speed, desired_dir), reason) = if matches!(
        vehicle.state,
        VehicleState::Driving | VehicleState::Panicking(_) | VehicleState::ReturningToDepot { .. }
    ) {
        let danger_length =
            (own_state.speed.powi(2) / (2.0 * vehicle.kind.deceleration())).min(100.0);
        let neighbours =
            grid.query_around(trans.pos.xy(), 12.0 + danger_length)
                .map(|(handle, pos)| {
                    let state = grid.get(handle).expect("Handle not in transport grid").1;
                    (OBSERVE.then_some(handle), pos, state)
                });
        calc_decision_traced(id, vehicle, map, time, trans, own_state, it, neighbours)
    } else {
        ((0.0, Vec3::ZERO), None)
    };

    physics(
        trans,
        speed,
        vehicle,
        own_state,
        map,
        desired_speed,
        desired_dir,
    );
    reason
}

pub fn vehicle_state_update_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("transportation::vehicle_state_update_system");
    let buffer = &*resources.read::<ParCommandBuffer<VehicleEnt>>();
    let map = &*resources.read::<Map>();
    let validation = &mut world.parking_validation;
    let revision = (map.identity(), map.parking.revision());
    let changed = validation.revision.replace(revision) != Some(revision);

    let recently_parked = &mut validation.recently_parked;
    if !changed {
        for ent in recently_parked.drain(..) {
            let Some(v) = world.vehicles.get_mut(ent) else {
                continue;
            };
            if matches!(v.vehicle.state, VehicleState::Parked(_)) {
                vehicle_state_update(
                    buffer,
                    map,
                    ent,
                    &mut v.vehicle,
                    &mut v.trans,
                    &mut v.speed,
                    &mut v.collider,
                );
            }
        }
    }
    recently_parked.clear();

    world.vehicles.iter_mut().for_each(|(ent, v)| {
        if !changed && matches!(v.vehicle.state, VehicleState::Parked(_)) {
            return;
        }
        if let VehicleState::ReturningToDepot {
            spot,
            stopped_ticks,
        } = &mut v.vehicle.state
        {
            const MAX_STOPPED_TICKS: u16 = 1_000;
            *stopped_ticks = if v.speed.0.abs() < 0.2 {
                stopped_ticks.saturating_add(1)
            } else {
                0
            };
            if !spot.exists(&map.parking)
                || v.it.is_wait_for_reroute().is_some()
                || v.it.has_ended(0.0)
                || *stopped_ticks >= MAX_STOPPED_TICKS
            {
                buffer.exec_ent(ent, move |sim| {
                    crate::souls::lifecycle::finish_truck_return(sim, ent);
                });
            }
            return;
        }
        let was_parking = matches!(v.vehicle.state, VehicleState::RoadToPark(..));
        vehicle_state_update(
            buffer,
            map,
            ent,
            &mut v.vehicle,
            &mut v.trans,
            &mut v.speed,
            &mut v.collider,
        );
        if was_parking && matches!(v.vehicle.state, VehicleState::Parked(_)) {
            recently_parked.push(ent);
        }
    });
}

#[derive(Default)]
pub(crate) struct ParkingValidation {
    revision: Option<(u64, u64)>,
    recently_parked: Vec<VehicleID>,
}

/// Decides whether a vehicle should change states, from parked to unparking to driving etc
pub fn vehicle_state_update(
    buf: &ParCommandBuffer<VehicleEnt>,
    map: &Map,
    ent: VehicleID,
    vehicle: &mut Vehicle,
    trans: &mut Transform,
    kin: &mut Speed,
    coll: &mut Option<Transporter>,
) {
    match vehicle.state {
        VehicleState::RoadToPark(_, ref mut t, _) => {
            // Vehicle is on rails when parking.
            *t += DELTA / TIME_TO_PARK;

            if *t >= 1.0 {
                let v = coll.take();
                if let Some(x) = v {
                    buf.exec_ent(ent, x.destroy());
                }
                kin.0 = 0.0;
                let spot = match std::mem::replace(&mut vehicle.state, VehicleState::Driving) {
                    VehicleState::RoadToPark(_, _, spot) => spot,
                    _ => unreachable!(),
                };
                vehicle.state = VehicleState::Parked(spot);
            }
        }
        VehicleState::Parked(ref spot) => {
            if let Some(p) = spot.get(&map.parking) {
                if p.trans != *trans {
                    *trans = p.trans;
                }
            } else {
                buf.kill(ent);
            }
        }
        _ => {}
    }
}

/// Handles actually moving the vehicles around, including acceleration and other physics stuff.
fn physics(
    trans: &mut Transform,
    kin: &mut Speed,
    vehicle: &mut Vehicle,
    obj: &TransportState,
    map: &Map,
    desired_speed: f32,
    desired_dir: Vec3,
) {
    match vehicle.state {
        VehicleState::Parked(ref id) => {
            let spot = unwrap_ret!(id.get(&map.parking));
            *trans = spot.trans;
            return;
        }
        VehicleState::RoadToPark(spline, t, _) => {
            trans.pos = spline.get(t);
            trans.dir = spline.derivative(t).normalize();
            return;
        }
        _ => {}
    }

    let speed = obj.speed;
    let kind = vehicle.kind;

    let speed = speed
        + (desired_speed - speed).clamp(-DELTA * kind.deceleration(), DELTA * kind.acceleration());

    let max_ang_vel = (speed.abs() / kind.min_turning_radius()).clamp(0.0, 3.0);

    let approx_angle = trans.dir.distance(desired_dir);

    vehicle.ang_velocity += DELTA * kind.ang_acc();
    vehicle.ang_velocity = vehicle
        .ang_velocity
        .min(4.0 * approx_angle)
        .min(max_ang_vel);

    trans.dir = angle_lerpxy(trans.dir, desired_dir, vehicle.ang_velocity * DELTA);

    kin.0 = speed;
}

/// Decide the appropriate velocity and direction to aim for.
pub fn calc_decision<'a>(
    me: VehicleID,
    vehicle: &mut Vehicle,
    map: &Map,
    time: &GameTime,
    trans: &Transform,
    self_obj: &TransportState,
    it: &Itinerary,
    neighs: impl Iterator<Item = (Vec2, &'a TransportState)>,
) -> (f32, Vec3) {
    calc_decision_traced(
        me,
        vehicle,
        map,
        time,
        trans,
        self_obj,
        it,
        neighs.map(|(pos, state)| (None, pos, state)),
    )
    .0
}

fn calc_decision_traced<'a>(
    me: VehicleID,
    vehicle: &mut Vehicle,
    map: &Map,
    time: &GameTime,
    trans: &Transform,
    self_obj: &TransportState,
    it: &Itinerary,
    neighs: impl Iterator<Item = (Option<GridHandle>, Vec2, &'a TransportState)>,
) -> ((f32, Vec3), Option<QueueHoldReason>) {
    let default_return = ((0.0, trans.dir), Some(QueueHoldReason::Unknown));
    if vehicle.wait_time > 0.0 {
        vehicle.wait_time -= DELTA;
        return default_return;
    }
    let Some(objective) = it.get_point() else {
        let reason = if it.is_wait_for_reroute().is_some() {
            QueueHoldReason::NoRoute
        } else {
            QueueHoldReason::Unknown
        };
        return ((0.0, trans.dir), Some(reason));
    };

    let speed = self_obj.speed;
    let time_to_stop = speed / vehicle.kind.deceleration();
    let stop_dist = time_to_stop * speed * 0.5;

    let controlled = junction_admission::controls_road_decision(map, it, trans.pos);
    if controlled && matches!(vehicle.state, VehicleState::Panicking(_)) {
        vehicle.state = VehicleState::Driving;
        vehicle.flag = 0;
    }
    let area = RoundaboutArea::at(map, it, trans.pos);
    let roundabout = area.contains(trans.pos);
    let stopped_gap = if roundabout {
        GAP
    } else {
        STOPPED_BLOCK_DISTANCE
    };
    let stopping_threshold = 0.8 + stop_dist;
    let (front_dist, flag, blocker) = calc_front_dist(
        vehicle,
        map,
        trans,
        self_obj,
        it,
        area,
        neighs,
        stopping_threshold.min(stopped_gap),
    );

    let position = trans.pos;
    let dir_to_pos = unwrap_or!(
        (objective - position).try_normalize(),
        return default_return
    );

    if let VehicleState::Panicking(since) = vehicle.state {
        // Gridlock recovery is agent physics: 20 real seconds.
        if since.elapsed(time).0 .0 > 20 * TICKS_PER_REALTIME_SECOND {
            vehicle.state = VehicleState::Driving;
        }
    } else if speed.abs() < STOPPED_SPEED && front_dist < stopped_gap {
        let me_u64: u64 = me.data().as_ffi();
        if !controlled && me_u64 == flag && matches!(vehicle.state, VehicleState::Driving) {
            vehicle.state = VehicleState::Panicking(time.instant());
            log::info!("gridlock!")
        }
        vehicle.flag = if vehicle.flag | flag == 0 {
            me_u64
        } else {
            flag
        };
        if !roundabout {
            vehicle.wait_time = (position.x * 1000.0).fract().abs() * 0.5;
        }
        return ((0.0, trans.dir), Some(blocker));
    } else {
        // Stop at 80 cm of object in front
        if front_dist < stopping_threshold {
            return ((0.0, dir_to_pos), Some(blocker));
        }
    }

    vehicle.flag = 0;

    if let Some(term_pos) = it.get_terminal() {
        if term_pos.is_close(position, stop_dist) {
            return ((0.0, dir_to_pos), None);
        }
    }

    let mut speed = 9.0;

    if let Some(Traversable {
        kind: TraverseKind::Lane(l_id),
        ..
    }) = it.get_travers()
    {
        if let Some(l) = map.lanes().get(*l_id) {
            speed = l.speed_limit;
            if !controlled {
                let light = l.control_point();

                match l.control.get_behavior(time.seconds) {
                    TrafficBehavior::RED | TrafficBehavior::ORANGE => {
                        if light.is_close(
                            position,
                            OBJECTIVE_OK_DIST * 1.05
                                + 2.0
                                + stop_dist
                                + (vehicle.kind.width() * 0.5 - OBJECTIVE_OK_DIST).max(0.0),
                        ) {
                            return ((0.0, dir_to_pos), Some(QueueHoldReason::Signal));
                        }
                    }
                    TrafficBehavior::STOP => {
                        if light.is_close_signed(position, stop_dist)
                            && !light.is_close(position, 1.0)
                        {
                            return ((0.0, dir_to_pos), Some(QueueHoldReason::StopDwell));
                        }
                    }
                    TrafficBehavior::GREEN => {
                        if light.is_close(position, stop_dist * 0.4) {
                            return ((0.0, dir_to_pos), Some(QueueHoldReason::Unknown));
                        }
                    }
                }
            }
        }
    }

    // Not facing the objective
    if dir_to_pos.dot(trans.dir) < 0.8 {
        return ((6.0, dir_to_pos), None);
    }

    (
        (
            vehicle.kind.speed_factor() * vehicle.max_speed_multiplier * speed,
            dir_to_pos,
        ),
        None,
    )
}

const ROUNDABOUT_ARM_REACH: f32 = 24.0;

#[derive(Clone, Copy)]
enum RoundaboutArea {
    Ordinary,
    Ring(RoundaboutGeometry),
    Arm {
        start: Vec3,
        end: Vec3,
        near_start: bool,
        near_end: bool,
    },
}

impl RoundaboutArea {
    fn at(map: &Map, itinerary: &Itinerary, position: Vec3) -> Self {
        let Some(step) = itinerary.get_travers() else {
            return Self::Ordinary;
        };
        if !map.needs_junction_entry_checks() {
            return Self::Ordinary;
        }
        match step.kind {
            TraverseKind::Turn(turn) if map.is_yield_roundabout(turn.parent) => map
                .get(turn.parent)
                .and_then(|junction| junction.roundabout_geometry())
                .map_or(Self::Ordinary, Self::Ring),
            TraverseKind::Lane(id) => {
                let Some(lane) = map.get(id) else {
                    return Self::Ordinary;
                };
                let start = lane.points.first();
                let end = lane.points.last();
                let near_start = start.is_close(position, ROUNDABOUT_ARM_REACH)
                    && map.is_yield_roundabout(lane.src);
                let near_end = end.is_close(position, ROUNDABOUT_ARM_REACH)
                    && map.is_yield_roundabout(lane.dst);
                if near_start || near_end {
                    Self::Arm {
                        start,
                        end,
                        near_start,
                        near_end,
                    }
                } else {
                    Self::Ordinary
                }
            }
            _ => Self::Ordinary,
        }
    }

    // The encounter can be at the far end of a short outbound lane.
    fn contains(self, position: Vec3) -> bool {
        match self {
            Self::Ordinary => false,
            Self::Ring(_) => true,
            Self::Arm {
                start,
                end,
                near_start,
                near_end,
            } => {
                let from_start = position.distance2(start);
                let from_end = position.distance2(end);
                if from_start < from_end {
                    near_start && from_start < ROUNDABOUT_ARM_REACH.powi(2)
                } else {
                    near_end && from_end < ROUNDABOUT_ARM_REACH.powi(2)
                }
            }
        }
    }
}

struct RingEntry {
    center: Vec3,
    port: Vec2,
    distance_to_hold: f32,
    distance_to_port: f32,
}

impl RingEntry {
    fn approaching(
        map: &Map,
        it: &Itinerary,
        trans: &Transform,
        bumper: f32,
        reach: f32,
    ) -> Option<Self> {
        let TraverseKind::Turn(id) = it.get_travers()?.kind else {
            return None;
        };
        let intersection = map.get(id.parent)?;
        let ring = intersection.roundabout_geometry()?;
        let keep_clear = ring.radius + 4.6;
        let nose = trans.pos + trans.dir * bumper;
        if nose.xy().distance(ring.center.xy()) < keep_clear - 0.03 {
            return None;
        }
        let turn = intersection.raw_turn(id)?;
        let merge = turn.points.as_slice()[junction_admission::roundabout_merge_index(turn, ring)];
        let path = it.local_path();
        // The arc join disappears from the itinerary once the driver has merged.
        if !path.contains(&merge) {
            return None;
        }
        let mut distance = 0.0;
        let mut from = trans.pos;
        for &to in path.iter().rev() {
            let delta = to - from;
            let length = delta.mag();
            let Some(direction) = delta.try_normalize() else {
                continue;
            };
            let body = from + direction * bumper;
            let radial = body.xy() - ring.center.xy();
            let tangent = direction.xy();
            // Find where the oriented nose first reaches the circulating
            // lane's clearance envelope along this straight piece.
            let along = radial.dot(tangent);
            let cross = along * along - tangent.mag2() * (radial.mag2() - keep_clear * keep_clear);
            if cross >= 0.0 {
                let entry = (-along - cross.sqrt()) / tangent.mag2();
                if entry <= length && (entry >= 0.0 || radial.mag() <= keep_clear) {
                    let distance_to_hold = (distance + entry.max(0.0) - 0.08).max(0.0);
                    return (distance_to_hold <= reach).then_some(Self {
                        center: ring.center,
                        port: merge.xy() - ring.center.xy(),
                        distance_to_hold,
                        distance_to_port: merge.xy().distance(trans.pos.xy()),
                    });
                }
            }
            distance += length;
            if distance > reach || to == merge {
                break;
            }
            from = to;
        }
        None
    }
}

fn can_clear_merge_contact(
    ring: crate::map::RoundaboutGeometry,
    it: &Itinerary,
    trans: &Transform,
    bumper: f32,
    other: &TransportState,
    footprint: Segment3,
) -> bool {
    let outward = footprint.middle().xy() - ring.center.xy();
    if (trans.pos.xy().distance(ring.center.xy()) - ring.radius).abs() > 0.25
        || outward.mag() <= ring.radius + 0.8
        || other.speed > 0.2
        || other.dir.dot(outward) > -0.35 * ring.radius
        || (footprint.src.z - trans.pos.z).abs() > 0.65
    {
        return false;
    }
    let body = trans.dir * bumper;
    let current = Segment3::new(trans.pos - body, trans.pos + body).flatten();
    let footprint = footprint.flatten();
    let gap = current.distance(&footprint);
    if gap > 2.0 * VEHICLE_BODY_RADIUS + GAP {
        return false;
    }
    let Some(step) = it
        .get_point()
        .and_then(|point| (point - trans.pos).try_normalize())
    else {
        return false;
    };
    let next = geom::Segment {
        src: current.src + step.xy() * 0.08,
        dst: current.dst + step.xy() * 0.08,
    };
    // A circulating vehicle can move out of a pinched merge while the
    // entry is stopped, provided the bodies do not move any closer.
    next.distance(&footprint) + 0.0001 >= gap
}

fn braking_path_conflicts(
    map: &Map,
    it: &Itinerary,
    trans: &Transform,
    bumper: f32,
    braking_distance: f32,
    obstacle: Segment3,
    clearance: f32,
) -> bool {
    const DECK_TOLERANCE: f32 = 0.65;
    const MAX_LOOKAHEAD_POINTS: usize = 256;
    let mut reach = braking_distance + bumper + GAP;
    let obstacle_xy = obstacle.flatten();
    let origin = trans.pos.xy();
    if !obstacle_xy
        .project(origin)
        .is_close(origin, reach.max(bumper) + clearance + 0.01)
    {
        return false;
    }
    let overlaps = |from: Vec3, to: Vec3| {
        let height = obstacle.src.z;
        from.z.min(to.z) <= height + DECK_TOLERANCE
            && from.z.max(to.z) >= height - DECK_TOLERANCE
            && segments_close(
                Segment3::new(from.xy().z0(), to.xy().z0()),
                Segment3::new(obstacle.src.xy().z0(), obstacle.dst.xy().z0()),
                clearance.powi(2),
            )
    };
    if overlaps(trans.pos, trans.pos + trans.dir * bumper) {
        return true;
    }

    let next = it
        .get_route()
        .and_then(|route| route.reversed_route.last())
        .and_then(|leg| Some((leg.dir, leg.raw_points(map)?.as_slice())));
    let next = next.into_iter().flat_map(|(direction, points)| {
        (0..points.len()).map(move |i| match direction {
            TraverseDirection::Forward => points[i],
            TraverseDirection::Backward => points[points.len() - 1 - i],
        })
    });
    let mut from = trans.pos;
    for (i, to) in it
        .local_path()
        .iter()
        .rev()
        .copied()
        .chain(next)
        .enumerate()
    {
        if i == MAX_LOOKAHEAD_POINTS {
            return true;
        }
        let offset = to - from;
        if overlaps(from, from + offset.cap_magnitude(reach)) {
            return true;
        }
        reach -= offset.mag();
        if reach <= 0.0 {
            break;
        }
        from = to;
    }
    false
}

/// Calculates the distance to the closest problematic object in front of the car.
/// It can be another car or a pedestrian, or it can be a potential collision point from a
/// car coming perpendicularly.
fn calc_front_dist<'a>(
    vehicle: &Vehicle,
    map: &Map,
    trans: &Transform,
    self_obj: &TransportState,
    it: &Itinerary,
    area: RoundaboutArea,
    neighs: impl Iterator<Item = (Option<GridHandle>, Vec2, &'a TransportState)>,
    cutoff: f32,
) -> (f32, u64, QueueHoldReason) {
    if matches!(area, RoundaboutArea::Ordinary) {
        calc_front_dist_in_area::<false>(vehicle, map, trans, self_obj, it, area, neighs, cutoff)
    } else {
        calc_front_dist_in_area::<true>(vehicle, map, trans, self_obj, it, area, neighs, cutoff)
    }
}

fn calc_front_dist_in_area<'a, const ROUNDABOUT: bool>(
    vehicle: &Vehicle,
    map: &Map,
    trans: &Transform,
    self_obj: &TransportState,
    it: &Itinerary,
    area: RoundaboutArea,
    neighs: impl Iterator<Item = (Option<GridHandle>, Vec2, &'a TransportState)>,
    cutoff: f32,
) -> (f32, u64, QueueHoldReason) {
    let position = trans.pos;
    let direction = trans.dir;
    let pos2 = position.xy();
    let dir2 = trans.dir.xy();

    let mut min_front_dist: f32 = 50.0;

    let my_ray = Ray {
        from: position.xy() - direction.xy() * vehicle.kind.width() * 0.5,
        dir: direction.xy(),
    };

    let my_radius = self_obj.radius;
    let speed = self_obj.speed;

    let on_lane = it.get_travers().map_or(false, |t| t.kind.is_lane());
    let mut flag = 0;
    let mut blocker = QueueHoldReason::Unknown;
    let ring = match area {
        RoundaboutArea::Ring(ring) if ROUNDABOUT => Some(ring),
        _ => None,
    };
    let circulating = ring.is_some();
    let on_arm = ROUNDABOUT && matches!(area, RoundaboutArea::Arm { .. });
    let braking_distance = speed.powi(2) / (2.0 * vehicle.kind.deceleration());
    let bumper = my_radius - VEHICLE_BODY_RADIUS;
    // Collision avoidance
    for (handle, his_pos, nei_physics_obj) in neighs {
        if (nei_physics_obj.height - position.z).abs() > 5.0 {
            continue;
        }
        let towards_vec: Vec2 = his_pos - pos2;
        // Ignore myself and very close cars
        if towards_vec.is_close(Vec2::ZERO, 1.0) && towards_vec.x > 0.0 {
            continue;
        }

        let (towards_dir, dist) = unwrap_or!(towards_vec.dir_dist(), continue);

        // cos of angle from self to obj
        let cos_angle = towards_dir.dot(dir2);

        // Ignore things behind
        if cos_angle < 0.0 {
            continue;
        }

        let dist_to_side = towards_vec.perp_dot(dir2).abs();

        let is_vehicle = matches!(nei_physics_obj.group, TransportationGroup::Vehicles);

        let cos_direction_angle = nei_physics_obj.dir.dot(dir2);

        let pedestrian = his_pos.z(nei_physics_obj.height);
        let local_arm = on_arm && area.contains(pedestrian);
        let lateral_limit = if local_arm && is_vehicle {
            2.0 * VEHICLE_BODY_RADIUS + GAP
        } else {
            3.0 + speed * 0.3
        };
        let in_front_cone = cos_angle > 0.85 - 0.015 * speed.min(10.0)
            && (!is_vehicle || cos_direction_angle > 0.0)
            && (!on_lane || dist_to_side < lateral_limit);
        let front_distance = if ROUNDABOUT
            && nei_physics_obj.group == TransportationGroup::Pedestrians
            && (circulating || local_arm)
        {
            let time = speed / vehicle.kind.deceleration() + 0.25;
            let swept = Segment3::new(
                pedestrian,
                pedestrian + (nei_physics_obj.dir * nei_physics_obj.speed * time).z0(),
            );
            braking_path_conflicts(
                map,
                it,
                trans,
                bumper,
                braking_distance,
                swept,
                VEHICLE_BODY_RADIUS + nei_physics_obj.radius + GAP * (speed / 3.0).clamp(0.25, 1.0),
            )
            .then_some(0.0)
        } else if circulating && is_vehicle {
            // An entry's tangent can point across the ring even while the
            // driver is yielding at the mouth. Follow its physical body
            // along our route instead of intersecting the two heading rays.
            let body = (nei_physics_obj.dir * (nei_physics_obj.radius - VEHICLE_BODY_RADIUS)).z0();
            let footprint = Segment3::new(pedestrian - body, pedestrian + body);
            (braking_path_conflicts(
                map,
                it,
                trans,
                bumper,
                (braking_distance
                    - nei_physics_obj.speed.powi(2) * cos_direction_angle.max(0.0)
                        / (2.0 * vehicle.kind.deceleration()))
                .max(0.0),
                footprint,
                2.0 * VEHICLE_BODY_RADIUS,
            ) && !(speed <= 1.6
                && ring.is_some_and(|ring| {
                    can_clear_merge_contact(ring, it, trans, bumper, nei_physics_obj, footprint)
                })))
            .then_some(0.0)
        } else if in_front_cone {
            let moving_leader = if local_arm && is_vehicle {
                nei_physics_obj.speed.powi(2) * cos_direction_angle.max(0.0)
                    / (2.0 * vehicle.kind.deceleration())
            } else {
                0.0
            };
            Some(
                dist - my_radius - nei_physics_obj.radius + moving_leader
                    - if is_vehicle { 0.0 } else { 1.0 },
            )
        } else {
            None
        };
        if let Some(distance) = front_distance {
            if distance < min_front_dist {
                min_front_dist = distance;
                flag = nei_physics_obj.flag;
                blocker = match (handle, nei_physics_obj.group) {
                    (Some(handle), TransportationGroup::Vehicles) => {
                        QueueHoldReason::Leader(handle)
                    }
                    (Some(handle), TransportationGroup::Pedestrians) => {
                        QueueHoldReason::Pedestrian(handle)
                    }
                    _ => QueueHoldReason::Unknown,
                };
            }
            if min_front_dist < cutoff {
                return (min_front_dist, flag, blocker);
            }
            continue;
        }
        if !is_vehicle || circulating || local_arm {
            continue;
        }

        // closest win
        let his_ray = Ray {
            from: his_pos - nei_physics_obj.radius * nei_physics_obj.dir,
            dir: nei_physics_obj.dir,
        };

        let (my_dist, his_dist) = unwrap_or!(my_ray.both_dist_to_inter(&his_ray), continue);

        if my_dist.max(his_dist) > 1000.0 {
            continue;
        }

        if nei_physics_obj.speed <= 0.01 {
            continue;
        }

        if my_dist - speed.min(2.5) - my_radius
            < his_dist - nei_physics_obj.speed.min(2.5) - nei_physics_obj.radius
        {
            continue;
        }

        let final_dist = dist - my_radius - nei_physics_obj.radius - 5.0;
        if final_dist < min_front_dist {
            min_front_dist = final_dist;
            flag = nei_physics_obj.flag;
            blocker = QueueHoldReason::Conflict(handle);
        }
    }
    (min_front_dist, flag, blocker)
}

#[cfg(test)]
#[test]
fn p7_cutoff_detects_a_cycle_of_stationary_vehicle_blockers() {
    use crate::tests::TestCtx;
    use crate::transportation::{spawn_driving_vehicle, transport_grid_synchronize, VehicleKind};

    let mut ctx = TestCtx::new();
    let sim = &mut ctx.g;
    let mut ids = Vec::new();
    for n in 0..8 {
        let angle = std::f32::consts::TAU * n as f32 / 8.0;
        let (y, x) = angle.sin_cos();
        let pos = Vec3::new(200.0 + 7.0 * x, 200.0 + 7.0 * y, 0.0);
        let dir = Vec3::new(-y, x, 0.0);
        let trans = Transform::new_dir(pos, dir);
        let id = spawn_driving_vehicle(sim, VehicleKind::Car, trans).unwrap();
        let vehicle = &mut sim.world_mut_unchecked().vehicles[id];
        vehicle.vehicle.wait_time = 0.4;
        vehicle.it = Itinerary::simple(vec![pos + dir * 30.0]);
        ids.push(id);
    }
    let (world, resources) = sim.world_res();
    transport_grid_synchronize(world, resources);
    assert_eq!(movement_cycles_at_cutoff(sim), [ids.clone()]);

    sim.world_mut_unchecked().vehicles[ids[0]].speed.0 = 2.0;
    assert!(movement_cycles_at_cutoff(sim).is_empty());
}

#[cfg(test)]
#[test]
fn p7_unruled_panic_keeps_stock_gridlock_recovery() {
    use crate::transportation::{TransportationGroup, VehicleKind};
    use geom::{vec2, vec3, Color};

    let time = GameTime::new(prototypes::Tick(1));
    let map = Map::empty();
    let approach = Transform::new(vec3(0.0, 0.0, 0.0));
    let itinerary = Itinerary::simple(vec![vec3(30.0, 0.0, 0.0)]);
    for (speed, crossing_pos) in [(8.0, vec2(12.0, -3.0)), (9.0, vec2(10.0, -5.0))] {
        let car = TransportState {
            speed,
            radius: VehicleKind::Car.width() * 0.5,
            group: TransportationGroup::Vehicles,
            ..TransportState::default()
        };
        let crossing = TransportState {
            dir: Vec2::Y,
            speed: 1.0,
            ..car
        };
        let above = TransportState {
            height: 10.0,
            ..crossing
        };
        let drive = |state, neighbors: &[(Vec2, &TransportState)]| {
            let mut vehicle = Vehicle {
                ang_velocity: 0.0,
                wait_time: 0.0,
                max_speed_multiplier: 1.0,
                state,
                kind: VehicleKind::Car,
                tint: Color::WHITE,
                flag: 0,
            };
            calc_decision(
                VehicleID::default(),
                &mut vehicle,
                &map,
                &time,
                &approach,
                &car,
                &itinerary,
                neighbors.iter().copied(),
            )
            .0
        };
        let panic = || VehicleState::Panicking(time.instant());
        let leader = (vec2(1.5, 0.0), &car);
        assert_eq!(
            drive(VehicleState::Driving, &[(crossing_pos, &crossing)]),
            0.0
        );
        assert_eq!(drive(panic(), &[leader, (crossing_pos, &crossing)]), 9.0);
        assert!(drive(panic(), &[leader, (crossing_pos, &above)]) > 0.0);
    }
}

#[cfg(test)]
mod vehicle_destination_tests {
    use super::*;
    use crate::map::{ApproachRole, JunctionRules, TrafficControl, TurnKind};
    use crate::tests::TestCtx;
    use crate::transportation::{spawn_driving_vehicle, VehicleKind};
    use geom::vec3;

    #[test]
    fn vehicle_destination_configured_stop_uses_generic_hold_without_trace() {
        let mut ctx = TestCtx::new();
        let center = vec3(270.0, 180.0, 0.0);
        ctx.build_roads(&[vec3(70.0, 180.0, 0.0), center, vec3(470.0, 180.0, 0.0)]);
        ctx.build_roads(&[center, vec3(270.0, 390.0, 0.0)]);
        let (turn, road, entry, start, itinerary) = {
            let map = ctx.g.map();
            let intersection = map
                .intersections()
                .values()
                .find(|intersection| intersection.pos.xy() == center.xy())
                .unwrap();
            let turn = intersection
                .turns()
                .find(|turn| turn.kind == TurnKind::Driving)
                .unwrap()
                .id;
            let lane = map.get(turn.src).unwrap();
            let entry = lane.control_point();
            let start = lane.points.point_along(lane.points.length() - 3.0);
            let itinerary = Itinerary::route_between_lanes(
                ctx.g.read::<GameTime>().tick,
                &map,
                (turn.src, lane.points.length() - 3.0),
                (turn.dst, 30.0),
            )
            .unwrap();
            (turn, lane.parent, entry, start, itinerary)
        };
        ctx.g.map_mut().lanes[turn.src].control = TrafficControl::StopSign;
        let vehicle = spawn_driving_vehicle(
            &mut ctx.g,
            VehicleKind::Car,
            Transform::new_dir(start, (entry - start).normalize()),
        )
        .unwrap();
        ctx.g.world.vehicles[vehicle].it = itinerary;

        assert_eq!(
            vehicle_hold_reason(&ctx.g, &ctx.g.map(), vehicle),
            Some(QueueHoldReason::StopDwell)
        );
        let rules = JunctionRules::new(vec![(road, ApproachRole::Stop)], Vec::new()).unwrap();
        ctx.g.map_mut().install_junction_rules(turn.parent, rules);
        let map = ctx.g.map();
        let itinerary = &ctx.g.get(vehicle).unwrap().it;
        assert!(junction_admission::controls_road_decision(
            &map, itinerary, start
        ));
        assert_eq!(
            vehicle_hold_reason(&ctx.g, &map, vehicle),
            Some(QueueHoldReason::Unknown)
        );
    }
}
