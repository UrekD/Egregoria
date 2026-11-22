use crate::gui::follow::FollowEntity;
use crate::gui::{InspectedBuilding, InspectedEntity, InspectedRoad, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use geom::{Ray3, Vec2, Vec3, AABB};
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
    let hit = |road: &Road| road_hit(road, ray, limit).map(|depth| (depth, road.id));
    let nearest =
        |a: &(f32, RoadID), b: &(f32, RoadID)| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1));
    let picked = if let Some(pos) = terrain {
        let from = ray.from.xy();
        let to = pos.xy();
        let bounds = AABB::new_ll_ur(from.min(to), from.max(to)).expand(0.5);
        map.spatial_map()
            .query(bounds, ProjectFilter::ROAD)
            .filter_map(|kind| match kind {
                ProjectKind::Road(id) => map.get(id),
                _ => None,
            })
            .filter_map(hit)
            .min_by(nearest)
    } else {
        // A ray can still hit an elevated deck when it misses the terrain.
        map.roads().values().filter_map(hit).min_by(nearest)
    };
    picked.map(|(_, id)| id)
}

fn road_hit(road: &Road, ray: Ray3, limit: f32) -> Option<f32> {
    // Each centerline segment defines a sloping strip with the width of the deck.
    road.interfaced_points()
        .segments()
        .filter_map(|segment| {
            let delta = segment.dst - segment.src;
            let length = delta.xy().mag();
            if length <= f32::EPSILON {
                return None;
            }
            let axis = delta.xy() / length;
            let grade = delta.z / length;
            let origin = ray.from.xy() - segment.src.xy();
            let vertical = ray.dir.z - grade * ray.dir.xy().dot(axis);
            if vertical.abs() <= f32::EPSILON {
                return None;
            }
            let depth = (segment.src.z + grade * origin.dot(axis) - ray.from.z) / vertical;
            if !(0.0..=limit).contains(&depth) {
                return None;
            }
            let at = origin + ray.dir.xy() * depth;
            ((0.0..=length).contains(&at.dot(axis))
                && at.dot(axis.perpendicular()).abs() <= road.width * 0.5)
                .then_some(depth)
        })
        .min_by(f32::total_cmp)
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
