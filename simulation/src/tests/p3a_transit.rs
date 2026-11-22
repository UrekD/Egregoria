use geom::{vec3, Transform, Vec2, Vec3};
use prototypes::{GameTime, Money, Tick, TICKS_PER_HOUR};

use super::p3_spine::loop_fixture;
use crate::economy::{Budget, BudgetCategory, Government, TripStats};
use crate::map::{LaneKind, LanePatternBuilder, ProjectFilter};
use crate::map_dynamic::transit::{self, WAIT_NOT_STARTED};
use crate::map_dynamic::{
    BuildingInfos, BusRun, Destination, Itinerary, LineID, Router, RoutingStep, StopID, Transit,
};
use crate::souls::human::{spawn_human, HumanDecisionKind};
use crate::souls::residence::Residences;
use crate::transportation::{spawn_driving_vehicle, Location, TransportGrid, VehicleKind};
use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
use crate::{HumanID, ParCommandBuffer, Simulation, SoulID, VehicleEnt, VehicleID};

fn two_stops(sim: &Simulation, line: LineID) -> [StopID; 2] {
    sim.read::<Transit>().lines[line]
        .stops
        .clone()
        .try_into()
        .unwrap()
}

fn set_elapsed(sim: &Simulation, tick: u64) {
    *sim.write::<GameTime>() = GameTime::new(Tick(crate::INITIAL_TICK + tick));
}

fn bus_at(
    sim: &mut Simulation,
    line: LineID,
    index: usize,
    dwell_until: Option<Tick>,
) -> VehicleID {
    let stops = two_stops(sim, line);
    let stop = sim.read::<Transit>().stops[stops[index]].clone();
    let bus = spawn_driving_vehicle(
        sim,
        VehicleKind::Bus,
        Transform {
            pos: stop.berth,
            dir: stop.dir.z0(),
        },
    )
    .unwrap();
    let now = transit::elapsed_ticks(sim.world_res().1);
    let mut transit = sim.write::<Transit>();
    transit.lines[line].target_vehicles += 1;
    transit.runs.insert(
        bus,
        BusRun {
            line,
            next_stop_idx: index,
            dwell_until,
            leg_started: now,
            failures: 0,
        },
    );
    bus
}

fn waiter(
    sim: &mut Simulation,
    line: LineID,
    stop: StopID,
    alight: StopID,
    since: Tick,
) -> HumanID {
    let home = *sim.read::<Residences>().by_building.keys().next().unwrap();
    sim.write::<Residences>()
        .by_building
        .get_mut(&home)
        .unwrap()
        .capacity = 100;
    let id = spawn_human(sim, home).unwrap();
    sim.write::<BuildingInfos>()
        .get_out(home, SoulID::Human(id));
    let transit = sim.read::<Transit>();
    let from = transit.stops[stop].walk_pos;
    let to = transit.stops[alight].walk_pos;
    drop(transit);
    let dest = Destination::Outside(to);
    let human = &mut sim.world.humans[id];
    human.location = Location::Outside;
    human.trans.pos = from;
    human.router.set_test_route(
        dest,
        vec![
            RoutingStep::WaitAtStop { stop, line, since },
            RoutingStep::RideTransit { line, alight },
            RoutingStep::WalkTo(to),
        ],
    );
    human.decision.kind = HumanDecisionKind::GoTo(dest);
    id
}

fn dispatch(sim: &mut Simulation) {
    let (world, resources) = sim.world_res();
    transit::transit_dispatch_update(world, resources);
    crate::utils::scheduler::drain_commands(sim);
}

fn board(sim: &mut Simulation) {
    let (world, resources) = sim.world_res();
    transit::transit_board_update(world, resources);
    crate::utils::scheduler::drain_commands(sim);
}

#[test]
fn p3a_arrival_requires_finished_itinerary_close_berth_and_low_speed() {
    let (mut ctx, line) = loop_fixture();
    let bus = bus_at(&mut ctx.g, line, 0, None);
    let berth = ctx.g.get(bus).unwrap().trans.pos;

    ctx.g.world.vehicles[bus].it = Itinerary::simple(vec![berth]);
    dispatch(&mut ctx.g);
    assert_eq!(ctx.g.read::<Transit>().runs[&bus].dwell_until, None);

    ctx.g.world.vehicles[bus].it = Itinerary::NONE;
    ctx.g.world.vehicles[bus].trans.pos += Vec3::X * 6.1;
    dispatch(&mut ctx.g);
    assert_eq!(ctx.g.read::<Transit>().runs[&bus].dwell_until, None);

    ctx.g.world.vehicles[bus].trans.pos = berth;
    ctx.g.world.vehicles[bus].speed.0 = 0.5;
    dispatch(&mut ctx.g);
    assert_eq!(ctx.g.read::<Transit>().runs[&bus].dwell_until, None);

    ctx.g.world.vehicles[bus].speed.0 = 0.49;
    dispatch(&mut ctx.g);
    assert_eq!(
        ctx.g.read::<Transit>().runs[&bus].dwell_until,
        Some(Tick(150))
    );
    ctx.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(123_456)))]);
    dispatch(&mut ctx.g);
    assert_eq!(
        ctx.g.read::<Transit>().runs[&bus].dwell_until,
        Some(Tick(150))
    );
}

#[test]
fn p3a_alights_before_fifo_boarding_with_capacity_and_distance_limit() {
    let (mut ctx, line) = loop_fixture();
    let [first, second] = two_stops(&ctx.g, line);
    set_elapsed(&ctx.g, 5);
    let bus = bus_at(&mut ctx.g, line, 0, Some(Tick(150)));
    let alighting = waiter(&mut ctx.g, line, second, first, Tick(0));
    let (world, resources) = ctx.g.world_res();
    assert!(Router::board(&mut world.humans[alighting], bus, resources));

    let late = waiter(&mut ctx.g, line, first, second, WAIT_NOT_STARTED);
    let far = waiter(&mut ctx.g, line, first, second, Tick(0));
    ctx.g.world.humans[far].trans.pos += Vec3::X * 8.1;
    let mut queued: Vec<_> = (0..41)
        .map(|_| waiter(&mut ctx.g, line, first, second, Tick(4)))
        .collect();
    queued.sort_unstable();
    let fare = Money::new_bucks(2);
    ctx.apply(&[WorldCommand::TransitSetFare { line, fare }]);
    let before = ctx.g.read::<Government>().money;
    board(&mut ctx.g);

    assert_eq!(ctx.g.get(alighting).unwrap().location, Location::Outside);
    assert!(ctx.g.get(alighting).unwrap().collider.is_some());
    for &id in &queued[..40] {
        let human = ctx.g.get(id).unwrap();
        assert_eq!(human.location, Location::Transit(bus));
        assert!(human.collider.is_none());
        assert!(human.it.has_ended(0.0));
    }
    for id in [late, far, queued[40]] {
        assert_eq!(ctx.g.get(id).unwrap().location, Location::Outside);
    }
    assert_eq!(
        ctx.g.read::<Government>().money.inner() - before.inner(),
        40 * fare.inner()
    );
    assert_eq!(
        ctx.g.read::<Budget>().total(BudgetCategory::TransitFares),
        40 * i128::from(fare.inner())
    );
}

#[test]
fn p3a_zero_fleet_keeps_riders_until_the_next_stop() {
    let (mut ctx, line) = loop_fixture();
    let [first, second] = two_stops(&ctx.g, line);
    let bus = bus_at(&mut ctx.g, line, 1, None);
    let id = waiter(&mut ctx.g, line, second, first, Tick(0));
    let dest = ctx.g.get(id).unwrap().router.target_dest;
    let (world, resources) = ctx.g.world_res();
    assert!(Router::board(&mut world.humans[id], bus, resources));
    let berth = world.vehicles[bus].trans.pos;
    world.vehicles[bus].it = Itinerary::simple(vec![berth]);
    assert!(ctx.g.read::<Transit>().headway(line).is_some());

    ctx.apply(&[WorldCommand::TransitSetVehicles { line, n: 0 }]);
    dispatch(&mut ctx.g);
    board(&mut ctx.g);
    assert_eq!(ctx.g.get(id).unwrap().location, Location::Transit(bus));
    assert!(ctx.g.read::<Transit>().headway(line).is_none());

    ctx.g.world.vehicles[bus].it = Itinerary::NONE;
    dispatch(&mut ctx.g);
    board(&mut ctx.g);
    let human = ctx.g.get(id).unwrap();
    let transit = ctx.g.read::<Transit>();
    assert_eq!(human.location, Location::Outside);
    assert_eq!(human.trans.pos, transit.stops[second].walk_pos);
    assert_eq!(human.router.target_dest, dest);
    assert!(!transit.allows_line_for_trip(id, &human.router, line));
    assert!(ctx.g.get(bus).is_none());
}

#[test]
fn p3a_road_split_keeps_valid_anchors_and_removes_stop_near_new_end() {
    let (mut ctx, line) = loop_fixture();
    let [kept, removed] = two_stops(&ctx.g, line);
    let transit = ctx.g.read::<Transit>();
    let preserved = transit.stops[kept].clone();
    let split = transit.stops[removed].pos;
    drop(transit);
    let from = ctx.g.map().project(split, 5.0, ProjectFilter::ROAD);
    let to = ctx
        .g
        .map()
        .project(split + Vec3::Y * -100.0, 0.0, ProjectFilter::ALL);
    ctx.apply(&[WorldCommand::MapMakeConnection {
        from,
        to,
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }]);

    assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);
    let transit = ctx.g.read::<Transit>();
    assert_eq!(transit.stops[kept].pos, preserved.pos);
    assert_eq!(transit.stops[kept].dir, preserved.dir);
    assert!(transit.stops.get(removed).is_none());
    assert!(transit.lines.get(line).is_none());
}

#[test]
fn p3a_stop_removal_replans_waiters_and_restores_walking() {
    let (mut ctx, line) = loop_fixture();
    let [first, second] = two_stops(&ctx.g, line);
    let id = waiter(&mut ctx.g, line, first, second, Tick(0));
    let dest = ctx.g.get(id).unwrap().router.target_dest;
    ctx.apply(&[WorldCommand::TransitRemoveStop { stop: second }]);

    let human = ctx.g.get(id).unwrap();
    assert!(human.router.cur_step().is_none());
    assert_eq!(human.location, Location::Outside);
    assert_eq!(human.router.target_dest, dest);
    assert!(human.collider.is_some());
    let (world, resources) = ctx.g.world_res();
    crate::map_dynamic::routing_changed_system(world, resources);
    assert!(!ctx.g.get(id).unwrap().router.references_stop(second));
}

#[test]
fn p3a_dropped_bus_ejects_once_to_the_sidewalk_and_preserves_destination() {
    let (mut ctx, line) = loop_fixture();
    let [first, second] = two_stops(&ctx.g, line);
    let bus = bus_at(&mut ctx.g, line, 0, None);
    let pos = ctx.g.get(bus).unwrap().trans.pos + Vec3::X * 15.0;
    ctx.g.world.vehicles[bus].trans.pos = pos;
    let id = waiter(&mut ctx.g, line, first, second, Tick(0));
    let dest = ctx.g.get(id).unwrap().router.target_dest;
    let (world, resources) = ctx.g.world_res();
    assert!(Router::board(&mut world.humans[id], bus, resources));
    ctx.g.read::<ParCommandBuffer<VehicleEnt>>().kill(bus);
    crate::utils::scheduler::drain_commands(&mut ctx.g);

    let human = ctx.g.get(id).unwrap();
    assert_eq!(human.location, Location::Outside);
    assert_eq!(human.router.target_dest, dest);
    let collider = human.collider.unwrap().0;
    let map = ctx.g.map();
    let sidewalk = map.nearest_lane(pos, LaneKind::Walking, None).unwrap();
    assert!(
        map.get(sidewalk)
            .unwrap()
            .points
            .project_dist2(human.trans.pos)
            < 0.01
    );
    drop(map);
    assert!(ctx.g.read::<TransportGrid>().get(collider).is_some());
    assert!(!ctx.g.read::<Transit>().runs.contains_key(&bus));

    let before = ctx.g.hashes();
    transit::on_vehicle_dropped(&mut ctx.g, bus, pos);
    assert_eq!(ctx.g.hashes(), before);
}

#[test]
fn p3a_suspends_after_three_skips_and_ejects_if_no_stop_is_reachable() {
    let (mut ctx, line) = loop_fixture();
    let [first, second] = two_stops(&ctx.g, line);
    let bus = bus_at(&mut ctx.g, line, 0, None);
    let id = waiter(&mut ctx.g, line, first, second, Tick(0));
    let (world, resources) = ctx.g.world_res();
    assert!(Router::board(&mut world.humans[id], bus, resources));
    ctx.build_roads(&[vec3(500.0, 100.0, 0.0), vec3(680.0, 100.0, 0.0)]);
    ctx.g.world.vehicles[bus].trans.pos = vec3(590.0, 100.0, 0.0);
    for skips in 1..=3 {
        set_elapsed(&ctx.g, skips * TICKS_PER_HOUR);
        dispatch(&mut ctx.g);
        if skips < 3 {
            assert_eq!(ctx.g.read::<Transit>().runs[&bus].failures, skips as u8);
            assert_eq!(ctx.g.get(id).unwrap().location, Location::Transit(bus));
        }
    }
    assert!(ctx.g.read::<Transit>().lines[line].suspended);
    assert!(ctx.g.read::<Transit>().headway(line).is_none());
    assert!(ctx.g.get(bus).is_none());
    assert_eq!(ctx.g.get(id).unwrap().location, Location::Outside);
}

#[test]
fn p3a_wait_gives_up_once_after_two_elapsed_hours() {
    let (mut ctx, line) = loop_fixture();
    let [first, second] = two_stops(&ctx.g, line);
    bus_at(&mut ctx.g, line, 0, None);
    let id = waiter(&mut ctx.g, line, first, second, Tick(0));
    let dest = ctx.g.get(id).unwrap().router.target_dest;
    set_elapsed(&ctx.g, 2 * TICKS_PER_HOUR);
    ctx.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(900_000)))]);
    board(&mut ctx.g);
    assert!(matches!(
        ctx.g.get(id).unwrap().router.cur_step(),
        Some(RoutingStep::WaitAtStop { .. })
    ));

    ctx.g.write::<GameTime>().tick.0 += 1;
    board(&mut ctx.g);
    board(&mut ctx.g);
    let human = ctx.g.get(id).unwrap();
    assert!(human.router.cur_step().is_none());
    assert!(human.collider.is_some());
    assert_eq!(human.router.target_dest, dest);
    assert_eq!(ctx.g.read::<TripStats>().give_ups, 1);
    let transit = ctx.g.read::<Transit>();
    assert!(!transit.allows_line_for_trip(id, &human.router, line));
    assert!(!transit.allows_line_for_trip(id, &human.router, LineID::default()));
    drop(transit);

    let started = transit::elapsed_ticks(ctx.g.world_res().1);
    ctx.g.world.humans[id].router.trip_start = Some(started);
    assert!(!ctx.g.read::<Transit>().allows_line_for_trip(
        id,
        &ctx.g.world.humans[id].router,
        line,
    ));
    board(&mut ctx.g);
    ctx.g.world.humans[id].router.trip_start = Some(Tick(started.0 + 1));
    assert!(ctx
        .g
        .read::<Transit>()
        .allows_line_for_trip(id, &ctx.g.world.humans[id].router, line,));
}

#[test]
fn p3a_invalid_commands_leave_all_simulation_hashes_unchanged() {
    let (mut ctx, line) = loop_fixture();
    let [first, second] = two_stops(&ctx.g, line);
    ctx.g.write::<crate::Replay>().enabled = true;
    let before = ctx.g.hashes();
    ctx.g.apply_commands(&[
        WorldCommand::TransitAddStop {
            pos: vec3(f32::NAN, 0.0, 0.0),
            dir: Vec2::X,
        },
        WorldCommand::TransitAddStop {
            pos: Vec3::ZERO,
            dir: Vec2::ZERO,
        },
        WorldCommand::TransitEditLine {
            line,
            stops: vec![first; 31],
        },
        WorldCommand::TransitEditLine {
            line,
            stops: vec![second, StopID::default()],
        },
        WorldCommand::TransitSetVehicles { line, n: 11 },
        WorldCommand::TransitSetFare {
            line,
            fare: Money::new_bucks(11),
        },
    ]);
    let outcomes = ctx.g.read::<CommandOutcomes>();
    assert!(outcomes.accepted.is_empty());
    assert_eq!(outcomes.last.len(), 6);
    assert_eq!(outcomes.last[3].1, Rejection::MissingReference);
    assert_eq!(ctx.g.hashes(), before);
}
