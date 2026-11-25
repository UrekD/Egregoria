use crate::gui::follow::FollowEntity;
use crate::gui::{InspectedBuilding, InspectedEntity, InspectedRoad, Tool};
use crate::inputmap::{InputAction, InputMap, SurfaceHits};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use engine::Tesselator;
use geom::{Intersect, LinearColor, Ray3, Segment, Shape, Vec2, Vec3, AABB};
use simulation::map::{Building, BuildingID, Map, ProjectFilter, ProjectKind, Road, RoadID};
use simulation::{AnyEntity, Simulation};
use std::collections::BTreeSet;
use std::ops::RangeInclusive;

pub fn select_radius(id: AnyEntity) -> f32 {
    match id {
        AnyEntity::VehicleID(_) => 5.0,
        AnyEntity::TrainID(_) => 10.0,
        AnyEntity::WagonID(_) => 10.0,
        AnyEntity::FreightStationID(_) => 0.0,
        AnyEntity::CompanyID(_) => 0.0,
        AnyEntity::HumanID(_) => 3.0,
    }
}

/// Select entities, buildings and road segments with the hand tool.
pub fn selectable(sim: &Simulation, uiworld: &UiWorld) {
    profiling::scope!("gui::selectable");
    let mut inspected = uiworld.write::<InspectedEntity>();
    let mut inspected_b = uiworld.write::<InspectedBuilding>();
    let mut inspected_road = uiworld.write::<InspectedRoad>();
    let inp = uiworld.read::<InputMap>();
    let tool = uiworld.read::<Tool>();
    let clicked = *tool == Tool::Hand && inp.just_act.contains(&InputAction::Select);

    if clicked && !inspected.dontclear {
        inspected.dist2 = f32::INFINITY;
        inspected.e = None;
        if let Some(unproj) = inp.unprojected {
            sim.world()
                .query_selectable_pos()
                .for_each(|(id, pos): (AnyEntity, Vec2)| {
                    let dist2 = (pos - unproj.xy()).mag2();
                    let rad = select_radius(id);
                    if dist2 >= rad * rad || dist2 >= inspected.dist2 {
                        return;
                    }
                    inspected.dist2 = dist2;
                    inspected.e = Some(id);
                });
        }
    }

    if clicked && !inspected_b.dontclear {
        inspected_b.e = None;
        if inspected.e.is_none() {
            let map = sim.map();
            inspected_b.e = inp
                .ray
                .and_then(|ray| pick_growable(&map, ray, inp.surface_hits(map.environment())))
                .or_else(|| {
                    map.spatial_map()
                        .query(inp.unprojected?.xy(), ProjectFilter::BUILDING)
                        .filter_map(|project| project.as_building())
                        .find(|&id| map.get(id).is_some_and(|b| b.development().is_none()))
                });
        }
    }
    if clicked && !inspected.dontclear && !inspected_b.dontclear {
        inspected_road.0 = None;
        if inspected.e.is_none() && inspected_b.e.is_none() {
            if let Some(ray) = inp.ray {
                inspected_road.0 = pick_road(&sim.map(), ray, inp.unprojected);
                if inspected_road.0.is_some() {
                    uiworld.write::<FollowEntity>().0 = None;
                }
            }
        }
    }
    inspected.dontclear = false;
    inspected_b.dontclear = false;

    if let Some(e) = inspected.e {
        if !sim.world().contains(e) {
            inspected.e = None;
        }
    }

    if let Some(b) = inspected_b.e {
        if !sim.map().buildings().contains_key(b) {
            inspected_b.e = None;
        }
    }

    if matches!(*tool, Tool::Bulldozer) {
        inspected.e = None;
        inspected_b.e = None;
    }
    if *tool != Tool::Hand {
        inspected_road.0 = None;
    }
    if let Some(id) = inspected_road.0 {
        let map = sim.map();
        let Some(road) = map.get(id) else {
            inspected_road.0 = None;
            return;
        };
        if inspected.e.is_none() && inspected_b.e.is_none() {
            let points: Vec<_> = road
                .interfaced_points()
                .iter()
                .map(|point| point.up(0.7))
                .collect();
            uiworld
                .write::<ImmediateDraw>()
                .polyline(points, (road.width * 0.28).clamp(1.2, 6.0), false)
                .color(simulation::colors().gui_primary);
        }
    }
}

// The orbit camera's maximum normal eye offset is about 1.7 km.
const GROWABLE_PICK_RANGE_M: f32 = 4096.0;

fn pick_growable(map: &Map, ray: Ray3, hits: SurfaceHits) -> Option<BuildingID> {
    if !ray.from.is_finite() || !ray.dir.is_finite() {
        return None;
    }
    let length = ray.dir.mag();
    if !length.is_finite() || length == 0.0 {
        return None;
    }
    let ray = Ray3::new(ray.from, ray.dir / length);
    let (start, end) = visible_pick_depth(map.environment().bounds(), ray, hits)?;

    let first = (ray.from + start * ray.dir).xy();
    let last = (ray.from + end * ray.dir).xy();
    let footprint_ray = Segment::new(first, last);
    let candidates: BTreeSet<_> = pick_query_bounds(footprint_ray)
        .flat_map(|bounds| map.spatial_map().query(bounds, ProjectFilter::BUILDING))
        .filter_map(|project| project.as_building())
        .collect();
    candidates
        .into_iter()
        .filter_map(|id| {
            let building = map.get(id)?;
            if !building.obb.intersects(&footprint_ray) {
                return None;
            }
            PreparedShell::new(building)?
                .ray_depth(ray, start..=end)
                .map(|depth| (depth, id))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, id)| id)
}

fn pick_query_bounds(segment: Segment) -> impl Iterator<Item = AABB> {
    // One spatial-grid cell width per query. Each box touches a fixed number
    // of cells, so diagonal work grows with the length of the segment.
    const STEP_M: f32 = 50.0;
    let steps = (segment.src.distance(segment.dst) / STEP_M).ceil().max(1.0) as u32;
    (0..steps).map(move |step| {
        let first = segment.src.lerp(segment.dst, step as f32 / steps as f32);
        let last = segment
            .src
            .lerp(segment.dst, (step + 1) as f32 / steps as f32);
        Segment::new(first, last).bbox()
    })
}

fn visible_pick_depth(bounds: AABB, ray: Ray3, hits: SurfaceHits) -> Option<(f32, f32)> {
    let limit = hits.nearest().map_or(GROWABLE_PICK_RANGE_M, |point| {
        ((point - ray.from).dot(ray.dir)).min(GROWABLE_PICK_RANGE_M)
    });
    clip_pick_ray(ray, bounds, limit)
}

fn clip_pick_ray(ray: Ray3, bounds: AABB, limit: f32) -> Option<(f32, f32)> {
    let mut start = 0.0_f32;
    let mut end = limit;
    for (origin, speed, low, high) in [
        (ray.from.x, ray.dir.x, bounds.ll.x, bounds.ur.x),
        (ray.from.y, ray.dir.y, bounds.ll.y, bounds.ur.y),
    ] {
        if speed == 0.0 {
            if !(low..=high).contains(&origin) {
                return None;
            }
        } else {
            let a = (low - origin) / speed;
            let b = (high - origin) / speed;
            start = start.max(a.min(b));
            end = end.min(a.max(b));
        }
    }
    (start <= end).then_some((start, end))
}

const MAX_SHELL_VERTICES: usize = 1_536;
const MAX_SHELL_INDICES: usize = 2_304;

/// The bounded, world-space surface view shared by the renderer and hand tool.
pub(crate) struct PreparedShell<'a> {
    building: &'a Building,
    pub(crate) panel_budget: usize,
}

impl<'a> PreparedShell<'a> {
    pub(crate) fn new(building: &'a Building) -> Option<Self> {
        building.development()?;
        if !building.height.is_finite() || !building.door_pos.is_finite() {
            return None;
        }
        let mut shell = Self {
            building,
            panel_budget: 0,
        };
        let mut vertices = 0;
        let mut indices = 0;
        for (face, color) in shell.raw_faces() {
            if !(3..=MAX_SHELL_VERTICES).contains(&face.len()) {
                return None;
            }
            vertices += face.len();
            indices += 3 * (face.len() - 2);
            if vertices > MAX_SHELL_VERTICES
                || indices > MAX_SHELL_INDICES
                || !face.iter().all(|p| p.is_finite())
                || !<[f32; 4]>::from(color).into_iter().all(f32::is_finite)
            {
                return None;
            }
        }
        if vertices == 0 {
            return None;
        }
        shell.panel_budget =
            ((MAX_SHELL_VERTICES - vertices) / 4).min((MAX_SHELL_INDICES - indices) / 6);
        Some(shell)
    }

    fn raw_faces(&self) -> impl Iterator<Item = (&[Vec3], LinearColor)> {
        self.building
            .mesh
            .faces
            .iter()
            .map(|(face, color)| (face.as_slice(), *color))
    }

    pub(crate) fn base_z(&self) -> f32 {
        self.building.height
    }

    pub(crate) fn door(&self) -> Vec3 {
        self.building.door_pos
    }

    pub(crate) fn faces(&self) -> impl Iterator<Item = (&[Vec3], Vec3, LinearColor)> {
        self.raw_faces().filter_map(|(face, color)| {
            let normal = face_fan(face.len()).find_map(|[a, b, c]| {
                (face[b] - face[a])
                    .cross(face[c] - face[a])
                    .try_normalize()
                    .filter(|normal| normal.is_finite() && normal.mag2() > 0.0)
            })?;
            Some((face, normal, color))
        })
    }

    fn ray_depth(&self, ray: Ray3, depth: RangeInclusive<f32>) -> Option<f32> {
        self.ray_hit(ray, depth).map(|(hit, _)| hit)
    }

    fn ray_hit(&self, ray: Ray3, depth: RangeInclusive<f32>) -> Option<(f32, Vec3)> {
        self.faces()
            .flat_map(|(face, normal, _)| {
                face_fan(face.len()).map(move |tri| (tri.map(|index| face[index]), normal))
            })
            .filter_map(|([a, b, c], normal)| {
                triangle_hit(ray, a, b, c, true).map(|hit| (hit, normal))
            })
            .filter(|(hit, _)| depth.contains(hit))
            .min_by(|(a, _), (b, _)| a.total_cmp(b))
    }
}

/// Verify the camera ray used by the scripted tower selection.
pub(crate) fn tower_pick_geometry(
    map: &Map,
    ray: Ray3,
    selected: &Building,
    background: Option<&Building>,
) -> bool {
    let hits = SurfaceHits::cast(map.environment(), ray);
    let Some((start, end)) = visible_pick_depth(map.environment().bounds(), ray, hits) else {
        return false;
    };
    let Some((depth, normal)) =
        PreparedShell::new(selected).and_then(|shell| shell.ray_hit(ray, start..=end))
    else {
        return false;
    };
    match background {
        Some(building) => {
            normal.z > 0.95
                && PreparedShell::new(building)
                    .and_then(|shell| shell.ray_depth(ray, start..=end))
                    .is_some_and(|hit| hit > depth)
        }
        None => hits.nearest().is_none() && normal.z.abs() < 0.1,
    }
}

pub(crate) fn face_fan(vertices: usize) -> impl Iterator<Item = [usize; 3]> {
    (1..vertices.saturating_sub(1)).map(|i| [0, i, i + 1])
}

fn pick_road(map: &Map, ray: Ray3, terrain: Option<Vec3>) -> Option<RoadID> {
    let limit = terrain.map_or(f32::INFINITY, |pos| {
        (pos - ray.from).dot(ray.dir) / ray.dir.mag2()
    });
    let ray_xy = terrain.map(|_| Segment::new(ray.from.xy(), (ray.from + ray.dir * limit).xy()));
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut mesh = Tesselator::new(&mut vertices, &mut indices, None, 1.0);

    map.roads()
        .values()
        .filter(|road| {
            // The tessellator caps a join's miter at ten times the half-width.
            // The spatial index uses the centerline and plain half-width.
            ray_xy.is_none_or(|ray| {
                road.interfaced_points()
                    .bbox()
                    .flatten()
                    .expand(5.0 * road.width)
                    .intersects(&ray)
            })
        })
        .filter_map(|road| road_hit(road, ray, limit, &mut mesh).map(|depth| (depth, road.id)))
        .min_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
        .map(|(_, id)| id)
}

fn road_hit(road: &Road, ray: Ray3, limit: f32, mesh: &mut Tesselator<'_>) -> Option<f32> {
    let points = road.interfaced_points();
    let first = points.first_dir()?.xy();
    let last = points.last_dir()?.xy();
    // Use the renderer's tessellator so the triangles follow the deck's mitered joins and grade.
    mesh.vertices.clear();
    mesh.indices.clear();
    mesh.draw_polyline_with_dir(points.as_slice(), first, last, road.width);

    mesh.indices
        .chunks_exact(3)
        .filter_map(|indices| {
            let point = |i: usize| Vec3::from(mesh.vertices[indices[i] as usize].position);
            triangle_hit(ray, point(0), point(1), point(2), false)
                .filter(|depth| (0.0..=limit).contains(depth))
        })
        .min_by(f32::total_cmp)
}

fn triangle_hit(ray: Ray3, a: Vec3, b: Vec3, c: Vec3, cull_backface: bool) -> Option<f32> {
    let ab = b - a;
    let ac = c - a;
    let cross = ray.dir.cross(ac);
    let det = ab.dot(cross);
    // The lit mesh pipeline treats counter-clockwise winding as front-facing.
    let facing = if cull_backface { det } else { det.abs() };
    if facing <= f32::EPSILON {
        return None;
    }
    let origin = ray.from - a;
    let u = origin.dot(cross) / det;
    let side = origin.cross(ab);
    let v = ray.dir.dot(side) / det;
    (u >= 0.0 && v >= 0.0 && u + v <= 1.0).then(|| ac.dot(side) / det)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use geom::vec3;
    use scenarios::ScenarioCtx;
    use simulation::map::{LanePatternBuilder, MapProject};
    use simulation::transportation::{spawn_driving_vehicle, VehicleKind};
    use simulation::SimulationOptions;

    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, LinearColor, OBB};
    use simulation::map::{
        procgen::ColoredMesh, Building, BuildingKind, Buildings, Density, Environment,
        GrowableDevelopment, GrowableSiteState, Intersections, Lanes, LotOrigin, Lots,
        ParkingSpots, Roads, ZoneCell, ZoneKind,
    };
    use std::collections::BTreeMap;

    // Exercise deserialized shell faces with the live map's rebuilt spatial index.
    pub(crate) fn profiled_house(
        id: BuildingID,
        obb: OBB,
        mesh: ColoredMesh,
        height: f32,
        roof_z: f32,
    ) -> Building {
        let development = GrowableDevelopment {
            zone: ZoneCell {
                kind: ZoneKind::Residential,
                density: Density::High,
            },
            level: 1,
            floors: 10,
            roof_z,
            approved_value_band: 0,
            frozen_shell: None,
            home: None,
            planned_jobs: 0,
            site: GrowableSiteState::Active,
            business: None,
        };
        let door = ((obb.corners[0] + obb.corners[1]) * 0.5).z(height);
        Bincode::decode(
            &Bincode::encode(&(
                id,
                door,
                BuildingKind::House,
                mesh,
                obb,
                height,
                None::<simulation::map::Zone>,
                None::<RoadID>,
                Some(development),
            ))
            .unwrap(),
        )
        .unwrap()
    }

    fn map_with_buildings(environment: Environment, buildings: Buildings) -> Map {
        #[derive(serde::Serialize)]
        struct MapSnapshot {
            roads: Roads,
            intersections: Intersections,
            buildings: Buildings,
            lanes: Lanes,
            parking: ParkingSpots,
            lots: Lots,
            environment: Environment,
            external_train_stations: Vec<BuildingID>,
        }
        let snapshot = MapSnapshot {
            roads: Default::default(),
            intersections: Default::default(),
            buildings,
            lanes: Default::default(),
            parking: Default::default(),
            lots: Default::default(),
            environment,
            external_train_stations: Vec::new(),
        };
        Bincode::decode(
            &Bincode::encode(&(snapshot, BTreeMap::<BuildingID, LotOrigin>::new())).unwrap(),
        )
        .unwrap()
    }

    fn tower(id: BuildingID, x: f32, roof_z: f32) -> Building {
        let footprint = OBB::from(&AABB::new_ll_ur(vec2(x, 90.0), vec2(x + 20.0, 110.0)));
        let ground = footprint.corners.map(|p| p.z(-4.0));
        let roof = footprint.corners.map(|p| p.z(roof_z));
        let mut faces = vec![(roof.to_vec(), LinearColor::WHITE)];
        for i in 0..4 {
            let j = (i + 1) % 4;
            faces.push((
                vec![ground[i], ground[j], roof[j], roof[i]],
                LinearColor::WHITE,
            ));
        }
        profiled_house(id, footprint, ColoredMesh { faces }, -4.0, roof_z)
    }

    fn dry_environment() -> Environment {
        let mut env = Environment::new(1, 1);
        env.terrain_apply(env.bounds(), |_| -4.0);
        env
    }

    fn click_building(sim: &Simulation, ray: Ray3, input_map: &Map) -> Option<BuildingID> {
        crate::init::init();
        let ui = UiWorld::with_defaults();
        let mut input = ui.write::<InputMap>();
        input.just_act.insert(InputAction::Select);
        input.set_world_ray(Some(ray), input_map, false);
        drop(input);
        selectable(sim, &ui);
        let selected = ui.read::<InspectedBuilding>().e;
        selected
    }

    #[test]
    fn zones2_native_hand_tool_ignores_exit_faces_and_orders_sky_hits() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let mut buildings = Buildings::default();
        let far = buildings.insert_with_key(|id| tower(id, 150.0, 80.0));
        let near = buildings.insert_with_key(|id| tower(id, 50.0, 60.0));
        ctx.sim
            .insert(map_with_buildings(dry_environment(), buildings));
        let map = ctx.sim.map();
        let direction = vec3(1.0, 0.02, 0.1).normalize();
        let sky = Ray3::new(vec3(-10.0, 98.0, 25.0), direction);
        assert!(map.environment().raycast(sky).is_none());
        assert_eq!(click_building(&ctx.sim, sky, &map), Some(near));
        let inside = Ray3::new(vec3(60.0, 100.0, 30.0), direction);
        assert_eq!(click_building(&ctx.sim, inside, &map), Some(far));
    }

    #[test]
    fn zones2_native_hand_tool_orders_tied_roof_hits_by_id() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let mut buildings = Buildings::default();
        let first = buildings.insert_with_key(|id| tower(id, 50.0, 60.0));
        let second = buildings.insert_with_key(|id| tower(id, 55.0, 60.0));
        ctx.sim
            .insert(map_with_buildings(dry_environment(), buildings));
        let map = ctx.sim.map();
        let roof = Ray3::new(vec3(62.0, 100.0, 75.0), -Vec3::Z);
        assert_eq!(
            click_building(&ctx.sim, roof, &map),
            Some(first.min(second))
        );
    }

    #[test]
    fn zones2_native_hand_tool_skips_rejected_shells() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let mut buildings = Buildings::default();
        buildings.insert_with_key(|id| {
            let mut b = tower(id, 50.0, 60.0);
            b.mesh.faces.push((vec![Vec3::ZERO], LinearColor::WHITE));
            b
        });
        let far = buildings.insert_with_key(|id| tower(id, 150.0, 70.0));
        ctx.sim
            .insert(map_with_buildings(dry_environment(), buildings));
        let map = ctx.sim.map();
        let sky = Ray3::new(vec3(0.0, 100.0, 25.0), Vec3::X);
        assert_eq!(click_building(&ctx.sim, sky, &map), Some(far));
    }

    #[test]
    fn zones2_native_hand_tool_uses_the_supplied_pointer_depth() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let mut buildings = Buildings::default();
        let target = buildings.insert_with_key(|id| tower(id, 60.0, -2.0));
        ctx.sim
            .insert(map_with_buildings(dry_environment(), buildings));
        let dry = ctx.sim.map();
        let mut wet_env = dry_environment();
        wet_env.set_sea_level(Some(0)).unwrap();
        let wet = map_with_buildings(wet_env, Buildings::default());
        let ray = Ray3::new(vec3(70.0, 100.0, 20.0), -Vec3::Z);

        assert_eq!(click_building(&ctx.sim, ray, &dry), Some(target));
        assert_eq!(click_building(&ctx.sim, ray, &wet), None);
    }

    #[test]
    fn zones2_native_pick_queries_follow_the_diagonal() {
        let diagonal = Segment::new(Vec2::ZERO, vec2(2896.0, 2896.0));
        let bounds: Vec<_> = pick_query_bounds(diagonal).collect();
        assert!(!bounds.iter().any(|aabb| aabb.contains(vec2(30.0, 2800.0))));
        assert!(bounds
            .iter()
            .all(|aabb| (aabb.ur.x - aabb.ll.x).max(aabb.ur.y - aabb.ll.y) <= 51.0));
        for i in 0..=100 {
            let point = diagonal.src.lerp(diagonal.dst, i as f32 / 100.0);
            assert!(bounds.iter().any(|aabb| aabb.contains(point)));
        }
    }

    #[test]
    fn zones2_native_pick_segment_clips_vertical_and_world_edge_rays() {
        let bounds = geom::AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(512.0));
        let edge = Ray3::new(vec3(-10.0, 512.0, 40.0), Vec3::X);
        assert_eq!(clip_pick_ray(edge, bounds, 200.0), Some((10.0, 200.0)));
        assert_eq!(clip_pick_ray(edge, bounds, 9.0), None);
        let vertical = Ray3::new(vec3(25.0, 25.0, 80.0), -Vec3::Z);
        assert_eq!(clip_pick_ray(vertical, bounds, 35.0), Some((0.0, 35.0)));
        assert_eq!(
            clip_pick_ray(Ray3::new(edge.from, Vec3::Y), bounds, 200.0),
            None
        );
    }

    #[test]
    fn zones2_native_pick_depth_stops_at_the_nearer_water_surface() {
        let mut env = Environment::new(1, 1);
        env.terrain_apply(env.bounds(), |_| -4.0);
        let ray = Ray3::new(vec3(250.0, 250.0, 20.0), -Vec3::Z);
        let (_, dry_depth) =
            visible_pick_depth(env.bounds(), ray, SurfaceHits::cast(&env, ray)).unwrap();
        assert!(dry_depth > 21.0);
        env.set_sea_level(Some(0)).unwrap();
        assert_eq!(
            visible_pick_depth(env.bounds(), ray, SurfaceHits::cast(&env, ray)),
            Some((0.0, 20.0))
        );
    }

    #[test]
    fn u5_road_click_hits_the_outside_of_a_mitered_bend() {
        use geom::PolyLine3;

        let ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            ..SimulationOptions::LEGACY
        });
        let mut map = ctx.sim.write::<Map>();
        let from = vec3(1400.0, 1400.0, 25.0);
        let elbow = from + vec3(100.0, 0.0, 5.0);
        let to = elbow + vec3(0.0, 100.0, 3.0);
        let endpoint = |pos| MapProject {
            pos,
            kind: ProjectKind::Ground,
        };
        let (_, id) = map
            .make_connection(
                endpoint(from),
                endpoint(to),
                None,
                &LanePatternBuilder::default().build(),
            )
            .unwrap();
        let mut road = map.get(id).unwrap().clone();
        road.interfaced_points = PolyLine3::new(vec![from, elbow, to]);
        let outside = elbow + vec3(1.0, -1.0, 0.0) * (road.width * 0.4);
        let ray = Ray3::new(outside.up(50.0), -Vec3::Z);
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut mesh = Tesselator::new(&mut vertices, &mut indices, None, 1.0);
        assert!(road_hit(&road, ray, 100.0, &mut mesh).is_some());
    }

    #[test]
    fn u5_road_click_uses_the_visible_deck_and_preserves_entity_priority() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            ..SimulationOptions::LEGACY
        });
        crate::init::init();
        let ui = UiWorld::with_defaults();
        let (upper, center) = {
            let mut map = ctx.sim.write::<Map>();
            let from = vec3(1400.0, 1400.0, 0.0);
            let to = vec3(1580.0, 1400.0, 0.0);
            let ground = [from, to]
                .iter()
                .map(|p| map.environment().height(p.xy()).unwrap())
                .reduce(f32::max)
                .unwrap();
            let mut road_at = |height| {
                let endpoint = |p: Vec3| MapProject {
                    pos: p.xy().z(height),
                    kind: ProjectKind::Ground,
                };
                map.make_connection(
                    endpoint(from),
                    endpoint(to),
                    None,
                    &LanePatternBuilder::default().build(),
                )
                .unwrap()
                .1
            };
            road_at(ground + 8.0);
            let upper = road_at(ground + 25.0);
            let points = map.get(upper).unwrap().interfaced_points();
            (upper, points.point_along(points.length() * 0.5))
        };
        let ground = center.up(-25.0);
        let click = |sim: &Simulation, ray, terrain| {
            let mut input = ui.write::<InputMap>();
            input.just_act.insert(InputAction::Select);
            input.ray = Some(ray);
            input.unprojected = Some(terrain);
            drop(input);
            selectable(sim, &ui);
        };
        let vertical = Ray3::new(center.up(100.0), -Vec3::Z);
        click(&ctx.sim, vertical, ground);
        assert_eq!(ui.read::<InspectedRoad>().0, Some(upper));

        crate::session::close_modal(&ui);
        assert_eq!(ui.read::<InspectedRoad>().0, None);

        let origin = center + vec3(0.0, -90.0, 90.0);
        let oblique = Ray3::new(origin, (center - origin).normalize());
        let offset_ground = center + vec3(0.0, 25.0, -25.0);
        click(&ctx.sim, oblique, offset_ground);
        assert_eq!(ui.read::<InspectedRoad>().0, Some(upper));
        *ui.write::<Tool>() = Tool::Bulldozer;
        selectable(&ctx.sim, &ui);
        assert_eq!(ui.read::<InspectedRoad>().0, None);
        *ui.write::<Tool>() = Tool::Hand;

        let car =
            spawn_driving_vehicle(&mut ctx.sim, VehicleKind::Car, geom::Transform::new(ground))
                .unwrap();
        click(&ctx.sim, vertical, ground);
        assert_eq!(ui.read::<InspectedEntity>().e, Some(car.into()));
        assert_eq!(ui.read::<InspectedRoad>().0, None);

        click(&ctx.sim, oblique, offset_ground);
        assert_eq!(ui.read::<InspectedRoad>().0, Some(upper));
        ctx.sim.write::<Map>().remove_road(upper).unwrap();
        ui.write::<InputMap>().just_act.clear();
        selectable(&ctx.sim, &ui);
        assert_eq!(ui.read::<InspectedRoad>().0, None);
    }
}
