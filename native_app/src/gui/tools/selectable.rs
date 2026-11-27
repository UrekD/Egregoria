use crate::gui::follow::FollowEntity;
use crate::gui::{InspectedBuilding, InspectedEntity, InspectedRoad, Tool};
use crate::inputmap::{InputAction, InputMap, SurfaceHits};
use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::world_snapshot::{ActorKind, ViewStamp};
use crate::uiworld::UiWorld;
use common::{FastMap, FastSet};
use engine::{GfxContext, Tesselator};
use flat_spatial::Grid;
use geom::{
    vec3, Camera, Intersect, LinearColor, Ray3, Segment, Shape, Transform, Vec2, Vec3, AABB, AABB3,
};
use prototypes::{
    try_prototype, FreightStationPrototype, GoodsCompanyPrototype, GrowableCompanyID,
    GrowableCompanyPrototype, RenderAsset, RollingStockID, RollingStockPrototype, ServiceID,
};
use simulation::map::{
    Building, BuildingID, BuildingKind, Buildings, Map, ProjectFilter, ProjectKind, RenderMap,
    Road, RoadID, SpatialMap,
};
use simulation::transportation::VehicleKind;
use simulation::{AnyEntity, Simulation, VehicleEnt, VehicleID, WagonEnt, WagonID};
use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;

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
        let mut picker = uiworld.write::<VehiclePicker>();
        inspected.e = picker
            .hand_hit
            .take()
            .unwrap_or_else(|| picker.pick(sim, &inp))
            .filter(|hit| hit.valid_in(sim))
            .map(|hit| hit.entity);
        drop(picker);
        if let Some(unproj) = inp.unprojected.filter(|_| inspected.e.is_none()) {
            for (id, human) in &sim.world().humans {
                let dist2 = human.trans.pos.xy().distance2(unproj.xy());
                let radius = select_radius(id.into());
                if dist2 < radius * radius && dist2 < inspected.dist2 {
                    inspected.dist2 = dist2;
                    inspected.e = Some(id.into());
                }
            }
        }
    }

    if clicked && !inspected_b.dontclear {
        inspected_b.e = None;
        if inspected.e.is_none() {
            let map = sim.map();
            inspected_b.e = inp
                .ray
                .and_then(|ray| {
                    let picker = uiworld.read::<VehiclePicker>();
                    let ray = normalize_pick_ray(ray)?;
                    let (start, end) = visible_pick_depth(
                        map.environment().bounds(),
                        ray,
                        inp.surface_hits(map.environment()),
                    )?;
                    picker
                        .building_before(&map, ray, start, end)
                        .map(|(_, id)| id)
                })
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

#[cfg(test)]
thread_local! {
    pub(crate) static VEHICLE_SCAN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Copy)]
pub(crate) struct VehicleHit {
    pub(crate) entity: AnyEntity,
    part: AnyEntity,
}

impl VehicleHit {
    pub(crate) fn valid_in(self, sim: &Simulation) -> bool {
        match self.part {
            AnyEntity::VehicleID(id) => sim.get(id).is_some(),
            AnyEntity::WagonID(id) => sim.get(id).is_some_and(|wagon| {
                let leader = wagon.itfollower.leader;
                AnyEntity::from(leader) == self.entity && sim.get(leader).is_some()
            }),
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DisplayedVehicle {
    pub part: AnyEntity,
    pub entity: AnyEntity,
    pub kind: ActorKind,
    pub transform: Transform,
}

pub(crate) struct VehiclePicker {
    presented: Option<(ViewStamp, Camera, Arc<RenderMap>)>,
    displayed: Vec<DisplayedVehicle>,
    pub(crate) hand_hit: Option<Option<VehicleHit>>,
    snapshot: Option<(u64, usize, usize)>,
    grid: Grid<AnyEntity, Vec2>,
    radius: f32,
    building_meshes: FastMap<BuildingKind, PickMesh>,
    prefab_radius: f32,
    road_bounds: [Option<AABB3>; 3],
    rail_bounds: FastMap<RollingStockID, AABB3>,
}

impl Default for VehiclePicker {
    fn default() -> Self {
        Self {
            presented: None,
            displayed: Vec::new(),
            hand_hit: None,
            snapshot: None,
            grid: Grid::new(64),
            radius: 0.0,
            building_meshes: FastMap::default(),
            prefab_radius: 0.0,
            road_bounds: [None; 3],
            rail_bounds: FastMap::default(),
        }
    }
}

impl VehiclePicker {
    pub(crate) fn reset(uiworld: &UiWorld, _generation: u64) {
        let mut picker = uiworld.write::<Self>();
        picker.presented = None;
        picker.displayed.clear();
        picker.hand_hit = None;
        picker.snapshot = None;
        drop(picker.grid.clear());
        picker.radius = 0.0;
    }

    pub(crate) fn record_displayed(
        &mut self,
        stamp: ViewStamp,
        camera: Camera,
        map: Arc<RenderMap>,
    ) -> &mut Vec<DisplayedVehicle> {
        self.presented = Some((stamp, camera, map));
        self.displayed.clear();
        &mut self.displayed
    }

    pub(crate) fn displayed_ray(&self, screen: Vec2, generation: u64) -> Option<Ray3> {
        let (stamp, camera, _) = self.presented.as_ref()?;
        if stamp.generation != generation {
            return None;
        }
        camera.unproj_ray(screen)
    }

    pub(crate) fn pick_displayed(&self, ray: Ray3, generation: u64) -> Option<VehicleHit> {
        let (stamp, _, map) = self.presented.as_ref()?;
        if stamp.generation != generation {
            return None;
        }
        self.pick_displayed_in(map, ray)
    }

    fn pick_displayed_in(&self, map: &RenderMap, ray: Ray3) -> Option<VehicleHit> {
        let ray = normalize_pick_ray(ray)?;
        let env = map.environment();
        let (start, end) = visible_pick_depth(env.bounds(), ray, SurfaceHits::cast(env, ray))?;
        let query = ray_segment(ray, start, end).bbox();
        let (hit, depth) = self
            .displayed
            .iter()
            .filter_map(|part| {
                let bounds = match part.kind {
                    ActorKind::Car => self.road_bounds(VehicleKind::Car),
                    ActorKind::Truck => self.road_bounds(VehicleKind::Truck),
                    ActorKind::Bus => self.road_bounds(VehicleKind::Bus),
                    ActorKind::RollingStock(stock) => *self.rail_bounds.get(&stock)?,
                    ActorKind::Pedestrian => return None,
                };
                if !query
                    .expand(pick_radius(bounds, part.transform))
                    .contains(part.transform.pos.xy())
                {
                    return None;
                }
                let depth = instance_depth(ray, part.transform, bounds)?;
                (depth <= end).then_some((
                    VehicleHit {
                        entity: part.entity,
                        part: part.part,
                    },
                    depth,
                ))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))?;
        self.building_before_in(map.spatial_map(), map.buildings(), ray, start, depth)
            .is_none()
            .then_some(hit)
    }

    pub(crate) fn load_models(&mut self, gfx: &mut GfxContext) {
        fn bounds(gfx: &mut GfxContext, path: &Path) -> Option<AABB3> {
            let mesh = gfx.mesh(path).ok()?;
            mesh.lods.first().map(|lod| lod.aabb3)
        }

        let car = bounds(gfx, "simple_car.glb".as_ref());
        let truck = bounds(gfx, "truck.glb".as_ref());
        self.road_bounds = [
            car,
            truck,
            truck.map(|b| AABB3::new(b.ll * 1.5, b.ur * 1.5)),
        ];
        self.load_pick_meshes(Path::new("assets"));
    }

    fn load_pick_meshes(&mut self, assets: &Path) {
        self.rail_bounds = RollingStockPrototype::iter()
            .filter_map(|stock| {
                let RenderAsset::Mesh { path } = &stock.asset else {
                    return None;
                };
                Some((stock.id, PickMesh::load(assets, path)?.bounds))
            })
            .collect();

        let kinds = GoodsCompanyPrototype::iter()
            .filter(|proto| legacy_growable(BuildingKind::GoodsCompany(proto.id)).is_none())
            .map(|proto| (BuildingKind::GoodsCompany(proto.id), &proto.asset))
            .chain(
                FreightStationPrototype::iter()
                    .map(|proto| (BuildingKind::RailFreightStation(proto.id), &proto.asset)),
            );
        self.building_meshes = kinds
            .filter_map(|(kind, asset)| {
                let RenderAsset::Mesh { path } = asset else {
                    return None;
                };
                Some((kind, PickMesh::load(assets, path)?))
            })
            .collect();
        if let Some(mesh) = PickMesh::load(assets, Path::new("external_trading.glb")) {
            self.building_meshes
                .insert(BuildingKind::ExternalTrading, mesh);
        }
        self.prefab_radius = self
            .building_meshes
            .values()
            .map(|mesh| mesh.bounds.ll.abs().max(mesh.bounds.ur.abs()).xy().mag())
            .fold(0.0, f32::max);
    }

    fn road_bounds(&self, kind: VehicleKind) -> AABB3 {
        let (index, length, width, height) = match kind {
            VehicleKind::Car => (0, 2.8, 1.1, 1.9),
            VehicleKind::Truck => (1, 4.0, 1.4, 3.7),
            VehicleKind::Bus => (2, 6.0, 2.1, 5.6),
        };
        self.road_bounds[index]
            .unwrap_or_else(|| AABB3::new(vec3(-length, -width, 0.0), vec3(length, width, height)))
    }

    fn snapshot_key(sim: &Simulation) -> (u64, usize, usize) {
        let world = sim.world();
        (sim.get_tick(), world.vehicles.len(), world.wagons.len())
    }

    pub(crate) fn start_snapshot(&mut self, sim: &Simulation) -> bool {
        let snapshot = Self::snapshot_key(sim);
        if self.snapshot == Some(snapshot) {
            return false;
        }
        self.snapshot = Some(snapshot);
        drop(self.grid.clear());
        self.radius = 0.0;
        true
    }

    pub(crate) fn index_road(&mut self, id: VehicleID, vehicle: &VehicleEnt) {
        let bounds = self.road_bounds(vehicle.vehicle.kind);
        self.radius = self.radius.max(pick_radius(bounds, vehicle.trans));
        self.grid.insert(vehicle.trans.pos.xy(), id.into());
    }

    pub(crate) fn index_wagon(&mut self, id: WagonID, wagon: &WagonEnt) {
        let Some(&bounds) = self.rail_bounds.get(&wagon.wagon.rolling_stock) else {
            return;
        };
        self.radius = self.radius.max(pick_radius(bounds, wagon.trans));
        self.grid.insert(wagon.trans.pos.xy(), id.into());
    }

    pub(crate) fn pick(&mut self, sim: &Simulation, input: &InputMap) -> Option<VehicleHit> {
        if let Some((_, _, map)) = &self.presented {
            return self
                .pick_displayed_in(map, input.ray?)
                .filter(|hit| hit.valid_in(sim));
        }
        let map = sim.map();
        let ray = PickRay::from_input(&map, input)?;
        if self.start_snapshot(sim) {
            #[cfg(test)]
            VEHICLE_SCAN_COUNT.with(|count| count.set(count.get() + 1));
            let world = sim.world();
            for (id, vehicle) in &world.vehicles {
                self.index_road(id, vehicle);
            }
            for (id, wagon) in &world.wagons {
                self.index_wagon(id, wagon);
            }
        }
        self.pick_indexed(sim, &map, ray)
    }

    pub(crate) fn pick_indexed(
        &self,
        sim: &Simulation,
        map: &Map,
        PickRay { ray, start, end }: PickRay,
    ) -> Option<VehicleHit> {
        if self.snapshot != Some(Self::snapshot_key(sim)) {
            return None;
        }
        let segment = ray_segment(ray, start, end);
        let mut visited = FastSet::default();
        let (hit, depth) = pick_query_bounds(segment, self.radius)
            .flat_map(|query| self.grid.query_aabb(query.ll, query.ur))
            .filter(|(handle, _)| visited.insert(*handle))
            .filter_map(|(handle, _)| self.grid.get(handle))
            .filter_map(|(_, &part)| self.part_depth(sim, ray, part))
            .filter(|(_, depth)| *depth <= end)
            .min_by(|a, b| a.1.total_cmp(&b.1))?;
        self.building_before(map, ray, start, depth)
            .is_none()
            .then_some(hit)
    }

    #[cfg(test)]
    pub(crate) fn still_visible(
        &self,
        sim: &Simulation,
        map: &Map,
        PickRay { ray, start, end }: PickRay,
        hit: VehicleHit,
    ) -> bool {
        self.part_depth(sim, ray, hit.part)
            .is_some_and(|(found, depth)| {
                found.entity == hit.entity
                    && depth <= end
                    && self.building_before(map, ray, start, depth).is_none()
            })
    }

    fn part_depth(
        &self,
        sim: &Simulation,
        ray: Ray3,
        part: AnyEntity,
    ) -> Option<(VehicleHit, f32)> {
        let (entity, depth) = match part {
            AnyEntity::VehicleID(id) => {
                if sim.car_stored_inside_garage(id) {
                    return None;
                }
                let vehicle = sim.get(id)?;
                let bounds = self.road_bounds(vehicle.vehicle.kind);
                (part, instance_depth(ray, vehicle.trans, bounds)?)
            }
            AnyEntity::WagonID(id) => {
                let wagon = sim.get(id)?;
                let leader = wagon.itfollower.leader;
                sim.get(leader)?;
                let bounds = *self.rail_bounds.get(&wagon.wagon.rolling_stock)?;
                (leader.into(), instance_depth(ray, wagon.trans, bounds)?)
            }
            _ => return None,
        };
        Some((VehicleHit { entity, part }, depth))
    }

    fn building_before(
        &self,
        map: &Map,
        ray: Ray3,
        start: f32,
        end: f32,
    ) -> Option<(f32, BuildingID)> {
        self.building_before_in(map.spatial_map(), map.buildings(), ray, start, end)
    }

    fn building_before_in(
        &self,
        spatial: &SpatialMap,
        buildings: &Buildings,
        ray: Ray3,
        start: f32,
        end: f32,
    ) -> Option<(f32, BuildingID)> {
        if end < start {
            return None;
        }
        let segment = ray_segment(ray, start, end);
        // A prefab can reach beyond the footprint indexed by the map. Expand
        // by the largest model radius, then test each prefab in model space.
        let candidates: BTreeSet<_> = pick_query_bounds(segment, self.prefab_radius)
            .flat_map(|bounds| spatial.building_candidates(bounds))
            .collect();
        candidates
            .into_iter()
            .filter_map(|id| {
                let building = buildings.get(id)?;
                let model = self
                    .building_meshes
                    .get(&building.kind)
                    .filter(|_| building.development().is_none());
                if model.is_none() && !building.obb.intersects(&segment) {
                    return None;
                }
                building_depth(building, ray, start..=end, model).map(|depth| (depth, id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)))
    }
}

fn normalize_pick_ray(ray: Ray3) -> Option<Ray3> {
    let length = ray.dir.mag();
    (ray.from.is_finite() && length.is_finite() && length > 0.0)
        .then(|| Ray3::new(ray.from, ray.dir / length))
}

#[derive(Clone, Copy)]
pub(crate) struct PickRay {
    ray: Ray3,
    start: f32,
    end: f32,
}

impl PickRay {
    pub(crate) fn from_input(map: &Map, input: &InputMap) -> Option<Self> {
        let ray = normalize_pick_ray(input.ray?)?;
        let (start, end) = visible_pick_depth(
            map.environment().bounds(),
            ray,
            input.surface_hits(map.environment()),
        )?;
        Some(Self { ray, start, end })
    }
}

fn ray_segment(ray: Ray3, start: f32, end: f32) -> Segment {
    Segment::new(
        (ray.from + ray.dir * start).xy(),
        (ray.from + ray.dir * end).xy(),
    )
}

fn instance_ray(ray: Ray3, transform: Transform) -> Option<Ray3> {
    // Match the instanced mesh shader's local axes, grade and uniform scale.
    let scale = transform.dir.mag();
    let forward = transform.dir.try_normalize()?;
    let left = (-forward.perp_up()).try_normalize()?;
    let up = forward.cross(left);
    let local = |v: Vec3| vec3(v.dot(forward), v.dot(left), v.dot(up)) / scale;
    Some(Ray3::new(local(ray.from - transform.pos), local(ray.dir)))
}

fn instance_depth(ray: Ray3, transform: Transform, bounds: AABB3) -> Option<f32> {
    box_depth(instance_ray(ray, transform)?, bounds)
}

fn box_depth(ray: Ray3, bounds: AABB3) -> Option<f32> {
    // Slabs parallel to the ray still admit points lying exactly on the box.
    let mut start = 0.0_f32;
    let mut end = f32::INFINITY;
    for (origin, speed, low, high) in [
        (ray.from.x, ray.dir.x, bounds.ll.x, bounds.ur.x),
        (ray.from.y, ray.dir.y, bounds.ll.y, bounds.ur.y),
        (ray.from.z, ray.dir.z, bounds.ll.z, bounds.ur.z),
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
    (start <= end).then_some(start)
}

fn pick_radius(bounds: AABB3, transform: Transform) -> f32 {
    bounds.ll.abs().max(bounds.ur.abs()).mag() * transform.dir.mag()
}

fn legacy_growable(kind: BuildingKind) -> Option<&'static GrowableCompanyPrototype> {
    let BuildingKind::GoodsCompany(id) = kind else {
        return None;
    };
    try_prototype(GrowableCompanyID::from(id)).filter(|proto| proto.growable)
}

struct PickMesh {
    bounds: AABB3,
    triangles: Vec<[Vec3; 3]>,
}

impl PickMesh {
    fn load(assets: &Path, path: &Path) -> Option<Self> {
        use engine::meshload::{find_nodes, load_mesh_geometry};
        let (document, buffers) = load_mesh_geometry(assets, path).ok()?;
        let scene = document.default_scene()?;
        let mut triangles = Vec::new();
        // Both vehicle and prefab instances draw the first mesh LOD.
        for (node, lod, _, transform, _) in
            find_nodes(&scene, |id| document.nodes().nth(id).unwrap())
        {
            let Some(mesh) = node.mesh().filter(|_| lod == 0) else {
                continue;
            };
            for primitive in mesh.primitives() {
                primitive.material().index()?;
                let reader = primitive.reader(|b| Some(&buffers.get(b.index())?.0[..b.length()]));
                let (Some(positions), Some(normals), Some(uv), Some(indices)) = (
                    reader.read_positions(),
                    reader.read_normals(),
                    reader.read_tex_coords(0),
                    reader.read_indices(),
                ) else {
                    continue;
                };
                let vertices: Vec<_> = positions
                    .zip(normals)
                    .zip(uv.into_f32())
                    .map(|((point, _), _)| {
                        let point = transform * Vec3::from(point).w(1.0);
                        point.xyz() / point.w
                    })
                    .collect();
                let indices: Vec<_> = indices.into_u32().collect();
                triangles.extend(indices.chunks_exact(3).filter_map(|tri| {
                    Some([
                        *vertices.get(tri[0] as usize)?,
                        *vertices.get(tri[1] as usize)?,
                        *vertices.get(tri[2] as usize)?,
                    ])
                }));
            }
        }
        let first = triangles.first()?[0];
        let bounds = triangles
            .iter()
            .flatten()
            .copied()
            .fold(AABB3::new(first, first), AABB3::union_vec);
        Some(Self { bounds, triangles })
    }

    fn depth(&self, ray: Ray3, range: RangeInclusive<f32>) -> Option<f32> {
        if box_depth(ray, self.bounds)? > *range.end() {
            return None;
        }
        self.triangles
            .iter()
            .filter_map(|&[a, b, c]| triangle_hit(ray, a, b, c, true))
            .filter(|depth| range.contains(depth))
            .min_by(f32::total_cmp)
    }
}

fn building_depth(
    building: &Building,
    ray: Ray3,
    range: RangeInclusive<f32>,
    model: Option<&PickMesh>,
) -> Option<f32> {
    if building.development().is_some() {
        return PreparedShell::new(building)?.ray_depth(ray, range);
    }
    let center = building.obb.center().z(building.height);
    let transform = Transform::new_dir(center, building.obb.axis()[0].try_normalize()?.z0());
    if let Some(model) = model {
        return model.depth(instance_ray(ray, transform)?, range);
    }
    let growable = legacy_growable(building.kind);
    let has_shell = building
        .mesh
        .faces
        .iter()
        .flat_map(|(face, _)| face)
        .any(|point| point.z > building.height + 0.2);
    let flat_sprite = match building.kind {
        BuildingKind::GoodsCompany(id) => try_prototype(id).is_some_and(|proto| {
            matches!(proto.asset, RenderAsset::Sprite { .. })
                && proto.zone.is_none()
                && proto.storage.is_none()
                && try_prototype(ServiceID::from(id)).is_none()
        }),
        _ => false,
    };
    if growable.is_none() && (has_shell || flat_sprite) {
        return building
            .mesh
            .faces
            .iter()
            .flat_map(|(face, _)| face_fan(face.len()).map(move |tri| tri.map(|i| face[i])))
            .filter_map(|[a, b, c]| triangle_hit(ray, a, b, c, true))
            .filter(|depth| range.contains(depth))
            .min_by(f32::total_cmp);
    }

    // The legacy renderer uses this volume for growable and placeholder buildings.
    let [width, length] = building.obb.axis().map(Vec2::mag);
    let height = (width.min(length) * 0.45).clamp(3.0, 18.0)
        * (1.0 + f32::from(growable.map_or(0, |proto| proto.level.saturating_sub(1))) * 0.45);
    let bounds = AABB3::new(
        vec3(-width * 0.5, -length * 0.5, 0.0),
        vec3(width * 0.5, length * 0.5, height),
    );
    instance_depth(ray, transform, bounds).filter(|depth| range.contains(depth))
}

// The orbit camera's maximum normal eye offset is about 1.7 km.
const GROWABLE_PICK_RANGE_M: f32 = 4096.0;

fn pick_query_bounds(segment: Segment, padding: f32) -> impl Iterator<Item = AABB> {
    // Follow the ray in short boxes so diagonal work grows with ray length.
    // Wider boxes take longer steps to limit overlap between adjacent queries.
    let step_m = 50.0 + 2.0 * padding;
    let steps = (segment.src.distance(segment.dst) / step_m).ceil().max(1.0) as u32;
    (0..steps).map(move |step| {
        let first = segment.src.lerp(segment.dst, step as f32 / steps as f32);
        let last = segment
            .src
            .lerp(segment.dst, (step + 1) as f32 / steps as f32);
        Segment::new(first, last).bbox().expand(padding)
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

pub(crate) fn pick_road(map: &Map, ray: Ray3, terrain: Option<Vec3>) -> Option<RoadID> {
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

    fn rail_pick_fixture(stock: &[RollingStockID]) -> Simulation {
        use simulation::world_command::WorldCommand;

        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 2,
            ..SimulationOptions::LEGACY
        });
        let lane = {
            let mut map = ctx.sim.write::<Map>();
            let endpoint = |x| MapProject {
                pos: vec3(x, 100.0, 50.0),
                kind: ProjectKind::Ground,
            };
            let (_, road) = map
                .make_connection(
                    endpoint(60.0),
                    endpoint(360.0),
                    None,
                    &LanePatternBuilder {
                        rail: true,
                        ..Default::default()
                    }
                    .build(),
                )
                .unwrap();
            let (lane, _) = map
                .get(road)
                .unwrap()
                .lanes_iter()
                .find(|&(_, kind)| kind == simulation::map::LaneKind::Rail)
                .unwrap();
            lane
        };
        WorldCommand::SpawnTrain {
            wagons: stock.to_vec(),
            lane,
            dist: 180.0,
        }
        .apply(&mut ctx.sim);
        ctx.sim
    }

    fn test_assets() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../assets"))
    }

    #[test]
    fn client_max_crossing_vehicles_and_train_picking_use_displayed_poses() {
        let stock = ["locomotive", "freight-wagon"].map(RollingStockID::new);
        let mut sim = rail_pick_fixture(&stock);
        let (part, wagon) = sim
            .world()
            .wagons
            .iter()
            .find(|(_, wagon)| wagon.wagon.rolling_stock == stock[1])
            .unwrap();
        let train = wagon.itfollower.leader;
        let rail_pose = wagon.trans;
        let visible_road = vec3(80.0, 80.0, 100.0);
        let other_road = vec3(110.0, 80.0, 100.0);
        let car =
            spawn_driving_vehicle(&mut sim, VehicleKind::Car, Transform::new(other_road)).unwrap();
        let crossing =
            spawn_driving_vehicle(&mut sim, VehicleKind::Car, Transform::new(visible_road))
                .unwrap();
        sim.world_mut_unchecked().wagons[part].trans.pos.y += 60.0;
        let mut picker = VehiclePicker::default();
        picker.load_pick_meshes(test_assets());
        let stamp = ViewStamp {
            generation: 4,
            view_seq: 7,
            completed_step: 100,
            sample_time: std::time::Instant::now(),
            map_epoch: 2,
        };
        let map = Arc::new(sim.map().render_snapshot());
        let mut camera = Camera::new(visible_road, 800.0, 600.0);
        camera.update();
        picker.record_displayed(stamp, camera, map).extend([
            DisplayedVehicle {
                part: car.into(),
                entity: car.into(),
                kind: crate::rendering::world_snapshot::ActorKind::Car,
                transform: Transform::new(visible_road),
            },
            DisplayedVehicle {
                part: crossing.into(),
                entity: crossing.into(),
                kind: crate::rendering::world_snapshot::ActorKind::Car,
                transform: Transform::new(other_road),
            },
            DisplayedVehicle {
                part: part.into(),
                entity: train.into(),
                kind: crate::rendering::world_snapshot::ActorKind::RollingStock(stock[1]),
                transform: rail_pose,
            },
        ]);
        let (screen, _) = camera.project(visible_road);
        let ray = picker.displayed_ray(screen, stamp.generation).unwrap();
        assert!(picker.displayed_ray(screen, stamp.generation + 1).is_none());
        assert_eq!(
            picker
                .pick_displayed(ray, stamp.generation)
                .map(|hit| hit.entity),
            Some(car.into())
        );
        let mut input = InputMap::default();
        input.ray = Some(Ray3::new(visible_road.up(24.0), -Vec3::Z));
        let car_hit = picker.pick(&sim, &input).map(|hit| hit.entity);
        input.ray = Some(Ray3::new(rail_pose.pos.up(24.0), -Vec3::Z));
        let train_hit = picker.pick(&sim, &input).map(|hit| hit.entity);
        assert_eq!((car_hit, train_hit), (Some(car.into()), Some(train.into())));
        let mut hover = crate::debug_gui::VehicleHover::default();
        let now = std::time::Instant::now();
        hover.update_displayed(&picker, &sim, Some(ray), stamp.generation, now);
        assert!(hover.has_hit());
        sim.world_mut_unchecked().vehicles.remove(car);
        hover.update_displayed(&picker, &sim, Some(ray), stamp.generation, now);
        assert!(!hover.has_hit());
        assert!(picker.pick_displayed(ray, stamp.generation).is_some());
        assert!(!picker
            .pick_displayed(ray, stamp.generation)
            .unwrap()
            .valid_in(&sim));
    }

    #[test]
    fn vehicle_destination_pick_uses_rendered_rail_stock() {
        let stock =
            ["locomotive", "freight-wagon", "passenger-emu-middle"].map(RollingStockID::new);
        let sim = rail_pick_fixture(&stock);
        let train = sim.world().trains.keys().next().unwrap();
        let mut input = InputMap::default();
        let mut picker = VehiclePicker::default();
        picker.load_pick_meshes(test_assets());
        for (&stock, offset) in stock.iter().skip(1).zip([8.0, 13.5]) {
            let wagon = sim
                .world()
                .wagons
                .values()
                .find(|wagon| wagon.wagon.rolling_stock == stock)
                .unwrap();
            let end = wagon.trans.pos + wagon.trans.dir * offset;
            input.ray = Some(Ray3::new(end.up(18.0), -Vec3::Z));
            assert_eq!(
                picker.pick(&sim, &input).map(|hit| hit.entity),
                Some(train.into()),
                "{stock:?}"
            );
        }
    }

    fn assert_rail_end_pick(stock: &str, visible_end: Vec3) {
        let sim = rail_pick_fixture(&[RollingStockID::new(stock)]);
        let (part, wagon) = sim.world().wagons.iter().next().unwrap();
        let train = wagon.itfollower.leader;
        let center = wagon.trans.pos;
        let forward = wagon.trans.dir;
        let left = -forward.perp_up().normalize();
        let up = forward.cross(left);
        let world_end =
            center + forward * visible_end.x + left * visible_end.y + up * visible_end.z;
        let mut input = InputMap::default();
        input.ray = Some(Ray3::new(world_end + up * 18.0, -up));
        let mut picker = VehiclePicker::default();
        picker.load_pick_meshes(test_assets());
        assert_eq!(
            picker.pick(&sim, &input).map(|hit| (hit.entity, hit.part)),
            Some((train.into(), part.into())),
        );
    }

    #[test]
    fn vehicle_destination_round2_picks_the_rendered_locomotive_end() {
        assert_rail_end_pick("locomotive", vec3(8.6806, 0.4852, 0.5674));
    }

    #[test]
    fn vehicle_destination_round2_picks_the_rendered_freight_wagon_end() {
        assert_rail_end_pick("freight-wagon", vec3(8.6246, -0.4852, 0.5674));
    }

    #[test]
    fn vehicle_destination_round2_prefab_overhang_blocks_vehicle_picking() {
        use prototypes::BuildingGen;

        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let mut map = map_with_buildings(dry_environment(), Buildings::default());
        let center = vec2(100.0, 120.0);
        let footprint = OBB::new(center, Vec2::X, 100.0, 38.0);
        let kind = BuildingKind::ExternalTrading;
        let building = map
            .build_special_building(
                &footprint,
                kind,
                BuildingGen::NoWalkway {
                    door_pos: Vec2::ZERO,
                },
                None,
                None,
            )
            .unwrap();
        let overhang = center + footprint.axis()[0].normalize() * 40.0;
        assert!(!footprint.contains(overhang));
        let position = overhang.z(map.get(building).unwrap().height);
        ctx.sim.insert(map);
        let car = spawn_driving_vehicle(&mut ctx.sim, VehicleKind::Car, Transform::new(position))
            .unwrap();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        let mut input = ui.write::<InputMap>();
        input.just_act.insert(InputAction::Select);
        let map = ctx.sim.map();
        input.set_world_ray(Some(Ray3::new(position.up(24.0), -Vec3::Z)), &map, false);
        let mut picker = ui.write::<VehiclePicker>();
        assert_eq!(
            picker.pick(&ctx.sim, &input).map(|hit| hit.entity),
            Some(car.into())
        );
        picker.load_pick_meshes(test_assets());
        let local_ray = Ray3::new(vec3(40.0, 0.0, 24.0), -Vec3::Z);
        let roof = picker.building_meshes[&kind]
            .depth(local_ray, 0.0..=24.0)
            .unwrap();
        assert!((24.0 - roof - 4.6521).abs() < 0.01);
        assert!(picker.pick(&ctx.sim, &input).is_none());
        drop(picker);
        drop(input);
        selectable(&ctx.sim, &ui);
        assert_eq!(ui.read::<InspectedBuilding>().e, Some(building));
        assert_eq!(ui.read::<InspectedEntity>().e, None);
    }

    #[test]
    fn vehicle_destination_building_blocks_hover_and_hand_click() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        let mut buildings = Buildings::default();
        let building = buildings.insert_with_key(|id| tower(id, 50.0, 35.0));
        ctx.sim
            .insert(map_with_buildings(dry_environment(), buildings));
        let position = vec3(95.0, 100.0, -4.0);
        let car = spawn_driving_vehicle(
            &mut ctx.sim,
            VehicleKind::Car,
            geom::Transform::new(position),
        )
        .unwrap();
        let camera = vec3(15.0, 100.0, 28.0);
        let ray = Ray3::new(camera, (position.up(1.0) - camera).normalize());
        let map = ctx.sim.map();
        let mut input = InputMap::default();
        let mut picker = VehiclePicker::default();
        input.set_world_ray(Some(ray), &map, false);
        assert_eq!(picker.pick(&ctx.sim, &input).map(|hit| hit.entity), None);
        assert_eq!(click_building(&ctx.sim, ray, &map), Some(building));

        let clear_ray = Ray3::new(position.up(24.0), -Vec3::Z);
        input.set_world_ray(Some(clear_ray), &map, false);
        assert_eq!(
            picker.pick(&ctx.sim, &input).map(|hit| hit.entity),
            Some(car.into())
        );
    }

    #[test]
    fn vehicle_destination_repeated_empty_queries_reuse_broad_phase() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            ..SimulationOptions::LEGACY
        });
        spawn_driving_vehicle(
            &mut ctx.sim,
            VehicleKind::Car,
            geom::Transform::new(vec3(80.0, 80.0, 100.0)),
        )
        .unwrap();
        VEHICLE_SCAN_COUNT.with(|count| count.set(0));
        let mut input = InputMap::default();
        let mut picker = VehiclePicker::default();
        for x in [130.0, 131.0, 140.0] {
            input.ray = Some(Ray3::new(vec3(x, 150.0, 150.0), -Vec3::Z));
            assert_eq!(picker.pick(&ctx.sim, &input).map(|hit| hit.entity), None);
        }
        VEHICLE_SCAN_COUNT.with(|count| assert_eq!(count.get(), 1));
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
        let bounds: Vec<_> = pick_query_bounds(diagonal, 0.0).collect();
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
