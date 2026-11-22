use crate::map::{
    Environment, LanePatternBuilder, Map, MapProject, ProjectKind, Road, MAX_SLOPE, ROAD_Z_OFFSET,
};
use common::saveload::{Bincode, Encoder};
use geom::{vec2, vec3, PolyLine, PolyLine3, Vec3};

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
    let deck_height = 20.0;
    let midpoint = path.point_along(110.0);
    assert_eq!(env.height(midpoint), Some(0.0));

    let (points, error) = Road::heightfinder(&path, deck_height, deck_height, MAX_SLOPE, &env);
    assert!(error.is_none());
    assert_eq!(points.first(), path.first().z(deck_height + ROAD_Z_OFFSET));
    assert_eq!(points.last(), path.last().z(deck_height + ROAD_Z_OFFSET));
    let midpoint_height = points.project_2d(midpoint).z;
    assert!(
        points
            .iter()
            .all(|p| p.z >= deck_height - MAX_SLOPE + ROAD_Z_OFFSET),
        "level bridge midpoint at {midpoint_height} m"
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
fn valley_road_keeps_legacy_clipped_geometry_after_junction_edit() {
    let mut map = Map::empty();
    map.environment = Environment::new(1, 1);
    map.environment
        .terrain_apply(map.environment.bounds(), |p| {
            ((p.x - 160.0).abs() - 32.0).max(0.0) * 0.125
        });
    let start = vec2(32.0, 64.0);
    let end = vec2(288.0, 64.0);
    let start = start.z(map.environment.height(start).unwrap());
    let end = end.z(map.environment.height(end).unwrap());
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
        MapProject::ground(side.z(map.environment.height(side).unwrap())),
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
fn rising_bridge_and_lanes_follow_the_deck_after_load_and_junction_edit() {
    let mut map = Map::empty();
    map.environment = Environment::new(1, 1);
    map.environment
        .terrain_apply(map.environment.bounds(), |p| {
            (24.0 - (p.x - 48.0).abs()).max(0.0) * 0.11
        });
    let start = vec3(32.0, 64.0, 2.0);
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
}
