use std::collections::{BTreeMap, VecDeque};

use geom::{Transform, Vec3};
use prototypes::{Tick, DELTA, TICKS_PER_HOUR, TICKS_PER_SECOND};

use super::{
    detach_for_replan, elapsed_ticks, walkable_egress, Berth, BusRun, Transit, TransitStop,
};
use crate::economy::Budget;
use crate::economy::{transit_stats, BudgetCategory, TransitStats, TripStats};
use crate::map::Map;
use crate::map_dynamic::{Itinerary, LineID, Router, RoutingStep, StopID};
use crate::transportation::{
    put_pedestrian_in_transport_grid, spawn_driving_vehicle, Location, TransportGrid, VehicleKind,
};
use crate::utils::resources::Resources;
use crate::{HumanEnt, HumanID, ParCommandBuffer, Simulation, VehicleEnt, VehicleID, World};

pub(super) const DWELL_TICKS: u64 = 30 * TICKS_PER_SECOND;
const GIVE_UP_TICKS: u64 = 2 * TICKS_PER_HOUR;
const MAX_CONSECUTIVE_SKIPS: u8 = 3;
const CAPACITY: usize = 40;
const BERTH_DISTANCE: f32 = 6.0;
const BOARDING_DISTANCE: f32 = 8.0;
const ARRIVAL_SPEED: f32 = 0.5;
const BUS_SPEED: f32 = 8.0;

pub(super) fn travel_ticks(distance: f32) -> u64 {
    // DELTA is the physics step in real seconds. Dwell uses game seconds.
    ((distance / (BUS_SPEED * DELTA)).ceil() as u64).max(1)
}

pub(super) fn route_ticks(map: &Map, it: &Itinerary, position: Vec3, to: Berth) -> u64 {
    let mut previous = position;
    let mut distance = 0.0;
    for &point in it.local_path().iter().rev() {
        distance += previous.distance(point);
        previous = point;
    }
    if let Some(route) = it.get_route() {
        for (index, traversal) in route.reversed_route.iter().enumerate() {
            if index == 0 {
                distance += to.offset;
            } else if let Some(points) = traversal.raw_points(map) {
                distance += points.length();
            }
        }
    }
    // Leave time for acceleration and braking even when two berths are close together.
    travel_ticks(distance).saturating_add(50)
}

pub(super) fn begin_leg(
    transit: &mut Transit,
    map: &Map,
    bus: VehicleID,
    vehicle: &mut VehicleEnt,
    now: Tick,
    from: Option<StopID>,
) {
    let run = &transit.runs[&bus];
    let line = &transit.lines[run.line];
    let target = line.stops[run.next_stop_idx];
    let Some(berth) = transit.berth(map, target) else {
        vehicle.it = Itinerary::NONE;
        return;
    };
    let start = from.and_then(|stop| transit.berth(map, stop));
    let it = if let Some(start) = start {
        Itinerary::route_between_lanes(now, map, start.lane_offset(), berth.lane_offset())
    } else {
        Itinerary::route_to_lane(now, vehicle.trans.pos, map, berth.lane, berth.offset)
    };
    let expected = it.as_ref().map_or_else(
        || expected_ticks(transit, run, vehicle.trans.pos, berth.pos),
        |it| route_ticks(map, it, vehicle.trans.pos, berth),
    );
    install_leg(
        transit,
        bus,
        vehicle,
        now,
        from,
        expected,
        it.unwrap_or_else(|| Itinerary::wait_for_reroute_to_lane(berth.lane, berth.offset)),
    );
}

fn expected_ticks(transit: &Transit, run: &BusRun, position: Vec3, berth: Vec3) -> u64 {
    let len = transit.lines[run.line].stops.len();
    let previous = (run.next_stop_idx + len - 1) % len;
    transit
        .leg_times
        .get(&run.line)
        .and_then(|times| times.get(previous))
        .copied()
        .unwrap_or(0)
        .max(travel_ticks(position.distance(berth)).saturating_add(50))
}

fn install_leg(
    transit: &mut Transit,
    bus: VehicleID,
    vehicle: &mut VehicleEnt,
    now: Tick,
    from: Option<StopID>,
    expected: u64,
    it: Itinerary,
) {
    vehicle.it = it;
    let run = transit.runs.get_mut(&bus).unwrap();
    run.leg_started = now;
    run.dwell_until = None;
    let leg = transit.legs.entry(bus).or_default();
    leg.from = from;
    leg.expected_ticks = expected;
}

fn at_berth(vehicle: &VehicleEnt, stop: &TransitStop) -> bool {
    vehicle.trans.pos.distance2(stop.berth) <= BERTH_DISTANCE * BERTH_DISTANCE
}

fn arrived(vehicle: &VehicleEnt, stop: &TransitStop) -> bool {
    at_berth(vehicle, stop) && vehicle.it.has_ended(0.0) && vehicle.speed.0 < ARRIVAL_SPEED
}

pub fn transit_dispatch_update(world: &mut World, resources: &mut Resources) {
    let mut transit = resources.write::<Transit>();
    if transit.runs.is_empty()
        && !transit
            .lines
            .values()
            .any(|line| line.target_vehicles > 0 && !line.suspended)
    {
        return;
    }
    let now = elapsed_ticks(resources);
    let map = resources.read::<Map>();
    let vehicles = resources.read::<ParCommandBuffer<VehicleEnt>>();
    let mut suspensions = Vec::new();
    let mut counts = BTreeMap::<LineID, usize>::new();

    let buses: Vec<_> = transit.runs.keys().copied().collect();
    for bus in buses {
        let Some(vehicle) = world.vehicles.get_mut(bus) else {
            continue;
        };
        let run = &transit.runs[&bus];
        let Some(line) = transit.lines.get(run.line) else {
            continue;
        };
        *counts.entry(run.line).or_default() += 1;
        let Some(&stop_id) = line.stops.get(run.next_stop_idx) else {
            continue;
        };
        let Some(stop) = transit.stops.get(stop_id) else {
            continue;
        };

        if let Some(until) = run.dwell_until {
            if now < until || line.suspended || line.target_vehicles == 0 {
                continue;
            }
            let next = (run.next_stop_idx + 1) % line.stops.len();
            transit.runs.get_mut(&bus).unwrap().next_stop_idx = next;
            begin_leg(&mut transit, &map, bus, vehicle, now, Some(stop_id));
        } else if arrived(vehicle, stop) {
            let run = transit.runs.get_mut(&bus).unwrap();
            run.dwell_until = Some(Tick(now.0.saturating_add(DWELL_TICKS)));
            run.failures = 0;
            vehicle.it = Itinerary::NONE;
        } else {
            let expected = transit
                .legs
                .get(&bus)
                .filter(|leg| leg.expected_ticks > 0)
                .map_or_else(
                    || expected_ticks(&transit, run, vehicle.trans.pos, stop.berth),
                    |leg| leg.expected_ticks,
                );
            if now.0.saturating_sub(run.leg_started.0) <= expected.saturating_mul(3) {
                continue;
            }
            if line.suspended {
                vehicles.kill(bus);
                continue;
            }
            let line_id = run.line;
            let next = (run.next_stop_idx + 1) % line.stops.len();
            let run = transit.runs.get_mut(&bus).unwrap();
            run.next_stop_idx = next;
            run.failures += 1;
            let suspend = run.failures >= MAX_CONSECUTIVE_SKIPS;
            transit.legs.entry(bus).or_default().skipped.insert(stop_id);
            if suspend {
                transit.suspend(line_id, now);
                suspensions.push((line_id, bus));
            } else {
                begin_leg(&mut transit, &map, bus, vehicle, now, None);
            }
        }
    }

    for (line, trigger) in suspensions {
        let buses: Vec<_> = transit
            .runs
            .iter()
            .filter_map(|(&bus, run)| (run.line == line).then_some(bus))
            .collect();
        for bus in buses {
            let Some(vehicle) = world.vehicles.get_mut(bus) else {
                continue;
            };
            let run = &transit.runs[&bus];
            // Buses dwelling or already following a route can serve that stop.
            if bus != trigger && (run.dwell_until.is_some() || vehicle.it.get_route().is_some()) {
                continue;
            }
            if !route_to_retirement_stop(&mut transit, &map, bus, vehicle, now) {
                vehicles.kill(bus);
            }
        }
    }

    let spawn: Vec<_> = transit
        .lines
        .iter()
        .filter_map(|(id, line)| {
            (!line.suspended
                && counts.get(&id).copied().unwrap_or(0) < line.target_vehicles as usize)
                .then_some(id)
        })
        .collect();
    if !spawn.is_empty() {
        // The scheduler drains this before boarding. A single callback keeps
        // spawn order stable when multiple lines share a berth.
        vehicles.exec_ent(VehicleID::default(), move |sim| {
            for line in spawn {
                spawn_bus(sim, line, now);
            }
        });
    }
}

fn route_to_retirement_stop(
    transit: &mut Transit,
    map: &Map,
    bus: VehicleID,
    vehicle: &mut VehicleEnt,
    now: Tick,
) -> bool {
    let run = &transit.runs[&bus];
    let stops = &transit.lines[run.line].stops;
    if let Some((index, berth, it)) = (0..stops.len()).find_map(|i| {
        let index = (run.next_stop_idx + i) % stops.len();
        let berth = transit.berth(map, stops[index])?;
        let it = Itinerary::route_to_lane(now, vehicle.trans.pos, map, berth.lane, berth.offset)?;
        Some((index, berth, it))
    }) {
        transit.runs.get_mut(&bus).unwrap().next_stop_idx = index;
        let expected = route_ticks(map, &it, vehicle.trans.pos, berth);
        install_leg(transit, bus, vehicle, now, None, expected, it);
        true
    } else {
        false
    }
}

fn spawn_bus(sim: &mut Simulation, line: LineID, now: Tick) {
    let berth = {
        let transit = sim.read::<Transit>();
        let Some(&stop) = transit.lines.get(line).and_then(|line| line.stops.first()) else {
            return;
        };
        transit.berth(&sim.map(), stop)
    };
    let Some(berth) = berth else { return };
    let trans = Transform {
        pos: berth.pos,
        dir: berth.dir,
    };
    let Some(bus) = spawn_driving_vehicle(sim, VehicleKind::Bus, trans) else {
        return;
    };
    sim.write::<Transit>().runs.insert(
        bus,
        BusRun {
            line,
            next_stop_idx: 0,
            dwell_until: Some(Tick(now.0.saturating_add(DWELL_TICKS))),
            leg_started: now,
            failures: 0,
        },
    );
}

struct StopService {
    line: LineID,
    stop: StopID,
    retiring: bool,
    force_alight: bool,
    remaining: usize,
}

pub fn transit_board_update(world: &mut World, resources: &mut Resources) {
    let mut transit = resources.write::<Transit>();
    if transit.runs.is_empty()
        && transit.trip_exclusions.is_empty()
        && !transit.lines.values().any(|line| line.target_vehicles > 0)
    {
        return;
    }
    let now = elapsed_ticks(resources);
    let map = resources.read::<Map>();
    let vehicles = resources.read::<ParCommandBuffer<VehicleEnt>>();
    let mut service = BTreeMap::<VehicleID, StopService>::new();
    let mut queues = BTreeMap::<(LineID, StopID), Vec<(Tick, HumanID)>>::new();
    let mut fleet = BTreeMap::<LineID, usize>::new();

    for (&bus, run) in &transit.runs {
        let Some(line) = transit.lines.get(run.line) else {
            continue;
        };
        let count = fleet.entry(run.line).or_default();
        *count += 1;
        if run.dwell_until.is_none() {
            continue;
        }
        let Some(stop) = line
            .stops
            .get(run.next_stop_idx)
            .and_then(|&id| transit.stops.get(id))
        else {
            continue;
        };
        let Some(vehicle) = world.vehicles.get(bus) else {
            continue;
        };
        if !at_berth(vehicle, stop) {
            continue;
        }
        let force_alight = line.suspended || line.target_vehicles == 0;
        let retiring = force_alight || *count > line.target_vehicles as usize;
        service.insert(
            bus,
            StopService {
                line: run.line,
                stop: stop.id,
                retiring,
                force_alight,
                remaining: 0,
            },
        );
        if !retiring {
            queues.entry((run.line, stop.id)).or_default();
        }
    }

    transit.trip_exclusions.retain(|&id, exclusions| {
        let Some(human) = world
            .humans
            .get(id)
            .filter(|human| exclusions.matches(&human.router))
        else {
            return false;
        };
        // A replacement destination may have been requested before the forced exit.
        exclusions.started = human.router.trip_start;
        true
    });
    for (id, human) in world.humans.iter_mut() {
        if let Location::Transit(bus) = human.location {
            if let Some(at) = service.get_mut(&bus) {
                alight_passenger(&mut transit, id, human, at, bus, resources);
            }
            continue;
        }
        if human.location != Location::Outside {
            continue;
        }
        human.router.stamp_wait_start(now);
        let Some(RoutingStep::WaitAtStop { stop, line, since }) = human.router.cur_step() else {
            // Routing can replace a wait after the boarding system has run.
            if human.collider.is_none() {
                let mut grid = resources.write::<TransportGrid>();
                let collider = put_pedestrian_in_transport_grid(&mut grid, human.trans.pos);
                grid.get_mut(collider.0).unwrap().1.height = human.trans.pos.z;
                human.collider = Some(collider);
            }
            continue;
        };
        let (stop, line, since) = (*stop, *line, *since);
        let timed_out = now.0.saturating_sub(since.0) > GIVE_UP_TICKS;
        let serving_line = transit.lines.get(line).is_some_and(|info| {
            !info.suspended && info.target_vehicles > 0 && info.stops.contains(&stop)
        });
        let Some(boarding_stop) = transit
            .stops
            .get(stop)
            .filter(|_| serving_line && !timed_out)
        else {
            let pos = walkable_egress(&map, human.trans.pos);
            detach_for_replan(
                &mut transit,
                id,
                human,
                pos,
                resources,
                (!timed_out).then_some(line),
            );
            if timed_out {
                let mut stats = resources.write::<TripStats>();
                stats.give_ups = stats.give_ups.saturating_add(1);
            }
            continue;
        };

        if let Some(collider) = human.collider.take() {
            resources
                .write::<TransportGrid>()
                .remove_maintain(collider.0);
        }
        human.it = Itinerary::NONE;
        human.speed.0 = 0.0;
        if human.trans.pos.distance2(boarding_stop.walk_pos)
            <= BOARDING_DISTANCE * BOARDING_DISTANCE
        {
            if let Some(queue) = queues.get_mut(&(line, stop)) {
                queue.push((since, id));
            }
        }
    }

    let mut queues: BTreeMap<_, VecDeque<_>> = queues
        .into_iter()
        .map(|(stop, mut queue)| {
            queue.sort_unstable();
            (stop, queue.into())
        })
        .collect();
    for (bus, at) in service {
        if let Some(leg) = transit.legs.get_mut(&bus) {
            leg.skipped.clear();
        }
        if at.retiring {
            if at.remaining == 0 {
                vehicles.kill(bus);
            }
            continue;
        }
        let queue = queues.get_mut(&(at.line, at.stop)).unwrap();
        let mut remaining = at.remaining;
        while remaining < CAPACITY {
            let Some((_, id)) = queue.pop_front() else {
                break;
            };
            let human = &mut world.humans[id];
            if !Router::board(human, bus, resources) {
                continue;
            }
            let line = &transit.lines[at.line];
            if matches!(human.router.cur_step(), Some(RoutingStep::RideTransit { alight, .. }) if !line.stops.contains(alight))
            {
                let walk_pos = transit.stops[at.stop].walk_pos;
                detach_for_replan(&mut transit, id, human, walk_pos, resources, Some(at.line));
                continue;
            }
            remaining += 1;
            Budget::apply(resources, BudgetCategory::TransitFares, line.fare);
            transit_stats::on_board(&mut resources.write::<TransitStats>(), at.line);
        }
    }
}

fn alight_passenger(
    transit: &mut Transit,
    id: HumanID,
    human: &mut HumanEnt,
    at: &mut StopService,
    bus: VehicleID,
    resources: &Resources,
) {
    let walk_pos = transit.stops[at.stop].walk_pos;
    let Some(RoutingStep::RideTransit { line, alight }) = human.router.cur_step() else {
        detach_for_replan(transit, id, human, walk_pos, resources, Some(at.line));
        return;
    };
    let line_valid = *line == at.line && transit.lines[at.line].stops.contains(alight);
    let skipped = transit
        .legs
        .get(&bus)
        .is_some_and(|leg| leg.skipped.contains(alight));
    if line_valid && !skipped && *alight == at.stop {
        Router::alight(human, walk_pos, resources);
    } else if at.force_alight || !line_valid || skipped {
        detach_for_replan(transit, id, human, walk_pos, resources, Some(at.line));
    } else {
        at.remaining += 1;
    }
}
