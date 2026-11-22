use super::TestCtx;
use crate::map::special_placement::{snap_special_building, PlacementError};
use crate::map::{BuildingKind, LaneKind, LanePattern, LanePatternBuilder, ProjectFilter, RoadID};
use crate::world_command::CommandOutcomes;
use crate::WorldCommand;
use geom::{vec2, vec3, Vec3};
use prototypes::{GoodsCompanyID, Size2D};

fn street() -> LanePattern {
    // A narrow 100 m street has 82 m between its end interfaces.
    LanePatternBuilder::new()
        .one_way(true)
        .parking(false)
        .build()
}

fn connect(test: &TestCtx, from: Vec3, to: Vec3, pattern: &LanePattern) -> RoadID {
    let mut map = test.g.map_mut();
    let filter = ProjectFilter::INTER | ProjectFilter::ROAD;
    let from = map.project(from, 0.0, filter);
    let to = map.project(to, 0.0, filter);
    map.make_connection(from, to, None, pattern).unwrap().1
}

#[test]
fn flour_factory_fits_on_a_100_m_street_and_commits_the_preview() {
    let mut test = TestCtx::new_fast();
    let road = connect(
        &test,
        vec3(100.0, 100.0, 0.0),
        vec3(200.0, 100.0, 0.0),
        &street(),
    );
    let proto = GoodsCompanyID::new("flour-factory").prototype();
    let cursor = vec3(150.0, 110.0, 0.0);
    let snap = snap_special_building(&test.g.map(), cursor, proto.size).unwrap();
    assert_eq!(snap.connected_road, road);
    assert_eq!(snap.obb.center().x, cursor.x);
    assert!(test
        .g
        .map()
        .spatial_map()
        .query(snap.obb, ProjectFilter::ROAD)
        .next()
        .is_none());

    let kind = BuildingKind::GoodsCompany(proto.id);
    test.apply(&[WorldCommand::MapBuildSpecialBuilding {
        pos: snap.obb,
        kind,
        gen: proto.bgen,
        zone: None,
        connected_road: Some(snap.connected_road),
    }]);
    assert!(test.g.read::<CommandOutcomes>().last.is_empty());
    let map = test.g.map();
    let building = map.buildings().values().find(|b| b.kind == kind).unwrap();
    assert_eq!(building.obb.corners, snap.obb.corners);
    assert_eq!(building.connected_road, Some(road));
    assert_eq!(building.height, snap.height);
    assert!(building.door_pos.y < building.obb.center().y);
    assert_eq!(
        snap_special_building(&map, cursor, proto.size).unwrap_err(),
        PlacementError::Occupied,
    );
}

#[test]
fn coal_plant_reports_short_frontage_and_fits_on_a_longer_street() {
    let test = TestCtx::new_fast();
    let short = connect(
        &test,
        vec3(100.0, 100.0, 0.0),
        vec3(200.0, 100.0, 0.0),
        &street(),
    );
    let long = connect(
        &test,
        vec3(100.0, 300.0, 0.0),
        vec3(400.0, 300.0, 0.0),
        &street(),
    );
    let map = test.g.map();
    let size = Size2D::new(165.0, 165.0);
    let error = snap_special_building(&map, vec3(150.0, 88.0, 0.0), size).unwrap_err();
    let PlacementError::InsufficientFrontage {
        required_m,
        available_m,
    } = error
    else {
        panic!("expected insufficient frontage, got {error:?}");
    };
    assert!((required_m - 165.0).abs() < 0.01);
    assert!(
        (available_m
            - map
                .get(short)
                .unwrap()
                .interfaced_points()
                .flatten()
                .length())
        .abs()
            < 0.01
    );
    assert!((available_m - 82.0).abs() < 0.05);

    let snap = snap_special_building(&map, vec3(250.0, 312.0, 0.0), size).unwrap();
    assert_eq!(snap.connected_road, long);
    assert!(map
        .spatial_map()
        .query(snap.obb, ProjectFilter::ROAD)
        .next()
        .is_none());
}

#[test]
fn rectangular_footprint_clamps_along_the_road_and_clears_both_curbs() {
    let test = TestCtx::new_fast();
    let road = connect(
        &test,
        vec3(100.0, 100.0, 0.0),
        vec3(200.0, 100.0, 0.0),
        &street(),
    );
    let map = test.g.map();
    let road = map.get(road).unwrap();
    let size = Size2D::new(80.0, 40.0);
    for side in [-1.0, 1.0] {
        let snap =
            snap_special_building(&map, vec3(104.0, 100.0 + side * 10.0, 0.0), size).unwrap();
        assert_eq!(snap.connected_road, road.id);
        let start = road.interfaced_points().first().x;
        assert!((snap.obb.center().x - (start + 20.0)).abs() < 0.01);
        let near_edge = snap
            .obb
            .corners
            .iter()
            .map(|p| (p.y - 100.0) * side)
            .reduce(f32::min)
            .unwrap();
        assert!((near_edge - (road.width * 0.5 + 0.25)).abs() < 0.01);
        assert!(map
            .spatial_map()
            .query(snap.obb, ProjectFilter::ROAD)
            .next()
            .is_none());
    }
}

#[test]
fn sidewalk_must_be_on_the_cursor_side() {
    let test = TestCtx::new_fast();
    let mut pattern = street();
    pattern
        .lanes_backward
        .retain(|&(kind, _)| kind != LaneKind::Walking);
    let road = connect(
        &test,
        vec3(100.0, 100.0, 0.0),
        vec3(200.0, 100.0, 0.0),
        &pattern,
    );
    let map = test.g.map();
    let size = Size2D::new(80.0, 40.0);
    assert_eq!(
        snap_special_building(&map, vec3(150.0, 90.0, 0.0), size)
            .unwrap()
            .connected_road,
        road,
    );
    assert_eq!(
        snap_special_building(&map, vec3(150.0, 110.0, 0.0), size).unwrap_err(),
        PlacementError::NoSidewalk,
    );
}

#[test]
fn intersection_inside_the_footprint_blocks_placement() {
    let test = TestCtx::new_fast();
    connect(
        &test,
        vec3(100.0, 100.0, 0.0),
        vec3(200.0, 100.0, 0.0),
        &street(),
    );
    test.g.map_mut().add_intersection(vec3(150.0, 155.0, 0.0));
    assert_eq!(
        snap_special_building(
            &test.g.map(),
            vec3(150.0, 110.0, 0.0),
            Size2D::new(80.0, 80.0)
        )
        .unwrap_err(),
        PlacementError::IntersectionOverlap,
    );
}

#[test]
fn road_candidates_break_distance_ties_by_id_and_skip_blocked_roads() {
    let test = TestCtx::new_fast();
    let a = connect(
        &test,
        vec3(100.0, 100.0, 0.0),
        vec3(300.0, 100.0, 0.0),
        &street(),
    );
    let b = connect(
        &test,
        vec3(100.0, 140.0, 0.0),
        vec3(300.0, 140.0, 0.0),
        &street(),
    );
    let cursor = vec3(200.0, 120.0, 0.0);
    let size = Size2D::new(12.0, 40.0);
    let snap = snap_special_building(&test.g.map(), cursor, size).unwrap();
    assert_eq!(snap.connected_road, a.min(b));

    let blocked = snap.obb.center().z(0.0);
    test.g.map_mut().add_intersection(blocked);
    let fallback = snap_special_building(&test.g.map(), cursor, size).unwrap();
    assert_eq!(fallback.connected_road, a.max(b));
}

#[test]
fn road_bending_into_its_own_footprint_blocks_placement() {
    let test = TestCtx::new_fast();
    let mut map = test.g.map_mut();
    let from = map.project(vec3(100.0, 120.0, 0.0), 0.0, ProjectFilter::INTER);
    let to = map.project(vec3(320.0, 120.0, 0.0), 0.0, ProjectFilter::INTER);
    let (_, id) = map
        .make_connection(from, to, Some(vec2(210.0, 330.0)), &street())
        .unwrap();
    let road = map.get(id).unwrap();
    let usable = road.interfaced_points().flatten();
    let midpoint = usable.point_along(usable.length() * 0.5);
    let size = Size2D::new(80.0, 80.0);
    assert_eq!(
        snap_special_building(&map, (midpoint - vec2(0.0, 12.0)).z(0.0), size).unwrap_err(),
        PlacementError::Occupied,
    );
    let outer = snap_special_building(&map, (midpoint + vec2(0.0, 12.0)).z(0.0), size).unwrap();
    assert_eq!(outer.connected_road, id);
    assert!(map
        .spatial_map()
        .query(outer.obb, ProjectFilter::ROAD)
        .next()
        .is_none());
}

#[test]
fn slopes_use_horizontal_frontage() {
    let test = TestCtx::new_fast();
    let id = connect(
        &test,
        vec3(100.0, 100.0, 10.0),
        vec3(200.0, 100.0, 26.0),
        &street(),
    );
    let map = test.g.map();
    let points = map.get(id).unwrap().interfaced_points();
    let available = points.flatten().length();
    assert!(points.length() > available + 0.5);
    let required = (available + points.length()) * 0.5;
    let error = snap_special_building(&map, vec3(150.0, 110.0, 18.0), Size2D::new(40.0, required))
        .unwrap_err();
    assert_eq!(
        error,
        PlacementError::InsufficientFrontage {
            required_m: required,
            available_m: available,
        }
    );
}
