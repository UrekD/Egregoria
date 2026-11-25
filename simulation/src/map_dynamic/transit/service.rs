use std::collections::{BTreeMap, VecDeque};

use geom::{Transform, Vec3};
use prototypes::{Tick, DELTA, TICKS_PER_REALTIME_SECOND};

use super::{
    detach_for_replan, elapsed_ticks, walkable_egress, Berth, BusRun, Transit, TransitStop,
    GIVE_UP_TICKS,
};
use crate::economy::{on_wait_timeout, transit_stats, TripProgress};
use crate::map::Map;
use crate::map_dynamic::{Itinerary, LineID, Router, RoutingStep, StopID};
use crate::transportation::{spawn_driving_vehicle, Location, TransportGrid, VehicleKind};
use crate::utils::resources::Resources;
use crate::{
    HumanEnt, HumanID, HumanWork, ParCommandBuffer, Simulation, VehicleEnt, VehicleID, World,
};

/// Boarding is agent movement, so the dwell is in real time.
pub(super) const DWELL_TICKS: u64 = 3 * TICKS_PER_REALTIME_SECOND;
const MAX_CONSECUTIVE_SKIPS: u8 = 3;
const CAPACITY: usize = 40;
const BERTH_DISTANCE: f32 = 6.0;
const BOARDING_DISTANCE: f32 = 8.0;
const ARRIVAL_SPEED: f32 = 0.5;
const BRAKING_GRACE_TICKS: u64 = 3 * TICKS_PER_REALTIME_SECOND;
const BUS_SPEED: f32 = 8.0;

fn travel_ticks(distance: f32) -> u64 {
    // DELTA is the physics step in real seconds.
    ((distance / (BUS_SPEED * DELTA)).ceil() as u64).max(1)
}

pub(super) fn direct_travel_ticks(from: Vec3, to: Vec3) -> u64 {
    travel_ticks(from.distance(to)).saturating_add(50)
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
        || nominal_leg_ticks(transit, run, vehicle.trans.pos, berth.pos),
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

fn nominal_leg_ticks(transit: &Transit, run: &BusRun, position: Vec3, berth: Vec3) -> u64 {
    let len = transit.lines[run.line].stops.len();
    let previous = (run.next_stop_idx + len - 1) % len;
    transit
        .leg_times
        .get(&run.line)
        .and_then(|times| times.nominal.get(previous))
        .copied()
        .unwrap_or(0)
        .max(direct_travel_ticks(position, berth))
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
            let previous = (run.next_stop_idx + line.stops.len() - 1) % line.stops.len();
            let line_id = run.line;
            let actual = now.0.saturating_sub(run.leg_started.0);
            if transit.legs.get(&bus).and_then(|leg| leg.from) == Some(line.stops[previous]) {
                if let Some(expected) = transit
                    .leg_times
                    .get_mut(&line_id)
                    .and_then(|times| times.learned.get_mut(previous))
                {
                    // Smooth travel observations from completed legs, including
                    // delays from traffic.
                    *expected = (7 * u128::from(*expected) + u128::from(actual)).div_ceil(8) as u64;
                }
            }
            let run = transit.runs.get_mut(&bus).unwrap();
            run.dwell_until = Some(Tick(now.0.saturating_add(DWELL_TICKS)));
            run.failures = 0;
            vehicle.it = Itinerary::NONE;
        } else {
            let planned_leg = transit.legs.get(&bus).filter(|leg| leg.expected_ticks > 0);
            let expected = planned_leg.map_or_else(
                    || nominal_leg_ticks(&transit, run, vehicle.trans.pos, stop.berth),
                    |leg| leg.expected_ticks,
                );
            let nominal_timeout = expected.saturating_mul(3);
            // Buses completing a scheduled call during retirement get the same
            // traffic allowance. A finished route at the berth gets time to brake.
            let completed_route = vehicle.it.has_ended(0.0);
            let braking = completed_route && at_berth(vehicle, stop);
            let timeout = if vehicle.it.get_route().is_some() && (!completed_route || braking) {
                let braking_grace = if braking { BRAKING_GRACE_TICKS } else { 0 };
                nominal_timeout
                    .max(GIVE_UP_TICKS)
                    .saturating_add(braking_grace)
            } else {
                nominal_timeout
            };
            if now.0.saturating_sub(run.leg_started.0) <= timeout {
                if planned_leg.is_some() && vehicle.it.is_missing() && !at_berth(vehicle, stop) {
                    if let Some(berth) = transit.berth(&map, stop_id) {
                        // An in-flight route loss is distinct from arriving or
                        // skipping a stop. Keep the run and its riders together.
                        vehicle.it = Itinerary::wait_for_reroute_to_lane(berth.lane, berth.offset);
                    }
                }
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
    world.humans.edit_active(HumanWork::Outside, |id, human| {
        let human: &mut HumanEnt = human;
        if let Location::Transit(bus) = human.location {
            if let Some(at) = service.get_mut(&bus) {
                alight_passenger(&mut transit, id, human, at, bus, resources);
            }
            return;
        }
        if human.location != Location::Outside {
            return;
        }
        human.router.stamp_wait_start(now);
        let Some(RoutingStep::WaitAtStop { stop, line, since }) = human.router.cur_step() else {
            return;
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
                on_wait_timeout(resources);
            }
            return;
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
    });

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
            let Some((since, id)) = queue.pop_front() else {
                break;
            };
            world.edit_human(id, |human| {
                if !Router::board(human, bus, resources) {
                    return;
                }
                let line = &transit.lines[at.line];
                if matches!(human.router.cur_step(), Some(RoutingStep::RideTransit { alight, .. }) if !line.stops.contains(alight))
                {
                    let walk_pos = transit.stops[at.stop].walk_pos;
                    detach_for_replan(&mut transit, id, human, walk_pos, resources, Some(at.line));
                    return;
                }
                remaining += 1;
                human.router.trip_mode = TripProgress::RodeTransit;
                transit_stats::on_board_with_fare(
                    resources,
                    at.line,
                    line.fare,
                    now.0.saturating_sub(since.0),
                );
            });
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

#[cfg(test)]
pub(in crate::map_dynamic) mod tests {
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3};
    use prototypes::{GameTime, Money, TICKS_PER_HOUR};

    use super::super::WAIT_NOT_STARTED;
    use super::*;
    use crate::economy::{Budget, BudgetCategory, TICKS_PER_BUDGET_DAY};
    use crate::map::{LaneKind, LanePatternBuilder, ProjectFilter};
    use crate::map_dynamic::{BuildingInfos, Destination};
    use crate::souls::human::spawn_human;
    use crate::tests::TestCtx;
    use crate::utils::scheduler::drain_commands;
    use crate::world_command::{CommandOutcomes, WorldCommand};
    use crate::SoulID;

    fn loop_fixture() -> (TestCtx, LineID) {
        let mut ctx = TestCtx::new_fast();
        ctx.build_roads(&[
            vec3(100.0, 100.0, 0.0),
            vec3(300.0, 100.0, 0.0),
            vec3(300.0, 300.0, 0.0),
            vec3(100.0, 300.0, 0.0),
            vec3(100.0, 100.0, 0.0),
        ]);
        let stops = {
            let map = ctx.g.map();
            let lane = map
                .lanes()
                .values()
                .find(|lane| {
                    lane.kind == LaneKind::Driving
                        && lane.points.middle().xy().is_close(vec2(200.0, 100.0), 20.0)
                })
                .unwrap();
            [0.3, 0.7].map(|fraction| {
                let (pos, dir) = lane.points.point_dir_along(lane.points.length() * fraction);
                WorldCommand::TransitAddStop { pos, dir: dir.xy() }
            })
        };
        ctx.apply(&stops);
        let stops: Vec<_> = ctx.g.read::<Transit>().stops.keys().collect();
        assert_eq!(stops.len(), 2);
        ctx.apply(&[WorldCommand::TransitCreateLine { stops }]);
        let line = ctx.g.read::<Transit>().lines.keys().next().unwrap();
        (ctx, line)
    }

    #[test]
    fn p3_fix2_unrelated_road_edit_preserves_learned_leg_times() {
        let (mut ctx, line) = loop_fixture();
        let (learned, nominal) = {
            let mut transit = ctx.g.write::<Transit>();
            let times = transit.leg_times.get_mut(&line).unwrap();
            for time in &mut times.learned {
                *time *= 2;
            }
            (times.learned.clone(), times.nominal.clone())
        };
        ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
        assert_eq!(ctx.g.read::<Transit>().leg_times[&line].nominal, nominal);

        let (from, to) = {
            let map = ctx.g.map();
            (
                map.project(vec3(500.0, 100.0, 0.0), 0.0, ProjectFilter::ALL),
                map.project(vec3(680.0, 100.0, 0.0), 0.0, ProjectFilter::ALL),
            )
        };
        ctx.apply(&[WorldCommand::MapMakeConnection {
            from,
            to,
            inter: None,
            pat: LanePatternBuilder::default().build(),
        }]);

        assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);
        let transit = ctx.g.read::<Transit>();
        assert_eq!(transit.directed_leg_times(line).unwrap().0, learned);
    }

    #[test]
    fn p3_fix2_legacy_leg_times_keep_their_estimates_and_scale_when_nominal_times_change() {
        let bytes = Bincode::encode(&vec![150_u64, 401]).unwrap();
        let mut times: super::super::LineTimes = Bincode::decode(&bytes).unwrap();
        times.refresh(vec![100, 200]);
        assert_eq!(times.learned, [150, 401]);
        times.refresh(vec![120, 200]);
        assert_eq!(times.learned, [180, 401]);
    }

    fn start_routed_bus(ctx: &mut TestCtx, line: LineID, from_berth: bool) -> (VehicleID, u64) {
        let (from, start) = {
            let transit = ctx.g.read::<Transit>();
            let first = transit.lines[line].stops[0];
            (first, transit.berth(&ctx.g.map(), first).unwrap())
        };
        let bus = spawn_driving_vehicle(
            &mut ctx.g,
            VehicleKind::Bus,
            Transform {
                pos: start.pos,
                dir: start.dir,
            },
        )
        .unwrap();
        let (world, resources) = ctx.g.world_res();
        let mut transit = resources.write::<Transit>();
        let map = resources.read::<Map>();
        transit.lines[line].target_vehicles += 1;
        transit.runs.insert(
            bus,
            BusRun {
                line,
                next_stop_idx: 1,
                dwell_until: None,
                leg_started: Tick(0),
                failures: 0,
            },
        );
        let vehicle = &mut world.vehicles[bus];
        begin_leg(
            &mut transit,
            &map,
            bus,
            vehicle,
            Tick(0),
            from_berth.then_some(from),
        );
        assert!(vehicle.it.get_route().is_some());
        (bus, transit.legs[&bus].expected_ticks)
    }

    fn reach_berth(world: &mut World, resources: &Resources, bus: VehicleID, now: Tick) {
        let vehicle = &mut world.vehicles[bus];
        vehicle.trans.pos =
            vehicle
                .it
                .update(vehicle.trans.pos, 1_000.0, now, 0, &resources.read::<Map>());
        assert!(vehicle.it.has_ended(0.0));
    }

    #[test]
    fn p3_watchdog_keeps_a_delayed_routable_stop() {
        let (mut ctx, line) = loop_fixture();
        let (bus, nominal) = start_routed_bus(&mut ctx, line, true);
        let (world, resources) = ctx.g.world_res();

        resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + nominal * 3 + 1);
        transit_dispatch_update(world, resources);
        let transit = resources.read::<Transit>();
        assert_eq!(transit.runs[&bus].next_stop_idx, 1);
        assert_eq!(transit.runs[&bus].failures, 0);
        drop(transit);

        let arrival_tick = Tick(nominal * 3 + 2);
        reach_berth(world, resources, bus, arrival_tick);
        world.vehicles[bus].speed.0 = ARRIVAL_SPEED;
        resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + arrival_tick.0);
        transit_dispatch_update(world, resources);
        {
            let transit = resources.read::<Transit>();
            assert_eq!(transit.runs[&bus].next_stop_idx, 1);
            assert_eq!(transit.runs[&bus].failures, 0);
            assert!(transit.runs[&bus].dwell_until.is_none());
        }

        world.vehicles[bus].speed.0 = 0.0;
        resources.write::<GameTime>().tick.0 += 1;
        transit_dispatch_update(world, resources);
        let transit = resources.read::<Transit>();
        assert_eq!(
            transit.runs[&bus].dwell_until,
            Some(Tick(arrival_tick.0 + 1 + DWELL_TICKS))
        );
    }

    #[test]
    fn p3_watchdog_skips_a_blocked_route_after_its_grace() {
        let (mut ctx, line) = loop_fixture();
        let (bus, nominal) = start_routed_bus(&mut ctx, line, true);
        assert!(nominal * 3 < GIVE_UP_TICKS);
        let (world, resources) = ctx.g.world_res();
        assert!(!world.vehicles[bus].it.has_ended(0.0));

        resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + GIVE_UP_TICKS);
        transit_dispatch_update(world, resources);
        assert_eq!(resources.read::<Transit>().runs[&bus].next_stop_idx, 1);
        assert_eq!(resources.read::<Transit>().runs[&bus].failures, 0);

        resources.write::<GameTime>().tick.0 += 1;
        transit_dispatch_update(world, resources);
        assert_eq!(resources.read::<Transit>().runs[&bus].next_stop_idx, 0);
        assert_eq!(resources.read::<Transit>().runs[&bus].failures, 1);
    }

    #[test]
    fn p3_watchdog_allows_bounded_braking_after_the_deadline() {
        let (mut ctx, line) = loop_fixture();
        let (bus, nominal) = start_routed_bus(&mut ctx, line, true);
        assert!(nominal * 3 < GIVE_UP_TICKS);
        let (world, resources) = ctx.g.world_res();
        reach_berth(world, resources, bus, Tick(GIVE_UP_TICKS));
        world.vehicles[bus].speed.0 = ARRIVAL_SPEED;

        let first_braking_tick = GIVE_UP_TICKS + 1;
        resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + first_braking_tick);
        transit_dispatch_update(world, resources);
        assert_eq!(resources.read::<Transit>().runs[&bus].next_stop_idx, 1);
        assert_eq!(resources.read::<Transit>().runs[&bus].dwell_until, None);

        let last_braking_tick = GIVE_UP_TICKS + 3 * TICKS_PER_REALTIME_SECOND;
        resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + last_braking_tick);
        transit_dispatch_update(world, resources);
        assert_eq!(resources.read::<Transit>().runs[&bus].next_stop_idx, 1);

        resources.write::<GameTime>().tick.0 += 1;
        transit_dispatch_update(world, resources);
        assert_eq!(resources.read::<Transit>().runs[&bus].next_stop_idx, 0);
        assert_eq!(resources.read::<Transit>().runs[&bus].failures, 1);
    }

    #[test]
    fn p7_life_route_loss_keeps_the_original_bus_and_alighting_stop() {
        let (mut ctx, line) = loop_fixture();
        let (bus, _) = start_routed_bus(&mut ctx, line, true);
        let home = ctx.build_house_near(vec2(200.0, 100.0));
        let rider = spawn_human(&mut ctx.g, home).unwrap();
        ctx.g
            .write::<BuildingInfos>()
            .get_out(home, SoulID::Human(rider));
        let (from, to, berth, walk_pos) = {
            let transit = ctx.g.read::<Transit>();
            let stops = &transit.lines[line].stops;
            let target = &transit.stops[stops[1]];
            (stops[0], stops[1], target.berth, target.walk_pos)
        };
        let (world, resources) = ctx.g.world_res();
        world.edit_human(rider, |human| {
            human.location = Location::Outside;
            human.router.set_test_route(
                Destination::Outside(walk_pos),
                vec![
                    RoutingStep::WaitAtStop {
                        stop: from,
                        line,
                        since: WAIT_NOT_STARTED,
                    },
                    RoutingStep::RideTransit { line, alight: to },
                    RoutingStep::WalkTo(walk_pos),
                ],
            );
            assert!(Router::board(human, bus, resources));
        });
        let mut schedule = Simulation::schedule();
        schedule.retain_systems(|name| {
            matches!(
                name,
                "pedestrian_decision_system"
                    | "transport_grid_synchronize"
                    | "vehicle_decision_system"
                    | "transit_dispatch_update"
                    | "transit_board_update"
                    | "routing_update_system"
                    | "itinerary_update"
            )
        });
        let start = ctx.g.world.vehicles[bus].trans.pos;
        for _ in 0..100 {
            ctx.g.tick(&mut schedule, []);
        }
        let current = ctx.g.world.vehicles[bus].trans.pos;
        assert!(current.distance(start) > 1.0 && current.distance(berth) > BERTH_DISTANCE);
        ctx.g.world.vehicles[bus].it = Itinerary::NONE;
        ctx.g.tick(&mut schedule, []);
        assert!(ctx.g.world.vehicles[bus].it.get_route().is_some());
        let mut resumed = Bincode::decode::<Simulation>(&Bincode::encode(&ctx.g).unwrap()).unwrap();
        for _ in 0..6000 {
            resumed.tick(&mut schedule, []);
            if resumed.get(rider).unwrap().location == Location::Outside {
                assert!(resumed.world.vehicles[bus].trans.pos.distance(berth) <= BERTH_DISTANCE);
                assert!(resumed.get(rider).unwrap().trans.pos.distance(walk_pos) < 0.5);
                return;
            }
        }
        panic!("the original rider never reached the original stop");
    }

    #[test]
    fn p3_watchdog_preserves_a_delayed_riders_stop_during_suspension() {
        let (mut ctx, line) = loop_fixture();
        let (bus, nominal) = start_routed_bus(&mut ctx, line, true);
        let home = ctx.build_house_near(vec2(200.0, 100.0));
        let rider = spawn_human(&mut ctx.g, home).unwrap();
        ctx.g
            .write::<BuildingInfos>()
            .get_out(home, SoulID::Human(rider));
        let (from, to, walk_pos) = {
            let transit = ctx.g.read::<Transit>();
            let stops = &transit.lines[line].stops;
            (stops[0], stops[1], transit.stops[stops[1]].walk_pos)
        };
        let failing_bus = spawn_driving_vehicle(
            &mut ctx.g,
            VehicleKind::Bus,
            Transform::new(vec3(590.0, 100.0, 0.0)),
        )
        .unwrap();
        {
            let (world, resources) = ctx.g.world_res();
            let human = &mut world.humans[rider];
            human.location = Location::Outside;
            human.router.set_test_route(
                Destination::Outside(walk_pos),
                vec![
                    RoutingStep::WaitAtStop {
                        stop: from,
                        line,
                        since: WAIT_NOT_STARTED,
                    },
                    RoutingStep::RideTransit { line, alight: to },
                    RoutingStep::WalkTo(walk_pos),
                ],
            );
            assert!(Router::board(human, bus, resources));
            let mut transit = resources.write::<Transit>();
            transit.lines[line].target_vehicles += 1;
            transit.runs.insert(
                failing_bus,
                BusRun {
                    line,
                    next_stop_idx: 1,
                    dwell_until: None,
                    leg_started: Tick(0),
                    failures: MAX_CONSECUTIVE_SKIPS - 1,
                },
            );
            transit.legs.entry(failing_bus).or_default().expected_ticks = 1;
        }

        let now = nominal * 3 + 1;
        assert!(now + 2 < GIVE_UP_TICKS);
        for tick in [now, now + 1] {
            let (world, resources) = ctx.g.world_res();
            resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + tick);
            transit_dispatch_update(world, resources);
            drain_commands(&mut ctx.g);
            assert!(ctx.g.read::<Transit>().lines[line].suspended);
            assert_eq!(ctx.g.get(rider).unwrap().location, Location::Transit(bus));
        }

        let (world, resources) = ctx.g.world_res();
        let arrival = Tick(now + 2);
        reach_berth(world, resources, bus, arrival);
        world.vehicles[bus].speed.0 = 0.0;
        resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + arrival.0);
        transit_dispatch_update(world, resources);
        transit_board_update(world, resources);
        drain_commands(&mut ctx.g);
        let rider = ctx.g.get(rider).unwrap();
        assert_eq!(rider.location, Location::Outside);
        assert_eq!(rider.trans.pos, walk_pos);
        assert!(ctx.g.get(bus).is_none());
    }

    pub(in crate::map_dynamic) fn recovery_leg_fixture() -> (TestCtx, u64) {
        let (mut ctx, line) = loop_fixture();
        let (bus, expected) = start_routed_bus(&mut ctx, line, false);
        assert!(expected > 120);
        {
            let mut transit = ctx.g.write::<Transit>();
            assert_eq!(transit.legs[&bus].from, None);
            transit.leg_times.get_mut(&line).unwrap().learned = vec![100, 200];
        }
        (ctx, expected)
    }

    #[test]
    fn p3_fix2_watchdog_skips_an_unroutable_leg_using_nominal_time() {
        let (mut ctx, line) = loop_fixture();
        ctx.build_roads(&[vec3(500.0, 100.0, 0.0), vec3(680.0, 100.0, 0.0)]);
        let pos = vec3(590.0, 100.0, 0.0);
        let bus = spawn_driving_vehicle(
            &mut ctx.g,
            VehicleKind::Bus,
            Transform { pos, dir: Vec3::X },
        )
        .unwrap();
        let (world, resources) = ctx.g.world_res();
        let map = resources.read::<Map>();
        {
            let mut transit = resources.write::<Transit>();
            transit.lines[line].target_vehicles = 1;
            transit
                .leg_times
                .get_mut(&line)
                .unwrap()
                .learned
                .fill(TICKS_PER_HOUR);
            transit.runs.insert(
                bus,
                BusRun {
                    line,
                    next_stop_idx: 0,
                    dwell_until: None,
                    leg_started: Tick(0),
                    failures: 0,
                },
            );
            let vehicle = &mut world.vehicles[bus];
            begin_leg(&mut transit, &map, bus, vehicle, Tick(0), None);
            assert!(vehicle.it.is_wait_for_reroute().is_some());
        }
        drop(map);
        // Long past three nominal legs, but within three learned ones.
        resources.write::<GameTime>().tick = Tick(crate::INITIAL_TICK + 3 * TICKS_PER_HOUR);
        transit_dispatch_update(world, resources);
        assert_eq!(resources.read::<Transit>().runs[&bus].failures, 1);
    }

    #[test]
    fn p3_int_scheduled_bus_upkeep_survives_save_load() {
        let (mut ctx, line) = loop_fixture();
        ctx.apply(&[WorldCommand::TransitSetVehicles { line, n: 1 }]);

        // Save with a fractional hourly upkeep remainder still outstanding.
        let before_save = 11 * TICKS_PER_HOUR;
        for _ in 0..before_save {
            ctx.tick_fast();
        }
        let mut loaded: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
        let mut schedule = Simulation::schedule();
        for _ in before_save..TICKS_PER_BUDGET_DAY {
            ctx.tick_fast();
            loaded.tick(&mut schedule, &[]);
        }

        for sim in [&ctx.g, &loaded] {
            let transit = sim.read::<Transit>();
            assert_eq!(transit.runs.len(), 1);
            assert!(!transit.lines[line].suspended);
            assert_eq!(
                sim.read::<Budget>().days()[&0]
                    .get(&BudgetCategory::TransitUpkeep)
                    .copied(),
                Some(-i128::from(Money::new_bucks(25).inner()))
            );
        }
        assert_eq!(ctx.g.hashes(), loaded.hashes());
    }
}
