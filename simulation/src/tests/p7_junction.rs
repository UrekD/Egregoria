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

#[test]
fn p7_junction_rule_preserves_panic_at_the_opposite_end() {
    check_rule_locality(70.0, false);
    check_rule_locality(232.0, true);
}

fn check_rule_locality(origin_x: f32, short_lane: bool) {
    use crate::map::road_edit::{junction_edit_stamp, preview_junction_edit};
    use crate::transportation::road::calc_decision;
    use crate::transportation::{TransportState, TransportationGroup, Vehicle, VehicleState};
    use crate::world_command::{CommandOutcomes, PrepareBudget};
    use crate::WorldCommand;

    let mut ctx = TestCtx::new();
    let a = vec3(origin_x, 200.0, 0.0);
    let b = vec3(270.0, 200.0, 0.0);
    let c = vec3(455.0, 345.0, 0.0);
    ctx.build_roads(&[a, b, c]);
    let (junction, from, to, road, length) = {
        let map = ctx.g.map();
        let from = lane_at(&map, a, b);
        let lane = map.get(from).unwrap();
        assert!(map.junction_rules(lane.src).is_none());
        (
            lane.dst,
            from,
            lane_at(&map, b, c),
            lane.parent,
            lane.points.length(),
        )
    };
    assert_eq!(length < 36.0, short_lane, "fixture lane length {length}");
    let far = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, length - 2.0);
    let time = GameTime::new(Tick(ctx.g.get_tick()));
    let drive = |sim: &Simulation, id| {
        let entity = &sim.world().vehicles[id];
        let mut vehicle = Vehicle {
            ang_velocity: 0.0,
            wait_time: 0.0,
            max_speed_multiplier: 1.0,
            state: VehicleState::Panicking(time.instant()),
            kind: VehicleKind::Car,
            tint: geom::Color::WHITE,
            flag: 0,
        };
        let state = TransportState {
            dir: entity.trans.dir.xy(),
            height: entity.trans.pos.z,
            radius: vehicle.kind.width() * 0.5,
            group: TransportationGroup::Vehicles,
            ..TransportState::default()
        };
        let leader = entity.trans.pos.xy() + entity.trans.dir.xy() * 1.5;
        let (speed, _) = calc_decision(
            id,
            &mut vehicle,
            &sim.map(),
            &time,
            &entity.trans,
            &state,
            &entity.it,
            [(leader, &state)].into_iter(),
        );
        (speed, matches!(vehicle.state, VehicleState::Panicking(_)))
    };
    let stock = drive(&ctx.g, far);
    assert!(stock.0 > 0.0 && stock.1);

    let rules = JunctionRules::new(vec![(road, ApproachRole::Stop)], vec![]).unwrap();
    let budget = &mut PrepareBudget::for_preview(4_194_304);
    let expected = junction_edit_stamp(&ctx.g, junction, budget).unwrap();
    let quote = preview_junction_edit(&ctx.g, junction, expected, &rules, budget)
        .unwrap()
        .quote;
    ctx.g.apply_commands([&WorldCommand::MapSetJunctionRules {
        junction,
        expected,
        rules,
        quote,
    }]);
    assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);
    assert_eq!(drive(&ctx.g, far), stock);
    let near = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 2.0);
    assert_eq!(drive(&ctx.g, near), (0.0, false));
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

fn roundabout_fixture(
    directions: &[f32],
    radius: f32,
    slope: bool,
) -> (TestCtx, IntersectionID, Vec<Vec3>) {
    use crate::map::{LanePatternBuilder, MapProject, ProjectFilter, RoundaboutPolicy};
    use geom::Radians;

    let ctx = TestCtx::new();
    let center = vec3(260.0, 260.0, if slope { 16.0 } else { 0.0 });
    let mut ends = Vec::new();
    let mut map = ctx.g.map_mut();
    for (i, &angle) in directions.iter().enumerate() {
        let dir = Radians::from_deg(angle).vec2();
        let end = (center.xy() + dir * 155.0).z(if slope { 12.0 + dir.x * 8.0 } else { 0.0 });
        let start = map.project(center, 0.0, ProjectFilter::INTER);
        map.make_connection(
            start,
            MapProject::ground(end),
            None,
            &LanePatternBuilder::new()
                .n_lanes(1 + (i % 3) as u32)
                .build(),
        )
        .unwrap();
        ends.push(end);
    }
    let junction = junction_at(&map, center);
    map.update_intersection(junction, |intersection| {
        intersection.turn_policy.roundabout = Some(RoundaboutPolicy { radius });
    });
    drop(map);
    (ctx, junction, ends)
}

#[test]
fn roadtools_roundabout_keeps_tight_arm_sweeps_on_the_deck() {
    use geom::{Polygon, Segment};

    let (ctx, junction, _) = roundabout_fixture(&[0.0, 30.0, 119.0, 194.0, 279.0], 8.0, false);
    let map = ctx.g.map();
    let inter = map.get(junction).unwrap();
    let ring = inter.roundabout_geometry().unwrap();
    let asphalt: Polygon = inter
        .roads
        .iter()
        .zip(inter.roads.iter().cycle().skip(1))
        .flat_map(|(&a, &b)| {
            ring.road_corner(junction, &map.roads()[a], &map.roads()[b], false)
                .unwrap()
        })
        .map(Vec3::xy)
        .collect();
    let edges = asphalt.segments().collect::<Vec<_>>();
    for (i, &a) in edges.iter().enumerate() {
        for &b in edges.iter().skip(i + 2) {
            if a.src != b.dst {
                assert!(
                    a.intersection_point(&b).is_none(),
                    "self-intersection {a:?} {b:?}"
                );
            }
        }
    }
    for turn in inter.turns().filter(|turn| turn.kind == TurnKind::Driving) {
        for segment in turn.points.segments() {
            for step in 1..8 {
                let p = segment.src.lerp(segment.dst, step as f32 / 8.0).xy();
                assert!(asphalt.contains(p), "driving sweep outside deck at {p:?}");
                assert!(p.distance(ring.center.xy()) > ring.island_radius());
            }
            let sweep = Segment::new(segment.src.xy(), segment.dst.xy());
            assert!(
                edges.iter().all(|edge| sweep
                    .intersection_point(edge)
                    .is_none_or(|p| p.is_close(turn.points.first().xy(), 0.001)
                        || p.is_close(turn.points.last().xy(), 0.001))),
                "driving sweep crosses the curb"
            );
        }
    }
}

#[test]
fn roadtools_roundabout_leaves_room_to_exit_and_uses_stock_lights_on_fallback() {
    use crate::map::{LanePatternBuilder, MapProject, ProjectFilter, RoundaboutPolicy};

    let mut ctx = TestCtx::new();
    let center = vec3(260.0, 260.0, 0.0);
    let short = center + vec3(26.0, 0.0, 0.0);
    let long = center - vec3(155.0, 0.0, 0.0);
    let (junction, incoming, outgoing) = {
        let mut map = ctx.g.map_mut();
        for end in [center + vec3(0.0, 155.0, 0.0), long, short] {
            let start = map.project(center, 0.0, ProjectFilter::INTER);
            let pattern = LanePatternBuilder::new()
                .sidewalks(false)
                .parking(false)
                .build();
            map.make_connection(start, MapProject::ground(end), None, &pattern)
                .unwrap();
        }
        let junction = junction_at(&map, center);
        map.update_intersection(junction, |inter| {
            inter.turn_policy.roundabout = Some(RoundaboutPolicy { radius: 28.0 });
            inter.light_policy = LightPolicy::NoLights;
        });
        (
            junction,
            lane_at(&map, long, center),
            lane_at(&map, center, short),
        )
    };
    let (start, direction, itinerary) = {
        let map = ctx.g.map();
        let lane = map.get(incoming).unwrap();
        let (start, direction) = lane.points.point_dir_along(lane.points.length() - 5.0);
        let itinerary =
            Itinerary::route_to_lane(Tick(ctx.g.get_tick()), start, &map, outgoing, 1.0).unwrap();
        (start, direction, itinerary)
    };
    let vehicle = spawn_driving_vehicle(
        &mut ctx.g,
        VehicleKind::Car,
        Transform::new_dir(start, direction),
    )
    .unwrap();
    ctx.g.world.vehicles[vehicle].it = itinerary;
    for _ in 0..1600 {
        ctx.tick_fast();
        if ctx.g.world.vehicles[vehicle]
            .it
            .get_travers()
            .is_some_and(|path| path.kind == TraverseKind::Lane(outgoing))
        {
            break;
        }
    }
    assert!(
        ctx.g.world.vehicles[vehicle]
            .it
            .get_travers()
            .is_some_and(|path| path.kind == TraverseKind::Lane(outgoing)),
        "vehicle did not enter the empty short exit"
    );
    let mut map = ctx.g.map_mut();
    map.update_intersection(junction, |inter| {
        inter.turn_policy.roundabout = Some(RoundaboutPolicy { radius: 3.0 });
        inter.light_policy = LightPolicy::StopSigns;
    });
    assert!(map.get(junction).unwrap().roundabout_geometry().is_none());
    assert!(!map.is_yield_roundabout(junction));
    assert!(map.get(incoming).unwrap().control.is_stop_sign());
}

#[test]
fn roadtools_roundabout_rejects_connected_rail_in_both_edit_orders() {
    use crate::map::{LanePatternBuilder, MapProject, ProjectFilter, RoundaboutPolicy};
    use crate::world_command::WorldCommand;

    let (ctx, junction, _) = roundabout_fixture(&[0.0, 180.0], 18.0, false);
    let center = ctx.g.map().get(junction).unwrap().pos;
    let build_rail = |map: &mut Map, delta: Vec3| {
        let start = map.project(center, 0.0, ProjectFilter::INTER);
        map.try_connect(
            start,
            MapProject::ground(center + delta),
            None,
            &LanePatternBuilder::new().rail(true).build(),
        )
    };
    {
        let mut map = ctx.g.map_mut();
        assert!(build_rail(&mut map, vec3(0.0, 155.0, 0.0)).is_err());
        map.update_intersection(junction, |inter| inter.turn_policy.roundabout = None);
        build_rail(&mut map, vec3(0.0, 155.0, 0.0)).unwrap();
        build_rail(&mut map, vec3(0.0, -155.0, 0.0)).unwrap();
    }
    let mut turn = ctx.g.map().get(junction).unwrap().turn_policy;
    turn.roundabout = Some(RoundaboutPolicy { radius: 18.0 });
    let command = WorldCommand::MapUpdateIntersectionPolicy {
        inter: junction,
        turn,
        light: LightPolicy::NoLights,
    };
    assert!(
        command.preview(&ctx.g).is_err(),
        "accepted rail through the island"
    );
    let mut map = ctx.g.map_mut();
    map.update_intersection(junction, |inter| inter.turn_policy = turn);
    assert!(map.get(junction).unwrap().roundabout_geometry().is_none());
    assert!(!map.is_yield_roundabout(junction));
}

#[test]
fn roadtools_roundabout_rejects_same_deck_track_and_allows_overpasses() {
    use crate::map::{LanePatternBuilder, MapProject, RoundaboutPolicy};
    use crate::world_command::{CommandOutcomes, WorldCommand};

    for track_first in [false, true] {
        for height in [10.0, 24.0] {
            let mut ctx = TestCtx::new();
            let center = vec3(270.0, 200.0, 10.0);
            ctx.build_roads(&[vec3(70.0, 200.0, 10.0), center, vec3(480.0, 200.0, 10.0)]);
            ctx.build_roads(&[center, vec3(270.0, 410.0, 10.0)]);
            let junction = junction_at(&ctx.g.map(), center);
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
            let mut turn = ctx.g.map().get(junction).unwrap().turn_policy;
            turn.roundabout = Some(RoundaboutPolicy { radius: 18.0 });
            let policy = WorldCommand::MapUpdateIntersectionPolicy {
                inter: junction,
                turn,
                light: LightPolicy::NoLights,
            };
            if track_first && height == 10.0 {
                assert!(policy.preview(&ctx.g).is_err(), "accepted a crossing track");
                continue;
            }
            ctx.g.apply_commands([&policy]);
            assert!(ctx.g.read::<CommandOutcomes>().last.is_empty());
            assert!(ctx.g.map().is_yield_roundabout(junction));
            if !track_first {
                ctx.g.apply_commands([&track]);
                assert_eq!(
                    ctx.g.read::<CommandOutcomes>().last.is_empty(),
                    height != 10.0
                );
            }
        }
    }
}

#[test]
fn roadtools_roundabout_rejects_new_track_entirely_inside_its_island() {
    use crate::map::{ConnectionError, LanePatternBuilder, MapProject};

    let (ctx, junction, _) = roundabout_fixture(&[0.0, 90.0, 180.0, 270.0], 36.0, false);
    let mut map = ctx.g.map_mut();
    let center = map.get(junction).unwrap().pos;
    let pattern = LanePatternBuilder::new().rail(true).build();
    let mut connect = |height| {
        let a = MapProject::ground(center + vec3(0.0, -12.0, height));
        let b = MapProject::ground(center + vec3(0.0, 12.0, height));
        map.try_connect(a, b, None, &pattern)
    };
    assert!(matches!(
        connect(0.0),
        Err(ConnectionError::RoundaboutRailConflict)
    ));
    assert!(connect(14.0).is_ok());
}

#[test]
fn roadtools_roundabout_has_shared_ring_and_connected_sidewalks() {
    for (directions, radius, slope) in [
        (&[0.0, 180.0][..], 18.0, false),
        (&[0.0, 107.0, 218.0][..], 24.0, true),
        (&[0.0, 90.0, 180.0, 270.0][..], 36.0, false),
        (&[0.0, 38.0, 119.0, 194.0, 279.0][..], 28.0, true),
    ] {
        let (ctx, junction, _) = roundabout_fixture(directions, radius, slope);
        let map = ctx.g.map();
        let inter = map.get(junction).unwrap();
        let turns = inter.turns().collect::<Vec<_>>();
        for &id in &inter.roads {
            let road = map.get(id).unwrap();
            assert!(
                road.interface_from(junction) > radius,
                "short interface at {directions:?}"
            );
            for &(lane, kind) in road.incoming_lanes_to(junction) {
                if kind.vehicles() {
                    for &other in inter.roads.iter().filter(|&&other| other != id) {
                        assert!(turns.iter().any(|turn| turn.id.src == lane
                            && map.get(turn.id.dst).unwrap().parent == other));
                    }
                }
            }
        }

        for turn in &turns {
            let start = map.get(turn.id.src).unwrap().get_inter_node_pos(junction);
            let end = map.get(turn.id.dst).unwrap().get_inter_node_pos(junction);
            assert_eq!(turn.points.first(), start);
            assert_eq!(turn.points.last(), end);
            if matches!(turn.kind, TurnKind::Driving | TurnKind::WalkingCorner) {
                let expected = if turn.kind == TurnKind::Driving {
                    radius
                } else {
                    radius + 4.0 + LaneKind::Walking.width() * 0.5
                };
                let on_ring =
                    |point: &Vec3| (point.xy().distance(inter.pos.xy()) - expected).abs() < 0.02;
                assert!(
                    turn.points
                        .as_slice()
                        .windows(3)
                        .any(|points| points.iter().all(on_ring)),
                    "turn {:?} misses its shared ring at {directions:?}",
                    turn.id
                );
                for points in turn.points.as_slice().windows(3) {
                    let a = (points[1] - points[0]).xy();
                    let b = (points[2] - points[1]).xy();
                    assert!(
                        a.angle(b).abs() < 0.95,
                        "sharp turn in {:?} at {directions:?}",
                        turn.id
                    );
                }
            }
        }
        assert_eq!(
            turns
                .iter()
                .filter(|turn| turn.kind == TurnKind::WalkingCorner)
                .count(),
            directions.len()
        );
        assert_eq!(
            turns
                .iter()
                .filter(|turn| turn.kind == TurnKind::Crosswalk)
                .count(),
            directions.len()
        );
    }
}

#[test]
fn roadtools_roundabout_yields_to_circulating_traffic_and_clears_the_queue() {
    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0, 180.0, 270.0], 28.0, false);
    let (from, to, contender, exits) = {
        let map = ctx.g.map();
        let center = map.get(junction).unwrap().pos;
        let from = lane_at(&map, ends[0], center);
        let to = lane_at(&map, center, ends[3]);
        let contender = lane_at(&map, ends[1], center);
        let exits = ends
            .iter()
            .map(|end| lane_at(&map, center, *end))
            .collect::<Vec<_>>();
        (from, to, contender, exits)
    };
    let lead = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 5.0);
    ctx.g.world.vehicles[lead].speed.0 = 350.0;
    let tick = ctx.g.get_tick();
    update_at(&mut ctx.g, tick + 1);
    let lead_turn = TurnID::new(junction, from, to, false);
    assert!(ctx.g.read::<JunctionAdmission>().crossing_turn(lead_turn));
    let queued = staged_vehicle(&mut ctx.g, VehicleKind::Car, contender, to, 5.0);
    ctx.g.world.vehicles[queued].speed.0 = 350.0;
    update_at(&mut ctx.g, tick + 2);
    assert_eq!(ctx.g.world.vehicles[queued].speed.0, 0.0);

    let more = {
        let map = ctx.g.map();
        let center = map.get(junction).unwrap().pos;
        [
            lane_at(&map, ends[2], center),
            lane_at(&map, ends[3], center),
        ]
    };
    let west = staged_vehicle(&mut ctx.g, VehicleKind::Car, more[0], exits[0], 15.0);
    let south = staged_vehicle(&mut ctx.g, VehicleKind::Car, more[1], exits[1], 15.0);
    let routes = [
        (lead, to),
        (queued, to),
        (west, exits[0]),
        (south, exits[1]),
    ];
    let mut discharged = std::collections::BTreeSet::new();
    for _ in 0..3200 {
        ctx.tick_fast();
        for &(id, lane) in &routes {
            let car = &ctx.g.world.vehicles[id];
            if car
                .it
                .get_travers()
                .is_some_and(|path| path.kind == TraverseKind::Lane(lane))
            {
                discharged.insert(id);
            }
        }
        if discharged.len() == routes.len() {
            break;
        }
    }
    assert_eq!(
        discharged.len(),
        routes.len(),
        "roundabout queue failed to discharge"
    );
}

fn roundabout_hotfix_movement_only() -> crate::utils::scheduler::SeqSchedule {
    let mut schedule = Simulation::schedule();
    schedule.retain_systems(|name| {
        matches!(
            name,
            "pedestrian_decision_system"
                | "transport_grid_synchronize"
                | "vehicle_decision_system"
                | "itinerary_update"
        )
    });
    schedule
}

fn roundabout_hotfix_walker(
    sim: &mut Simulation,
    house: crate::map::BuildingID,
    position: Vec3,
    itinerary: Itinerary,
) -> crate::HumanID {
    use crate::transportation::{put_pedestrian_in_transport_grid, Location};

    let id = crate::souls::human::spawn_human(sim, house).unwrap();
    let collider = put_pedestrian_in_transport_grid(&mut sim.write::<TransportGrid>(), position);
    sim.world.edit_human(id, |human| {
        human.location = Location::Outside;
        human.trans.pos = position;
        human.it = itinerary;
        human.collider = Some(collider);
    });
    id
}

#[test]
fn roundabout_hotfix_walks_from_corner_into_crosswalk() {
    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0, 180.0, 270.0], 28.0, false);
    let mut schedule = roundabout_hotfix_movement_only();
    let house = ctx.build_house_near(ends[0].xy());
    let (start, destination, route) = {
        let map = ctx.g.map();
        let sidewalks = |arm| {
            let end = junction_at(&map, ends[arm]);
            map.lanes().values().filter(move |lane| {
                lane.kind == LaneKind::Walking && (lane.src == end || lane.dst == end)
            })
        };
        sidewalks(0)
            .flat_map(|from| sidewalks(2).map(move |to| (from, to)))
            .find_map(|(from, to)| {
                let near_mouth = |lane: &crate::map::Lane| {
                    let offset = if lane.dst == junction {
                        lane.points.length() - 5.0
                    } else {
                        5.0
                    };
                    lane.points.point_along(offset)
                };
                let start = near_mouth(from);
                let end = near_mouth(to);
                let route = Itinerary::route(Tick(0), start, end, &map, PathKind::Pedestrian)?;
                route.get_route()?.reversed_route.windows(2).any(|pair| {
                    matches!(pair[0].kind, TraverseKind::Turn(id)
                        if map.get(junction).unwrap().find_turn(id).unwrap().kind == TurnKind::Crosswalk)
                        && matches!(pair[1].kind, TraverseKind::Turn(id)
                            if map.get(junction).unwrap().find_turn(id).unwrap().kind == TurnKind::WalkingCorner)
                }).then_some((start, end, route))
            })
            .unwrap()
    };
    let id = roundabout_hotfix_walker(&mut ctx.g, house, start, route);
    // Slow walkers need more than 150 real seconds for two corners and a crossing.
    for _ in 0..12_000 {
        ctx.g.tick(&mut schedule, &[]);
        if ctx.g.world.humans[id].trans.pos.is_close(destination, 0.01) {
            return;
        }
    }
    let walker = &ctx.g.world.humans[id];
    panic!(
        "walker stopped at {:?}, traversal {:?}, intended destination {destination:?}",
        walker.trans.pos,
        walker.it.get_travers()
    );
}

#[test]
fn roundabout_hotfix_clears_a_cancelled_crosswalk() {
    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 107.0, 218.0], 24.0, false);
    let mut schedule = roundabout_hotfix_movement_only();
    let house = ctx.build_house_near(ends[0].xy());
    let (start, curbs) = {
        let map = ctx.g.map();
        let crossing = map.get(junction).unwrap().crosswalks().next().unwrap();
        (
            crossing.points.middle(),
            [crossing.points.first(), crossing.points.last()],
        )
    };
    let id = roundabout_hotfix_walker(&mut ctx.g, house, start, Itinerary::NONE);
    for _ in 0..2_000 {
        ctx.g.tick(&mut schedule, &[]);
    }
    let walker = &ctx.g.world.humans[id];
    assert!(
        curbs
            .iter()
            .any(|&curb| walker.trans.pos.is_close(curb, 0.01)),
        "walker stopped at {:?}",
        walker.trans.pos
    );
    assert!(walker.collider.is_none());
}

#[test]
fn roundabout_hotfix_dense_sidewalk_does_not_close_entries() {
    let (mut ctx, junction, ends) =
        roundabout_fixture(&[0.0, 38.0, 119.0, 194.0, 279.0], 28.0, false);
    let mut schedule = roundabout_hotfix_movement_only();
    let house = ctx.build_house_near(ends[0].xy());
    ctx.g
        .write::<crate::souls::residence::Residences>()
        .by_building
        .get_mut(&house)
        .unwrap()
        .capacity = 260;
    let (from, to, sidewalk) = {
        let map = ctx.g.map();
        let center = map.get(junction).unwrap().pos;
        let sidewalk = map
            .lanes()
            .values()
            .find(|lane| {
                lane.kind == LaneKind::Walking
                    && lane.src == junction
                    && lane.dst == junction_at(&map, ends[2])
            })
            .unwrap();
        (
            lane_at(&map, ends[0], center),
            lane_at(&map, center, ends[1]),
            sidewalk.points.clone(),
        )
    };
    for _ in 0..260 {
        let position = sidewalk.point_along(2.0);
        let route = Itinerary::simple(vec![sidewalk.point_along(5.0), sidewalk.point_along(85.0)]);
        roundabout_hotfix_walker(&mut ctx.g, house, position, route);
    }
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 8.0);
    for _ in 0..350 {
        ctx.g.tick(&mut schedule, &[]);
    }
    let car = &ctx.g.world.vehicles[car];
    assert_ne!(
        car.it.get_travers().unwrap().kind,
        TraverseKind::Lane(from),
        "a sidewalk crowd closed a clear entry: {:?} at {:?}",
        car.it.get_travers(),
        car.trans.pos
    );
}

#[test]
fn roundabout_hotfix_four_arm_mixed_traffic_soak() {
    use crate::utils::par_command_buffer::SimDrop;
    use std::collections::BTreeMap;

    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0, 180.0, 270.0], 20.0, false);
    let mut schedule = roundabout_hotfix_movement_only();
    let house = ctx.build_house_near(ends[0].xy());
    ctx.g
        .write::<crate::souls::residence::Residences>()
        .by_building
        .get_mut(&house)
        .unwrap()
        .capacity = 224;
    let (driving, walking) = {
        let map = ctx.g.map();
        let inter = map.get(junction).unwrap();
        let roads = ends
            .iter()
            .map(|&end| {
                let other = junction_at(&map, end);
                inter
                    .roads
                    .iter()
                    .map(|&id| map.get(id).unwrap())
                    .find(|road| road.other_end(junction) == Some(other))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let driving = roads
            .iter()
            .enumerate()
            .flat_map(|(arm, &road)| {
                road.incoming_lanes_to(junction)
                    .iter()
                    .filter(|(_, kind)| kind.vehicles())
                    .map(move |&(lane, _)| (arm, lane))
            })
            .map(|(arm, from)| {
                let exits = (1..4)
                    .map(|delta| {
                        roads[(arm + delta) % 4]
                            .outgoing_lanes_from(junction)
                            .iter()
                            .find(|(_, kind)| kind.vehicles())
                            .unwrap()
                            .0
                    })
                    .collect::<Vec<_>>();
                (from, exits)
            })
            .collect::<Vec<_>>();
        let walking = roads
            .iter()
            .flat_map(|road| {
                let sidewalks = road.sidewalks(junction);
                [sidewalks.incoming.unwrap(), sidewalks.outgoing.unwrap()]
            })
            .collect::<Vec<_>>();
        (driving, walking)
    };
    let mut arrived_cars = 0;
    let mut arrived_walkers = 0;
    let mut stuck_cars = BTreeMap::new();
    let mut stuck_walkers = BTreeMap::new();
    for wave in 0..8 {
        let mut cars = BTreeMap::new();
        let mut walkers = BTreeMap::new();
        for (i, (from, exits)) in driving.iter().enumerate() {
            for row in 0..4 {
                let to = exits[(i + row + wave) % exits.len()];
                let kind = match (i + row + wave) % 8 {
                    0 => VehicleKind::Bus,
                    1 => VehicleKind::Truck,
                    _ => VehicleKind::Car,
                };
                let id = staged_vehicle(
                    &mut ctx.g,
                    kind,
                    *from,
                    to,
                    7.0 + row as f32 * 23.0 + (i % 3) as f32 * 7.5,
                );
                cars.insert(id, to);
            }
        }
        for i in 0..28 {
            let (start, end, route) = {
                let map = ctx.g.map();
                let from = map.get(walking[i % walking.len()]).unwrap();
                let to = map
                    .get(walking[(i + 3 + wave % 4) % walking.len()])
                    .unwrap();
                let near_mouth = |lane: &crate::map::Lane, distance| {
                    lane.points.point_along(if lane.dst == junction {
                        lane.points.length() - distance
                    } else {
                        distance
                    })
                };
                let start = near_mouth(from, 3.0 + (i / walking.len()) as f32 * 4.0);
                let end = near_mouth(to, 12.0);
                (
                    start,
                    end,
                    Itinerary::route(
                        Tick(ctx.g.get_tick()),
                        start,
                        end,
                        &map,
                        PathKind::Pedestrian,
                    )
                    .unwrap(),
                )
            };
            let id = roundabout_hotfix_walker(&mut ctx.g, house, start, route);
            walkers.insert(id, end);
        }
        let mut clear_ticks = 0;
        let mut dispatched = Vec::new();
        for _ in 0..36_000 {
            ctx.g.tick(&mut schedule, &[]);
            cars.retain(|&id, &mut to| {
                let car = &ctx.g.world.vehicles[id];
                let traversal = car.it.get_travers().map(|t| t.kind);
                if traversal == Some(TraverseKind::Lane(to)) {
                    let map = ctx.g.map();
                    if car.trans.pos.distance(map.get(to).unwrap().points.first()) >= 22.0 {
                        dispatched.push(id);
                        stuck_cars.remove(&id);
                        return false;
                    }
                }
                if matches!(traversal, Some(TraverseKind::Turn(turn)) if turn.parent == junction)
                    && car.speed.0 < 0.01
                {
                    *stuck_cars.entry(id).or_insert(0) += 1;
                } else {
                    stuck_cars.remove(&id);
                }
                true
            });
            for id in dispatched.drain(..) {
                ctx.g
                    .world
                    .vehicles
                    .remove(id)
                    .unwrap()
                    .sim_drop(id, &mut ctx.g);
                arrived_cars += 1;
            }
            walkers.retain(|&id, &mut end| {
                let human = &ctx.g.world.humans[id];
                if human.trans.pos.is_close(end, 0.01) {
                    arrived_walkers += 1;
                    stuck_walkers.remove(&id);
                    return false;
                }
                if human.speed.0 < 0.01 {
                    let map = ctx.g.map();
                    if map.get(junction).unwrap().crosswalks().any(|turn| {
                        turn.points.project_dist2(human.trans.pos) < 0.01
                            && !human.trans.pos.is_close(turn.points.first(), 0.3)
                            && !human.trans.pos.is_close(turn.points.last(), 0.3)
                    }) {
                        *stuck_walkers.entry(id).or_insert(0) += 1;
                    }
                } else {
                    stuck_walkers.remove(&id);
                }
                true
            });
            let stuck = stuck_cars
                .values()
                .chain(stuck_walkers.values())
                .any(|&ticks| ticks > 100);
            clear_ticks = if stuck { 0 } else { clear_ticks + 1 };
            if cars.is_empty() && walkers.is_empty() {
                break;
            }
        }
        assert!(cars.is_empty() && walkers.is_empty() && clear_ticks > 5 * prototypes::TICKS_PER_MINUTE,
            "wave {wave}: {} cars and {} walkers unfinished, {} cars stopped in the ring, {} walkers stopped on crossings, {clear_ticks} clear ticks",
            cars.len(), walkers.len(), stuck_cars.len(), stuck_walkers.len());
    }
    assert_eq!((arrived_cars, arrived_walkers), (224, 224));
}

#[test]
fn roadtools_roundabout_recovers_saved_two_arm_junctions() {
    for directions in [&[0.0, 180.0][..], &[0.0, 90.0][..]] {
        let (ctx, junction, _) = roundabout_fixture(directions, 18.0, false);
        let legacy_bytes = {
            let mut map = ctx.g.map_mut();
            let policy = map.get(junction).unwrap().turn_policy;
            map.update_intersection(junction, |inter| inter.turn_policy.roundabout = None);
            assert!(map
                .get(junction)
                .unwrap()
                .roads
                .iter()
                .all(|&id| { map.get(id).unwrap().interface_from(junction) < 18.0 }));
            map.intersections[junction].turn_policy = policy;
            Bincode::encode(&*map).unwrap()
        };
        let loaded: Map = Bincode::decode(&legacy_bytes).unwrap();
        let ring = loaded.get(junction).unwrap().roundabout_geometry().unwrap();
        assert_eq!(ring.radius, 18.0);
        assert!(loaded.is_yield_roundabout(junction));
        let saved = Bincode::encode(&loaded).unwrap();
        let reloaded: Map = Bincode::decode(&saved).unwrap();
        assert_eq!(Bincode::encode(&reloaded).unwrap(), saved);
    }
}

#[test]
fn roadtools_roundabout_resumes_a_full_legacy_snapshot_mid_turn() {
    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0], 18.0, false);
    let (from, to) = {
        let mut map = ctx.g.map_mut();
        map.update_intersection(junction, |inter| inter.turn_policy.roundabout = None);
        let center = map.get(junction).unwrap().pos;
        (
            lane_at(&map, ends[0], center),
            lane_at(&map, center, ends[1]),
        )
    };
    let id = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 5.0);
    let turn = TurnID::new(junction, from, to, false);
    let path = ctx
        .g
        .map()
        .get(junction)
        .unwrap()
        .find_turn(turn)
        .unwrap()
        .points
        .clone();
    let (mid, dir) = path.point_dir_along(path.length() * 0.5);
    let car = &mut ctx.g.world.vehicles[id];
    car.trans = Transform::new_dir(mid, dir);
    let ItineraryKind::Route(route, _) = &mut car.it.kind else {
        unreachable!()
    };
    route.cur = route.reversed_route.pop().unwrap();
    car.it.reversed_local_path = path.iter().rev().take(2).copied().collect();
    let old_waypoints = car.it.local_path().to_vec();
    let tick = crate::map_dynamic::transit::elapsed_ticks(&ctx.g.resources).0;
    ctx.g
        .write::<JunctionAdmission>()
        .seed_saved_vehicle_claim(id, turn, tick);

    let house = ctx.build_house_near(ends[0].xy());
    let human_id = crate::souls::human::spawn_human(&mut ctx.g, house).unwrap();
    let (walk_turn, walk_path, walk_destination, walk_target) = {
        let map = ctx.g.map();
        let corner = map
            .get(junction)
            .unwrap()
            .turns()
            .find(|turn| turn.kind == TurnKind::WalkingCorner)
            .unwrap();
        let lane = map.get(corner.id.dst).unwrap();
        (
            corner.id,
            corner.points.clone(),
            lane.id,
            lane.points.point_along(12.0),
        )
    };
    let (position, direction) = walk_path.point_dir_along(walk_path.length() * 0.5);
    let collider = crate::transportation::put_pedestrian_in_transport_grid(
        &mut ctx.g.write::<TransportGrid>(),
        position,
    );
    let old_walk = walk_path.iter().rev().take(2).copied().collect::<Vec<_>>();
    ctx.g.world.edit_human(human_id, |human| {
        human.location = Location::Outside;
        human.trans = Transform::new_dir(position, direction);
        human.collider = Some(collider);
        human.it = Itinerary {
            kind: ItineraryKind::Route(
                Route {
                    reversed_route: vec![forward_lane(walk_destination)],
                    end_pos: walk_target,
                    cur: Traversable::new(
                        TraverseKind::Turn(walk_turn),
                        TraverseDirection::Forward,
                    ),
                    target: ItineraryTarget::Position(walk_target),
                },
                PathKind::Pedestrian,
            ),
            reversed_local_path: old_walk.clone(),
            ..Itinerary::default()
        };
    });
    ctx.g.map_mut().intersections[junction]
        .turn_policy
        .roundabout = Some(crate::map::RoundaboutPolicy { radius: 18.0 });

    let snapshot = ctx
        .g
        .encode_slot(crate::SnapshotRevision::new(1))
        .unwrap()
        .into_snapshot_bytes();
    let mut loaded =
        Simulation::load_compressed_snapshot(&snapshot, crate::MAX_SNAPSHOT_BYTES).unwrap();
    let resumed = &loaded.world.vehicles[id].it;
    assert_eq!(
        resumed.get_travers().unwrap().kind,
        TraverseKind::Turn(turn)
    );
    assert_ne!(resumed.local_path(), old_waypoints);
    assert_ne!(loaded.world.humans[human_id].it.local_path(), old_walk);
    assert!(loaded.read::<JunctionAdmission>().crossing_turn(turn));
    // The migrated walker stays on the sidewalk and needs no crosswalk claim.
    assert!(!loaded.read::<JunctionAdmission>().crossing_turn(walk_turn));
    {
        let map = loaded.map();
        let ring = map.get(junction).unwrap().roundabout_geometry().unwrap();
        assert!(
            loaded.world.vehicles[id]
                .trans
                .pos
                .xy()
                .distance(ring.center.xy())
                > ring.island_radius()
        );
    }
    let mut schedule = Simulation::schedule();
    for _ in 0..2000 {
        loaded.tick(&mut schedule, &[]);
        if loaded.world.vehicles[id]
            .it
            .get_travers()
            .is_some_and(|path| path.kind == TraverseKind::Lane(to))
        {
            return;
        }
    }
    panic!("resumed vehicle did not clear the junction");
}

#[test]
fn roadtools_roundabout_preserves_a_saved_claim_at_the_exit_mouth() {
    use crate::transportation::junction_admission::{HoldReason, JunctionFrame};

    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0, 180.0], 18.0, false);
    let (from, to, other_from, other_to, new_exit, new_dir) = {
        let map = ctx.g.map();
        let center = map.get(junction).unwrap().pos;
        let to = lane_at(&map, center, ends[1]);
        let exit = &map.get(to).unwrap().points;
        (
            lane_at(&map, ends[0], center),
            to,
            lane_at(&map, ends[2], center),
            lane_at(&map, center, ends[2]),
            exit.first(),
            exit.first_dir().unwrap(),
        )
    };
    ctx.g.map_mut().update_intersection(junction, |inter| {
        inter.turn_policy.roundabout = None;
    });

    let turn = TurnID::new(junction, from, to, false);
    let cleared_turn = TurnID::new(junction, other_from, to, false);
    let vehicle = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 5.0);
    let cleared = staged_vehicle(&mut ctx.g, VehicleKind::Car, other_from, to, 5.0);
    let conflict = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, other_to, 12.0);
    let old_exit = ctx.g.map().get(to).unwrap().points.clone();
    assert!(old_exit.first().distance(new_exit) > VehicleKind::Car.width());
    for (id, offset) in [(vehicle, 0.5), (cleared, 35.0)] {
        let actor = &mut ctx.g.world.vehicles[id];
        let (pos, dir) = old_exit.point_dir_along(offset);
        actor.trans = Transform::new_dir(pos, dir);
        let ItineraryKind::Route(route, _) = &mut actor.it.kind else {
            unreachable!();
        };
        route.cur = forward_lane(to);
        route.reversed_route.clear();
        route.target = ItineraryTarget::DirectedLane(to, 80.0);
        route.end_pos = old_exit.point_along(80.0);
        actor.it.reversed_local_path = vec![route.end_pos];
    }
    let tick = crate::map_dynamic::transit::elapsed_ticks(&ctx.g.resources).0;
    {
        let mut admission = ctx.g.write::<JunctionAdmission>();
        admission.seed_saved_vehicle_claim(vehicle, turn, tick);
        admission.seed_saved_vehicle_claim(cleared, cleared_turn, tick);
    }
    ctx.g.map_mut().intersections[junction]
        .turn_policy
        .roundabout = Some(crate::map::RoundaboutPolicy { radius: 18.0 });

    let snapshot = ctx.g.encode_slot(crate::SnapshotRevision::new(1)).unwrap();
    let mut loaded = Simulation::load_compressed_snapshot(
        &snapshot.into_snapshot_bytes(),
        crate::MAX_SNAPSHOT_BYTES,
    )
    .unwrap();
    assert!(loaded.read::<JunctionAdmission>().crossing_turn(turn));
    assert!(!loaded
        .read::<JunctionAdmission>()
        .crossing_turn(cleared_turn));
    assert_eq!(loaded.world.vehicles[vehicle].trans.pos, new_exit);

    let past_exit = new_exit + new_dir * 15.0;
    let (entry, direction) = {
        let map = loaded.map();
        let approach = &map.get(from).unwrap().points;
        (approach.last(), approach.last_dir().unwrap())
    };
    let half = VehicleKind::Car.width() * 0.5;
    let approach = entry - direction * (half + 0.1);
    let claimant = &mut loaded.world.vehicles[conflict];
    claimant.trans = Transform::new_dir(approach, direction);
    claimant.speed.0 = 30.0;
    let prepare = |sim: &mut Simulation| {
        let tick = crate::map_dynamic::transit::elapsed_ticks(&sim.resources).0;
        let (world, resources) = sim.world_res();
        crate::transportation::transport_grid_synchronize(world, resources);
        JunctionFrame::prepare(
            world,
            &resources.read::<Map>(),
            &resources.read::<TransportGrid>(),
            &mut resources.write::<JunctionAdmission>(),
            tick,
        )
    };
    assert_eq!(
        prepare(&mut loaded).denial(conflict),
        Some(HoldReason::Conflict)
    );
    assert!(loaded.read::<JunctionAdmission>().crossing_turn(turn));

    loaded.world.vehicles[vehicle].trans.pos = past_exit;
    assert_eq!(prepare(&mut loaded).denial(conflict), None);
    assert!(!loaded.read::<JunctionAdmission>().crossing_turn(turn));
}
