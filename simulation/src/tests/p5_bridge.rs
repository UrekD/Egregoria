use crate::map::{
    Environment, LanePatternBuilder, Map, MapProject, ProjectKind, Road, RoadSegmentKind,
    MAX_SLOPE, ROAD_Z_OFFSET,
};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, PolyLine, PolyLine3, Vec2, Vec3};

fn point_bits(points: &PolyLine3) -> Vec<[u32; 3]> {
    points
        .iter()
        .map(|p| [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()])
        .collect()
}

fn assert_straight_deck(points: &PolyLine3, start: Vec3, end: Vec3) {
    for point in points.iter() {
        let expected_z = start.z + (end.z - start.z) * ((point.x - start.x) / (end.x - start.x));
        assert!(
            (point.z - expected_z - ROAD_Z_OFFSET).abs() < 0.001,
            "{point:?} is off the deck (expected z = {})",
            expected_z + ROAD_Z_OFFSET
        );
    }
}

#[test]
fn elevated_level_span_keeps_its_deck_above_flat_terrain() {
    let mut env = Environment::new(1, 1);
    env.terrain_apply(env.bounds(), |_| 0.0);
    let path = PolyLine::new(vec![vec2(32.0, 64.0), vec2(252.0, 64.0)]);
    let deck_height = 2.5;
    let midpoint = path.point_along(110.0);
    assert_eq!(env.height(midpoint), Some(0.0));

    let (points, error) = Road::heightfinder(&path, deck_height, deck_height, MAX_SLOPE, &env);
    assert!(error.is_none());
    assert_eq!(points.first(), path.first().z(deck_height + ROAD_Z_OFFSET));
    assert_eq!(points.last(), path.last().z(deck_height + ROAD_Z_OFFSET));
    assert_straight_deck(
        &points,
        path.first().z(deck_height),
        path.last().z(deck_height),
    );
}

#[test]
fn terrain_level_valley_road_keeps_its_original_points() {
    let mut env = Environment::new(1, 1);
    env.terrain_apply(env.bounds(), |p| {
        ((p.x - 160.0).abs() - 32.0).max(0.0) * 0.125
    });
    let path = PolyLine::new(vec![vec2(32.0, 64.0), vec2(288.0, 64.0)]);
    let start_height = env.height(path.first()).unwrap();
    let end_height = env.height(path.last()).unwrap();
    assert!(start_height > 10.0 && end_height > 10.0);

    let (points, error) = Road::heightfinder(&path, start_height, end_height, MAX_SLOPE, &env);
    assert!(error.is_none());
    assert_eq!(points.project_2d(vec2(160.0, 64.0)).z, ROAD_Z_OFFSET);
    // Centerline from the unmodified solver, including the exact height bits after packing.
    let expected = [
        (32.0_f32, 0x4144_8e4d),
        (128.0, 0x3e99_999a),
        (192.0, 0x3e99_999a),
        (288.0, 0x4144_8e4d),
    ]
    .map(|(x, z)| [x.to_bits(), 64.0_f32.to_bits(), z]);
    assert_eq!(point_bits(&points), expected);
}

#[test]
fn noisy_ground_clicks_keep_the_legacy_valley_profile() {
    let mut map = Map::empty();
    let mut terrain = Environment::new(1, 1);
    terrain.terrain_apply(terrain.bounds(), |p| {
        ((p.x - 160.0).abs() - 32.0).max(0.0) * 0.125
    });
    assert!(map.install_environment(terrain));
    let ground_click =
        |pos: Vec2| MapProject::ground(pos.z(map.environment().height(pos).unwrap() + 0.3));
    let (_, id) = map
        .make_connection(
            ground_click(vec2(32.0, 64.0)),
            ground_click(vec2(288.0, 64.0)),
            None,
            &LanePatternBuilder::default().build(),
        )
        .unwrap();

    // Full centerline from the pre-PORT2 solver (2b47bffd) with the same clicks.
    let expected = [
        (32.0_f32, 0x4149_5b1a),
        (33.0, 0x4147_fe87),
        (34.0, 0x4144_add3),
        (35.0, 0x4140_9083),
        (36.0, 0x413c_ce23),
        (128.0, 0x3e99_999a),
        (192.0, 0x3e99_999a),
        (284.0, 0x413c_ce24),
        (285.0, 0x4140_9085),
        (286.0, 0x4144_add2),
        (287.0, 0x4147_fe87),
        (288.0, 0x4149_5b1a),
    ]
    .map(|(x, z)| [x.to_bits(), 64.0_f32.to_bits(), z]);
    assert_eq!(point_bits(map.roads[id].points()), expected);
}

#[test]
fn splitting_a_valley_road_snaps_the_junction_to_terrain() {
    let mut map = Map::empty();
    let mut terrain = Environment::new(1, 1);
    terrain.terrain_apply(terrain.bounds(), |p| {
        let distance = (p.x - 160.0).abs();
        (distance - 80.0).max(0.0) * 0.125 + distance.min(32.0) * 0.00625
    });
    assert!(map.install_environment(terrain));
    let ground = |map: &Map, pos: Vec2| pos.z(map.environment().height(pos).unwrap());
    let start = ground(&map, vec2(32.0, 64.0));
    let end = ground(&map, vec2(288.0, 64.0));
    let pattern = LanePatternBuilder::default().build();
    let (_, original) = map
        .make_connection(
            MapProject::ground(start),
            MapProject::ground(end),
            None,
            &pattern,
        )
        .unwrap();
    let midpoint = ground(&map, vec2(160.0, 64.0));
    let projected = map.roads[original].points().project_2d(midpoint.xy());
    assert!(projected.z - ROAD_Z_OFFSET > midpoint.z + 0.05);
    let (junction, side_road) = map
        .make_connection(
            MapProject::ground(ground(&map, vec2(160.0, 144.0))),
            MapProject {
                pos: projected,
                kind: ProjectKind::Road(original),
            },
            None,
            &pattern,
        )
        .unwrap();
    let junction = &map.intersections[junction];
    assert_eq!(junction.pos, midpoint);
    assert!(!map.roads.contains_key(original));
    assert_eq!(junction.roads.len(), 3);
    for &id in junction.roads.iter().filter(|&&id| id != side_road) {
        let road = &map.roads[id];
        let (split_end, outer_end) = if road.src == junction.id {
            (road.points().first(), road.points().last())
        } else {
            (road.points().last(), road.points().first())
        };
        assert_eq!(split_end, midpoint + Vec3::z(ROAD_Z_OFFSET));
        let sample = vec2((outer_end.x + midpoint.x) * 0.5, midpoint.y);
        for profile in [road.points(), road.interfaced_points()] {
            let height = profile.project_2d(sample).z - ROAD_Z_OFFSET;
            assert!((height - ground(&map, sample).z).abs() < 0.25);
        }
    }
}

#[test]
fn splitting_at_a_centerline_vertex_replaces_its_height() {
    let mut map = Map::empty();
    let mut terrain = Environment::new(1, 1);
    terrain.terrain_apply(terrain.bounds(), |_| 0.0);
    assert!(map.install_environment(terrain));
    let src = map.add_intersection(vec3(32.0, 64.0, 0.0));
    let dst = map.add_intersection(vec3(288.0, 64.0, 0.0));
    let projected = vec3(160.0, 64.0, ROAD_Z_OFFSET + 0.3);
    let centerline = PolyLine3::new(vec![
        vec3(32.0, 64.0, ROAD_Z_OFFSET),
        projected,
        vec3(288.0, 64.0, ROAD_Z_OFFSET),
    ]);
    let pattern = LanePatternBuilder::default().build();
    let original = map
        .connect(src, dst, &pattern, RoadSegmentKind::Arbitrary(centerline))
        .unwrap();
    let (junction, side_road) = map
        .make_connection(
            MapProject::ground(vec3(160.0, 144.0, 0.0)),
            MapProject {
                pos: projected,
                kind: ProjectKind::Road(original),
            },
            None,
            &pattern,
        )
        .unwrap();
    let junction = &map.intersections[junction];
    for &id in junction.roads.iter().filter(|&&id| id != side_road) {
        assert_eq!(map.roads[id].points().len(), 2);
    }
    assert_eq!(junction.pos.z, 0.0);
}

#[test]
fn valley_road_keeps_legacy_clipped_geometry_after_junction_edit() {
    let mut map = Map::empty();
    let mut terrain = Environment::new(1, 1);
    terrain.terrain_apply(terrain.bounds(), |p| {
        ((p.x - 160.0).abs() - 32.0).max(0.0) * 0.125
    });
    assert!(map.install_environment(terrain));
    let start = vec2(32.0, 64.0);
    let end = vec2(288.0, 64.0);
    let start = start.z(map.environment().height(start).unwrap());
    let end = end.z(map.environment().height(end).unwrap());
    let pattern = LanePatternBuilder::default().build();
    let (junction, id) = map
        .make_connection(
            MapProject::ground(start),
            MapProject::ground(end),
            None,
            &pattern,
        )
        .unwrap();
    let before = map.roads[id].interfaced_points().clone();

    let side = vec2(288.0, 140.0);
    map.make_connection(
        MapProject {
            pos: end,
            kind: ProjectKind::Intersection(junction),
        },
        MapProject::ground(side.z(map.environment().height(side).unwrap())),
        None,
        &pattern,
    )
    .unwrap();
    map.update_intersection(junction, |junction| junction.turn_policy.back_turns = true);
    let after = map.roads[id].interfaced_points();

    // Coordinate bits from the pre-PORT2 solver (2b47bffd). Both layouts
    // have the same source approach and differ at the edited junction.
    let shared = [
        (0x423c_54e9, 0x4144_8e4d),
        (0x4240_54e9, 0x4144_09c7),
        (0x4244_54e9, 0x4142_8f1d),
        (0x4248_54e9, 0x4140_3a99),
        (0x424c_54e9, 0x413d_2894),
        (0x4250_54e9, 0x4139_755c),
        (0x4254_54e9, 0x4135_3d41),
        (0x425c_54e9, 0x412b_afb0),
        (0x426c_54e9, 0x4117_37ff),
        (0x4270_54e9, 0x4112_76a7),
        (0x4274_54e9, 0x410e_12fa),
        (0x4278_54e9, 0x410a_294a),
        (0x427c_54e9, 0x4106_d5e8),
        (0x4280_2a74, 0x4104_3526),
        (0x4300_153a, 0x3e99_999a),
        (0x4340_153a, 0x3e9e_dd88),
    ];
    let dead_end = [
        (0x437f_153a, 0x4102_95c9),
        (0x4380_0a9d, 0x4105_4784),
        (0x4380_8a9d, 0x4108_b05b),
        (0x4381_0a9d, 0x410c_b34f),
        (0x4381_8a9d, 0x4111_3351),
        (0x4384_8a9d, 0x4130_1bdb),
        (0x4385_0a9d, 0x4134_da65),
        (0x4385_8a9d, 0x4139_2dba),
        (0x4386_0a9d, 0x413c_f8d5),
        (0x4386_8a9d, 0x4140_1ead),
        (0x4387_0a9d, 0x4142_8239),
        (0x4387_8a9d, 0x4144_0671),
        (0x4388_7563, 0x4144_8e4d),
    ];
    let corner = [
        (0x4385_8a9d, 0x411a_bbe5),
        (0x4386_0a9d, 0x411d_debb),
        (0x4386_8a9d, 0x4121_e1a1),
        (0x4387_0a9d, 0x4126_853e),
        (0x4388_8a9d, 0x4135_bb4c),
        (0x4389_0a9d, 0x413a_68a7),
        (0x4389_8a9d, 0x413e_7a25),
        (0x438a_0a9d, 0x4141_b077),
        (0x438a_8a9d, 0x4143_cc4a),
        (0x438b_495e, 0x4144_8e4d),
    ];
    let expected = |tail: &[(u32, u32)]| {
        shared
            .iter()
            .chain(tail)
            .map(|&(x, z)| [x, 64.0_f32.to_bits(), z])
            .collect::<Vec<_>>()
    };
    assert_eq!(point_bits(&before), expected(&dead_end));
    assert_eq!(point_bits(after), expected(&corner));
}

#[test]
fn clear_span_near_the_grade_limit_stays_straight() {
    let mut env = Environment::new(1, 1);
    env.terrain_apply(env.bounds(), |_| 0.0);
    let start = vec3(32.0, 64.0, 20.0);
    let end = vec3(288.0, 64.0, 83.0);
    let path = PolyLine::new(vec![start.xy(), end.xy()]);

    let (points, error) = Road::heightfinder(&path, start.z, end.z, MAX_SLOPE, &env);
    assert!(error.is_none());
    assert_straight_deck(&points, start, end);
}

#[test]
fn low_rising_bridge_keeps_a_deck_in_both_directions() {
    let mut env = Environment::new(1, 1);
    env.terrain_apply(env.bounds(), |p| {
        (24.0 - (p.x - 48.0).abs()).max(0.0) * 0.11
    });
    let low = vec3(32.0, 64.0, 2.0);
    let high = vec3(288.0, 64.0, 16.0);
    let clearance = low.z - env.height(low.xy()).unwrap();
    assert!((0.5..2.5).contains(&clearance));

    for (start, end) in [(low, high), (high, low)] {
        let path = PolyLine::new(vec![start.xy(), end.xy()]);
        let (points, error) = Road::heightfinder(&path, start.z, end.z, MAX_SLOPE, &env);
        assert!(error.is_none());
        assert_straight_deck(&points, start, end);
    }

    let path = PolyLine::new(vec![low.xy(), high.xy()]);
    let near_ground = env.height(low.xy()).unwrap() + 0.3;
    let (points, error) = Road::heightfinder(&path, near_ground, high.z, MAX_SLOPE, &env);
    assert!(error.is_none());
    assert_eq!(points.project_2d(vec2(160.0, 64.0)).z, ROAD_Z_OFFSET);
}

#[test]
fn split_bridge_keeps_both_decks_over_high_ground() {
    let mut map = Map::empty();
    let mut terrain = Environment::new(1, 1);
    terrain.terrain_apply(terrain.bounds(), |p| {
        (1.0 - (p.x - 160.0).abs() / 32.0).max(0.0) * 4.9
    });
    assert!(map.install_environment(terrain));
    let start = vec3(32.0, 64.0, 5.0);
    let end = vec3(288.0, 64.0, 5.0);
    let pattern = LanePatternBuilder::default().build();
    let (_, original) = map
        .make_connection(
            MapProject::ground(start),
            MapProject::ground(end),
            None,
            &pattern,
        )
        .unwrap();
    let midpoint = vec3(160.0, 64.0, start.z);
    let clearance = midpoint.z - map.environment().height(midpoint.xy()).unwrap();
    assert!((0.0..0.5).contains(&clearance));
    assert_straight_deck(map.roads[original].interfaced_points(), start, end);

    let from = MapProject::ground(vec3(160.0, 144.0, midpoint.z));
    let to = MapProject {
        pos: midpoint + Vec3::z(ROAD_Z_OFFSET),
        kind: ProjectKind::Road(original),
    };
    let preview_end = to.road_connection_position(&map).unwrap();
    assert_eq!(preview_end, midpoint);
    let (preview, error) = Road::generate_points(
        from.road_connection_position(&map).unwrap(),
        preview_end,
        RoadSegmentKind::Straight,
        false,
        map.environment(),
    );
    assert!(error.is_none());

    let (junction, side_road) = map.make_connection(from, to, None, &pattern).unwrap();
    assert_eq!(preview.as_slice(), map.roads[side_road].points().as_slice());
    let junction = &map.intersections[junction];
    assert_eq!(junction.pos, midpoint);
    assert_eq!(junction.roads.len(), 3);
    for &id in junction.roads.iter().filter(|&&id| id != side_road) {
        let road = &map.roads[id];
        assert_straight_deck(road.points(), start, end);
        assert_straight_deck(road.interfaced_points(), start, end);
        for (lane, _) in road.lanes_iter() {
            assert_straight_deck(&map.lanes[lane].points, start, end);
        }
    }
}

#[test]
fn rising_bridge_and_lanes_follow_the_deck_after_load_and_junction_edit() {
    let mut map = Map::empty();
    let mut terrain = Environment::new(1, 1);
    terrain.terrain_apply(terrain.bounds(), |p| {
        (24.0 - (p.x - 48.0).abs()).max(0.0) * 0.11
    });
    assert!(map.install_environment(terrain));
    let start = vec3(32.0, 64.0, 4.0);
    let end = vec3(288.0, 64.0, 16.0);
    let pattern = LanePatternBuilder::default().build();
    let (junction, id) = map
        .make_connection(
            MapProject::ground(start),
            MapProject::ground(end),
            None,
            &pattern,
        )
        .unwrap();

    let check_deck = |map: &Map| {
        let road = &map.roads[id];
        assert_straight_deck(road.points(), start, end);
        assert_straight_deck(road.interfaced_points(), start, end);
        for (lane, _) in road.lanes_iter() {
            assert_straight_deck(&map.lanes[lane].points, start, end);
        }
    };
    check_deck(&map);
    let old_clip = map.roads[id].interface_point(junction);

    let mut map: Map = Bincode::decode(&Bincode::encode(&map).unwrap()).unwrap();
    map.make_connection(
        MapProject {
            pos: end,
            kind: ProjectKind::Intersection(junction),
        },
        MapProject::ground(vec3(288.0, 140.0, end.z)),
        None,
        &pattern,
    )
    .unwrap();
    map.update_intersection(junction, |junction| junction.turn_policy.back_turns = true);
    assert_ne!(map.roads[id].interface_point(junction).x, old_clip.x);
    check_deck(&map);

    let midpoint = vec3(160.0, 64.0, (start.z + end.z) * 0.5);
    let (junction, side_road) = map
        .make_connection(
            MapProject::ground(vec3(160.0, 144.0, midpoint.z)),
            MapProject {
                pos: midpoint + Vec3::z(ROAD_Z_OFFSET),
                kind: ProjectKind::Road(id),
            },
            None,
            &pattern,
        )
        .unwrap();
    let junction = &map.intersections[junction];
    assert!((junction.pos.z - midpoint.z).abs() < 0.001);
    assert_eq!(junction.roads.len(), 3);
    for &id in junction.roads.iter().filter(|&&id| id != side_road) {
        let road = &map.roads[id];
        assert_straight_deck(road.points(), start, end);
        assert_straight_deck(road.interfaced_points(), start, end);
    }
}

fn wet_crossing_map() -> Map {
    let mut environment = Environment::new(1, 1);
    environment.terrain_apply(environment.bounds(), |_| 4.0);
    environment
        .carve_river(&[vec2(256.0, 16.0), vec2(256.0, 496.0)], 48.0, 6.0)
        .unwrap();
    assert_eq!(
        environment.water_surface_elevation(vec2(256.0, 256.0)),
        Some(3.0)
    );
    let mut map = Map::empty();
    assert!(map.install_environment(environment));
    map
}

#[test]
fn wet_crossing_forms_a_bridge_from_dry_ground_anchors() {
    let mut map = wet_crossing_map();
    let pattern = LanePatternBuilder::default().build();
    let (_, id) = map
        .make_connection(
            MapProject::ground(vec3(80.0, 256.0, 4.0)),
            MapProject::ground(vec3(432.0, 256.0, 4.0)),
            None,
            &pattern,
        )
        .expect("dry banks can be joined by an elevated crossing");
    let bridge = &map.roads[id];
    assert!(bridge.elevated_span());
    for x in [224.0, 240.0, 256.0, 272.0, 287.9] {
        let sample = vec2(x, 256.0);
        let water = map.environment().water_at(sample).unwrap();
        let deck = bridge.interfaced_points().project_2d(sample);
        assert!(deck.z - ROAD_Z_OFFSET >= water.surface_z + 2.5);
    }
}

#[test]
fn wet_crossing_rejects_a_ground_anchor_atomically() {
    let mut map = wet_crossing_map();
    let before = Bincode::encode(&map).unwrap();
    let result = map.make_connection(
        MapProject::ground(vec3(80.0, 256.0, 4.0)),
        MapProject::ground(vec3(256.0, 256.0, 3.0)),
        None,
        &LanePatternBuilder::default().build(),
    );
    assert!(result.is_none());
    assert_eq!(Bincode::encode(&map).unwrap(), before);
}

#[test]
fn wet_crossing_rejects_an_explicit_at_grade_deck() {
    let mut map = wet_crossing_map();
    let start = vec3(80.0, 256.0, 4.0);
    let end = vec3(432.0, 256.0, 4.0);
    let src = map.add_intersection(start);
    let dst = map.add_intersection(end);
    let before = Bincode::encode(&map).unwrap();
    let result = map.connect(
        src,
        dst,
        &LanePatternBuilder::default().build(),
        RoadSegmentKind::Arbitrary(PolyLine3::new(vec![
            start.up(ROAD_Z_OFFSET),
            end.up(ROAD_Z_OFFSET),
        ])),
    );
    assert!(result.is_none());
    assert_eq!(Bincode::encode(&map).unwrap(), before);
}

#[test]
fn wet_crossing_split_and_merge_keep_the_span_and_deck() {
    let mut map = wet_crossing_map();
    let pattern = LanePatternBuilder::default().build();
    let (_, original) = map
        .make_connection(
            MapProject::ground(vec3(80.0, 256.0, 4.0)),
            MapProject::ground(vec3(432.0, 256.0, 4.0)),
            None,
            &pattern,
        )
        .unwrap();
    let split = vec2(256.0, 256.0);
    let deck_point = map.roads[original].points().project_2d(split);
    let (junction, spur) = map
        .make_connection(
            MapProject::ground(vec3(112.0, 136.0, 4.0)),
            MapProject {
                pos: deck_point,
                kind: ProjectKind::Road(original),
            },
            None,
            &pattern,
        )
        .unwrap();
    assert!(!map.roads.contains_key(original));
    assert!((map.intersections[junction].pos.z + ROAD_Z_OFFSET - deck_point.z).abs() < 0.0001);
    assert!(map.remove_road(spur).is_some());
    let [first, second] = map.intersections[junction].roads.as_slice() else {
        panic!("expected the original span's two split pieces");
    };
    let [first, second] = [*first, *second];
    assert!(map.roads[first].elevated_span() && map.roads[second].elevated_span());
    let merged = map.merge_road(first, second).unwrap();
    assert!(!map.roads.contains_key(first) && !map.roads.contains_key(second));
    assert!(map.roads[merged].elevated_span());
    assert_eq!(map.roads[merged].points().project_2d(split), deck_point);
    let decoded: Map = Bincode::decode(&Bincode::encode(&map).unwrap()).unwrap();
    assert!(decoded.roads[merged].elevated_span());
}

#[test]
fn wet_crossing_curved_rail_matches_its_final_preview() {
    let mut map = wet_crossing_map();
    let pattern = LanePatternBuilder {
        rail: true,
        ..LanePatternBuilder::default()
    }
    .build();
    let start = MapProject::ground(vec3(80.0, 232.0, 5.0));
    let end = MapProject::ground(vec3(432.0, 304.0, 5.0));
    let elbow = Some(vec2(238.0, 330.0));
    let preview = map.preview_connection(start, end, elbow, &pattern).unwrap();
    assert!(preview.crosses_water);
    assert_eq!(preview.junctions.len(), 2);
    let (_, id) = map.make_connection(start, end, elbow, &pattern).unwrap();
    let road = &map.roads[id];
    assert_eq!(
        point_bits(&preview.deck),
        point_bits(road.interfaced_points())
    );
    for (id, _) in road.lanes_iter() {
        for segment in map.lanes[id].points.as_slice().windows(2) {
            let delta = segment[1] - segment[0];
            assert!(delta.z.abs() <= delta.xy().mag() * MAX_SLOPE + 0.0001);
        }
    }
}

#[test]
fn wet_crossing_indexed_replay_keeps_the_accepted_bridge() {
    use crate::map::{WaterEdit, WaterStroke};
    use crate::world_command::{CommandOutcomes, WorldCommand};
    use crate::{MapPreset, NewGameOptions, Replay, Simulation};

    super::init_test_simulation();
    let mut sim = Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: 16,
        ..NewGameOptions::default()
    })
    .unwrap();
    let start = vec2(2080.0, 2304.0);
    let end = vec2(2432.0, 2304.0);
    let ground = sim.map().environment().height(start).unwrap();
    let level_dm = ((ground - 1.0) * 10.0).floor() as i16;
    sim.edit_water(WaterEdit::CarveRiver {
        polyline: WaterStroke::new(&[vec2(2256.0, 2080.0), vec2(2256.0, 2528.0)]).unwrap(),
        width_m: 48.0,
        depth_m: 6.0,
        level_dm: Some(level_dm),
    })
    .unwrap();
    let projects = vec![
        MapProject::ground(start.z(ground)),
        MapProject::ground(end.z(ground)),
        MapProject::ground(vec3(2256.0, 2336.0, ground)),
    ];
    let pattern = LanePatternBuilder::default().build();
    let command = WorldCommand::MapMakeMultipleConnections(
        projects,
        vec![(0, 2, None, pattern.clone()), (0, 1, None, pattern)],
    );
    let commands_before = sim.read::<Replay>().command_count();
    sim.apply_commands([&command]);
    {
        let outcomes = sim.read::<CommandOutcomes>();
        assert!(outcomes.link_outcomes(0)[0].1.is_err());
        assert!(outcomes.link_outcomes(0)[1].1.is_ok());
    }
    assert_eq!(sim.read::<Replay>().command_count(), commands_before + 1);

    let replay = sim.read::<Replay>().clone();
    let (mut loaded, mut loader) = Simulation::try_from_replay(replay).unwrap();
    let mut schedule = Simulation::schedule();
    assert!(loader.advance_tick(&mut loaded, &mut schedule));
    assert_eq!(sim.hashes()["map"], loaded.hashes()["map"]);
}

#[test]
fn wet_crossing_uses_deck_width_even_with_a_dry_centerline() {
    use geom::Polygon;

    let mut environment = Environment::new(1, 1);
    environment.terrain_apply(environment.bounds(), |_| 4.0);
    environment
        .fill_lake(
            &Polygon::centered_rect(vec2(256.0, 256.0), 64.0, 96.0),
            30,
            5.0,
        )
        .unwrap();
    let mut map = Map::empty();
    assert!(map.install_environment(environment));
    let start = MapProject::ground(vec3(220.0, 64.0, 4.0));
    let end = MapProject::ground(vec3(220.0, 440.0, 4.0));
    let pattern = LanePatternBuilder::default().build();
    assert!(!map.environment().is_water(vec2(220.0, 256.0)));
    assert!(map.environment().is_water(vec2(225.0, 256.0)));
    let preview = map.preview_connection(start, end, None, &pattern).unwrap();
    assert!(preview.crosses_water);
    let (_, id) = map.make_connection(start, end, None, &pattern).unwrap();
    assert_eq!(
        point_bits(&preview.deck),
        point_bits(map.roads[id].interfaced_points())
    );
}

#[test]
fn wet_crossing_bed_change_moves_supports_with_fixed_ids_and_deck() {
    use crate::map::{TerraformKind, UpdateType};
    use crate::world_command::{CommandOutcomes, WorldCommand};
    use crate::Simulation;

    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(1, 0).unwrap();
    *sim.write::<Map>() = wet_crossing_map();
    let (junction, road) = sim
        .map_mut()
        .make_connection(
            MapProject::ground(vec3(64.0, 256.0, 4.0)),
            MapProject::ground(vec3(416.0, 256.0, 4.0)),
            None,
            &LanePatternBuilder::default().build(),
        )
        .unwrap();
    let mut support = sim.map().subscribe(UpdateType::RoadSupport);
    let pylon = |map: &Map| {
        Road::pylons_positions(map.roads[road].interfaced_points(), map.environment())
            .find(|pylon| map.environment().is_water(pylon.pos.xy()))
            .unwrap()
    };
    let before = pylon(&sim.map());
    let lanes: Vec<_> = sim.map().roads[road].lanes_iter().collect();
    sim.apply_commands([&WorldCommand::Terraform {
        kind: TerraformKind::Level,
        center: before.pos.xy(),
        radius: 24.0,
        amount: 1.0,
        level: -7.0,
        slope: None,
    }]);
    assert!(sim.read::<CommandOutcomes>().rejections_for(0).is_empty());
    let map = sim.map();
    let after = pylon(&map);
    assert_eq!(before.pos, after.pos);
    assert!(after.terrain_height < before.terrain_height);
    assert!(map.intersections.contains_key(junction));
    assert_eq!(map.roads[road].lanes_iter().collect::<Vec<_>>(), lanes);
    assert!(support.take_updated_chunks().next().is_some());
}
