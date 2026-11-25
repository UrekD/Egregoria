use super::*;
use crate::map::{ApproachRole, IntersectionID, JunctionRules, LightPolicy, TurnID, TurnKind};
use crate::tests::TestCtx;
use crate::transportation::spawn_driving_vehicle;
use crate::Simulation;
use common::saveload::{Bincode, Encoder};
use geom::{vec3, Transform};

fn update_at(sim: &mut Simulation, tick: u64) {
    *sim.write::<GameTime>() = GameTime::new(Tick(tick));
    let (world, resources) = sim.world_res();
    crate::transportation::transport_grid_synchronize(world, resources);
    itinerary_update(world, resources);
}

#[test]
fn p7_junction_mask_stale_clamps_a_bus_before_its_nose_enters() {
    let mut ctx = TestCtx::new();
    let position = vec3(270.0, 180.0, 0.0);
    ctx.build_roads(&[vec3(70.0, 180.0, 0.0), position, vec3(470.0, 180.0, 0.0)]);
    ctx.build_roads(&[position, vec3(270.0, 390.0, 0.0)]);

    let (junction, turns) = {
        let map = ctx.g.map();
        let junction = map
            .intersections()
            .values()
            .find(|intersection| intersection.pos.xy() == position.xy())
            .unwrap();
        let turns = junction
            .turns()
            .filter(|turn| turn.kind == TurnKind::Driving)
            .map(|turn| turn.id)
            .collect::<Vec<_>>();
        (junction.id, turns)
    };
    let turn = turns[0];
    let rules = JunctionRules::new(Vec::new(), vec![turn]).unwrap();
    ctx.g.map_mut().install_junction_rules(junction, rules);
    let (entry, transform, itinerary) = {
        let map = ctx.g.map();
        let lane = map.get(turn.src).unwrap();
        let entry = lane.points.last();
        let start = lane.points.point_along(lane.points.length() - 12.0);
        let end = map.get(turn.dst).unwrap().points.point_along(30.0);
        let itinerary = Itinerary {
            kind: ItineraryKind::Route(
                Route {
                    reversed_route: vec![
                        Traversable::new(TraverseKind::Lane(turn.dst), TraverseDirection::Forward),
                        Traversable::new(TraverseKind::Turn(turn), TraverseDirection::Forward),
                    ],
                    end_pos: end,
                    cur: Traversable::new(TraverseKind::Lane(turn.src), TraverseDirection::Forward),
                    target: ItineraryTarget::DirectedLane(turn.dst, 30.0),
                },
                PathKind::Vehicle,
            ),
            reversed_local_path: vec![entry],
            ..Itinerary::default()
        };
        (
            entry,
            Transform::new_dir(start, (entry - start).normalize()),
            itinerary,
        )
    };
    let bus = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Bus, transform).unwrap();
    ctx.g.world.vehicles[bus].it = itinerary;
    ctx.g.world.vehicles[bus].speed.0 = 400.0;
    let tick = ctx.g.get_tick();
    update_at(&mut ctx.g, tick + 1);
    let bus = &ctx.g.world.vehicles[bus];
    assert!(bus.trans.pos.distance(entry) >= VehicleKind::Bus.width() * 0.5);
    assert_eq!(bus.speed.0, 0.0);
    assert!(bus.it.is_wait_for_reroute().is_some());
}

fn junction_at(map: &Map, position: Vec3) -> IntersectionID {
    map.intersections()
        .values()
        .find(|inter| inter.pos.xy() == position.xy())
        .unwrap()
        .id
}

fn lane_at(map: &Map, from: Vec3, to: Vec3) -> LaneID {
    let source = junction_at(map, from);
    let destination = junction_at(map, to);
    map.lanes()
        .values()
        .find(|lane| {
            lane.kind == LaneKind::Driving && lane.src == source && lane.dst == destination
        })
        .unwrap()
        .id
}

fn forward_lane(id: LaneID) -> Traversable {
    Traversable::new(TraverseKind::Lane(id), TraverseDirection::Forward)
}

fn staged_vehicle(
    sim: &mut Simulation,
    kind: VehicleKind,
    from: LaneID,
    to: LaneID,
    before_boundary: f32,
) -> crate::VehicleID {
    let (start, end, terminal, dir, junction) = {
        let map = sim.map();
        let lane = map.get(from).unwrap();
        let start = lane
            .points
            .point_along(lane.points.length() - before_boundary);
        let end = lane.points.last();
        let terminal = map.get(to).unwrap().points.point_along(50.0);
        (start, end, terminal, (end - start).normalize(), lane.dst)
    };
    let id = spawn_driving_vehicle(sim, kind, Transform::new_dir(start, dir)).unwrap();
    sim.world.vehicles[id].it = Itinerary {
        kind: ItineraryKind::Route(
            Route {
                reversed_route: vec![
                    forward_lane(to),
                    Traversable::new(
                        TraverseKind::Turn(TurnID::new(junction, from, to, false)),
                        TraverseDirection::Forward,
                    ),
                ],
                end_pos: terminal,
                cur: forward_lane(from),
                target: ItineraryTarget::DirectedLane(to, 50.0),
            },
            PathKind::Vehicle,
        ),
        reversed_local_path: vec![end],
        ..Itinerary::default()
    };
    id
}

#[test]
fn p7_junction_mask_replans_original_vehicle_to_physical_arrival() {
    use crate::map::road_edit::{
        junction_edit_stamp, preview_road_edit, RoadEditPreview, RoadEditRequest,
    };
    use crate::world_command::PrepareBudget;
    use crate::WorldCommand;

    let mut ctx = TestCtx::new();
    let a = vec3(80.0, 200.0, 0.0);
    let b = vec3(250.0, 200.0, 0.0);
    let c = vec3(510.0, 200.0, 0.0);
    let d = vec3(720.0, 200.0, 0.0);
    let e = vec3(300.0, 485.0, 0.0);
    let f = vec3(480.0, 485.0, 0.0);
    ctx.build_roads(&[a, b, c, d]);
    ctx.build_roads(&[b, e, f, c]);
    let (from, direct, alternate, terminal, junction, terminal_pos) = {
        let map = ctx.g.map();
        let lane = lane_at(&map, c, d);
        (
            lane_at(&map, a, b),
            lane_at(&map, b, c),
            lane_at(&map, b, e),
            lane,
            junction_at(&map, b),
            map.get(lane).unwrap().points.point_along(50.0),
        )
    };
    ctx.g
        .map_mut()
        .update_intersection(junction, |inter| inter.light_policy = LightPolicy::NoLights);
    let turn = TurnID::new(junction, from, direct, false);
    let alternate = TurnID::new(junction, from, alternate, false);
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, direct, 16.0);
    let direct_exit = ctx.g.map().get(direct).unwrap().dst;
    if let ItineraryKind::Route(route, _) = &mut ctx.g.world.vehicles[car].it.kind {
        route.reversed_route.insert(
            0,
            Traversable::new(
                TraverseKind::Turn(TurnID::new(direct_exit, direct, terminal, false)),
                TraverseDirection::Forward,
            ),
        );
        route.reversed_route.insert(0, forward_lane(terminal));
        route.end_pos = terminal_pos;
        route.target = ItineraryTarget::DirectedLane(terminal, 50.0);
    }
    let rules = JunctionRules::new(vec![], vec![turn]).unwrap();
    let expected =
        junction_edit_stamp(&ctx.g, junction, &mut PrepareBudget::for_preview(10_000)).unwrap();
    let RoadEditPreview::JunctionRules(preview) = preview_road_edit(
        &ctx.g,
        &RoadEditRequest::JunctionRules {
            junction,
            expected,
            rules: rules.clone(),
        },
        &mut PrepareBudget::for_preview(10_000),
    )
    .unwrap() else {
        panic!("expected a junction preview");
    };
    let quote = preview.quote;
    ctx.g.apply_commands(&[WorldCommand::MapSetJunctionRules {
        junction,
        expected,
        rules,
        quote,
    }]);
    assert!(!ctx.g.map().allows_turn(turn));
    assert!(ctx.g.map().allows_turn(alternate));

    let start_tick = ctx.g.get_tick();
    let mut used_alternate = false;
    let mut arrived = false;
    for tick in start_tick + 1..start_tick + 6000 {
        ctx.g.world.vehicles[car].speed.0 = 14.0;
        update_at(&mut ctx.g, tick);
        let vehicle = &ctx.g.world.vehicles[car];
        let current = vehicle.it.get_travers().map(|path| path.kind);
        assert_ne!(current, Some(TraverseKind::Turn(turn)));
        used_alternate |= current == Some(TraverseKind::Turn(alternate));
        if vehicle.trans.pos.distance(terminal_pos) < 0.1 {
            arrived = true;
            break;
        }
    }
    assert!(used_alternate && arrived);
}

#[test]
fn p7_junction_receiving_and_rear_resume() {
    let mut ctx = TestCtx::new();
    let a = vec3(70.0, 200.0, 0.0);
    let b = vec3(270.0, 200.0, 0.0);
    let c = vec3(480.0, 200.0, 0.0);
    let d = vec3(270.0, 410.0, 0.0);
    ctx.build_roads(&[a, b, c]);
    ctx.build_roads(&[d, b]);
    let (junction, from, to, competing, start_egress, safe_egress, road, turn_length) = {
        let map = ctx.g.map();
        let junction = junction_at(&map, b);
        let from = lane_at(&map, a, b);
        let to = lane_at(&map, b, c);
        let competing = lane_at(&map, d, b);
        let lane = map.get(to).unwrap();
        (
            junction,
            from,
            to,
            competing,
            lane.points.point_along(6.0),
            lane.points.point_along(36.0),
            map.get(from).unwrap().parent,
            map.get(junction)
                .unwrap()
                .raw_turn(TurnID::new(junction, from, to, false))
                .unwrap()
                .points
                .length(),
        )
    };
    ctx.g
        .map_mut()
        .update_intersection(junction, |inter| inter.light_policy = LightPolicy::NoLights);
    ctx.g.map_mut().install_junction_rules(
        junction,
        JunctionRules::new(vec![(road, ApproachRole::Priority)], vec![]).unwrap(),
    );
    let bus = staged_vehicle(&mut ctx.g, VehicleKind::Bus, from, to, 9.0);
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, competing, to, 8.0);
    let blocker = spawn_driving_vehicle(
        &mut ctx.g,
        VehicleKind::Car,
        Transform::new_dir(start_egress, Vec3::X),
    )
    .unwrap();
    ctx.g.world.vehicles[blocker].it = Itinerary {
        kind: ItineraryKind::Route(
            Route {
                reversed_route: vec![],
                end_pos: safe_egress,
                cur: forward_lane(to),
                target: ItineraryTarget::DirectedLane(to, 36.0),
            },
            PathKind::Vehicle,
        ),
        reversed_local_path: vec![safe_egress],
        ..Itinerary::default()
    };
    let tick = ctx.g.get_tick();
    ctx.g.world.vehicles[bus].speed.0 = 500.0;
    ctx.g.world.vehicles[car].speed.0 = 500.0;
    update_at(&mut ctx.g, tick + 1);
    assert_eq!(ctx.g.world.vehicles[bus].speed.0, 0.0);
    assert_eq!(ctx.g.world.vehicles[car].speed.0, 0.0);
    ctx.g.world.vehicles[blocker].trans.pos = safe_egress;
    ctx.g.world.vehicles[bus].speed.0 =
        (VehicleKind::Bus.width() * 0.5 + turn_length + 2.0) / DELTA;
    ctx.g.world.vehicles[car].speed.0 = 50.0;
    update_at(&mut ctx.g, tick + 2);
    assert_eq!(
        ctx.g.world.vehicles[bus].it.get_travers().unwrap().kind,
        TraverseKind::Lane(to)
    );
    assert_eq!(ctx.g.world.vehicles[car].speed.0, 0.0);
    let bytes = Bincode::encode(&ctx.g).unwrap();
    let mut resumed = Bincode::decode::<Simulation>(&bytes).unwrap();
    for sim in [&mut ctx.g, &mut resumed] {
        sim.world.vehicles[car].speed.0 = 100.0;
        sim.world.vehicles[bus].speed.0 = 0.0;
        update_at(sim, tick + 3);
        assert_eq!(sim.world.vehicles[car].speed.0, 0.0);
        sim.world.vehicles[bus].speed.0 = 400.0;
        sim.world.vehicles[car].speed.0 = 100.0;
        update_at(sim, tick + 4);
        assert_eq!(sim.world.vehicles[car].speed.0, 0.0);
        sim.world.vehicles[car].speed.0 = 150.0;
        update_at(sim, tick + 5);
        assert_ne!(sim.world.vehicles[car].speed.0, 0.0);
    }
    assert_eq!(
        ctx.g.world.vehicles[car].trans.pos,
        resumed.world.vehicles[car].trans.pos
    );
}

#[test]
fn p7_junction_stop_dwell_resumes_from_saved_elapsed_time() {
    let mut ctx = TestCtx::new();
    let a = vec3(70.0, 200.0, 0.0);
    let b = vec3(270.0, 200.0, 0.0);
    let c = vec3(455.0, 345.0, 0.0);
    ctx.build_roads(&[a, b, c]);
    let (junction, from, to, road, entry) = {
        let map = ctx.g.map();
        let from = lane_at(&map, a, b);
        let lane = map.get(from).unwrap();
        (
            junction_at(&map, b),
            from,
            lane_at(&map, b, c),
            lane.parent,
            lane.points.last(),
        )
    };
    ctx.g.map_mut().install_junction_rules(
        junction,
        JunctionRules::new(vec![(road, ApproachRole::Stop)], vec![]).unwrap(),
    );
    let id = staged_vehicle(&mut ctx.g, VehicleKind::Truck, from, to, 6.0);
    let tick = ctx.g.get_tick();
    ctx.g.world.vehicles[id].speed.0 = 300.0;
    update_at(&mut ctx.g, tick + 1);
    let hold = ctx.g.world.vehicles[id].trans.pos;
    assert!((hold.distance(entry) - VehicleKind::Truck.width() * 0.5).abs() < 0.001);
    assert_eq!(ctx.g.world.vehicles[id].speed.0, 0.0);
    update_at(&mut ctx.g, tick + 2);
    let bytes = Bincode::encode(&ctx.g).unwrap();
    let mut resumed = Bincode::decode::<Simulation>(&bytes).unwrap();
    for sim in [&mut ctx.g, &mut resumed] {
        sim.world.vehicles[id].speed.0 = 0.15;
        update_at(sim, tick + 3);
        assert_eq!(sim.world.vehicles[id].trans.pos, hold);
        sim.world.vehicles[id].speed.0 = 0.15;
        update_at(sim, tick + 4);
        assert_ne!(sim.world.vehicles[id].trans.pos, hold);
    }
    assert_eq!(
        ctx.g.world.vehicles[id].trans.pos,
        resumed.world.vehicles[id].trans.pos
    );
}

#[test]
fn p7_junction_same_tick_walk_and_vehicle_share_the_barrier() {
    use crate::souls::human::spawn_human;

    let mut ctx = TestCtx::new();
    let a = vec3(70.0, 200.0, 0.0);
    let b = vec3(270.0, 200.0, 0.0);
    let c = vec3(480.0, 200.0, 0.0);
    let d = vec3(270.0, 410.0, 0.0);
    ctx.build_roads(&[a, b, c]);
    ctx.build_roads(&[d, b]);
    let house = ctx.build_house_near(vec3(160.0, 200.0, 0.0).xy());
    let human = spawn_human(&mut ctx.g, house).unwrap();
    let (junction, from, to, road, walker_path, walker_pos, walker_entry) = {
        let map = ctx.g.map();
        let junction = junction_at(&map, b);
        let from = lane_at(&map, a, b);
        let to = lane_at(&map, b, c);
        let road = map.get(from).unwrap().parent;
        let turn = map
            .get(junction)
            .unwrap()
            .turns()
            .find(|turn| {
                turn.kind == TurnKind::Crosswalk && map.get(turn.id.src).unwrap().parent == road
            })
            .unwrap();
        let lane = map.get(turn.id.src).unwrap();
        let direction = lane.dir_from(if lane.src == junction {
            lane.dst
        } else {
            lane.src
        });
        let entry = turn.points.first();
        let start = if direction == TraverseDirection::Forward {
            lane.points.point_along(lane.points.length() - 1.0)
        } else {
            lane.points.point_along(1.0)
        };
        let exit = map.get(turn.id.dst).unwrap();
        let terminal = exit.points.middle();
        let itinerary = Itinerary {
            kind: ItineraryKind::Route(
                Route {
                    reversed_route: vec![
                        Traversable::new(TraverseKind::Lane(exit.id), exit.dir_from(junction)),
                        Traversable::new(TraverseKind::Turn(turn.id), TraverseDirection::Forward),
                    ],
                    end_pos: terminal,
                    cur: Traversable::new(TraverseKind::Lane(lane.id), direction),
                    target: ItineraryTarget::Position(terminal),
                },
                PathKind::Pedestrian,
            ),
            reversed_local_path: vec![entry],
            ..Itinerary::default()
        };
        (junction, from, to, road, itinerary, start, entry)
    };
    ctx.g
        .map_mut()
        .update_intersection(junction, |inter| inter.light_policy = LightPolicy::NoLights);
    ctx.g.map_mut().install_junction_rules(
        junction,
        JunctionRules::new(vec![(road, ApproachRole::Priority)], vec![]).unwrap(),
    );
    let collider = crate::transportation::put_pedestrian_in_transport_grid(
        &mut ctx.g.write::<TransportGrid>(),
        walker_pos,
    );
    ctx.g.world.edit_human(human, |human| {
        human.trans.pos = walker_pos;
        human.location = Location::Outside;
        human.collider = Some(collider);
        human.it = walker_path;
        human.speed.0 = 100.0;
    });
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 7.0);
    ctx.g.world.vehicles[car].speed.0 = 450.0;
    let tick = ctx.g.get_tick();
    update_at(&mut ctx.g, tick + 1);
    assert_eq!(ctx.g.world.humans[human].speed.0, 0.0);
    assert!((ctx.g.world.humans[human].trans.pos.distance(walker_entry) - 0.3).abs() < 0.001);
    assert!(matches!(
        ctx.g.world.vehicles[car].it.get_travers().unwrap().kind,
        TraverseKind::Turn(_)
    ));
}

#[test]
fn p7_junction_aged_stop_precedes_fresh_priority_for_the_same_exit() {
    let mut ctx = TestCtx::new();
    let a = vec3(70.0, 200.0, 0.0);
    let b = vec3(270.0, 200.0, 0.0);
    let c = vec3(480.0, 200.0, 0.0);
    let d = vec3(270.0, 410.0, 0.0);
    ctx.build_roads(&[a, b, c]);
    ctx.build_roads(&[d, b]);
    let (junction, from, to, competing, stop_road, priority_road) = {
        let map = ctx.g.map();
        let from = lane_at(&map, a, b);
        let competing = lane_at(&map, d, b);
        (
            junction_at(&map, b),
            from,
            lane_at(&map, b, c),
            competing,
            map.get(from).unwrap().parent,
            map.get(competing).unwrap().parent,
        )
    };
    ctx.g
        .map_mut()
        .update_intersection(junction, |inter| inter.light_policy = LightPolicy::NoLights);
    ctx.g.map_mut().install_junction_rules(
        junction,
        JunctionRules::new(
            vec![
                (stop_road, ApproachRole::Stop),
                (priority_road, ApproachRole::Priority),
            ],
            vec![],
        )
        .unwrap(),
    );
    let distance = VehicleKind::Car.width() * 0.5;
    let oldest = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, distance);
    let stopped = ctx.g.world.vehicles[oldest].trans.pos;
    let tick = ctx.g.get_tick();
    let first_aged = tick + 1 + game_secs_to_ticks(60);
    for tick in tick + 1..first_aged {
        update_at(&mut ctx.g, tick);
    }
    let younger = staged_vehicle(&mut ctx.g, VehicleKind::Car, competing, to, 9.0);
    ctx.g.world.vehicles[oldest].speed.0 = 0.15;
    ctx.g.world.vehicles[younger].speed.0 = 500.0;
    update_at(&mut ctx.g, first_aged);
    assert_ne!(ctx.g.world.vehicles[oldest].trans.pos, stopped);
    assert_eq!(ctx.g.world.vehicles[younger].speed.0, 0.0);
    assert!(ctx
        .g
        .read::<JunctionAdmission>()
        .crossing_turn(TurnID::new(junction, from, to, false)));
}

fn controlled_straight(sim: &mut TestCtx, role: ApproachRole) -> (LaneID, LaneID, TurnID) {
    let a = vec3(70.0, 200.0, 0.0);
    let b = vec3(270.0, 200.0, 0.0);
    let c = vec3(480.0, 200.0, 0.0);
    sim.build_roads(&[a, b, c]);
    sim.build_roads(&[b, vec3(270.0, 410.0, 0.0)]);
    let (from, to, junction, approach) = {
        let map = sim.g.map();
        let from = lane_at(&map, a, b);
        let incoming = map.get(from).unwrap();
        (from, lane_at(&map, b, c), incoming.dst, incoming.parent)
    };
    sim.g
        .map_mut()
        .update_intersection(junction, |inter| inter.light_policy = LightPolicy::NoLights);
    sim.g.map_mut().install_junction_rules(
        junction,
        JunctionRules::new(vec![(approach, role)], vec![]).unwrap(),
    );
    (from, to, TurnID::new(junction, from, to, false))
}

#[test]
fn p7_junction_fix_heavy_vehicles_leave_stop_from_rest() {
    for kind in [VehicleKind::Bus, VehicleKind::Truck] {
        let mut ctx = TestCtx::new();
        let (from, to, turn) = controlled_straight(&mut ctx, ApproachRole::Stop);
        let vehicle = staged_vehicle(&mut ctx.g, kind, from, to, 30.0);
        let terminal = ctx.g.map().get(to).unwrap().points.point_along(50.0);
        let mut schedule = Simulation::schedule();
        let mut crossed = false;
        for _ in 0..1500 {
            ctx.g.tick(&mut schedule, []);
            let actor = &ctx.g.world.vehicles[vehicle];
            crossed |= actor
                .it
                .get_travers()
                .is_some_and(|t| t.kind == TraverseKind::Turn(turn));
            if actor.trans.pos.distance(terminal) <= 0.05 {
                break;
            }
        }
        let actor = &ctx.g.world.vehicles[vehicle];
        assert!(
            crossed && actor.trans.pos.distance(terminal) <= 0.05,
            "{kind:?}"
        );
    }
}

#[test]
fn p7_junction_fix_physical_blocker_and_stop_spillback() {
    use crate::transportation::queue_diagnostics::{watch_spillback, QueueDiagnostics};

    let mut ctx = TestCtx::new();
    let (from, to, turn) = controlled_straight(&mut ctx, ApproachRole::Stop);
    let entrant = staged_vehicle(&mut ctx.g, VehicleKind::Bus, from, to, 8.0);
    let exit = ctx.g.map().get(to).unwrap().points.point_along(6.0);
    let blocker = spawn_driving_vehicle(
        &mut ctx.g,
        VehicleKind::Car,
        Transform::new_dir(exit, Vec3::X),
    )
    .unwrap();
    assert!(ctx.g.world.vehicles[blocker].it.get_route().is_none());
    assert!(watch_spillback(&ctx.g, turn));
    let tick = ctx.g.get_tick();
    for step in 1..=5 {
        let (world, resources) = ctx.g.world_res();
        crate::transportation::transport_grid_synchronize(world, resources);
        ctx.g.world.vehicles[entrant].speed.0 = if step == 1 { 400.0 } else { 0.15 };
        update_at(&mut ctx.g, tick + step);
        let (world, resources) = ctx.g.world_res();
        crate::transportation::queue_diagnostics::queue_diagnostics_update(world, resources);
        let counts = ctx.g.read::<QueueDiagnostics>().spillback(turn).unwrap();
        if step <= 3 {
            assert_eq!(
                counts.longest_permissive_ticks, 0,
                "Stop dwell is ineligible"
            );
        }
    }
    let counts = ctx.g.read::<QueueDiagnostics>().spillback(turn).unwrap();
    assert_eq!(
        ctx.g.world.vehicles[entrant].speed.0, 0.0,
        "route-less collider fills the exit"
    );
    assert!(
        counts.longest_permissive_ticks > 0,
        "approach hold counts toward spillback"
    );
    ctx.g.world.vehicles[blocker].trans.pos.z += 8.0;
    ctx.g.world.vehicles[entrant].speed.0 = 0.15;
    update_at(&mut ctx.g, tick + 6);
    assert!(ctx.g.world.vehicles[entrant].speed.0 > 0.0);
}

#[test]
fn p7_junction_fix_unrelated_indoor_population_does_not_block_grants() {
    let mut ctx = TestCtx::new();
    let (from, to, turn) = controlled_straight(&mut ctx, ApproachRole::Priority);
    let house = ctx.build_house_near(vec3(160.0, 200.0, 0.0).xy());
    let human = crate::souls::human::spawn_human(&mut ctx.g, house).unwrap();
    let template = Bincode::encode(&ctx.g.world.humans[human]).unwrap();
    for _ in 0..65_536 {
        ctx.g
            .world
            .insert(Bincode::decode::<crate::world::HumanEnt>(&template).unwrap());
    }
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 8.0);
    ctx.g.world.vehicles[car].speed.0 = 400.0;
    let (world, resources) = ctx.g.world_res();
    crate::transportation::transport_grid_synchronize(world, resources);
    let tick = ctx.g.get_tick();
    update_at(&mut ctx.g, tick + 1);
    assert_eq!(
        ctx.g.world.vehicles[car].it.get_travers().unwrap().kind,
        TraverseKind::Turn(turn)
    );
}

#[test]
fn p7_junction_fix_track_and_controller_conflict_in_both_build_orders() {
    use crate::map::road_edit::{junction_edit_stamp, preview_junction_edit};
    use crate::map::{LanePatternBuilder, MapProject};
    use crate::world_command::{CommandOutcomes, PrepareBudget};
    use crate::WorldCommand;

    for track_first in [false, true] {
        for height in [10.0, 24.0] {
            let mut ctx = TestCtx::new();
            let a = vec3(70.0, 200.0, 10.0);
            let b = vec3(270.0, 200.0, 10.0);
            let c = vec3(480.0, 200.0, 10.0);
            ctx.build_roads(&[a, b, c]);
            ctx.build_roads(&[b, vec3(270.0, 410.0, 10.0)]);
            let (junction, road) = {
                let map = ctx.g.map();
                let lane = map.get(lane_at(&map, a, b)).unwrap();
                (lane.dst, lane.parent)
            };
            ctx.g
                .map_mut()
                .update_intersection(junction, |inter| inter.light_policy = LightPolicy::NoLights);
            let track = WorldCommand::MapMakeConnection {
                from: MapProject::ground(vec3(100.0, 75.0, height)),
                to: MapProject::ground(vec3(440.0, 325.0, height)),
                inter: None,
                pat: LanePatternBuilder::new().rail(true).build(),
            };
            if track_first {
                ctx.g.apply_commands([&track]);
                assert!(ctx.g.read::<CommandOutcomes>().last.is_empty());
            }
            let rules = JunctionRules::new(vec![(road, ApproachRole::Priority)], vec![]).unwrap();
            let mut budget = PrepareBudget::for_preview(280_000);
            let expected = junction_edit_stamp(&ctx.g, junction, &mut budget).unwrap();
            let preview = preview_junction_edit(&ctx.g, junction, expected, &rules, &mut budget);
            if track_first && height == 10.0 {
                assert!(matches!(
                    preview,
                    Err(crate::world_command::Rejection::InvalidJunctionRules(
                        crate::map::JunctionRuleError::RailConflict
                    ))
                ));
                continue;
            }
            ctx.g.apply_commands([&WorldCommand::MapSetJunctionRules {
                junction,
                expected,
                rules,
                quote: preview.unwrap().quote,
            }]);
            assert!(ctx
                .g
                .read::<CommandOutcomes>()
                .junction_edit(0)
                .unwrap()
                .is_ok());
            if !track_first {
                let hashes = ctx.g.hashes();
                ctx.g.apply_commands([&track]);
                assert_eq!(
                    ctx.g.read::<CommandOutcomes>().last.is_empty(),
                    height != 10.0
                );
                if height == 10.0 {
                    assert_eq!(ctx.g.hashes(), hashes);
                }
            }
            let bytes = Bincode::encode(&ctx.g).unwrap();
            Bincode::decode::<Simulation>(&bytes).unwrap();
        }
    }
}

#[test]
fn p7_junction_fix_loaded_hold_line_claim_cannot_skip_dwell() {
    let mut ctx = TestCtx::new();
    let (from, to, turn) = controlled_straight(&mut ctx, ApproachRole::Stop);
    let vehicle = staged_vehicle(&mut ctx.g, VehicleKind::Bus, from, to, 8.0);
    let lane_end = ctx.g.map().get(from).unwrap().points.last();
    let half = VehicleKind::Bus.width() * 0.5;
    ctx.g.world.vehicles[vehicle].trans.pos = lane_end - Vec3::X * half;
    let hold = ctx.g.world.vehicles[vehicle].trans.pos;
    ctx.g
        .write::<JunctionAdmission>()
        .seed_saved_vehicle_claim(vehicle, turn, 0);
    ctx.g.write::<crate::Replay>().registry = Some(prototypes::prototypes().fingerprint());
    let bytes = Bincode::encode(&ctx.g).unwrap();
    let mut loaded = Bincode::decode::<Simulation>(&bytes).unwrap();
    assert_eq!(loaded.read::<JunctionAdmission>().crossing_count(), 0);
    let tick = loaded.get_tick();
    loaded.world.vehicles[vehicle].speed.0 = 0.15;
    update_at(&mut loaded, tick + 1);
    assert_eq!(loaded.world.vehicles[vehicle].trans.pos, hold);
}
