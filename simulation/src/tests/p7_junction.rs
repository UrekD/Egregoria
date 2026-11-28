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
    try_staged_vehicle(sim, kind, from, to, before_boundary).unwrap()
}

fn try_staged_vehicle(
    sim: &mut Simulation,
    kind: VehicleKind,
    from: LaneID,
    to: LaneID,
    before_boundary: f32,
) -> Option<crate::VehicleID> {
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
    let id = spawn_driving_vehicle(sim, kind, Transform::new_dir(start, dir))?;
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
    Some(id)
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
    roundabout_fixture_with_parking(directions, radius, slope, true)
}

fn roundabout_fixture_with_parking(
    directions: &[f32],
    radius: f32,
    slope: bool,
    parking: bool,
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
                .parking(parking)
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
fn roundabout_hotfix_empty_exit_crosswalk_ignores_sidewalk_traffic() {
    use crate::transportation::queue_diagnostics::{
        watch_queue, QueueDiagnostics, QueueHoldReason, QueueRevision,
    };

    let mut waits = 0;
    let mut brakes = 0;
    let mut arrival_delays = Vec::new();
    for directions in [
        &[0.0, 107.0, 218.0][..],
        &[0.0, 90.0, 180.0, 270.0],
        &[0.0, 38.0, 119.0, 194.0, 279.0],
    ] {
        let mut no_walkers_arrival = 0;
        for count in [0, 1, 20] {
            let (mut ctx, junction, ends) =
                roundabout_fixture_with_parking(directions, 24.0, false, false);
            let house = ctx.build_house_near(ends[0].xy());
            ctx.g
                .write::<crate::souls::residence::Residences>()
                .by_building
                .get_mut(&house)
                .unwrap()
                .capacity = 20;
            let mut schedule = roundabout_hotfix_movement_only();
            let (from, to, sidewalk, start, direction, destination, crossing) = {
                let map = ctx.g.map();
                let inter = map.get(junction).unwrap();
                let street = inter
                    .roads
                    .iter()
                    .map(|&id| &map.roads()[id])
                    .find(|road| road.other_end(junction) == Some(junction_at(&map, ends[0])))
                    .unwrap();
                let sidewalk = street.sidewalks(junction).outgoing.unwrap();
                let to = street
                    .outgoing_lanes_from(junction)
                    .iter()
                    .rev()
                    .find(|(_, kind)| kind.vehicles())
                    .unwrap()
                    .0;
                let lane = &map.lanes()[to];
                let from = inter
                    .turns()
                    .find(|turn| turn.id.dst == to && turn.kind == TurnKind::Driving)
                    .unwrap()
                    .id
                    .src;
                let crossing = inter
                    .crosswalks()
                    .find(|turn| turn.id.dst == sidewalk || turn.id.src == sidewalk)
                    .unwrap()
                    .points
                    .clone();
                (
                    from,
                    to,
                    sidewalk,
                    lane.points.point_along(0.5),
                    lane.points.first_dir().unwrap(),
                    lane.points.point_along(50.0),
                    crossing,
                )
            };
            let car = staged_vehicle(&mut ctx.g, VehicleKind::Bus, from, to, 8.0);
            {
                let car = &mut ctx.g.world.vehicles[car];
                car.trans = Transform::new_dir(start, direction);
                let ItineraryKind::Route(route, _) = &mut car.it.kind else {
                    unreachable!()
                };
                route.cur = forward_lane(to);
                route.reversed_route.clear();
                car.it.reversed_local_path = vec![destination];
            }
            let mut walkers = Vec::new();
            for i in 0..count {
                let (start, route) = {
                    let map = ctx.g.map();
                    let lane = &map.lanes()[sidewalk];
                    // Walk near the road-facing edge of the sidewalk, outside the zebra.
                    let roadward =
                        (map.lanes()[to].points.first() - lane.points.first()).normalize();
                    let start = lane.points.point_along(6.7 + i as f32 * 0.4) + roadward * 0.7;
                    let end = lane.points.point_along(35.0);
                    let route =
                        Itinerary::route(Tick(0), start, end, &map, PathKind::Pedestrian).unwrap();
                    assert!(route.get_route().unwrap().reversed_route.is_empty());
                    (start, route)
                };
                walkers.push(roundabout_hotfix_walker(&mut ctx.g, house, start, route));
            }
            assert!(watch_queue(&ctx.g, to));
            let mut case_waits = 0;
            let mut case_brakes = 0;
            let mut arrived_at = None;
            for tick in 1..=2_000 {
                ctx.g.tick(&mut schedule, &[]);
                let bus = &ctx.g.world.vehicles[car];
                if bus.trans.pos.is_close(destination, 0.01) {
                    arrived_at = Some(tick);
                    break;
                }
                if !bus.trans.pos.is_close(start, 15.0) {
                    continue;
                }
                assert!(walkers.iter().all(|&id| crossing
                    .project_dist2(ctx.g.world.humans[id].trans.pos)
                    > 2.0f32.powi(2)));
                let reason = ctx.g.read::<QueueDiagnostics>().recent_hold(
                    car,
                    forward_lane(to),
                    QueueRevision::current(&ctx.g.map()),
                    crate::diagnostics::elapsed_tick(&ctx.g).0,
                );
                if matches!(reason, Some(QueueHoldReason::Pedestrian(_))) {
                    case_brakes += 1;
                    case_waits += usize::from(bus.speed.0 <= 0.2);
                }
            }
            let arrived_at = arrived_at.expect("vehicle should reach its destination");
            eprintln!("arms={} sidewalk_walkers={count} empty_crosswalk_brake_ticks={case_brakes} empty_crosswalk_wait_ticks={case_waits} arrival_tick={arrived_at}", directions.len());
            if count == 0 {
                no_walkers_arrival = arrived_at;
            } else {
                arrival_delays.push(arrived_at as i32 - no_walkers_arrival as i32);
            }
            waits += case_waits;
            brakes += case_brakes;
        }
    }
    assert_eq!(
        waits, 0,
        "vehicles yielded to walkers leaving an empty exit crossing"
    );
    assert_eq!(
        brakes, 0,
        "vehicles braked for pedestrians outside the crossing"
    );
    assert_eq!(arrival_delays, [0; 6], "sidewalk traffic slowed vehicles");
}

#[test]
fn roundabout_hotfix_empty_zebras_with_live_entry_and_exit() {
    use crate::transportation::queue_diagnostics::{
        watch_queue, QueueDiagnostics, QueueHoldReason, QueueRevision,
    };

    for count in [0, 6] {
        let (mut ctx, junction, ends) =
            roundabout_fixture_with_parking(&[0.0, 90.0, 180.0, 270.0], 24.0, false, false);
        let house = ctx.build_house_near(ends[0].xy());
        ctx.g
            .write::<crate::souls::residence::Residences>()
            .by_building
            .get_mut(&house)
            .unwrap()
            .capacity = 2 * count as u32;
        let mut schedule = roundabout_hotfix_movement_only();
        let (from, to, sidewalk_out, entry, exit, destination) = {
            let map = ctx.g.map();
            let inter = map.get(junction).unwrap();
            let from = lane_at(&map, ends[1], inter.pos);
            let road = inter
                .roads
                .iter()
                .map(|&id| &map.roads()[id])
                .find(|road| road.other_end(junction) == Some(junction_at(&map, ends[0])))
                .unwrap();
            let to = road
                .outgoing_lanes_from(junction)
                .iter()
                .rev()
                .find(|(_, kind)| kind.vehicles())
                .unwrap()
                .0;
            let sidewalk_out = road.sidewalks(junction).outgoing.unwrap();
            let zebra = |lane: LaneID| {
                let sidewalk = map.roads()[map.lanes()[lane].parent]
                    .sidewalks(junction)
                    .outgoing
                    .unwrap();
                inter
                    .crosswalks()
                    .find(|turn| turn.id.src == sidewalk || turn.id.dst == sidewalk)
                    .unwrap()
                    .points
                    .clone()
            };
            (
                from,
                to,
                sidewalk_out,
                zebra(from),
                zebra(to),
                map.lanes()[to].points.point_along(50.0),
            )
        };
        let car = staged_vehicle(&mut ctx.g, VehicleKind::Bus, from, to, 18.0);
        assert!(watch_queue(&ctx.g, from));
        assert!(watch_queue(&ctx.g, to));
        let revision = QueueRevision::current(&ctx.g.map());
        let mut walkers = Vec::new();
        let mut spawned_exit_walkers = false;
        let mut holds = [0; 2];
        let mut reached = [false; 2];
        let mut arrival = None;
        for tick in 1..=5_000 {
            let on_ring_by_exit = {
                let car = &ctx.g.world.vehicles[car];
                exit.project_dist2(car.trans.pos) < 8.0f32.powi(2)
                    && car
                        .it
                        .get_travers()
                        .is_some_and(|step| matches!(step.kind, TraverseKind::Turn(_)))
            };
            if tick == 1 || (!spawned_exit_walkers && on_ring_by_exit) {
                let lane_id = if tick == 1 { from } else { to };
                let sidewalk = if tick == 1 {
                    let map = ctx.g.map();
                    map.roads()[map.lanes()[from].parent]
                        .sidewalks(junction)
                        .incoming
                        .unwrap()
                } else {
                    spawned_exit_walkers = true;
                    sidewalk_out
                };
                for i in 0..count {
                    let (start, route) = {
                        let map = ctx.g.map();
                        let walk = &map.lanes()[sidewalk];
                        let roadward = (map.lanes()[lane_id].get_inter_node_pos(junction)
                            - walk.get_inter_node_pos(junction))
                        .normalize();
                        let offset = if tick == 1 { 7.5 } else { 6.7 } + i as f32 * 0.4;
                        let at = |distance| {
                            walk.points.point_along(if walk.src == junction {
                                distance
                            } else {
                                walk.points.length() - distance
                            })
                        };
                        let start = at(offset) + roadward * 0.7;
                        (
                            start,
                            Itinerary::route(Tick(0), start, at(40.0), &map, PathKind::Pedestrian)
                                .unwrap(),
                        )
                    };
                    walkers.push(roundabout_hotfix_walker(&mut ctx.g, house, start, route));
                }
            }
            ctx.g.tick(&mut schedule, &[]);
            let bus = &ctx.g.world.vehicles[car];
            if bus.trans.pos.is_close(destination, 0.01) {
                arrival = Some(tick);
                break;
            }
            let Some(&traversal) = bus.it.get_travers() else {
                continue;
            };
            let reason = ctx.g.read::<QueueDiagnostics>().recent_hold(
                car,
                traversal,
                revision,
                ctx.g.get_tick(),
            );
            for (i, zebra) in [&entry, &exit].into_iter().enumerate() {
                if zebra.project_dist2(bus.trans.pos) > 15.0f32.powi(2) {
                    continue;
                }
                assert!(walkers
                    .iter()
                    .all(|&id| zebra.project_dist2(ctx.g.world.humans[id].trans.pos)
                        > 2.0f32.powi(2)));
                reached[i] = true;
                holds[i] += usize::from(matches!(
                    reason,
                    Some(QueueHoldReason::Pedestrian(_) | QueueHoldReason::Conflict(None))
                ));
            }
        }
        assert_eq!(reached, [true, true]);
        eprintln!("live_zebras sidewalk_walkers={count} entry_and_exit_hold_ticks={holds:?} arrival={arrival:?}");
        assert!(arrival.is_some());
        assert_eq!(
            holds,
            [0, 0],
            "empty zebras held a bus on its original route"
        );
    }
}

#[test]
fn roundabout_hotfix_brakes_for_a_walker_heading_across_the_approach() {
    use crate::transportation::{road::calc_decision, TransportState, TransportationGroup};

    let (mut ctx, junction, ends) =
        roundabout_fixture_with_parking(&[0.0, 90.0, 180.0, 270.0], 24.0, false, false);
    let (from, to) = {
        let map = ctx.g.map();
        let center = map.get(junction).unwrap().pos;
        (
            lane_at(&map, ends[0], center),
            lane_at(&map, center, ends[1]),
        )
    };
    let id = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 15.0);
    let map = ctx.g.resources.read::<crate::map::Map>();
    let car = &mut ctx.g.world.vehicles[id];
    let forward = car.trans.dir.xy();
    let cross = forward.perpendicular();
    let position = car.trans.pos.xy() + 5.0 * forward + 3.8 * cross;
    let state = TransportState {
        dir: forward,
        speed: 9.0,
        radius: VehicleKind::Car.width() * 0.5,
        height: car.trans.pos.z,
        group: TransportationGroup::Vehicles,
        ..TransportState::default()
    };
    let mut walker = TransportState {
        dir: -cross,
        speed: 1.6,
        radius: 0.3,
        height: car.trans.pos.z,
        group: TransportationGroup::Pedestrians,
        ..TransportState::default()
    };
    let speed = |vehicle: &mut crate::transportation::Vehicle, walker: &TransportState| {
        calc_decision(
            id,
            vehicle,
            &map,
            &GameTime::new(Tick(0)),
            &car.trans,
            &state,
            &car.it,
            [(position, walker)].into_iter(),
        )
        .0
    };
    assert_eq!(speed(&mut car.vehicle, &walker), 0.0);
    walker.dir = cross;
    assert!(speed(&mut car.vehicle, &walker) > 0.0);
}

#[test]
fn roundabout_hotfix_keeps_ordinary_zebra_braking_on_a_short_arm() {
    use crate::map::RoundaboutPolicy;
    use crate::transportation::{road::calc_decision, TransportState, TransportationGroup};

    let mut ctx = TestCtx::new();
    let center = vec3(260.0, 260.0, 0.0);
    let ordinary = vec3(315.0, 260.0, 0.0);
    let west = vec3(105.0, 260.0, 0.0);
    let east = vec3(450.0, 260.0, 0.0);
    ctx.build_roads(&[west, center, ordinary, east]);
    ctx.build_roads(&[center, vec3(260.0, 105.0, 0.0)]);
    ctx.build_roads(&[ordinary, vec3(315.0, 420.0, 0.0)]);
    let (roundabout, junction) = {
        let map = ctx.g.map();
        (junction_at(&map, center), junction_at(&map, ordinary))
    };
    ctx.g.map_mut().update_intersection(roundabout, |inter| {
        inter.turn_policy.roundabout = Some(RoundaboutPolicy { radius: 18.0 });
    });
    ctx.g.map_mut().update_intersection(junction, |inter| {
        inter.light_policy = LightPolicy::NoLights;
    });
    let (from, to, start, forward, walker_position) = {
        let map = ctx.g.map();
        assert!(map.is_yield_roundabout(roundabout));
        let from = lane_at(&map, center, ordinary);
        let to = lane_at(&map, ordinary, east);
        let arm = &map.lanes()[from];
        let start = arm.points.point_along(arm.points.length() * 0.45);
        let forward = arm.points.last_dir().unwrap();
        let position = arm.points.last() + forward.xy().perpendicular().z0() * 2.3;
        assert!(start.is_close(arm.points.first(), 24.0));
        assert!(map
            .get(junction)
            .unwrap()
            .crosswalks()
            .any(|zebra| zebra.points.project_dist2(position) < 0.05));
        (from, to, start, forward, position)
    };
    let id = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 5.0);
    let map = ctx.g.resources.read::<crate::map::Map>();
    let car = &mut ctx.g.world.vehicles[id];
    car.trans = Transform::new_dir(start, forward);
    let state = TransportState {
        dir: forward.xy(),
        speed: 12.0,
        radius: VehicleKind::Car.width() * 0.5,
        height: start.z,
        group: TransportationGroup::Vehicles,
        ..TransportState::default()
    };
    let walker = TransportState {
        radius: 0.3,
        height: walker_position.z,
        group: TransportationGroup::Pedestrians,
        ..TransportState::default()
    };
    let (clear, _) = calc_decision(
        id,
        &mut car.vehicle,
        &map,
        &GameTime::new(Tick(0)),
        &car.trans,
        &state,
        &car.it,
        std::iter::empty(),
    );
    assert!(
        clear > 0.0,
        "the empty ordinary approach should remain open"
    );
    let (desired, _) = calc_decision(
        id,
        &mut car.vehicle,
        &map,
        &GameTime::new(Tick(0)),
        &car.trans,
        &state,
        &car.it,
        [(walker_position.xy(), &walker)].into_iter(),
    );
    assert_eq!(
        desired, 0.0,
        "roundabout filtering reached the ordinary zebra"
    );
}

#[test]
fn roundabout_hotfix_mid_ring_ignores_walkers_outside_the_driving_path() {
    use crate::transportation::{road::calc_decision, TransportState, TransportationGroup};

    let (mut ctx, junction, _) = roundabout_fixture(&[0.0, 90.0, 180.0, 270.0], 28.0, false);
    let (turn, points) = {
        let map = ctx.g.map();
        let turn = map
            .get(junction)
            .unwrap()
            .turns()
            .filter(|turn| turn.kind == TurnKind::Driving)
            .max_by_key(|turn| ordered_float::OrderedFloat(turn.points.length()))
            .unwrap();
        (turn.id, turn.points.clone())
    };
    let id = staged_vehicle(&mut ctx.g, VehicleKind::Car, turn.src, turn.dst, 7.0);
    let map = ctx.g.resources.read::<crate::map::Map>();
    let car = &mut ctx.g.world.vehicles[id];
    let i = points.len() / 2;
    let forward = (points.as_slice()[i + 1] - points.as_slice()[i]).normalize();
    let position = points.as_slice()[i];
    assert!(
        position
            .distance(points.first())
            .min(position.distance(points.last()))
            > 12.0
    );
    car.trans = Transform::new_dir(position, forward);
    let ItineraryKind::Route(route, _) = &mut car.it.kind else {
        unreachable!()
    };
    route.cur = Traversable::new(TraverseKind::Turn(turn), TraverseDirection::Forward);
    route.reversed_route.pop();
    car.it.reversed_local_path = points.as_slice()[i + 1..].iter().rev().copied().collect();
    let outward = (position - map.get(junction).unwrap().pos).xy().normalize();
    let target = car.trans.pos.xy() + 6.0 * forward.xy() + 4.0 * outward;
    let state = TransportState {
        dir: forward.xy(),
        speed: 9.0,
        radius: VehicleKind::Car.width() * 0.5,
        height: position.z,
        group: TransportationGroup::Vehicles,
        ..TransportState::default()
    };
    let walker = TransportState {
        dir: forward.xy(),
        speed: 1.2,
        radius: 0.3,
        height: position.z,
        group: TransportationGroup::Pedestrians,
        ..TransportState::default()
    };
    let (desired, _) = calc_decision(
        id,
        &mut car.vehicle,
        &map,
        &GameTime::new(Tick(0)),
        &car.trans,
        &state,
        &car.it,
        [(target, &walker)].into_iter(),
    );
    assert!(desired > 0.0, "a sidewalk walker braked a circulating car");
}

#[test]
fn roundabout_hotfix_approach_yields_to_routed_crosswalk_walker() {
    for already_crossing in [false, true] {
        roundabout_hotfix_routed_crosswalk_walker(already_crossing);
    }
}

fn roundabout_hotfix_routed_crosswalk_walker(already_crossing: bool) {
    let (mut ctx, junction, ends) =
        roundabout_fixture_with_parking(&[0.0, 107.0, 218.0], 24.0, false, false);
    let mut schedule = roundabout_hotfix_movement_only();
    let house = ctx.build_house_near(ends[0].xy());
    let (turn, crossing, start, destination, route, car_destination) = {
        let map = ctx.g.map();
        let inter = map.get(junction).unwrap();
        let turn = inter
            .turns()
            .find(|turn| turn.kind == TurnKind::Driving)
            .unwrap()
            .id;
        let road = &map.roads()[map.lanes()[turn.src].parent];
        let sidewalk = road.sidewalks(junction).incoming.unwrap();
        let crossing = inter
            .crosswalks()
            .find(|turn| turn.id.src == sidewalk || turn.id.dst == sidewalk)
            .unwrap()
            .id;
        let at_curb = |lane: LaneID, distance: f32| {
            let lane = &map.lanes()[lane];
            let offset = if lane.src == junction {
                distance
            } else {
                lane.points.length() - distance
            };
            lane.points.point_along(offset)
        };
        let start = at_curb(crossing.src, 0.5);
        let destination = at_curb(crossing.dst, 5.0);
        let route =
            Itinerary::route(Tick(0), start, destination, &map, PathKind::Pedestrian).unwrap();
        assert!(route
            .get_route()
            .unwrap()
            .reversed_route
            .iter()
            .any(|step| step.kind == TraverseKind::Turn(crossing)));
        (
            turn,
            crossing,
            start,
            destination,
            route,
            map.lanes()[turn.dst].points.point_along(50.0),
        )
    };
    let walker = roundabout_hotfix_walker(&mut ctx.g, house, start, route);
    if already_crossing {
        for _ in 0..4_000 {
            ctx.g.tick(&mut schedule, &[]);
            if ctx.g.read::<JunctionAdmission>().crossing_turn(crossing) {
                break;
            }
        }
        assert!(ctx.g.read::<JunctionAdmission>().crossing_turn(crossing));
    }
    let bus = staged_vehicle(&mut ctx.g, VehicleKind::Bus, turn.src, turn.dst, 14.0);
    let entry = ctx.g.map().lanes()[turn.src].points.last();
    let heading = ctx.g.map().lanes()[turn.src].points.last_dir().unwrap();
    let clearance = 1.3;
    let mut minimum_gap = f32::INFINITY;
    let mut yielded = false;
    for _ in 0..4_000 {
        ctx.g.tick(&mut schedule, &[]);
        let vehicle = &ctx.g.world.vehicles[bus];
        let pedestrian = ctx.g.world.humans[walker].trans.pos;
        let body = vehicle.trans.dir * (VehicleKind::Bus.width() * 0.5 - 1.0);
        let footprint = geom::Segment3::new(vehicle.trans.pos - body, vehicle.trans.pos + body);
        let gap = footprint.project(pedestrian).distance(pedestrian);
        minimum_gap = minimum_gap.min(gap);
        assert!(
            gap >= clearance,
            "crossing pedestrian touched the bus body at {pedestrian:?}, gap {gap}"
        );
        let approach = (entry - vehicle.trans.pos).dot(heading);
        if ctx.g.read::<JunctionAdmission>().crossing_turn(crossing) {
            yielded |= vehicle.speed.0 <= 0.2 && (0.0..=8.0).contains(&approach);
        }
        if vehicle.trans.pos.is_close(car_destination, 0.01)
            && ctx.g.world.humans[walker]
                .trans
                .pos
                .is_close(destination, 0.01)
        {
            assert!(
                !already_crossing || yielded,
                "the bus never yielded to the routed pedestrian, minimum axis gap {minimum_gap}"
            );
            return;
        }
    }
    panic!("the bus or pedestrian failed to clear the crossing");
}

#[test]
fn roundabout_hotfix_releases_the_cleared_half_of_a_crosswalk() {
    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 107.0, 218.0], 24.0, false);
    let house = ctx.build_house_near(ends[0].xy());
    roundabout_releases_the_cleared_half(&mut ctx, junction, &ends, house, false);
}

#[test]
fn roundabout_hotfix_keeps_crosswalk_clear_during_corner_handoff() {
    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 107.0, 218.0], 24.0, false);
    let house = ctx.build_house_near(ends[0].xy());
    roundabout_releases_the_cleared_half(&mut ctx, junction, &ends, house, true);
}

fn roundabout_releases_the_cleared_half(
    ctx: &mut TestCtx,
    junction: IntersectionID,
    ends: &[Vec3],
    house: crate::map::BuildingID,
    handoff: bool,
) {
    let (turn, crossing, position, mut itinerary, corner) = {
        let map = ctx.g.map();
        let inter = &map.intersections()[junction];
        let from = lane_at(&map, ends[0], inter.pos);
        let to = lane_at(&map, inter.pos, ends[1]);
        let turn = TurnID::new(junction, from, to, false);
        let entry = inter.find_turn(turn).unwrap().points.first();
        let crossing = inter
            .crosswalks()
            .min_by_key(|walk| ordered_float::OrderedFloat(walk.points.project_dist2(entry)))
            .unwrap();
        let first = crossing.points.first();
        let last = crossing.points.last();
        let (direction, curb) = if first.distance2(entry) < last.distance2(entry) {
            (TraverseDirection::Forward, last)
        } else {
            (TraverseDirection::Backward, first)
        };
        let position = curb + (crossing.points.middle() - curb).normalize() * 0.5;
        let active = Traversable::new(TraverseKind::Turn(crossing.id), direction);
        let curb_lane = active.destination_lane();
        let corner = inter
            .turns()
            .find(|turn| {
                turn.kind == TurnKind::WalkingCorner
                    && (turn.id.src == curb_lane
                        || (turn.id.bidirectional && turn.id.dst == curb_lane))
            })
            .map(|turn| {
                Traversable::new(
                    TraverseKind::Turn(turn.id),
                    if turn.id.src == curb_lane {
                        TraverseDirection::Forward
                    } else {
                        TraverseDirection::Backward
                    },
                )
            })
            .unwrap();
        let itinerary = Itinerary {
            kind: ItineraryKind::Route(
                Route {
                    reversed_route: Vec::new(),
                    end_pos: curb,
                    cur: active,
                    target: ItineraryTarget::Position(curb),
                },
                PathKind::Pedestrian,
            ),
            reversed_local_path: vec![curb],
            ..Itinerary::default()
        };
        (turn, crossing.id, position, itinerary, corner)
    };
    if handoff {
        let ItineraryKind::Route(route, _) = &mut itinerary.kind else {
            unreachable!()
        };
        route.reversed_route.push(corner);
        let map = ctx.g.map();
        route.end_pos = corner.raw_points(&map).unwrap().point_along(5.0);
        route.target = ItineraryTarget::Position(route.end_pos);
        drop(map);
        let duplicate = {
            let route = itinerary.get_route().unwrap();
            Itinerary {
                kind: ItineraryKind::Route(
                    Route {
                        reversed_route: route.reversed_route.clone(),
                        end_pos: route.end_pos,
                        cur: route.cur,
                        target: route.target,
                    },
                    PathKind::Pedestrian,
                ),
                reversed_local_path: itinerary.reversed_local_path.clone(),
                ..Itinerary::default()
            }
        };
        let walking = roundabout_hotfix_walker(&mut ctx.g, house, position, itinerary);
        let tail = roundabout_hotfix_walker(&mut ctx.g, house, position, duplicate);
        ctx.g.world.edit_human(tail, |walker| walker.speed.0 = 30.0);
        update_at(&mut ctx.g, 1);
        assert_eq!(ctx.g.world.humans[tail].it.get_travers(), Some(&corner));
        assert!(ctx.g.read::<JunctionAdmission>().crossing_turn(crossing));
        // Check both combinations that used to restore the full sweep.
        assert_ne!(ctx.g.world.humans[walking].it.get_travers(), Some(&corner));
    } else {
        roundabout_hotfix_walker(&mut ctx.g, house, position, itinerary);
    }
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, turn.src, turn.dst, 2.3);
    ctx.g.world.vehicles[car].speed.0 = 5.0;
    let tick = u64::from(handoff);
    update_at(&mut ctx.g, tick + 1);
    let admission = ctx.g.read::<JunctionAdmission>();
    assert!(admission.crossing_turn(crossing));
    assert!(
        admission.crossing_turn(turn),
        "the walker has cleared the vehicle's half of the crossing"
    );
}

#[test]
fn roundabout_hotfix_releases_ring_behind_circulating_vehicle() {
    use crate::transportation::junction_admission::segments_close;

    let (mut ctx, junction, _) = roundabout_fixture(&[0.0, 90.0, 180.0, 270.0], 28.0, false);
    let (lead, follower, path) = {
        let map = ctx.g.map();
        let inter = map.get(junction).unwrap();
        let turns = || inter.turns().filter(|turn| turn.kind == TurnKind::Driving);
        let clearance = (VehicleKind::Car.width() + 0.8).powi(2);
        let pair = turns()
            .find_map(|lead| {
                let rest = &lead.points.as_slice()[lead.points.len() - 8..];
                let follower = turns().find(|other| {
                    lead.id.src != other.id.src
                        && lead.id.dst != other.id.dst
                        && lead.points.segments().any(|a| {
                            other
                                .points
                                .segments()
                                .any(|b| segments_close(a, b, clearance))
                        })
                        && rest.windows(2).all(|pair| {
                            other.points.segments().all(|b| {
                                !segments_close(geom::Segment3::new(pair[0], pair[1]), b, clearance)
                            })
                        })
                        && map
                            .get(other.id.dst)
                            .unwrap()
                            .points
                            .first()
                            .distance(rest[0])
                            > 15.0
                })?;
                Some((lead.id, follower.id, rest.to_vec()))
            })
            .unwrap();
        pair
    };
    let circulating = staged_vehicle(&mut ctx.g, VehicleKind::Car, lead.src, lead.dst, 7.0);
    let car = &mut ctx.g.world.vehicles[circulating];
    car.trans = Transform::new_dir(path[0], (path[1] - path[0]).normalize());
    let ItineraryKind::Route(route, _) = &mut car.it.kind else {
        unreachable!()
    };
    route.cur = Traversable::new(TraverseKind::Turn(lead), TraverseDirection::Forward);
    route.reversed_route.pop();
    car.it.reversed_local_path = path.into_iter().skip(1).rev().collect();

    let waiting = staged_vehicle(
        &mut ctx.g,
        VehicleKind::Car,
        follower.src,
        follower.dst,
        5.0,
    );
    ctx.g.world.vehicles[waiting].speed.0 = 350.0;
    let tick = ctx.g.get_tick();
    update_at(&mut ctx.g, tick + 1);
    assert!(ctx.g.read::<JunctionAdmission>().crossing_turn(lead));
    assert!(
        ctx.g.read::<JunctionAdmission>().crossing_turn(follower),
        "a cleared ring segment still blocked the approach"
    );
}

#[test]
fn roundabout_hotfix_walks_from_corner_into_crosswalk() {
    walks_from_corner_into_crosswalk(true);
}

#[test]
fn p7_junction_ruled_walks_from_corner_into_crosswalk() {
    walks_from_corner_into_crosswalk(false);
}

fn walks_from_corner_into_crosswalk(roundabout: bool) {
    use crate::transportation::junction_admission::entry_boundary;

    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0, 180.0, 270.0], 28.0, false);
    if !roundabout {
        let mut map = ctx.g.map_mut();
        map.update_intersection(junction, |inter| {
            inter.turn_policy.roundabout = None;
            inter.light_policy = LightPolicy::NoLights;
        });
        let road = map.get(junction).unwrap().roads[0];
        let rules = JunctionRules::new(vec![(road, ApproachRole::Yield)], Vec::new()).unwrap();
        map.install_junction_rules(junction, rules);
        assert!(crate::transportation::junction_admission::configured(&map, junction).is_some());
    } else {
        assert!(ctx.g.map().is_yield_roundabout(junction));
    }
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
    let mut awaiting_crossing = None;
    let mut crossed = false;
    for _ in 0..12_000 {
        let walker = &ctx.g.world.humans[id];
        let map = ctx.g.map();
        let route = walker
            .it
            .get_route()
            .expect("the original route must stay active");
        let kind = |step: Traversable| match step.kind {
            TraverseKind::Turn(turn) if turn.parent == junction => {
                Some(map.get(junction).unwrap().find_turn(turn).unwrap().kind)
            }
            _ => None,
        };
        if !crossed && awaiting_crossing == Some(route.cur) {
            let TraverseKind::Turn(turn) = route.cur.kind else {
                unreachable!()
            };
            assert!(ctx.g.read::<JunctionAdmission>().crossing_turn(turn));
            crossed = true;
        }
        if kind(route.cur) == Some(TurnKind::WalkingCorner) {
            if let Some(&next) = route
                .reversed_route
                .last()
                .filter(|&&step| kind(step) == Some(TurnKind::Crosswalk))
            {
                if let Some(boundary) = entry_boundary(&map, &walker.it, walker.trans.pos, 0.1, 0.3)
                {
                    assert!(
                        boundary.valid && boundary.controlled,
                        "directed corner entry is invalid"
                    );
                    awaiting_crossing = Some(next);
                }
            }
        }
        drop(map);
        ctx.g.tick(&mut schedule, &[]);
        if ctx.g.world.humans[id].trans.pos.is_close(destination, 0.01) {
            assert!(
                crossed,
                "walker arrived without a protected corner-to-crosswalk transition"
            );
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
fn roundabout_hotfix_recovers_offset_crosswalk_before_car_enters() {
    roundabout_recovery_with_traffic(Some(1.2));
    roundabout_recovery_with_traffic(Some(-1.2));
}

#[test]
fn roundabout_hotfix_recovers_unrouted_walker_on_ring() {
    roundabout_recovery_with_traffic(None);
}

fn roundabout_recovery_with_traffic(crosswalk_offset: Option<f32>) {
    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 107.0, 218.0], 24.0, false);
    let house = ctx.build_house_near(ends[0].xy());
    let (from, to, start) = {
        let map = ctx.g.map();
        let inter = map.get(junction).unwrap();
        let from = lane_at(&map, ends[0], inter.pos);
        let to = lane_at(&map, inter.pos, ends[2]);
        let turn = inter
            .find_turn(TurnID::new(junction, from, to, false))
            .unwrap();
        let start = if let Some(offset) = crosswalk_offset {
            let entry = turn.points.first();
            let walk = inter
                .crosswalks()
                .min_by_key(|walk| ordered_float::OrderedFloat(walk.points.project_dist2(entry)))
                .unwrap();
            let across = walk.points.first_dir().unwrap();
            walk.points.project(entry) + across.xy().perpendicular().z0() * offset
        } else {
            turn.points.middle()
        };
        (from, to, start)
    };
    let walker = roundabout_hotfix_walker(&mut ctx.g, house, start, Itinerary::NONE);
    let half = VehicleKind::Car.width() * 0.5;
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, half + 0.02);
    ctx.g.world.vehicles[car].speed.0 = 35.0;
    let tick = ctx.g.get_tick();
    update_at(&mut ctx.g, tick + 1);
    assert_eq!(
        ctx.g.world.vehicles[car].speed.0, 0.0,
        "car entered an active recovery sweep"
    );

    let mut schedule = roundabout_hotfix_movement_only();
    for _ in 0..12_000 {
        ctx.g.tick(&mut schedule, &[]);
        let human = &ctx.g.world.humans[walker];
        if human.collider.is_none() && ctx.g.world.vehicles[car].it.get_point().is_none() {
            assert!(
                human.trans.pos.distance(start) > 1.25,
                "walker did not reach a curb"
            );
            return;
        }
    }
    let human = &ctx.g.world.humans[walker];
    let vehicle = &ctx.g.world.vehicles[car];
    panic!(
        "recovery from {start:?} ({crosswalk_offset:?}) stopped at {:?} with {:?}; car stopped at {:?} with {:?}",
        human.trans.pos, human.it.local_path(), vehicle.trans.pos, vehicle.it.get_travers(),
    );
}

#[test]
fn roundabout_hotfix_dense_sidewalk_does_not_close_entries() {
    assert!(roundabout_dense_sidewalk_allows_entry(260));
}

#[test]
fn roundabout_hotfix_overfull_sample_defers_entry() {
    assert!(!roundabout_dense_sidewalk_allows_entry(520));
}

fn roundabout_dense_sidewalk_allows_entry(count: u32) -> bool {
    let (mut ctx, junction, ends) =
        roundabout_fixture(&[0.0, 38.0, 119.0, 194.0, 279.0], 28.0, false);
    let mut schedule = roundabout_hotfix_movement_only();
    let house = ctx.build_house_near(ends[0].xy());
    ctx.g
        .write::<crate::souls::residence::Residences>()
        .by_building
        .get_mut(&house)
        .unwrap()
        .capacity = count as _;
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
    for _ in 0..count {
        let position = sidewalk.point_along(2.0);
        let route = Itinerary::simple(vec![sidewalk.point_along(5.0), sidewalk.point_along(85.0)]);
        roundabout_hotfix_walker(&mut ctx.g, house, position, route);
    }
    let car = staged_vehicle(&mut ctx.g, VehicleKind::Car, from, to, 8.0);
    for _ in 0..350 {
        ctx.g.tick(&mut schedule, &[]);
    }
    let car = &ctx.g.world.vehicles[car];
    car.it.get_travers().unwrap().kind != TraverseKind::Lane(from)
}

#[test]
fn roundabout_hotfix_four_arm_mixed_traffic_soak() {
    use crate::transportation::queue_diagnostics::{
        watch_queue, QueueDiagnostics, QueueHoldReason, QueueRevision,
    };
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
    let (carriageway, destinations) = {
        let map = ctx.g.map();
        let inter = map.get(junction).unwrap();
        let lanes = inter.roads.iter().flat_map(|&road| {
            map.get(road)
                .unwrap()
                .lanes_iter()
                .filter(|(_, kind)| kind.vehicles() || *kind == LaneKind::Parking)
        });
        let carriageway = lanes
            .map(|(lane, kind)| (map.get(lane).unwrap().points.clone(), kind.width() * 0.5))
            .chain(
                inter
                    .turns()
                    .filter(|turn| turn.kind == TurnKind::Driving)
                    .map(|turn| (turn.points.clone(), 2.0)),
            )
            .collect::<Vec<_>>();
        let destinations = driving
            .iter()
            .flat_map(|(_, exits)| exits.iter().copied())
            .map(|to| (to, map.get(to).unwrap().points.point_along(50.0)))
            .collect::<BTreeMap<_, _>>();
        (carriageway, destinations)
    };

    for lane in driving
        .iter()
        .map(|(from, _)| from)
        .chain(destinations.keys())
    {
        assert!(watch_queue(&ctx.g, *lane));
    }
    let revision = QueueRevision::current(&ctx.g.map());
    let zebras = ctx
        .g
        .map()
        .get(junction)
        .unwrap()
        .crosswalks()
        .map(|turn| turn.points.clone())
        .collect::<Vec<_>>();
    let mut stopped_by_reason = BTreeMap::new();
    let mut empty_zebra_holds = BTreeMap::new();

    const INJECTION_PERIOD: u64 = 4_000;
    const WAVES: u64 = 8;
    const OBSERVE_PERIOD: u64 = 25;
    const STOP_WINDOW: u64 = prototypes::TICKS_PER_MINUTE;
    const SOAK_TICKS: u64 = 160_000;
    const MAX_VEHICLE_STOP: u64 = 5 * STOP_WINDOW;
    let mut arrived_cars = 0;
    let mut arrived_walkers = 0;
    let mut cars = BTreeMap::new();
    let mut walkers = BTreeMap::new();
    let mut pending = Vec::new();
    let mut dispatched = Vec::new();
    let mut clear_ticks = 0;
    let mut active_clear_ticks = 0;
    let mut longest_active_clear = 0;
    let mut peak_traffic = 0;
    let mut longest_vehicle_stop = 0;
    let mut longest_walker_stop = 0;
    let mut last_car_arrival = 0;
    let mut worst_stop = None;
    eprintln!(
        "approaches={:?}",
        driving
            .iter()
            .map(|(lane, _)| (lane, ctx.g.map().lanes()[*lane].points.last()))
            .collect::<Vec<_>>()
    );
    let mut stuck_cars = std::collections::BTreeSet::new();
    for tick in 0..SOAK_TICKS {
        let wave = (tick / INJECTION_PERIOD) as usize;
        if tick % INJECTION_PERIOD == 0 && wave < WAVES as usize {
            for (i, (from, exits)) in driving.iter().enumerate() {
                for row in 0..4 {
                    let to = exits[(i + row + wave) % exits.len()];
                    let kind = match (i + row + wave) % 8 {
                        0 => VehicleKind::Bus,
                        1 => VehicleKind::Truck,
                        _ => VehicleKind::Car,
                    };
                    pending.push((
                        kind,
                        *from,
                        to,
                        7.0 + row as f32 * 23.0 + (i % 3) as f32 * 7.5,
                    ));
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
                    let route = Itinerary::route(
                        Tick(ctx.g.get_tick()),
                        start,
                        end,
                        &map,
                        PathKind::Pedestrian,
                    )
                    .unwrap();
                    (start, end, route)
                };
                let id = roundabout_hotfix_walker(&mut ctx.g, house, start, route);
                walkers.insert(id, (end, start, tick));
            }
        }
        if tick % OBSERVE_PERIOD == 0 {
            pending.retain(|&(kind, from, to, offset)| {
                let Some(id) = try_staged_vehicle(&mut ctx.g, kind, from, to, offset) else {
                    return true;
                };
                cars.insert(id, (to, ctx.g.world.vehicles[id].trans.pos, tick));
                false
            });
        }

        ctx.g.tick(&mut schedule, &[]);
        cars.retain(|&id, &mut (to, _, _)| {
            let car = &ctx.g.world.vehicles[id];
            if car.it.get_point().is_none() && car.trans.pos.is_close(destinations[&to], 0.01) {
                dispatched.push(id);
                return false;
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
            last_car_arrival = tick + 1;
        }
        walkers.retain(|&id, &mut (end, _, _)| {
            if ctx.g.world.humans[id].trans.pos.is_close(end, 0.01) {
                arrived_walkers += 1;
                return false;
            }
            true
        });
        peak_traffic = peak_traffic.max(cars.len() + walkers.len());
        {
            let diagnostics = ctx.g.read::<QueueDiagnostics>();
            let now = ctx.g.get_tick();
            let mut empty = vec![None; zebras.len()];
            for &id in cars.keys() {
                let car = &ctx.g.world.vehicles[id];
                let Some(&traversal) = car.it.get_travers() else {
                    continue;
                };
                let reason = diagnostics.recent_hold(id, traversal, revision, now);
                let category = match reason {
                    Some(QueueHoldReason::Conflict(None)) => "admission",
                    Some(QueueHoldReason::Conflict(Some(_))) => "road conflict",
                    Some(QueueHoldReason::ExitFull) => "exit full",
                    Some(QueueHoldReason::Leader(_)) => "leader",
                    Some(QueueHoldReason::Pedestrian(_)) => "pedestrian",
                    _ => "other",
                };
                if car.speed.0 > 0.2 {
                    continue;
                }
                if tick % OBSERVE_PERIOD == 0 {
                    let phase = match traversal.kind {
                        TraverseKind::Lane(from)
                            if driving.iter().any(|&(lane, _)| lane == from) =>
                        {
                            "entry"
                        }
                        TraverseKind::Lane(_) => "exit",
                        TraverseKind::Turn(_) => "ring",
                    };
                    *stopped_by_reason.entry((phase, category)).or_insert(0) += 1;
                }
                if matches!(
                    reason,
                    Some(QueueHoldReason::Pedestrian(_) | QueueHoldReason::Conflict(None))
                ) && zebras.iter().zip(&mut empty).any(|(zebra, empty)| {
                    zebra.project_dist2(car.trans.pos)
                        <= (car.vehicle.kind.width() * 0.5 + 1.0).powi(2)
                        && *empty.get_or_insert_with(|| {
                            ctx.g.world.humans.iter().all(|(_, walker)| {
                                walker.collider.is_none()
                                    || zebra.project_dist2(walker.trans.pos) > 2.0f32.powi(2)
                            })
                        })
                }) {
                    *empty_zebra_holds.entry(category).or_insert(0) += 1;
                }
            }
        }
        // Measure physical progress from the approach through the receiving
        // lane, including cars that are still held before the ring.
        let mut current_vehicle_stop = 0;
        for (&id, (_, anchor, last_progress)) in &mut cars {
            let position = ctx.g.world.vehicles[id].trans.pos;
            if !position.is_close(*anchor, 0.5) {
                (*anchor, *last_progress) = (position, tick);
            }
            let stop = tick - *last_progress;
            current_vehicle_stop = current_vehicle_stop.max(stop);
            if stop > longest_vehicle_stop {
                longest_vehicle_stop = stop;
                let car = &ctx.g.world.vehicles[id];
                worst_stop = Some((tick, id, car.it.get_travers().copied(), position));
            }
            if stop > MAX_VEHICLE_STOP && stuck_cars.insert(id) {
                let car = &ctx.g.world.vehicles[id];
                let holds = ctx.g.read::<QueueDiagnostics>();
                let reason = car
                    .it
                    .get_travers()
                    .and_then(|&step| holds.recent_hold(id, step, revision, ctx.g.get_tick()));
                let other = match reason {
                    Some(
                        QueueHoldReason::Leader(handle)
                        | QueueHoldReason::Pedestrian(handle)
                        | QueueHoldReason::Conflict(Some(handle)),
                    ) => ctx
                        .g
                        .read::<TransportGrid>()
                        .get(handle)
                        .map(|(pos, state)| (pos, state.dir, state.speed)),
                    _ => None,
                };
                eprintln!("delay tick={tick} car={id:?} pose={position:?} traversal={:?} next={:?} hold={reason:?} other={other:?} speed={}", car.it.get_travers(), car.it.get_route().and_then(|route| route.reversed_route.last()), car.speed.0);
            }
        }
        if tick % OBSERVE_PERIOD != 0 {
            continue;
        }
        let mut current_walker_stop = 0;
        for (&id, (_, anchor, last_progress)) in &mut walkers {
            let position = ctx.g.world.humans[id].trans.pos;
            if !position.is_close(*anchor, 0.5) {
                (*anchor, *last_progress) = (position, tick);
            }
            if tick - *last_progress >= STOP_WINDOW
                && carriageway.iter().any(|(lane, width)| {
                    let (center, _) = lane.project_segment_2d(position.xy());
                    (center.z - position.z).abs() <= 0.65
                        && center.xy().is_close(position.xy(), width + 0.3)
                })
            {
                current_walker_stop = current_walker_stop.max(tick - *last_progress);
            }
        }
        longest_walker_stop = longest_walker_stop.max(current_walker_stop);
        let stuck = current_vehicle_stop >= STOP_WINDOW || current_walker_stop >= STOP_WINDOW;
        clear_ticks = if stuck {
            0
        } else {
            clear_ticks + OBSERVE_PERIOD
        };
        active_clear_ticks = if stuck || cars.is_empty() {
            0
        } else {
            active_clear_ticks + OBSERVE_PERIOD
        };
        longest_active_clear = longest_active_clear.max(active_clear_ticks);
        if (arrived_cars, arrived_walkers) == (224, 224) && clear_ticks > 5 * STOP_WINDOW {
            break;
        }
    }
    eprintln!("peak active {peak_traffic}, longest vehicle stop {longest_vehicle_stop}, longest carriageway walker stop {longest_walker_stop}, longest active clear ticks {longest_active_clear}, final clear ticks {clear_ticks}");
    eprintln!("arrived_cars={arrived_cars} arrived_walkers={arrived_walkers} last_car_arrival_tick={last_car_arrival} cars_per_game_hour={:.2} cars_stopped_over_five_minutes={}",
        arrived_cars as f64 * prototypes::TICKS_PER_HOUR as f64 / last_car_arrival as f64, stuck_cars.len());
    eprintln!("stopped_samples={stopped_by_reason:?} empty_zebra_hold_ticks={empty_zebra_holds:?} worst_stop={worst_stop:?}");
    if !cars.is_empty() {
        let cycles = crate::transportation::road::movement_cycles_at_cutoff(&ctx.g);
        eprintln!("movement_cycles={cycles:?}");
        for &id in cars.keys() {
            let car = &ctx.g.world.vehicles[id];
            if car
                .it
                .get_travers()
                .is_some_and(|step| matches!(step.kind, TraverseKind::Turn(_)))
            {
                eprintln!("unfinished ring vehicle id={id:?} collider={:?} trans={:?} speed={} current={:?} next point={:?}",
                    car.collider, car.trans, car.speed.0, car.it.get_travers(), car.it.get_point());
            }
        }
    }
    assert_eq!(empty_zebra_holds.get("pedestrian"), None);
    assert!(
        stuck_cars.is_empty(),
        "{} vehicles stopped more than five game minutes, longest stop {longest_vehicle_stop} ticks",
        stuck_cars.len(),
    );
    assert!(
        longest_walker_stop < STOP_WINDOW,
        "a walker stood on the carriageway for {longest_walker_stop} ticks"
    );
    assert_eq!(
        (arrived_cars, arrived_walkers),
        (224, 224),
        "unfinished after {SOAK_TICKS} ticks: {} driving, {} pending, {} walking",
        cars.len(),
        pending.len(),
        walkers.len()
    );
    assert!(clear_ticks > 5 * STOP_WINDOW);
    assert!(
        longest_active_clear > 5 * STOP_WINDOW,
        "no five-minute clearance window with vehicles moving"
    );
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
fn roadtools_roundabout_preserves_imported_clips_on_reload() {
    use crate::map::{LanePatternBuilder, MapProject, ProjectFilter};

    let (ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0, 180.0], 18.0, false);
    {
        let mut map = ctx.g.map_mut();
        let start = map.project(ends[0], 0.0, ProjectFilter::INTER);
        let short_end = ends[0] + vec3(0.0, 16.0, 0.0);
        let short = map
            .make_connection(
                start,
                MapProject::ground(short_end),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap()
            .1;
        let next = map.project(short_end, 0.0, ProjectFilter::INTER);
        map.make_connection(
            next,
            MapProject::ground(short_end + vec3(100.0, 0.0, 0.0)),
            None,
            &LanePatternBuilder::new().build(),
        )
        .unwrap();
        let junctions: Vec<_> = map.intersections().keys().collect();
        let roads: Vec<_> = map.roads().keys().collect();
        map.finalize_import(&junctions, &roads);
        assert!(map.roads()[short].interfaced_points().length() <= 10.001);
        assert!(map.get(junction).unwrap().roundabout_geometry().is_some());
    }

    let snapshot = ctx
        .g
        .encode_slot(crate::SnapshotRevision::new(1))
        .unwrap()
        .into_snapshot_bytes();
    let loaded =
        Simulation::load_compressed_snapshot(&snapshot, crate::MAX_SNAPSHOT_BYTES).unwrap();
    assert_eq!(loaded.hashes()["map"], ctx.g.hashes()["map"]);
    assert!(loaded.map().get(junction).unwrap().roundabout_geometry().is_some());
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

    let mut loaded = load_legacy_roundabout(&ctx);
    let resumed = &loaded.world.vehicles[id].it;
    assert_eq!(
        resumed.get_travers().unwrap().kind,
        TraverseKind::Turn(turn)
    );
    assert_ne!(resumed.local_path(), old_waypoints);
    assert_ne!(loaded.world.humans[human_id].it.local_path(), old_walk);
    assert!(loaded.read::<JunctionAdmission>().crossing_turn(turn));
    // The migrated walker is still on a WalkingCorner, which needs no admission claim.
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

fn load_legacy_roundabout(ctx: &TestCtx) -> Simulation {
    let legacy = crate::tests::p9_fire::pre_fire_snapshot(
        ctx,
        r#"{"version":"0.12.0+commands.2","enabled":false,"commands":[]}"#,
    );
    let snapshot = Bincode::encode(&crate::SimulationSer {
        header: legacy._header,
        world: &legacy.world,
        res: legacy.res,
    })
    .unwrap();
    Simulation::decode_snapshot(&snapshot).unwrap()
}

#[test]
fn roadtools_roundabout_preserves_a_saved_claim_at_the_exit_mouth() {
    use crate::transportation::junction_admission::{HoldReason, JunctionFrame};

    let (mut ctx, junction, ends) = roundabout_fixture(&[0.0, 90.0, 180.0], 18.0, false);
    let (from, to, other_from, new_exit, new_dir) = {
        let map = ctx.g.map();
        let center = map.get(junction).unwrap().pos;
        let to = lane_at(&map, center, ends[1]);
        let exit = &map.get(to).unwrap().points;
        (
            lane_at(&map, ends[0], center),
            to,
            lane_at(&map, ends[2], center),
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
    let conflict = staged_vehicle(&mut ctx.g, VehicleKind::Car, other_from, to, 12.0);
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

    let mut loaded = load_legacy_roundabout(&ctx);
    assert!(loaded.read::<JunctionAdmission>().crossing_turn(turn));
    assert!(!loaded
        .read::<JunctionAdmission>()
        .crossing_turn(cleared_turn));
    assert_eq!(loaded.world.vehicles[vehicle].trans.pos, new_exit);

    let past_exit = new_exit + new_dir * 15.0;
    let (entry, direction) = {
        let map = loaded.map();
        let approach = &map.get(other_from).unwrap().points;
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
            &|_| false,
        )
    };
    assert!(matches!(
        prepare(&mut loaded).denial(conflict),
        Some(HoldReason::Conflict | HoldReason::ExitFull)
    ));
    assert!(loaded.read::<JunctionAdmission>().crossing_turn(turn));

    loaded.world.vehicles[vehicle].trans.pos = past_exit;
    assert_eq!(prepare(&mut loaded).denial(conflict), None);
    assert!(!loaded.read::<JunctionAdmission>().crossing_turn(turn));
}
