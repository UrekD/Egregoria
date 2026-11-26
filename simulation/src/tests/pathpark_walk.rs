use super::TestCtx;
use crate::map::{LaneKind, PathEndpoint, RoadSide, TurnKind};
use crate::world_command::{CommandOutcomes, WorldCommand};
use crate::Simulation;
use common::saveload::{Bincode, Encoder};
use geom::vec3;

#[test]
fn typed_split_joins_only_the_chosen_side_and_keeps_motor_through() {
    let mut ctx = TestCtx::new();
    let road = {
        use crate::map::{MapProject, ProjectKind, SmallRoadPreset};
        let ground = |x| MapProject {
            pos: vec3(x, 100., 0.),
            kind: ProjectKind::Ground,
        };
        ctx.g
            .map_mut()
            .make_connection(
                ground(40.),
                ground(200.),
                None,
                &SmallRoadPreset::LocalOneWay {
                    curb_parking: false,
                }
                .lane_pattern(),
            )
            .unwrap()
            .1
    };
    let from = ctx
        .g
        .map()
        .pick_path_sidewalk(road, RoadSide::Left, 8_000)
        .unwrap();
    let to = ctx.g.map().pick_path_ground(12_000, 16_000).unwrap();
    let cmd = WorldCommand::MapMakeWalkingPath {
        from,
        to,
        control: None,
    };
    let opposite = ctx
        .g
        .map()
        .pick_path_sidewalk(road, RoadSide::Right, 8_000)
        .unwrap();
    assert_ne!(from, opposite);
    let invalid = WorldCommand::MapMakeWalkingPath {
        from: opposite,
        to,
        control: None,
    };
    assert!(invalid.preview(&ctx.g).is_err());
    let car_pose = {
        let map = ctx.g.map();
        let lane_id = map.roads()[road]
            .lanes_iter()
            .find(|&(_, kind)| kind == LaneKind::Driving)
            .unwrap()
            .0;
        let (pos, dir) = map.lanes()[lane_id].points.point_dir_along(30.0);
        geom::Transform::new_dir(pos, dir)
    };
    let car = crate::transportation::spawn_driving_vehicle(
        &mut ctx.g,
        crate::transportation::VehicleKind::Car,
        car_pose,
    )
    .unwrap();
    ctx.g.apply_commands([&cmd]);
    assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);
    assert_eq!(ctx.g.world.vehicles[car].trans, car_pose);
    assert!(ctx.g.world.vehicles[car].collider.is_some());
    let path_id = ctx
        .g
        .read::<CommandOutcomes>()
        .walking_path(0)
        .unwrap()
        .unwrap();

    let check = |sim: &Simulation| {
        let map = sim.map();
        let path = &map.roads()[path_id];
        assert!(path.is_walking_path());
        assert!(!path.has_sidewalks());
        let joint = map.get(path.src).unwrap();
        let lane = path.lanes_iter().next().unwrap().0;
        assert_eq!(map.lanes()[lane].kind, LaneKind::Walking);
        let joins: Vec<_> = joint
            .turns()
            .filter(|turn| turn.id.src == lane || turn.id.dst == lane)
            .collect();
        assert_eq!(joins.len(), 2);
        for join in joins {
            assert!(join.id.bidirectional);
            let other = if join.id.src == lane {
                join.id.dst
            } else {
                join.id.src
            };
            assert!(map.lanes()[other].points.first().y > 100.0);
            use crate::map::{PathKind, Pathfinder, Traversable, TraverseDirection, TraverseKind};
            let walk = |from, to| {
                PathKind::Pedestrian.path(
                    &map,
                    prototypes::Tick(0),
                    Traversable::new(TraverseKind::Lane(from), TraverseDirection::Backward),
                    to,
                )
            };
            assert!(walk(lane, other).is_some());
            assert!(walk(other, lane).is_some());
        }
        assert_eq!(
            joint
                .turns()
                .filter(|turn| turn.kind == TurnKind::Driving)
                .count(),
            1
        );
        assert!(joint.turns().all(|turn| turn.kind != TurnKind::Crosswalk));
        assert!(map.lots().values().all(|lot| lot.parent != path.id));
        assert_ne!(
            map.electricity.net_id(path.id),
            map.electricity.net_id(joint.id)
        );
    };
    check(&ctx.g);
    ctx.g.apply_commands([&cmd]);
    assert!(ctx.g.read::<CommandOutcomes>().accepted.is_empty());
    let mut loaded: Simulation = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    check(&loaded);
    assert!(matches!(from, PathEndpoint::Sidewalk(_)));
    let branch = {
        let map = loaded.map();
        WorldCommand::MapMakeWalkingPath {
            from: map.pick_path_road(path_id, 3_000).unwrap(),
            to: map.pick_path_ground(16_000, 13_000).unwrap(),
            control: None,
        }
    };
    loaded.apply_commands([&branch]);
    let branch = loaded
        .read::<CommandOutcomes>()
        .walking_path(0)
        .unwrap()
        .unwrap();
    let map = loaded.map();
    let joint = map.get(map.roads()[branch].src).unwrap();
    assert_eq!(joint.roads.len(), 3);
    assert_eq!(joint.turns().count(), 3);
    assert!(joint
        .turns()
        .all(|turn| turn.id.bidirectional && turn.kind == TurnKind::WalkingCorner));
}

#[test]
fn ruled_and_signalized_junction_keeps_motor_mask_and_crosswalks_through_replay() {
    use crate::map::road_edit::{junction_edit_stamp, preview_junction_edit};
    use crate::map::{JunctionRules, LanePatternBuilder, LightPolicy, MapProject, ProjectKind};
    use crate::world_command::PrepareBudget;
    use crate::{MapPreset, NewGameOptions, Replay};

    super::init_test_simulation();
    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        ..NewGameOptions::default()
    })
    .unwrap();
    let joint_pos = vec3(825., 810., 0.);
    let projects = [
        vec3(605., 810., 0.),
        joint_pos,
        vec3(1030., 810., 0.),
        vec3(825., 1010., 0.),
    ]
    .into_iter()
    .map(|pos| MapProject {
        pos,
        kind: ProjectKind::Ground,
    })
    .collect();
    let links = [(0, 1), (1, 2), (1, 3)]
        .into_iter()
        .map(|(a, b)| (a, b, None, LanePatternBuilder::default().build()))
        .collect();
    sim.apply_commands([&WorldCommand::MapMakeMultipleConnections(projects, links)]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    let (joint, from_road, forbidden) = {
        let map = sim.map();
        let joint = map
            .intersections()
            .values()
            .find(|i| i.pos.xy() == joint_pos.xy())
            .unwrap();
        let left = joint
            .roads
            .iter()
            .copied()
            .find(|&id| map.get(id).unwrap().dir_from(joint.id).x < -0.9)
            .unwrap();
        let forbidden = joint
            .turns()
            .find(|t| t.kind == TurnKind::Driving && map.lanes()[t.id.src].parent == left)
            .unwrap()
            .id;
        (joint.id, left, forbidden)
    };
    let lights = WorldCommand::MapUpdateIntersectionPolicy {
        inter: joint,
        turn: Default::default(),
        light: LightPolicy::Lights,
    };
    sim.apply_commands([&lights]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);

    let rules = JunctionRules::new(vec![], vec![forbidden]).unwrap();
    let mut budget = PrepareBudget::for_preview(64_000);
    let expected = junction_edit_stamp(&sim, joint, &mut budget).unwrap();
    let quote = preview_junction_edit(&sim, joint, expected, &rules, &mut budget)
        .unwrap()
        .quote;
    sim.apply_commands([&WorldCommand::MapSetJunctionRules {
        junction: joint,
        expected,
        rules,
        quote,
    }]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);

    let (cmd, original) = {
        let map = sim.map();
        let from = map
            .pick_path_sidewalk(from_road, RoadSide::Right, 7_000)
            .unwrap();
        let road = map.get(from_road).unwrap();
        let (at, dir) = road.points().point_dir_along(70.0);
        let aim = at.xy() + dir.xy().perpendicular() * 60.0;
        let to = map
            .pick_path_ground(
                (aim.x * 100.0).round() as i32,
                (aim.y * 100.0).round() as i32,
            )
            .unwrap();
        (
            WorldCommand::MapMakeWalkingPath {
                from,
                to,
                control: None,
            },
            movements(&map, joint),
        )
    };
    assert!(cmd.preview(&sim).is_ok(), "{:?}", cmd.preview(&sim).err());
    sim.apply_commands([&cmd]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    assert_eq!(movements(&sim.map(), joint), original);
    let replay = sim.read::<Replay>().clone();
    let (mut played, mut loader) = Simulation::try_from_replay(replay).unwrap();
    assert!(loader.advance_tick(&mut played, &mut Simulation::schedule()));
    assert_eq!(movements(&played.map(), joint), original);
    assert_eq!(
        played
            .map()
            .roads()
            .values()
            .filter(|r| r.is_walking_path())
            .count(),
        1
    );
}

fn movements(
    map: &crate::map::Map,
    joint: crate::map::IntersectionID,
) -> Vec<(TurnKind, [i32; 4], bool, String)> {
    let inter = map.get(joint).unwrap();
    let mut result: Vec<_> = inter
        .raw_turns()
        .map(|turn| {
            let a = &map.lanes()[turn.id.src];
            let b = &map.lanes()[turn.id.dst];
            let a_pos = a.get_inter_node_pos(joint);
            let b_pos = b.get_inter_node_pos(joint);
            let key = [a_pos.x, a_pos.y, b_pos.x, b_pos.y].map(|n| (n * 20.0).round() as i32);
            (
                turn.kind,
                key,
                inter.find_turn(turn.id).is_some(),
                format!("{:?}", a.control),
            )
        })
        .collect();
    result.sort();
    result
}

#[test]
fn live_directed_target_survives_or_denies_walking_split() {
    use crate::map::{MapProject, SmallRoadPreset};
    use crate::map_dynamic::{
        DirectedEndpoint, DirectedSearch, DirectedSearchResult, ItineraryTarget,
    };
    use crate::transportation::{spawn_driving_vehicle, VehicleKind};

    let mut ctx = TestCtx::new();
    let road = ctx
        .g
        .map_mut()
        .make_connection(
            MapProject::ground(vec3(40., 100., 0.)),
            MapProject::ground(vec3(240., 100., 0.)),
            None,
            &SmallRoadPreset::LocalOneWay {
                curb_parking: false,
            }
            .lane_pattern(),
        )
        .unwrap()
        .1;
    let (cmd, trans, itinerary, destination) = {
        let map = ctx.g.map();
        let lane = map.roads()[road]
            .lanes_iter()
            .find(|&(_, k)| k == LaneKind::Driving)
            .unwrap()
            .0;
        let from = DirectedEndpoint {
            lane,
            offset_cm: 2_000,
        };
        let to = DirectedEndpoint {
            lane,
            offset_cm: 14_000,
        };
        let DirectedSearchResult::Feasible(plan) =
            DirectedSearch::new(from, to).advance(&map, &mut 128)
        else {
            panic!("expected a route on the original lane");
        };
        (
            WorldCommand::MapMakeWalkingPath {
                from: map.pick_path_sidewalk(road, RoadSide::Left, 8_000).unwrap(),
                to: map.pick_path_ground(12_000, 16_000).unwrap(),
                control: None,
            },
            from.transform(&map).unwrap(),
            plan.itinerary(&map).unwrap(),
            to.transform(&map).unwrap().pos,
        )
    };
    let car = spawn_driving_vehicle(&mut ctx.g, VehicleKind::Car, trans).unwrap();
    ctx.g.world.vehicles[car].it = itinerary;
    let before = ctx.g.hashes();
    let preview = cmd.preview(&ctx.g);
    ctx.g.apply_commands([&cmd]);
    if preview.is_err() {
        assert!(ctx.g.read::<CommandOutcomes>().accepted.is_empty());
        assert_eq!(ctx.g.hashes(), before);
    } else {
        assert_eq!(ctx.g.read::<CommandOutcomes>().accepted, [0]);
    }
    let Some(ItineraryTarget::DirectedLane(lane, offset)) = ctx.g.world.vehicles[car].it.target()
    else {
        panic!("the exact destination was lost");
    };
    assert!(ctx.g.map().get(lane).is_some_and(|l| l
        .points
        .point_along(offset)
        .distance(destination)
        < 0.03));
    ctx.g = Bincode::decode(&Bincode::encode(&ctx.g).unwrap()).unwrap();
    for _ in 0..250 {
        ctx.tick_fast();
    }
    assert!(ctx.g.world.vehicles[car].trans.pos.distance(trans.pos) > 1.);
}

#[test]
fn special_building_cannot_attach_to_a_walking_path() {
    use crate::map::BuildingKind;
    use geom::{vec2, Vec2, OBB};
    use prototypes::GoodsCompanyID;

    let mut ctx = TestCtx::new();
    let path = WorldCommand::MapMakeWalkingPath {
        from: ctx.g.map().pick_path_ground(4_000, 20_000).unwrap(),
        to: ctx.g.map().pick_path_ground(22_000, 20_000).unwrap(),
        control: None,
    };
    ctx.g.apply_commands([&path]);
    let road = ctx
        .g
        .read::<CommandOutcomes>()
        .walking_path(0)
        .unwrap()
        .unwrap();
    let school = GoodsCompanyID::new("school").prototype();
    let place = WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(vec2(135., 217.), Vec2::Y, 20., 20.),
        kind: BuildingKind::GoodsCompany(school.id),
        gen: school.bgen,
        zone: None,
        connected_road: Some(road),
    };
    assert!(place.preview(&ctx.g).is_err());
    let before = ctx.g.hashes();
    ctx.g.apply_commands([&place]);
    assert!(ctx.g.read::<CommandOutcomes>().accepted.is_empty());
    assert_eq!(ctx.g.hashes(), before);
}

#[test]
fn saved_building_cannot_smuggle_a_path_attachment() {
    use crate::map::{BuildingKind, MapProject, SmallRoadPreset};
    use geom::{vec2, Vec2, OBB};
    use prototypes::GoodsCompanyID;

    let mut ctx = TestCtx::new();
    let path = WorldCommand::MapMakeWalkingPath {
        from: ctx.g.map().pick_path_ground(4_000, 20_000).unwrap(),
        to: ctx.g.map().pick_path_ground(22_000, 20_000).unwrap(),
        control: None,
    };
    ctx.g.apply_commands([&path]);
    let path = ctx
        .g
        .read::<CommandOutcomes>()
        .walking_path(0)
        .unwrap()
        .unwrap();
    let school = GoodsCompanyID::new("school").prototype();
    let mut map = ctx.g.map_mut();
    let road = map
        .make_connection(
            MapProject::ground(vec3(40., 320., 0.)),
            MapProject::ground(vec3(220., 320., 0.)),
            None,
            &SmallRoadPreset::LocalOneWay {
                curb_parking: false,
            }
            .lane_pattern(),
        )
        .unwrap()
        .1;
    let school = map
        .build_special_building(
            &OBB::new(vec2(140., 340.), Vec2::Y, 20., 20.),
            BuildingKind::GoodsCompany(school.id),
            school.bgen,
            None,
            Some(road),
        )
        .unwrap();
    map.roads[road]
        .connected_buildings
        .retain(|&id| id != school);
    map.roads[path].connected_buildings.push(school);
    map.buildings[school].connected_road = Some(path);
    drop(map);
    assert!(Bincode::decode::<Simulation>(&Bincode::encode(&ctx.g).unwrap()).is_err());
}
