use crate::map::{Environment, Road, MAX_SLOPE, ROAD_Z_OFFSET};
use geom::{vec2, PolyLine};

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
    let point_bits: Vec<_> = points
        .iter()
        .map(|p| [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()])
        .collect();
    // Centerline from the unmodified solver, including the exact height bits after packing.
    let expected = [
        (32.0_f32, 0x4144_8e4d),
        (128.0, 0x3e99_999a),
        (192.0, 0x3e99_999a),
        (288.0, 0x4144_8e4d),
    ]
    .map(|(x, z)| [x.to_bits(), 64.0_f32.to_bits(), z]);
    assert_eq!(point_bits, expected);
}
