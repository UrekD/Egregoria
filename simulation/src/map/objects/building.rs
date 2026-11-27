use crate::map::import::growable_shell::FrozenGrowableShell;
use crate::map::procgen::{gen_exterior_farm, gen_exterior_house, ColoredMesh};
use crate::map::{
    Buildings, ElectricityCache, Environment, LanePattern, RoadID, Roads, SpatialMap, ZoneCell,
    ZoneKind,
};
use egui_inspect::debug_inspect_impl;
use geom::{Color, Polygon, Shape, Vec2, Vec3, AABB, OBB};
use prototypes::{
    BuildingGen, FreightStationPrototypeID, GoodsCompanyID, GrowableCompanyID, GrowableZoneKind,
    ResidencePrototypeID, MAX_PROFILED_SLOTS,
};
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
    SiteRemnant,
}

impl BuildingKind {
    pub fn as_goods_company(&self) -> Option<GoodsCompanyID> {
        match self {
            BuildingKind::GoodsCompany(id) => Some(*id),
            _ => None,
        }
    }

    pub(crate) fn is_mixed_growable(self) -> bool {
        self.as_goods_company().is_some_and(|company| {
            prototypes::try_prototypes().is_some()
                && prototypes::try_prototype(GrowableCompanyID::from(company)).is_some_and(
                    |growable| growable.chain_key().zone_kind == GrowableZoneKind::MixedUse,
                )
        })
    }

    fn allows_raw_placement(self) -> bool {
        self != Self::SiteRemnant && self.as_goods_company().is_none_or(|company| {
            prototypes::try_prototypes().is_some()
                && prototypes::try_prototype(company).is_some()
                && !self.is_mixed_growable()
        })
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
    #[serde(default = "Zone::default_fill_direction")]
    pub filldir: Vec2,
}

impl Zone {
    pub(crate) fn default_fill_direction() -> Vec2 {
        Vec2::X
    }

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GrowableSiteState {
    Active,
    Abandoned { since_elapsed_tick: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GrowableBusinessState {
    AwaitingSpawn,
    Open,
    ClosedUntil { retry_elapsed_tick: u64 },
}

impl GrowableBusinessState {
    pub fn can_open(self, elapsed_tick: u64) -> bool {
        match self {
            Self::AwaitingSpawn => true,
            Self::Open => false,
            Self::ClosedUntil { retry_elapsed_tick } => elapsed_tick >= retry_elapsed_tick,
        }
    }
}

/// Latched development and role state of a checked, profiled parcel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GrowableDevelopment {
    pub zone: ZoneCell,
    pub level: u8,
    pub floors: u8,
    pub roof_z: f32,
    pub approved_value_band: u8,
    pub home: Option<ResidencePrototypeID>,
    pub planned_jobs: u32,
    pub site: GrowableSiteState,
    pub business: Option<GrowableBusinessState>,
    #[serde(default)]
    pub frozen_shell: Option<FrozenGrowableShell>,
}

impl GrowableDevelopment {
    fn valid_job_slots(&self) -> bool {
        match (self.zone.kind, self.business) {
            (ZoneKind::Residential, None) => self.planned_jobs == 0,
            (ZoneKind::MixedUse, Some(_)) => self.planned_jobs == 12,
            (ZoneKind::Commercial | ZoneKind::Industrial | ZoneKind::Office, Some(_)) => {
                (1..=MAX_PROFILED_SLOTS).contains(&self.planned_jobs)
            }
            _ => false,
        }
    }
}

/// World-space appearance prepared by parcel fit before map insertion.
pub(crate) struct PreparedGrowableAppearance {
    pub development: GrowableDevelopment,
    pub base_z: f32,
    pub door: Vec3,
    pub mesh: ColoredMesh,
}

impl PreparedGrowableAppearance {
    pub(in crate::map) fn is_valid(&self, parcel: &OBB) -> bool {
        self.development.valid_job_slots()
            && self.base_z.is_finite()
            && self.development.roof_z.is_finite()
            && self.development.roof_z > self.base_z
            && self.door.is_finite()
            && parcel.contains(self.door.xy())
            && self.development.frozen_shell.as_ref().is_none_or(|shell| {
                shell.valid_for(parcel)
                    && shell.fit().base_z() == self.base_z
                    && shell.fit().door() == self.door
            })
            && self
                .mesh
                .faces
                .iter()
                .flat_map(|(vertices, _)| vertices)
                .all(|v| v.is_finite())
    }
}

/// A checked building without a building ID or external indexes.
pub(in crate::map) struct PreparedBuilding(Building);

impl PreparedBuilding {
    pub(in crate::map) fn connected_road(&self) -> Option<RoadID> {
        self.0.connected_road
    }

    pub(in crate::map) fn door(&self) -> Vec3 {
        self.0.door_pos
    }

    pub(in crate::map) fn base_height(&self) -> f32 {
        self.0.height
    }

    pub(in crate::map) fn insert(
        self,
        buildings: &mut Buildings,
        spatial_map: &mut SpatialMap,
        electricity: &mut ElectricityCache,
        roads: &mut Roads,
    ) -> BuildingID {
        let mut building = self.0;
        let id = buildings.insert_with_key(|id| {
            building.id = id;
            building
        });
        electricity.add_object(id);
        let building = &buildings[id];
        if let Some(road) = building.connected_road {
            electricity.add_edge(id, road);
            roads[road].connected_buildings.push(id);
        }
        spatial_map.insert(building);
        id
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Building {
    pub id: BuildingID,
    pub door_pos: Vec3,
    pub kind: BuildingKind,
    pub mesh: ColoredMesh,
    pub obb: OBB,
    pub height: f32,
    pub zone: Option<Zone>,
    pub connected_road: Option<RoadID>,
    #[serde(default)]
    pub(in crate::map) development: Option<GrowableDevelopment>,
}

impl Serialize for Building {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let empty_mesh = ColoredMesh::default();
        let mesh = if self
            .development()
            .is_some_and(|site| site.frozen_shell.is_some())
        {
            &empty_mesh
        } else {
            &self.mesh
        };
        let mut building = serializer.serialize_struct("Building", 9)?;
        building.serialize_field("id", &self.id)?;
        building.serialize_field("door_pos", &self.door_pos)?;
        building.serialize_field("kind", &self.kind)?;
        building.serialize_field("mesh", mesh)?;
        building.serialize_field("obb", &self.obb)?;
        building.serialize_field("height", &self.height)?;
        building.serialize_field("zone", &self.zone)?;
        building.serialize_field("connected_road", &self.connected_road)?;
        building.serialize_field("development", &self.development)?;
        building.end()
    }
}

impl Building {
    pub fn development(&self) -> Option<&GrowableDevelopment> {
        self.development.as_ref()
    }

    pub fn has_residential_use(&self) -> bool {
        self.kind != BuildingKind::SiteRemnant
            && (self.kind == BuildingKind::House
                || self.development().is_some_and(|site| site.home.is_some()))
    }

    pub fn is_mixed_use(&self) -> bool {
        self.development()
            .is_some_and(|site| site.zone.kind == ZoneKind::MixedUse)
    }

    pub fn is_abandoned_growable(&self) -> bool {
        self.development()
            .is_some_and(|site| matches!(site.site, GrowableSiteState::Abandoned { .. }))
    }

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
        if connected_road.is_some_and(|road| !roads.contains_key(road))
            || !Self::valid_footprint(&obb, gen)
            || zone
                .as_ref()
                .is_some_and(|zone| zone.validated_area().is_none())
            || !Self::dry_footprint(env, &obb, zone.as_ref())
        {
            return None;
        }
        let prepared = Self::prepare(env, obb, kind, gen, zone, connected_road, None)?;
        Some(prepared.insert(buildings, spatial_map, electricity, roads))
    }

    /// Build geometry for an already checked footprint.
    pub(in crate::map) fn prepare(
        env: &Environment,
        obb: OBB,
        kind: BuildingKind,
        gen: BuildingGen,
        zone: Option<Zone>,
        connected_road: Option<RoadID>,
        appearance: Option<PreparedGrowableAppearance>,
    ) -> Option<PreparedBuilding> {
        let (mesh, door_pos, height, development) = match appearance {
            Some(appearance) if appearance.is_valid(&obb) => (
                appearance.mesh,
                appearance.door,
                appearance.base_z,
                Some(appearance.development),
            ),
            Some(_) => return None,
            None if !kind.allows_raw_placement() => return None,
            None => {
                let (mesh, door, height) = Self::legacy_appearance(&obb, gen, env);
                (mesh, door, height, None)
            }
        };

        Some(PreparedBuilding(Self {
            id: BuildingID::default(),
            mesh,
            kind,
            door_pos,
            obb,
            height,
            zone,
            connected_road,
            development,
        }))
    }

    fn legacy_appearance(
        obb: &OBB,
        gen: BuildingGen,
        env: &Environment,
    ) -> (ColoredMesh, Vec3, f32) {
        let at = Self::origin(obb, env);
        let (mut mesh, axis, door_pos) = Self::geometry(obb, gen, at);

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

        (mesh, door_pos, at.z)
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
