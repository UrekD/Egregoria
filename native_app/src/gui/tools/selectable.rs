use crate::gui::follow::FollowEntity;
use crate::gui::{InspectedBuilding, InspectedEntity, InspectedRoad, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use engine::Tesselator;
use geom::{Intersect, Ray3, Segment, Vec2, Vec3};
use simulation::map::{Map, ProjectFilter, ProjectKind, Road, RoadID};
use simulation::{AnyEntity, Simulation};

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
            if let Some(unproj) = inp.unprojected {
                let map = sim.map();
                inspected_b.e = map
                    .spatial_map()
                    .query(unproj.xy(), ProjectFilter::BUILDING)
                    .find_map(|x| x.as_building());
            }
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
            triangle_hit(ray, point(0), point(1), point(2))
                .filter(|depth| (0.0..=limit).contains(depth))
        })
        .min_by(f32::total_cmp)
}

fn triangle_hit(ray: Ray3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let ab = b - a;
    let ac = c - a;
    let cross = ray.dir.cross(ac);
    let det = ab.dot(cross);
    if det.abs() <= f32::EPSILON {
        return None;
    }
    let origin = ray.from - a;
    let u = origin.dot(cross) / det;
    let side = origin.cross(ab);
    let v = ray.dir.dot(side) / det;
    (u >= 0.0 && v >= 0.0 && u + v <= 1.0).then(|| ac.dot(side) / det)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec3;
    use scenarios::ScenarioCtx;
    use simulation::map::{LanePatternBuilder, MapProject};
    use simulation::transportation::{spawn_driving_vehicle, VehicleKind};
    use simulation::SimulationOptions;

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
                .map(|p| map.environment.height(p.xy()).unwrap())
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
