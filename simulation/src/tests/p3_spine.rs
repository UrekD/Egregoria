use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, Color, Transform, Vec2, Vec3};
use prototypes::{GameTime, Money, Tick, TICKS_PER_HOUR};

use super::TestCtx;
use crate::economy::{transit_stats::TransitStats, TripStats};
use crate::map::LaneKind;
use crate::map_dynamic::transit::{self, WAIT_NOT_STARTED};
use crate::map_dynamic::{
    routing_changed_system, routing_update_system, BuildingInfos, BusRun, Destination, Itinerary,
    LineID, Router, RoutingStep, StopID, Transit, TransitLine, TransitStop,
};
use crate::souls::human::{spawn_human, HumanDecisionKind};
use crate::transportation::{
    spawn_driving_vehicle, Location, TransportGrid, VehicleKind, VehicleState,
};
use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
use crate::{HumanID, Simulation, SoulID, VehicleID};

#[test]
fn p3_spine_rejects_invalid_transit_commands_atomically() {
    let mut ctx = TestCtx::new();
    ctx.g.write::<crate::Replay>().enabled = true;
    let stop = Default::default();
    let line = Default::default();
    let commands = [
        WorldCommand::TransitAddStop {
            pos: Vec3::ZERO,
            dir: Vec2::X,
        },
        WorldCommand::TransitRemoveStop { stop },
        WorldCommand::TransitCreateLine {
            stops: vec![stop, stop],
        },
        WorldCommand::TransitEditLine {
            line,
            stops: vec![stop, stop],
        },
        WorldCommand::TransitRemoveLine { line },
        WorldCommand::TransitSetVehicles { line, n: 1 },
        WorldCommand::TransitSetFare {
            line,
            fare: Money::new_bucks(1),
        },
    ];
    let names = [
        "TransitAddStop",
        "TransitRemoveStop",
        "TransitCreateLine",
        "TransitEditLine",
        "TransitRemoveLine",
        "TransitSetVehicles",
        "TransitSetFare",
    ];
    let mut before = ctx.g.hashes();
    before.remove("replay");
    let recorded = ctx.g.read::<crate::Replay>().command_count();

    ctx.g.apply_commands(&commands);

    let outcomes = ctx.g.read::<CommandOutcomes>();
    assert_eq!(
        outcomes.last,
        names.map(|name| (name, Rejection::MissingReference))
    );
    assert!(outcomes.accepted.is_empty());
    assert_eq!(
        ctx.g.read::<crate::Replay>().command_count(),
        recorded + commands.len()
    );
    let mut after = ctx.g.hashes();
    after.remove("replay");
    assert_eq!(after, before);
}

#[test]
fn p3_spine_registers_and_round_trips_transit_resources() {
    let mut ctx = TestCtx::new();
    assert!(ctx.g.read::<Transit>().stops.is_empty());
    ctx.g.write::<TripStats>().transit.completed = 1;
    ctx.g
        .write::<TransitStats>()
        .boardings
        .insert(Default::default(), 1);
    ctx.tick_fast();
    let systems: Vec<_> = ctx.sched.latest_samples().map(|(name, _)| name).collect();
    assert!(systems.windows(4).any(|names| names
        == [
            "vehicle_state_update_system",
            "transit_stop_update",
            "transit_dispatch_update",
            "transit_board_update",
        ]));
    let hashes = ctx.g.hashes();
    for key in ["transit", "trip_stats", "transit_stats"] {
        assert!(hashes.contains_key(key), "missing {key}");
    }

    let loaded: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    assert_eq!(loaded.hashes(), hashes);
    assert_eq!(loaded.read::<TripStats>().transit.completed, 1);
    assert_eq!(
        loaded
            .read::<TransitStats>()
            .boardings
            .values()
            .sum::<u64>(),
        1
    );
}

#[test]
fn p3_spine_spawns_driving_bus_only_at_clear_berth() {
    let mut ctx = TestCtx::new();
    let mut trans = Transform::new(vec3(150.0, 125.0, 25.0));
    trans.dir = Vec3::X * 2.0;
    let before = ctx.g.hashes();
    let bus = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, trans).unwrap();
    let entity = ctx.g.get(bus).unwrap();
    assert_eq!(entity.trans.pos, trans.pos);
    assert_eq!(entity.trans.dir, Vec3::X);
    assert!(matches!(entity.vehicle.state, VehicleState::Driving));
    let grid = ctx.g.read::<TransportGrid>();
    let (_, state) = grid.get(entity.collider.unwrap().0).unwrap();
    assert_eq!(state.height, trans.pos.z);
    assert_eq!(state.dir, Vec2::X);
    drop(grid);
    let after = ctx.g.hashes();
    assert_eq!(before["pmanagement"], after["pmanagement"]);

    assert!(spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, trans).is_none());
    let tiny_dir = Transform {
        dir: Vec3::X * 1e-6,
        ..trans
    };
    assert!(spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, tiny_dir).is_none());
    assert_eq!(ctx.g.hashes(), after);
    trans.pos.z -= 10.0;
    assert!(spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, trans).is_some());
}

fn onboard_rider() -> (TestCtx, HumanID, VehicleID) {
    let mut ctx = TestCtx::new();
    ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
    let home = ctx.build_house_near(vec2(150.0, 125.0));
    let human = outside_rider(&mut ctx.g, home);
    let bus = spawn_driving_vehicle(
        &mut ctx.g,
        VehicleKind::Bus,
        Transform::new(vec3(170.0, 100.0, 0.0)),
    )
    .unwrap();
    ctx.g.world.vehicles[bus].it = Itinerary::simple(vec![vec3(220.0, 100.0, 0.0)]);
    (ctx, human, bus)
}

fn outside_rider(sim: &mut Simulation, home: crate::map::BuildingID) -> HumanID {
    let id = spawn_human(sim, home).unwrap();
    sim.write::<BuildingInfos>()
        .get_out(home, SoulID::Human(id));
    sim.world.humans[id].location = Location::Outside;
    id
}

fn queue_rider(
    sim: &mut Simulation,
    id: HumanID,
    line: LineID,
    stops: [StopID; 2],
    dest: Destination,
    egress: Vec3,
) {
    let rider = &mut sim.world.humans[id];
    rider.router.set_test_route(
        dest,
        vec![
            RoutingStep::WaitAtStop {
                stop: stops[0],
                line,
                since: WAIT_NOT_STARTED,
            },
            RoutingStep::RideTransit {
                line,
                alight: stops[1],
            },
            RoutingStep::WalkTo(egress),
        ],
    );
    rider.decision.kind = HumanDecisionKind::GoTo(dest);
    rider.decision.wait = 17;
}

#[test]
fn p3_spine_onboard_car_loss_keeps_destination_and_decision() {
    let (mut ctx, id, bus) = onboard_rider();
    let car_pos = vec3(150.0, 100.0, 0.0);
    let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, Transform::new(car_pos)).unwrap();
    let dest = Destination::Outside(vec3(230.0, 105.0, 0.0));
    let egress = vec3(225.0, 108.0, 0.0);
    queue_rider(
        &mut ctx.g,
        id,
        LineID::default(),
        [StopID::default(); 2],
        dest,
        egress,
    );
    let (world, resources) = ctx.g.world_res();
    let rider = &mut world.humans[id];
    rider.router.personal_car = Some(car);
    rider.router.use_vehicle(Some(car));
    rider.router.trip_start = Some(Tick(1));
    assert!(Router::board(rider, bus, resources));

    ctx.g
        .read::<crate::ParCommandBuffer<crate::VehicleEnt>>()
        .kill(car);
    ctx.g.apply_commands([]);

    let rider = &ctx.g.world.humans[id];
    assert!(ctx.g.get(car).is_none());
    assert_eq!(rider.location, Location::Transit(bus));
    assert_eq!(rider.router.personal_car, None);
    assert_eq!(rider.router.selected_vehicle(), None);
    assert_eq!(rider.router.target_dest, Some(dest));
    assert_eq!(rider.router.trip_start, Some(Tick(1)));
    assert!(matches!(rider.decision.kind, HumanDecisionKind::GoTo(d) if d == dest));
    assert_eq!(rider.decision.wait, 17);
    assert!(matches!(
        rider.router.cur_step(),
        Some(RoutingStep::RideTransit { .. })
    ));

    let (world, resources) = ctx.g.world_res();
    let stop_pos = vec3(210.0, 108.0, 0.0);
    Router::alight(&mut world.humans[id], stop_pos, resources);
    let collider = world.humans[id].collider.unwrap().0;
    Router::alight(&mut world.humans[id], stop_pos, resources);
    assert_eq!(world.humans[id].location, Location::Outside);
    assert_eq!(world.humans[id].collider.unwrap().0, collider);
    assert_eq!(world.humans[id].trans.pos, stop_pos);
    routing_changed_system(world, resources);
    routing_update_system(world, resources);
    assert!(
        matches!(world.humans[id].router.cur_step(), Some(RoutingStep::WalkTo(pos)) if *pos == egress)
    );
}

#[test]
fn p3_spine_onboard_destination_demolition_defers_cancellation() {
    let (mut ctx, id, bus) = onboard_rider();
    let building = ctx.build_house_near(vec2(250.0, 125.0));
    let dest = Destination::Building(building);
    queue_rider(
        &mut ctx.g,
        id,
        LineID::default(),
        [StopID::default(); 2],
        dest,
        vec3(250.0, 105.0, 0.0),
    );
    let (world, resources) = ctx.g.world_res();
    let rider = &mut world.humans[id];
    rider.router.trip_start = Some(Tick(1));
    assert!(Router::board(rider, bus, resources));

    ctx.remove_fixture_building(building);

    let rider = &ctx.g.world.humans[id];
    assert_eq!(rider.location, Location::Transit(bus));
    assert_eq!(rider.router.target_dest, Some(dest));
    assert_eq!(rider.router.trip_start, Some(Tick(1)));
    assert!(matches!(rider.decision.kind, HumanDecisionKind::GoTo(d) if d == dest));
    assert_eq!(
        ctx.g.world.vehicles[bus].it.end_pos(),
        Some(vec3(220.0, 100.0, 0.0))
    );
    assert!(matches!(
        rider.router.cur_step(),
        Some(RoutingStep::RideTransit { .. })
    ));

    let (world, resources) = ctx.g.world_res();
    Router::alight(&mut world.humans[id], vec3(210.0, 108.0, 0.0), resources);
    routing_changed_system(world, resources);
    routing_update_system(world, resources);
    let rider = &world.humans[id];
    assert_eq!(rider.router.target_dest, None);
    assert_eq!(rider.router.trip_start, None);
    assert!(rider.router.cur_step().is_none());
    assert!(matches!(rider.decision.kind, HumanDecisionKind::Yield));
}

#[test]
fn p3_spine_directed_upstream_berth_uses_loop() {
    let ctx = TestCtx::new();
    let a = vec3(100.0, 100.0, 0.0);
    let b = vec3(300.0, 100.0, 0.0);
    ctx.build_roads(&[a, b, vec3(300.0, 300.0, 0.0), vec3(100.0, 300.0, 0.0), a]);
    let map = ctx.g.map();
    let lane = map
        .lanes()
        .values()
        .find(|lane| {
            lane.kind == LaneKind::Driving && lane.points.middle().is_close((a + b) * 0.5, 15.0)
        })
        .unwrap();
    let offset = lane.points.length() * 0.25;
    let start = lane.points.point_along(lane.points.length() * 0.75);
    let end = lane.points.point_along(offset);
    let mut route = Itinerary::route_to_lane(Tick(1), start, &map, lane.id, offset).unwrap();
    let chain = &route.get_route().unwrap().reversed_route;
    assert!(!chain.is_empty());
    assert_eq!(chain[0].destination_lane(), lane.id);
    assert_eq!(route.update(start, 2_000.0, Tick(1), 0, &map), end);
    assert!(route.has_ended(0.0));
}

#[test]
fn p3_spine_directed_retry_survives_broken_intermediate_turn() {
    use crate::map::TraverseKind;
    use crate::map_dynamic::ItineraryTarget;

    let ctx = TestCtx::new();
    let a = vec3(100.0, 100.0, 0.0);
    let b = vec3(300.0, 100.0, 0.0);
    let c = vec3(300.0, 300.0, 0.0);
    let d = vec3(500.0, 300.0, 0.0);
    let e = vec3(500.0, 500.0, 0.0);
    ctx.build_roads(&[a, b, c, d, e]);
    let (start, end, target, mut route, removed) = {
        let map = ctx.g.map();
        let lane_between = |from: Vec3, to: Vec3| {
            map.lanes()
                .values()
                .find(|lane| {
                    lane.kind == LaneKind::Driving
                        && map.get(lane.src).unwrap().pos.xy() == from.xy()
                        && map.get(lane.dst).unwrap().pos.xy() == to.xy()
                })
                .unwrap()
        };
        let first = lane_between(a, b);
        let last = lane_between(d, e);
        let start = first.points.middle();
        let offset = last.points.length() * 0.5;
        let target = ItineraryTarget::DirectedLane(last.id, offset);
        let route = Itinerary::route_to_lane(Tick(1), start, &map, last.id, offset).unwrap();
        let removed = lane_between(b, c).parent;
        assert!(matches!(
            route
                .get_route()
                .unwrap()
                .reversed_route
                .last()
                .unwrap()
                .kind,
            TraverseKind::Turn(_)
        ));
        (
            start,
            last.points.point_along(offset),
            target,
            route,
            removed,
        )
    };
    ctx.g.map_mut().remove_road(removed);
    let pos = route.update(start, 1_000.0, Tick(2), 0, &ctx.g.map());
    assert!(route.is_wait_for_reroute().is_some());
    assert_eq!(route.target(), Some(target));
    let ItineraryTarget::DirectedLane(lane, offset) = target else {
        unreachable!()
    };
    assert_eq!(
        ctx.g.map().get(lane).unwrap().points.point_along(offset),
        end
    );

    ctx.build_roads(&[b, c]);
    let map = ctx.g.map();
    for tick in 3..=203 {
        route.update(pos, 0.0, Tick(tick), 0, &map);
        if route.is_wait_for_reroute().is_none() {
            break;
        }
    }
    assert_eq!(route.is_wait_for_reroute(), None);
    assert_eq!(route.target(), Some(target));
    assert_eq!(route.update(pos, 2_000.0, Tick(204), 0, &map), end);
}

fn add_fixture_line(sim: &Simulation) -> (LineID, [StopID; 2]) {
    let map = sim.map();
    let lane = map
        .lanes()
        .values()
        .find(|lane| {
            lane.kind == LaneKind::Driving
                && lane.points.middle().is_close(vec3(250.0, 100.0, 0.0), 15.0)
        })
        .unwrap();
    let sidewalk = map
        .get(
            map.get(lane.parent)
                .unwrap()
                .sidewalks(lane.src)
                .outgoing
                .unwrap(),
        )
        .unwrap();
    let mut transit = sim.write::<Transit>();
    let stops = [0.3, 0.7].map(|fraction| {
        let (pos, dir) = lane.points.point_dir_along(lane.points.length() * fraction);
        let walk_pos = sidewalk.points.project(pos);
        transit.stops.insert_with_key(|id| TransitStop {
            id,
            pos,
            dir: dir.xy(),
            walk_pos,
            berth: pos,
        })
    });
    let line = transit.lines.insert_with_key(|id| TransitLine {
        id,
        stops: stops.to_vec(),
        target_vehicles: 1,
        fare: Money::ZERO,
        color: Color::WHITE,
        suspended: false,
    });
    (line, stops)
}

#[test]
fn p3_spine_validates_live_directed_loop_before_apply() {
    let mut ctx = TestCtx::new();
    ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
    let (line, stops) = add_fixture_line(&ctx.g);
    let command = WorldCommand::TransitEditLine {
        line,
        stops: stops.to_vec(),
    };
    assert_eq!(transit::validate(&ctx.g, &command), Ok(()));
    let pos_dir = {
        let transit = ctx.g.read::<Transit>();
        let stop = &transit.stops[stops[0]];
        (stop.pos, stop.dir)
    };
    let (pos, dir) = pos_dir;
    assert_eq!(
        transit::validate(&ctx.g, &WorldCommand::TransitAddStop { pos, dir }),
        Ok(())
    );
    assert_eq!(
        transit::validate(
            &ctx.g,
            &WorldCommand::TransitCreateLine {
                stops: vec![stops[0]; 31]
            }
        ),
        Err(Rejection::InvalidConnection)
    );

    command.apply(&mut ctx.g);
    assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);

    let (road, disconnected) = {
        let map = ctx.g.map();
        let stop = ctx.g.read::<Transit>().stops[stops[0]].clone();
        (
            map.nearest_lane(stop.pos, LaneKind::Driving, None)
                .and_then(|id| map.get(id))
                .unwrap()
                .parent,
            stop,
        )
    };
    ctx.g.map_mut().remove_road(road);
    assert_eq!(
        transit::validate(&ctx.g, &command),
        Err(Rejection::InvalidConnection)
    );
    assert_eq!(
        transit::validate(
            &ctx.g,
            &WorldCommand::TransitAddStop {
                pos: disconnected.pos,
                dir: disconnected.dir
            }
        ),
        Err(Rejection::MissingReference)
    );
}

#[test]
fn p3_spine_resume_with_occupied_bus_and_waiting_queue() {
    let (mut ctx, rider, bus) = onboard_rider();
    let home = ctx.build_house_near(vec2(320.0, 125.0));
    let waiter = outside_rider(&mut ctx.g, home);
    let (line, stops) = add_fixture_line(&ctx.g);
    let (board_pos, alight_pos) = {
        let transit = ctx.g.read::<Transit>();
        (
            transit.stops[stops[0]].walk_pos,
            transit.stops[stops[1]].walk_pos,
        )
    };
    ctx.g.write::<Transit>().runs.insert(
        bus,
        BusRun {
            line,
            next_stop_idx: 1,
            dwell_until: None,
            leg_started: Tick(0),
            failures: 0,
        },
    );
    let dest = Destination::Outside(alight_pos);
    for id in [rider, waiter] {
        queue_rider(&mut ctx.g, id, line, stops, dest, alight_pos);
    }
    let (world, resources) = ctx.g.world_res();
    world.humans[waiter].trans.pos = board_pos;
    assert!(Router::board(&mut world.humans[rider], bus, resources));
    ctx.tick_fast();
    let queued_at = {
        let (world, resources) = ctx.g.world_res();
        let now = transit::elapsed_ticks(resources);
        assert!(matches!(world.humans[waiter].router.cur_step(),
            Some(RoutingStep::WaitAtStop { since, .. }) if *since == now));
        assert_eq!(world.humans[rider].location, Location::Transit(bus));
        now
    };
    ctx.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(100_000)))]);
    assert_eq!(transit::elapsed_ticks(ctx.g.world_res().1), queued_at);

    let bytes = Bincode::encode(&ctx.g).unwrap();
    let mut resumed: Simulation = Bincode::decode(&bytes).unwrap();
    assert_eq!(ctx.g.hashes(), resumed.hashes());
    for sim in [&mut ctx.g, &mut resumed] {
        let mut schedule = Simulation::schedule();
        sim.tick(
            &mut schedule,
            &[WorldCommand::SetGameTime(GameTime::new(Tick(100)))],
        );
        let (world, resources) = sim.world_res();
        assert_eq!(transit::elapsed_ticks(resources), Tick(queued_at.0 + 1));
        assert!(matches!(world.humans[waiter].router.cur_step(),
            Some(RoutingStep::WaitAtStop { since, .. }) if *since == queued_at));
        Router::alight(&mut world.humans[rider], alight_pos, resources);
        assert!(Router::board(&mut world.humans[waiter], bus, resources));
        Router::force_detach(&mut world.humans[waiter], board_pos, resources);
        for _ in 0..10 {
            sim.tick(&mut schedule, &[]);
        }
    }
    assert_eq!(ctx.g.hashes(), resumed.hashes());
    assert_eq!(resumed.get(waiter).unwrap().router.target_dest, Some(dest));
}

#[test]
fn p3a_bus_loop_serves_both_stops_and_dwells() {
    let (mut ctx, line) = loop_fixture();
    ctx.apply(&[WorldCommand::TransitSetVehicles { line, n: 1 }]);
    let mut served = [false; 2];
    for _ in 0..TICKS_PER_HOUR {
        ctx.tick_fast();
        for run in ctx.g.read::<Transit>().runs.values() {
            if run.line == line && run.dwell_until.is_some() {
                served[run.next_stop_idx] = true;
            }
        }
        if served == [true, true] {
            return;
        }
    }
    panic!("bus did not dwell at both stops: {served:?}");
}

#[test]
#[ignore = "P3d performance check requires a running transit line"]
fn p3d_active_transit_tick_cost_stays_within_ten_percent() {
    let (ctx, line) = loop_fixture();
    let save = Bincode::encode(&ctx.g).unwrap();
    let mut legacy: Simulation = Bincode::decode(&save).unwrap();
    let mut active: Simulation = Bincode::decode(&save).unwrap();
    WorldCommand::TransitSetVehicles { line, n: 1 }.apply(&mut active);

    let measure = |sim: &mut Simulation| -> std::time::Duration {
        let mut schedule = Simulation::schedule();
        for _ in 0..1_000 {
            sim.tick(&mut schedule, &[]);
        }
        (0..10_000).map(|_| sim.tick(&mut schedule, &[])).sum()
    };
    let legacy_time = measure(&mut legacy);
    let active_time = measure(&mut active);

    assert!(legacy.read::<Transit>().runs.is_empty());
    assert!(!active.read::<Transit>().runs.is_empty());
    assert_eq!(legacy.world().humans.len(), active.world().humans.len());
    assert!(
        active_time <= legacy_time.mul_f64(1.10),
        "{active_time:?} vs {legacy_time:?}"
    );
}

pub(super) fn loop_fixture() -> (TestCtx, LineID) {
    let mut ctx = TestCtx::new();
    ctx.build_roads(&[
        Vec3::new(100.0, 100.0, 0.0),
        Vec3::new(300.0, 100.0, 0.0),
        Vec3::new(300.0, 300.0, 0.0),
        Vec3::new(100.0, 300.0, 0.0),
        Vec3::new(100.0, 100.0, 0.0),
    ]);
    ctx.build_house_near(Vec2::new(200.0, 100.0));
    let add_stops = {
        let map = ctx.g.map();
        let lane = map
            .lanes()
            .values()
            .find(|lane| {
                lane.kind == LaneKind::Driving
                    && lane
                        .points
                        .middle()
                        .xy()
                        .is_close(Vec2::new(200.0, 100.0), 20.0)
            })
            .unwrap();
        [0.3, 0.7].map(|fraction| {
            let (pos, dir) = lane.points.point_dir_along(lane.points.length() * fraction);
            WorldCommand::TransitAddStop { pos, dir: dir.xy() }
        })
    };
    ctx.apply(&add_stops);
    let stops: Vec<_> = ctx.g.read::<Transit>().stops.keys().collect();
    assert_eq!(stops.len(), 2);
    ctx.apply(&[WorldCommand::TransitCreateLine { stops }]);
    let line = ctx.g.read::<Transit>().lines.keys().next().unwrap();
    (ctx, line)
}
