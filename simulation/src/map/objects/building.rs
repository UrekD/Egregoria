use crate::map::procgen::{gen_exterior_farm, gen_exterior_house, ColoredMesh};
use crate::map::{
    Buildings, ElectricityCache, Environment, LanePattern, RoadID, Roads, SpatialMap,
};
use egui_inspect::debug_inspect_impl;
use geom::{Color, Polygon, Shape, Vec2, Vec3, AABB, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID, GoodsCompanyID};
use serde::{Deserialize, Serialize};
use slotmapd::new_key_type;

new_key_type! {
    pub struct BuildingID;
}

debug_inspect_impl!(BuildingID);

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BuildingKind {
    House,
    GoodsCompany(GoodsCompanyID),
    RailFreightStation(FreightStationPrototypeID),
    TrainStation,
    ExternalTrading,
}

impl BuildingKind {
    pub fn as_goods_company(&self) -> Option<GoodsCompanyID> {
        match self {
            BuildingKind::GoodsCompany(id) => Some(*id),
            _ => None,
        }
    }

    pub fn is_cached_in_bkinds(&self) -> bool {
        matches!(self, BuildingKind::ExternalTrading)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StraightRoadGen {
    pub from: Vec3,
    pub to: Vec3,
    pub pattern: LanePattern,
}

pub const MAX_ZONE_AREA: f32 = 50000.0; // in m²
pub const MAX_ZONE_PERIMETER: f32 = 3000.0;

// Keep each independently indexed shape bounded before it reaches the 50 m grid.
pub(crate) fn valid_building_index_bounds(bounds: AABB) -> bool {
    const MAX_SPAN: f32 = MAX_ZONE_PERIMETER * 0.5;
    const MAX_COORD: f32 = 512.0 * crate::NewGameOptions::MAX_TERRAIN_CHUNKS as f32 + MAX_SPAN;
    bounds.ll.is_finite()
        && bounds.ur.is_finite()
        && bounds.ll.x >= -MAX_SPAN
        && bounds.ll.y >= -MAX_SPAN
        && bounds.ur.x <= MAX_COORD
        && bounds.ur.y <= MAX_COORD
        && bounds.size().x <= MAX_SPAN
        && bounds.size().y <= MAX_SPAN
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub poly: Polygon,
    pub area: f32,
    #[serde(default = "unit_x")]
    pub filldir: Vec2,
}

fn unit_x() -> Vec2 {
    Vec2::X
}

impl Zone {
    pub fn new(p: Polygon, filldir: Vec2) -> Self {
        Self {
            area: p.area(),
            poly: p,
            filldir: Self::normalize_fill_direction(filldir).unwrap_or(filldir),
        }
    }

    pub fn unit_fill_direction(&self) -> Option<Vec2> {
        Self::normalize_fill_direction(self.filldir)
    }

    fn normalize_fill_direction(dir: Vec2) -> Option<Vec2> {
        let scale = dir.x.abs().max(dir.y.abs());
        if !dir.is_finite() || scale < f32::MIN_POSITIVE {
            return None;
        }
        // Scale first so squaring a large or small component stays representable.
        // Divide component-wise because Vec2 division multiplies by the reciprocal.
        Some(Vec2::new(dir.x / scale, dir.y / scale).normalize())
    }

    pub(crate) fn normalized(mut self) -> Option<Self> {
        self.area = self.validated_area()?;
        self.filldir = self.unit_fill_direction()?;
        Some(self)
    }

    pub(crate) fn validated_area(&self) -> Option<f32> {
        let area = self.poly.area();
        let perimeter = self.poly.perimeter();
        (self.poly.len() >= 3
            && self.unit_fill_direction().is_some()
            && self.poly.iter().all(|point| point.is_finite())
            && area > 0.0
            && area <= MAX_ZONE_AREA
            && perimeter <= MAX_ZONE_PERIMETER
            && valid_building_index_bounds(self.poly.bbox()))
        .then_some(area)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Building {
    pub id: BuildingID,
    pub door_pos: Vec3,
    pub kind: BuildingKind,
    pub mesh: ColoredMesh,
    pub obb: OBB,
    pub height: f32,
    pub zone: Option<Zone>,
    pub connected_road: Option<RoadID>,
}

impl Building {
    pub fn is_orphaned(&self) -> bool {
        self.connected_road.is_none()
    }

    pub(crate) fn valid_footprint(obb: &OBB, gen: BuildingGen) -> bool {
        obb.corners.iter().all(|corner| corner.is_finite())
            && valid_building_index_bounds(obb.bbox())
            && obb
                .axis()
                .iter()
                .all(|axis| axis.mag2() > 0.0 && axis.mag2().is_finite())
            && match gen {
                BuildingGen::CenteredDoor { vertical_factor } => vertical_factor.is_finite(),
                BuildingGen::NoWalkway { door_pos } => door_pos.is_finite(),
                BuildingGen::House | BuildingGen::Farm => true,
            }
    }

    pub(crate) fn door_position(obb: &OBB, gen: BuildingGen, env: &Environment) -> Vec3 {
        let at = Self::origin(obb, env);
        let (_, _, door_pos) = Self::geometry(obb, gen, at);
        door_pos
    }

    fn origin(obb: &OBB, env: &Environment) -> Vec3 {
        let center = obb.center();
        center.z(env.height(center).unwrap_or(0.0))
    }

    fn geometry(obb: &OBB, gen: BuildingGen, at: Vec3) -> (ColoredMesh, Vec2, Vec3) {
        let [frontage, depth] = obb.axis();
        let axis = frontage.normalize();
        let frontage = frontage.mag();
        let seed = || common::rand::rand2(at.x, at.y).to_bits() as u64;
        let (mesh, door) = match gen {
            BuildingGen::House => gen_exterior_house(frontage, seed()),
            BuildingGen::Farm => Self::farm_geometry(frontage, depth.mag(), seed()),
            BuildingGen::CenteredDoor { vertical_factor } => (
                Default::default(),
                Vec2::y(-vertical_factor * 0.5 * depth.mag()),
            ),
            BuildingGen::NoWalkway { door_pos } => (Default::default(), door_pos),
        };
        (mesh, axis, door.rotated_by(axis).z0() + at + Vec3::z(0.1))
    }

    fn farm_geometry(frontage: f32, depth: f32, seed: u64) -> (ColoredMesh, Vec2) {
        let (mut mesh, door) = gen_exterior_farm(frontage, seed);
        let bounds = mesh.bbox();
        let size = bounds.size();
        let setback = 3.0_f32.min(depth * 0.5);
        let scale = (frontage / size.x).min((depth - setback) / size.y).min(1.0);

        // Retain the generated frontage position where it fits, and anchor
        // the exterior to the lot's road-facing edge using its actual depth.
        let x = bounds
            .ll
            .x
            .min(frontage * 0.5 - size.x * scale)
            .max(-frontage * 0.5);
        let offset = Vec2::new(x, setback - depth * 0.5) - bounds.ll * scale;
        for (face, _) in &mut mesh.faces {
            for vertex in face {
                *vertex = *vertex * scale + offset.z0();
            }
        }
        (mesh, door * scale + offset)
    }

    pub(crate) fn dry_footprint(env: &Environment, obb: &OBB, zone: Option<&Zone>) -> bool {
        !env.has_water_configuration()
            || (!env.intersects_water(&Polygon(obb.corners.to_vec()))
                && zone.is_none_or(|zone| !env.intersects_water(&zone.poly)))
    }

    pub fn make(
        buildings: &mut Buildings,
        spatial_map: &mut SpatialMap,
        electricity: &mut ElectricityCache,
        roads: &mut Roads,
        env: &Environment,
        obb: OBB,
        kind: BuildingKind,
        gen: BuildingGen,
        zone: Option<Zone>,
        connected_road: Option<RoadID>,
    ) -> Option<BuildingID> {
        if !Self::valid_footprint(&obb, gen)
            || connected_road.is_some_and(|road| !roads.contains_key(road))
            || zone
                .as_ref()
                .is_some_and(|zone| zone.validated_area().is_none())
            || !Self::dry_footprint(env, &obb, zone.as_ref())
        {
            return None;
        }

        let at = Self::origin(&obb, env);
        let (mut mesh, axis, door_pos) = Self::geometry(&obb, gen, at);

        for (poly, _) in &mut mesh.faces {
            for v in poly {
                *v = v.rotate_z(axis) + at;
            }
        }
        if let BuildingGen::House | BuildingGen::Farm | BuildingGen::CenteredDoor { .. } = gen {
            let bot = obb.segments()[0];
            let rpos = bot.project(door_pos.xy()).z(door_pos.z);
            let dir = bot.vec().normalize().z(0.0);

            let walkway = vec![
                rpos + dir * 1.5,
                rpos - dir * 1.5,
                door_pos - dir * 1.5,
                door_pos + dir * 1.5,
            ];

            mesh.faces.push((walkway, Color::gray(0.4).into()));
        }

        let b = buildings.insert_with_key(move |id| {
            electricity.add_object(id);
            if let Some(r) = connected_road {
                electricity.add_edge(id, r);

                roads[r].connected_buildings.push(id);
            }

            Self {
                id,
                mesh,
                kind,
                door_pos,
                obb,
                height: at.z,
                zone,
                connected_road,
            }
        });

        spatial_map.insert(&buildings[b]);

        Some(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangular_farm_exteriors_follow_the_front_edge_and_fit_the_lot() {
        let _test = crate::tests::TestCtx::new_fast();
        let at = Vec3::new(100.0, 120.0, 0.0);
        let front = Vec2::new(0.6, 0.8);
        for (frontage, depth) in [(80.0, 16.0), (24.0, 80.0)] {
            let obb = OBB::new(at.xy(), front, depth, frontage);
            let (mesh, axis, door) = Building::geometry(&obb, BuildingGen::Farm, at);
            let bounds = mesh.bbox();
            assert!(!mesh.faces.is_empty());
            assert!((bounds.ll.y + depth * 0.5 - 3.0).abs() < 0.001);
            assert!(bounds.ll.x >= -frontage * 0.5 - 0.001);
            assert!(bounds.ur.x <= frontage * 0.5 + 0.001);
            assert!(bounds.ur.y <= depth * 0.5 + 0.001);

            let door = (door - at).xy();
            assert!(door.dot(axis).abs() <= frontage * 0.5 + 0.001);
            assert!(door.dot(axis.perpendicular()).abs() <= depth * 0.5 + 0.001);
        }
    }
}
