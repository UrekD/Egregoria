use super::terrain::{CELL_SIZE, TERRAIN_CHUNK_RESOLUTION};
use crate::map::electricity_cache::ElectricityCache;
use crate::map::procgen::gen_exterior_house;
use crate::map::reachability::DrivingReachability;
use crate::map::{
    Building, BuildingID, BuildingKind, Environment, GrowableBusinessState, GrowableLotError,
    GrowableSiteState, Intersection, IntersectionID, Lane, LaneID, LaneKind, LanePattern, Lot,
    LotID, LotKind, LotLayout, MapSubscriber, MapSubscribers, ParkingSpotID, ParkingSpots,
    PreparedBuilding, PreparedGrowableAppearance, PreparedWaterEdit, ProjectFilter, ProjectKind,
    Road, RoadID, RoadSegmentKind, SpatialMap, SpatialMapObject, TerraformKind, TerrainChunkID,
    UpdateType, WaterDirtyChunks, WaterEdit, WaterEditError, WaterEditOutcome, WaterFootprint,
    WaterTileCoord, Zone, ZoneGrid, ROAD_Z_OFFSET,
};
use flat_spatial::grid::GridHandle;
use geom::OBB;
use geom::{pack_height, BaseNode, Intersect, PolyLine3, Shape, Vec2, Vec3, AABB, NO_OVERRIDE};
use ordered_float::OrderedFloat;
use prototypes::{BuildingGen, GrowableCompanyID, Tick};
use serde::{Deserialize, Serialize};
use slotmapd::{HopSlotMap, Key};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

pub type Roads = HopSlotMap<RoadID, Road>;
pub type Lanes = HopSlotMap<LaneID, Lane>;
pub type Intersections = HopSlotMap<IntersectionID, Intersection>;
pub type Buildings = HopSlotMap<BuildingID, Building>;
pub type Lots = HopSlotMap<LotID, Lot>;

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct MapProject {
    pub pos: Vec3,
    pub kind: ProjectKind,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct LotOrigin {
    pub parent_road: RoadID,
    pub shape: OBB,
    growable: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct ConnectionEndpoint {
    pub pos: Vec3,
    kind: ProjectKind,
    split_point: Option<(Vec3, usize)>,
}

pub(crate) struct RoadSplit {
    pub original: RoadID,
    pub junction: IntersectionID,
    roads: [RoadID; 2],
}

pub(crate) struct ConnectedRoad {
    pub src: IntersectionID,
    pub dst: IntersectionID,
    pub road: RoadID,
    pub splits: [Option<RoadSplit>; 2],
}

/// Map-side placement validated against a specific live lot and frontage.
pub(crate) struct PreparedGrowable {
    lot: LotID,
    frontage: RoadID,
    shape: OBB,
    generator: BuildingGen,
    building: PreparedBuilding,
}

pub struct Map {
    pub(crate) roads: Roads,
    pub(crate) lanes: Lanes,
    pub(crate) intersections: Intersections,
    pub(crate) buildings: Buildings,
    pub(crate) lots: Lots,
    pub(crate) spatial_map: SpatialMap,
    pub(crate) external_train_stations: Vec<BuildingID>,
    pub(crate) lot_origins: BTreeMap<BuildingID, LotOrigin>,
    // Derived identity for caches whose lifetime can exceed this map's.
    pub(super) identity: u64,
    pub(crate) walking_topology_generation: u64,
    // Derived from lanes and turns. Map deserialization starts with an empty cache.
    pub(super) driving_reachability: OnceLock<DrivingReachability>,
    // Imported networks reserve usable lane length when rebuilding junction geometry.
    pub(super) import_lane_reserve: bool,
    // Saved as its own `lot_layout` entry so the map's encoding is unchanged.
    pub(crate) lot_layout: LotLayout,
    // Roads the importer generated no lots for, e.g. bridges and synthetic links.
    // Saved as its own `road_frontage` entry so the map's encoding is unchanged.
    pub(crate) no_frontage: BTreeSet<RoadID>,

    pub electricity: ElectricityCache,
    pub(super) environment: Environment,
    pub parking: ParkingSpots,
    pub subscribers: MapSubscribers,
    pub(crate) pending_construction_cuts: BTreeSet<TerrainChunkID>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaterEditPreview {
    pub changed: bool,
    pub dirty: WaterDirtyChunks,
    pub strict_before: bool,
    pub strict_after: bool,
    pub proposed_surface_dm: Option<i16>,
    pub candidate_extent: Option<AABB>,
}

struct WaterEditEffects {
    wet_lots: BTreeSet<ProjectKind>,
    covered_zones: Vec<super::ZoneCellCoord>,
    wet_trees: Vec<(
        TerrainChunkID,
        OrderedFloat<f32>,
        OrderedFloat<f32>,
        GridHandle,
    )>,
    dirty: WaterDirtyChunks,
    cuts: Vec<(TerrainChunkID, CutHeights)>,
}

type CutHeights = [[u16; TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION];

impl Default for Map {
    fn default() -> Self {
        Self::empty()
    }
}

impl Clone for Map {
    fn clone(&self) -> Self {
        let mut spatial_map = SpatialMap::default();
        for building in self.buildings.values() {
            spatial_map.insert(building);
        }
        for road in self.roads.values() {
            spatial_map.insert(road);
        }
        for intersection in self.intersections.values() {
            spatial_map.insert(intersection);
        }
        for lot in self.lots.values() {
            spatial_map.insert(lot);
        }

        // Like a loaded map, the clone starts with its own subscriptions and identity.
        Self {
            roads: self.roads.clone(),
            lanes: self.lanes.clone(),
            intersections: self.intersections.clone(),
            buildings: self.buildings.clone(),
            lots: self.lots.clone(),
            spatial_map,
            external_train_stations: self.external_train_stations.clone(),
            lot_origins: self.lot_origins.clone(),
            walking_topology_generation: self.walking_topology_generation,
            import_lane_reserve: self.import_lane_reserve,
            lot_layout: self.lot_layout,
            no_frontage: self.no_frontage.clone(),
            pending_construction_cuts: self.pending_construction_cuts.clone(),
            electricity: self.electricity.clone(),
            environment: self.environment.clone(),
            parking: self.parking.clone(),
            ..Self::empty()
        }
    }
}

impl Map {
    // Public API
    pub fn empty() -> Self {
        static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);
        Self {
            roads: Roads::default(),
            lanes: Lanes::default(),
            intersections: Intersections::default(),
            parking: ParkingSpots::default(),
            buildings: Buildings::default(),
            lots: Lots::default(),
            environment: Environment::default(),
            spatial_map: SpatialMap::default(),
            external_train_stations: Default::default(),
            lot_origins: BTreeMap::new(),
            identity: NEXT_IDENTITY
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("map identities exhausted"),
            walking_topology_generation: 0,
            driving_reachability: OnceLock::new(),
            import_lane_reserve: false,
            lot_layout: LotLayout::default(),
            no_frontage: BTreeSet::new(),
            electricity: Default::default(),
            pending_construction_cuts: BTreeSet::new(),
            subscribers: MapSubscribers::default(),
        }
    }

    pub(crate) fn identity(&self) -> u64 {
        self.identity
    }

    /// Borrow the live terrain. Water edits go through the simulation dispatcher.
    ///
    /// ```compile_fail
    /// use simulation::{map::Map, Simulation};
    /// fn bypass(sim: &Simulation) {
    ///     let _ = sim.write::<Map>().environment.set_sea_level(Some(-100));
    /// }
    /// ```
    pub fn environment(&self) -> &Environment {
        &self.environment
    }

    #[cfg(test)]
    pub(crate) fn environment_mut_for_test(&mut self) -> &mut Environment {
        &mut self.environment
    }

    /// Install authored terrain into a new, empty map.
    pub fn install_environment(&mut self, environment: Environment) -> bool {
        let (width, height) = environment.size();
        if width.max(height) > super::MAX_WATER_CHUNKS_PER_AXIS
            || self.environment.size() != (0, 0)
            || self.environment.has_water_configuration()
            || self.spatial_map.objects().next().is_some()
        {
            return false;
        }
        self.environment = environment;
        true
    }

    /// Perform cleanups potentially required every frame
    pub fn update(&mut self) {
        profiling::scope!("map::update");
        for chunk in std::mem::take(&mut self.pending_construction_cuts) {
            let mut work = None;
            let heights = self
                .rebuild_construction_cuts(
                    chunk,
                    |node| {
                        self.environment.strict_water_siting()
                            && self.environment.is_wet_base_node(node)
                    },
                    &mut work,
                )
                .expect("scheduled cuts have no water transaction budget");
            self.environment.set_overrides(chunk, heights);
            self.subscribers.dispatch_chunk(UpdateType::Terrain, chunk);
        }
    }

    fn rebuild_construction_cuts(
        &self,
        chunk: TerrainChunkID,
        is_wet: impl Fn(BaseNode) -> bool,
        work: &mut Option<&mut usize>,
    ) -> Result<CutHeights, WaterEditError> {
        let mut heights = [[NO_OVERRIDE; TERRAIN_CHUNK_RESOLUTION]; TERRAIN_CHUNK_RESOLUTION];
        let chunk_bounds = chunk.bbox();
        for object in self.spatial_map.cut_candidates(chunk_bounds) {
            if let Some(work) = work.as_deref_mut() {
                spend_water_work(work, 1)?;
            }
            let mut add_cut = |bounds: AABB,
                               units: usize,
                               height: &dyn Fn(Vec2) -> Option<f32>|
             -> Result<(), WaterEditError> {
                let bounds = bounds.intersection(chunk_bounds).offset(-chunk_bounds.ll);
                let start = (bounds.ll / CELL_SIZE).ceil().max(Vec2::ZERO);
                let end = (bounds.ur / CELL_SIZE)
                    .floor()
                    .min(Vec2::splat((TERRAIN_CHUNK_RESOLUTION - 1) as f32));
                for y in start.y as usize..=end.y as usize {
                    for x in start.x as usize..=end.x as usize {
                        if let Some(work) = work.as_deref_mut() {
                            spend_water_work(work, units)?;
                        }
                        let node = (
                            chunk.0 as usize * TERRAIN_CHUNK_RESOLUTION + x,
                            chunk.1 as usize * TERRAIN_CHUNK_RESOLUTION + y,
                        );
                        if is_wet(node) {
                            continue;
                        }
                        let pos = Vec2::new(x as f32, y as f32) * CELL_SIZE + chunk_bounds.ll;
                        if let Some(z) = height(pos) {
                            let z = pack_height(z).max(NO_OVERRIDE + 1);
                            let old = &mut heights[y][x];
                            *old = if *old == NO_OVERRIDE {
                                z
                            } else {
                                (*old).min(z)
                            };
                        }
                    }
                }
                Ok(())
            };
            match object {
                ProjectKind::Building(id) => {
                    let building = &self.buildings[id];
                    let shape = building.obb.expand(25.0);
                    add_cut(shape.bbox(), 1, &|pos| {
                        shape.contains(pos).then_some(building.height)
                    })?;
                }
                ProjectKind::Intersection(id) => {
                    let intersection = &self.intersections[id];
                    let mut shape = intersection.bcircle();
                    shape.radius *= 2.0;
                    add_cut(shape.bbox(), 1, &|pos| {
                        shape.contains(pos).then_some(intersection.pos.z)
                    })?;
                }
                ProjectKind::Road(id) => {
                    let road = &self.roads[id];
                    let radius = 10.0 + road.width * 3.0;
                    add_cut(road.cut_influence().unwrap(), road.points().len(), &|pos| {
                        let projected = road.points().project_2d(pos);
                        projected
                            .xy()
                            .is_close(pos, radius)
                            .then_some(projected.z - ROAD_Z_OFFSET)
                    })?;
                }
                _ => unreachable!("construction cut index only holds contributors"),
            }
        }
        Ok(heights)
    }

    pub fn update_intersection(&mut self, id: IntersectionID, f: impl Fn(&mut Intersection)) {
        info!("update_intersection {:?}", id);

        let Some(inter) = self.intersections.get_mut(id) else {
            return;
        };
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            inter,
        );
        f(inter);
        self.invalidate(id);

        self.check_invariants()
    }

    pub fn remove_intersection(&mut self, src: IntersectionID) {
        info!("remove_intersection {:?}", src);
        self.remove_intersection_inner(src);

        self.check_invariants()
    }

    fn remove_intersection_inner(&mut self, src: IntersectionID) {
        let inter = unwrap_ret!(self.intersections.remove(src));
        self.lane_topology_changed();
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &inter,
        );
        self.subscribers.dispatch(UpdateType::Road, &inter);

        for road in inter.roads {
            let r = unwrap_cont!(self.remove_road_inner(road));

            let o = unwrap_cont!(r.other_end(src));
            self.invalidate(o);
        }

        self.electricity.remove_object(src);
        self.spatial_map.remove(src);
    }

    pub fn dispatch_all(&self) {
        self.subscribers
            .dispatch_all_terrain(self.environment.chunks().map(|(id, _)| id));
    }

    pub fn remove_building(&mut self, b: BuildingID) -> Option<Building> {
        info!("remove_building {:?}", b);

        let b = self.buildings.remove(b)?;
        queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, &b);
        self.subscribers.dispatch(UpdateType::Building, &b);

        if b.kind == BuildingKind::ExternalTrading {
            self.external_train_stations.retain(|id| *id != b.id);
        }

        if let Some(r) = b.connected_road {
            self.roads[r].connected_buildings.retain(|x| *x != b.id);
        }

        self.electricity.remove_object(b.id);
        self.spatial_map.remove(b.id);

        if let Some(origin) = self.lot_origins.remove(&b.id) {
            Lot::try_restore(self, origin);
        }
        self.check_invariants();

        Some(b)
    }

    pub(crate) fn resolve_connection_endpoint(
        &self,
        project: MapProject,
    ) -> Option<ConnectionEndpoint> {
        let mut split_point = None;
        let pos = match project.kind {
            ProjectKind::Ground => project.pos,
            ProjectKind::Intersection(id) => self.get(id)?.pos,
            ProjectKind::Road(id) => {
                if !project.pos.is_finite() {
                    return None;
                }
                let (point, segment) = self.road_split_point(id, project.pos)?;
                split_point = Some((point, segment));
                point - Vec3::z(ROAD_Z_OFFSET)
            }
            ProjectKind::Building(_) | ProjectKind::Lot(_) => return None,
        };
        pos.is_finite().then_some(ConnectionEndpoint {
            pos,
            kind: project.kind,
            split_point,
        })
    }

    pub(crate) fn reproject_split_endpoint(
        &self,
        original: RoadID,
        pos: Vec3,
        splits: &BTreeMap<RoadID, RoadSplit>,
        mut water_work: Option<&mut usize>,
    ) -> Result<Option<ConnectionEndpoint>, WaterEditError> {
        if !pos.is_finite() {
            return Ok(None);
        }
        let mut best = None;
        let mut remaining = vec![original];
        let mut consider = |endpoint: ConnectionEndpoint| {
            let surface = endpoint.pos + Vec3::z(ROAD_Z_OFFSET);
            let id = match endpoint.kind {
                ProjectKind::Intersection(id) => id.data(),
                ProjectKind::Road(id) => id.data(),
                _ => unreachable!(),
            };
            let key = (OrderedFloat(surface.distance2(pos)), id, endpoint.kind);
            if best.as_ref().is_none_or(|&(previous, _)| key < previous) {
                best = Some((key, endpoint));
            }
        };
        while let Some(id) = remaining.pop() {
            if let Some(work) = water_work.as_deref_mut() {
                spend_water_work(work, 1)?;
                if let Some(road) = self.get(id) {
                    spend_water_work(work, road.points().len() + road.connected_buildings.len())?;
                }
            }
            if let Some(split) = splits.get(&id) {
                remaining.extend(split.roads);
                if let Some(junction) = self.get(split.junction) {
                    if junction.pos.xy().distance(pos.xy()) <= junction.radius {
                        if let Some(endpoint) = self.resolve_connection_endpoint(MapProject {
                            pos,
                            kind: ProjectKind::Intersection(split.junction),
                        }) {
                            consider(endpoint);
                        }
                    }
                }
            } else if let Some(road) = self.get(id) {
                if let Some(endpoint) = self.resolve_connection_endpoint(MapProject {
                    pos,
                    kind: ProjectKind::Road(id),
                }) {
                    if endpoint.pos.xy().distance(pos.xy()) <= road.width * 0.5 {
                        consider(endpoint);
                    }
                }
            }
        }
        Ok(best.map(|(_, endpoint)| endpoint))
    }

    pub(crate) fn can_connect(
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        interpoint: Option<Vec2>,
        pattern: &LanePattern,
    ) -> bool {
        if interpoint.is_some_and(|point| !point.is_finite()) || pattern.lanes().next().is_none() {
            return false;
        }
        for lanes in [&pattern.lanes_forward, &pattern.lanes_backward] {
            if lanes
                .iter()
                .any(|&(_, speed)| !speed.is_finite() || speed <= 0.0)
                || (lanes.iter().any(|&(kind, _)| kind == LaneKind::Parking)
                    && !lanes.iter().any(|&(kind, _)| kind == LaneKind::Driving))
            {
                return false;
            }
        }
        // Each split removes its source road, so the endpoints cannot name the same road.
        if from.kind == to.kind && from.kind != ProjectKind::Ground {
            return false;
        }
        let distance = from.pos.xy().distance(to.pos.xy());
        distance.is_finite() && distance >= 1.0
    }

    fn road_split_point(&self, id: RoadID, pos: Vec3) -> Option<(Vec3, usize)> {
        let road = self.get(id)?;
        if !self.intersections.contains_key(road.src)
            || !self.intersections.contains_key(road.dst)
            || road
                .connected_buildings
                .iter()
                .any(|b| !self.buildings.contains_key(*b))
        {
            return None;
        }
        let (mut pos, segment) = road.points.project_segment(pos);
        pos.z = road.split_surface_height(pos, &self.environment);
        (pos.xy().distance(road.points.first().xy()) >= 1.0
            && pos.xy().distance(road.points.last().xy()) >= 1.0)
            .then_some((pos, segment))
    }

    pub fn make_connection(
        &mut self,
        from: MapProject,
        to: MapProject,
        interpoint: Option<Vec2>,
        pattern: &LanePattern,
    ) -> Option<(IntersectionID, RoadID)> {
        let mut work = self
            .environment
            .has_water_configuration()
            .then(WaterTopologyWork::new);
        let (from, to) = self
            .prepare_connection(from, to, interpoint, pattern, work.as_mut())
            .ok()?;
        Some(self.make_resolved_connection(from, to, interpoint, pattern, work.as_mut()))
    }

    pub(crate) fn resolve_water_connection_endpoint(
        &self,
        project: MapProject,
        work: &mut usize,
    ) -> Result<Option<ConnectionEndpoint>, WaterEditError> {
        if let ProjectKind::Road(id) = project.kind {
            if let Some(road) = self.get(id) {
                spend_water_work(work, road.points().len() + road.connected_buildings.len())?;
            }
        }
        Ok(self.resolve_connection_endpoint(project))
    }

    pub(crate) fn prepare_connection(
        &self,
        from: MapProject,
        to: MapProject,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
        work: Option<&mut WaterTopologyWork>,
    ) -> Result<(ConnectionEndpoint, ConnectionEndpoint), WaterEditError> {
        let mut work = work;
        let endpoints = if let Some(work) = work.as_deref_mut() {
            if !wet_lane_count_fits(pattern) {
                return Err(WaterEditError::Limit);
            }
            self.resolve_water_connection_endpoint(from, &mut work.remaining)?
                .zip(self.resolve_water_connection_endpoint(to, &mut work.remaining)?)
        } else {
            self.resolve_connection_endpoint(from)
                .zip(self.resolve_connection_endpoint(to))
        };
        let (from, to) = endpoints.ok_or(WaterEditError::InvalidInput)?;
        if !Self::can_connect(from, to, elbow, pattern) {
            return Err(WaterEditError::InvalidInput);
        }
        if let Some(work) = work {
            let samples = self.preflight_water_connection(from, to, elbow, pattern, work)?;
            work.spend_profile_samples(samples);
        }
        Ok((from, to))
    }

    /// Conservative gate for the legacy profiler. W2a will replace the dry
    /// envelope with its staged deck and junction geometry.
    pub(crate) fn preflight_water_connection(
        &self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
        work: &mut WaterTopologyWork,
    ) -> Result<usize, WaterEditError> {
        let map_bounds = self.environment.bounds();
        let a = from.pos.xy();
        let b = to.pos.xy();
        let mut bounds = AABB::new_ll_ur(a.min(b), a.max(b));
        let mut length = a.distance(b);
        if let Some(elbow) = elbow {
            if !map_bounds.contains(elbow) {
                return Err(WaterEditError::OutOfBounds);
            }
            bounds = bounds.union(AABB::new_ll_ur(elbow, elbow));
            length = a.distance(elbow) + elbow.distance(b);
        }
        if !map_bounds.contains(a) || !map_bounds.contains(b) {
            return Err(WaterEditError::OutOfBounds);
        }
        if !wet_lane_count_fits(pattern) || length > 65_536.0 {
            return Err(WaterEditError::Limit);
        }
        let lanes = pattern.lanes_forward.len() + pattern.lanes_backward.len();
        let mut junctions = BTreeSet::new();
        let mut splits = Vec::new();
        for endpoint in [from, to] {
            match endpoint.kind {
                ProjectKind::Intersection(id) => {
                    junctions.insert(id);
                }
                ProjectKind::Road(id) => {
                    let road = &self.roads[id];
                    junctions.extend([road.src, road.dst]);
                    splits.push(id);
                }
                _ => {}
            }
        }
        // The Hermite curve's control polygon is at most the chord plus 2/3
        // of the sum of the tangent lengths. Each tangent is its elbow leg
        // scaled by 1/sqrt(2), as in RoadSegmentKind::from_elbow.
        let path_length = if elbow.is_some() {
            a.distance(b) + length * (2.0 * std::f32::consts::FRAC_1_SQRT_2 / 3.0)
        } else {
            length
        };
        // Initial profiling, lane creation, and invalidation of the two endpoints.
        let mut profile_samples = profile_sample_bound(path_length).saturating_mul(4);
        let invalidations = 2 + 3 * splits.len();
        for &id in &splits {
            let road = &self.roads[id];
            if !road.elevated_span() {
                // Splitting partitions the horizontal path. Allow for rounding
                // and endpoint samples of both pieces, plus their lane creation.
                let samples = profile_sample_bound(road.length()).saturating_add(5);
                profile_samples = profile_samples
                    .saturating_add(samples.saturating_mul(1 + invalidations));
            }
        }
        let profile_samples = self.reserve_water_topology(
            junctions,
            &splits,
            lanes,
            length,
            profile_samples,
            invalidations,
            work,
        )?;
        let radius = pattern.width() + 16.0;
        let envelope = bounds.expand(radius);
        let clipped = envelope.intersection(map_bounds);
        let min = (clipped.ll / super::terrain::CELL_SIZE).floor();
        let max = (clipped.ur / super::terrain::CELL_SIZE).ceil();
        let tests = (max.x - min.x) as usize * (max.y - min.y) as usize;
        if tests > 2_000_000 {
            return Err(WaterEditError::Limit);
        }
        spend_water_work(&mut work.remaining, tests + length.ceil() as usize)?;
        let footprint = if elbow.is_some() {
            geom::Polygon(envelope.points().to_vec())
        } else {
            geom::Polygon(
                OBB::new((a + b) * 0.5, (b - a).normalize(), length + radius * 2.0, radius * 2.0)
                    .corners
                    .to_vec(),
            )
        };
        let shape = WaterFootprint::new(footprint.as_slice()).ok_or(WaterEditError::InvalidInput)?;
        if self.environment.strict_water_siting() {
            for x in min.x as i16..max.x as i16 {
                for y in min.y as i16..max.y as i16 {
                    let tile = WaterTileCoord(x, y);
                    if self.environment.is_water(tile.center()) && shape.overlaps_tile(tile) {
                        return Err(WaterEditError::TopologyConflict {
                            object: ProjectKind::Ground,
                            tile,
                        });
                    }
                }
            }
        }
        Ok(profile_samples)
    }

    pub(crate) fn make_resolved_connection(
        &mut self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        interpoint: Option<Vec2>,
        pattern: &LanePattern,
        mut work: Option<&mut WaterTopologyWork>,
    ) -> (IntersectionID, RoadID) {
        let connected = self.connect_projects(from, to, interpoint, pattern);
        let _ = self.merge_shallow_junction(connected.src, work.as_deref_mut());
        let _ = self.merge_shallow_junction(connected.dst, work);
        self.check_invariants();
        (connected.dst, connected.road)
    }

    pub(crate) fn connect_projects(
        &mut self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        interpoint: Option<Vec2>,
        pattern: &LanePattern,
    ) -> ConnectedRoad {
        let connection_segment = match interpoint {
            Some(x) => RoadSegmentKind::from_elbow(from.pos.xy(), to.pos.xy(), x),
            None => RoadSegmentKind::Straight,
        };

        let mut mk_inter = |endpoint: ConnectionEndpoint| match endpoint.kind {
            ProjectKind::Ground => (self.add_intersection(endpoint.pos), None),
            ProjectKind::Intersection(id) => (id, None),
            ProjectKind::Road(id) => {
                let (point, segment) = endpoint.split_point.expect("resolved road split point");
                let split = self
                    .split_road(id, point, segment)
                    .expect("validated road split");
                (split.junction, Some(split))
            }
            ProjectKind::Building(_) | ProjectKind::Lot(_) => unreachable!(),
        };

        let (src, from_split) = mk_inter(from);
        let (dst, to_split) = mk_inter(to);
        let road = self
            .connect(src, dst, pattern, connection_segment)
            .expect("validated connection intersections");

        self.invalidate(src);
        self.invalidate(dst);

        info!("connected {:?} {:?}: {:?}", src, dst, road);
        ConnectedRoad {
            src,
            dst,
            road,
            splits: [from_split, to_split],
        }
    }

    pub(crate) fn merge_shallow_junction(
        &mut self,
        id: IntersectionID,
        work: Option<&mut WaterTopologyWork>,
    ) -> Result<(), WaterEditError> {
        let Some(intersection) = self.intersections.get(id) else {
            return Ok(());
        };
        if let [first, second] = *intersection.roads {
            let angle = self.roads[first]
                .dir_from(id)
                .dot(self.roads[second].dir_from(id));
            if angle < -0.9 {
                if let Some(work) = work {
                    let a = &self.roads[first];
                    let b = &self.roads[second];
                    let junctions = BTreeSet::from([a.src, a.dst, b.src, b.dst]);
                    let length = a.length() + b.length();
                    // The merged road reuses the old path and may profile it when
                    // creating lanes and invalidating each of the two far ends.
                    let samples = self.reserve_water_topology(
                        junctions,
                        &[first, second],
                        a.n_lanes(),
                        length,
                        profile_sample_bound(length).saturating_mul(3),
                        2,
                        work,
                    )?;
                    work.spend_profile_samples(samples);
                }
                self.merge_road(first, second);
            }
        }
        Ok(())
    }

    fn reserve_water_topology(
        &self,
        mut junctions: BTreeSet<IntersectionID>,
        replaced_roads: &[RoadID],
        new_lanes: usize,
        new_length: f32,
        mut profile_samples: usize,
        invalidations: usize,
        work: &mut WaterTopologyWork,
    ) -> Result<usize, WaterEditError> {
        work.check_profile_samples(profile_samples)?;
        let remaining = &mut work.remaining;
        // Splits can invalidate three junctions each, followed by both ends of
        // the new link. Imported roads also rebuild lanes across the first ring
        // and turns at the far ends of those roads.
        const INVALIDATIONS: usize = 8;
        let profile = |length: f32, points: usize, lanes: usize| {
            (length.ceil() as usize)
                .saturating_add(points)
                .saturating_add(2)
                .saturating_mul(lanes + 1)
                .saturating_mul(INVALIDATIONS)
        };
        spend_water_work(remaining, profile(new_length * 3.0, 0, new_lanes))?;
        let mut added_lanes = new_lanes;
        for &id in replaced_roads {
            let road = &self.roads[id];
            added_lanes = added_lanes.saturating_add(road.n_lanes());
            spend_water_work(remaining, self.lots.len())?;
            spend_water_work(
                remaining,
                road.connected_buildings
                    .len()
                    .saturating_mul(road.points().len() + 2),
            )?;
        }

        let mut roads = BTreeSet::new();
        let mut parallel = BTreeMap::<_, usize>::new();
        let mut inspected = BTreeSet::new();
        for ring in 0..2 {
            let mut neighbors = BTreeSet::new();
            for &id in &junctions {
                if !inspected.insert(id) {
                    continue;
                }
                for &road in &self.intersections[id].roads {
                    spend_water_work(remaining, 1)?;
                    let r = &self.roads[road];
                    // A legacy invalidation profiles only its own roads. Imported
                    // topology may also profile a road incident to a neighbor.
                    if roads.insert(road)
                        && (ring == 0 || self.import_lane_reserve)
                        && !replaced_roads.contains(&road)
                        && !r.elevated_span()
                    {
                        let rebuilds = if !self.import_lane_reserve && invalidations == 2 {
                            usize::from(junctions.contains(&r.src))
                                + usize::from(junctions.contains(&r.dst))
                        } else {
                            invalidations
                        };
                        let samples = profile_sample_bound(r.length());
                        profile_samples = profile_samples
                            .saturating_add(samples.saturating_mul(rebuilds));
                    }
                    if let Some(other) = r.other_end(id) {
                        neighbors.insert(other);
                        *parallel.entry((id, other)).or_default() += 1;
                    }
                }
            }
            junctions.extend(neighbors);
        }
        // Legacy invalidation may recalculate a neighbor interface once per
        // parallel arm. Include the prospective links in that multiplicity.
        let profile_repeats = parallel.values().copied().max().unwrap_or(0) + 3;
        for id in roads {
            let road = &self.roads[id];
            if road.n_lanes() > MAX_WET_LINK_LANES {
                return Err(WaterEditError::Limit);
            }
            spend_water_work(
                remaining,
                profile(road.length(), road.points().len(), road.n_lanes()).saturating_add(
                    (road.length().ceil() as usize)
                        .saturating_mul(profile_repeats)
                        .saturating_mul(INVALIDATIONS),
                ),
            )?;
        }
        let reserve_turns = |lanes: usize, work: &mut usize| {
            spend_water_work(
                work,
                lanes.saturating_mul(lanes).saturating_mul(INVALIDATIONS),
            )
        };
        for id in junctions {
            let mut lanes = added_lanes;
            for &road in &self.intersections[id].roads {
                spend_water_work(remaining, 1)?;
                lanes = lanes.saturating_add(self.roads[road].n_lanes());
            }
            let roundabout = self.intersections[id]
                .turn_policy
                .roundabout
                .map_or(0.0, |roundabout| roundabout.radius);
            if !roundabout.is_finite()
                || roundabout.abs() > super::MAX_WATER_CHUNKS_PER_AXIS as f32 * 512.0
            {
                return Err(WaterEditError::Limit);
            }
            let samples = (roundabout.abs() / 10.0).ceil() as usize + 1;
            spend_water_work(
                remaining,
                lanes
                    .saturating_mul(lanes)
                    .saturating_mul(samples)
                    .saturating_mul(INVALIDATIONS),
            )?;
        }
        // New ground endpoints and the two possible split junctions.
        reserve_turns(added_lanes.saturating_mul(2), remaining)?;
        reserve_turns(added_lanes.saturating_mul(2), remaining)?;
        work.check_profile_samples(profile_samples)?;
        Ok(profile_samples)
    }

    pub fn update_zone(&mut self, id: BuildingID, zone: Zone) -> Option<()> {
        let zone = zone.normalized()?;
        let b = self.buildings.get_mut(id)?;
        if !Building::dry_footprint(&self.environment, &b.obb, Some(&zone)) {
            return None;
        }
        *b.zone.as_mut()? = zone;
        queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, b);
        self.subscribers.dispatch(UpdateType::Building, b);

        let z = b.zone.as_ref().unwrap();
        self.environment.remove_trees_near(&z.poly, |tree_chunk| {
            self.subscribers
                .dispatch_chunk(UpdateType::Terrain, tree_chunk)
        });

        let toclean = self
            .spatial_map
            .query(&z.poly, ProjectFilter::LOT)
            .collect();

        self.spatial_map.update(b);

        self.clean_lots_inner(toclean);

        self.check_invariants();
        Some(())
    }

    pub fn build_special_building(
        &mut self,
        obb: &OBB,
        kind: BuildingKind,
        gen: BuildingGen,
        zone: Option<Zone>,
        connected_road: Option<RoadID>,
    ) -> Option<BuildingID> {
        if kind.is_mixed_growable() {
            return None;
        }
        let zone = match zone {
            Some(zone) => Some(zone.normalized()?),
            None => None,
        };
        if !Building::valid_footprint(obb, gen) {
            return None;
        }
        if self.building_overlaps(*obb) {
            log::warn!("did not build {:?}: building overlaps", kind);
            return None;
        }
        log::info!(
            "build special {:?} with shape {:?} and gen {:?} and zone {:?}",
            kind,
            obb,
            gen,
            zone
        );

        let Some(id) = Building::make(
            &mut self.buildings,
            &mut self.spatial_map,
            &mut self.electricity,
            &mut self.roads,
            &self.environment,
            *obb,
            kind,
            gen,
            zone,
            connected_road,
        ) else {
            self.check_invariants();
            return None;
        };

        self.clean_lots_inner(self.spatial_map.query(obb, ProjectFilter::LOT).collect());

        self.environment
            .remove_trees_near(obb.expand(10.0), |tree_chunk| {
                self.subscribers
                    .dispatch_chunk(UpdateType::Terrain, tree_chunk)
            });

        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &self.buildings[id],
        );
        self.subscribers
            .dispatch(UpdateType::Building, &self.buildings[id]);

        if kind == BuildingKind::ExternalTrading {
            self.external_train_stations.push(id);
        }

        self.check_invariants();
        Some(id)
    }

    pub fn build_house(&mut self, lot_id: LotID) -> Option<BuildingID> {
        info!("build house on {:?}", lot_id);
        self.build_on_lot(lot_id, BuildingKind::House, BuildingGen::House, false)
    }

    pub fn place_growable(&mut self, lot_id: LotID, kind: BuildingKind) -> Option<BuildingID> {
        if kind.is_mixed_growable() {
            return None;
        }
        let gen = match kind {
            BuildingKind::House => BuildingGen::House,
            BuildingKind::GoodsCompany(company) => {
                let proto = prototypes::try_prototype(GrowableCompanyID::from(company))?;
                if !proto.growable {
                    return None;
                }
                proto.base.bgen
            }
            _ => return None,
        };
        let prepared = self.prepare_growable(lot_id, kind, gen, None).ok()?;
        self.commit_prepared_growable(prepared).ok()
    }

    pub(crate) fn prepare_growable(
        &self,
        lot_id: LotID,
        kind: BuildingKind,
        generator: BuildingGen,
        appearance: Option<PreparedGrowableAppearance>,
    ) -> Result<PreparedGrowable, GrowableLotError> {
        let lot = self.get(lot_id).ok_or(GrowableLotError::MissingLot)?;
        lot.check_growable_footprint(self, generator)?;
        let building = Building::prepare(
            &self.environment,
            lot.shape,
            kind,
            generator,
            None,
            Some(lot.parent),
            appearance,
        )
        .ok_or(GrowableLotError::InfeasibleGrade)?;
        Ok(PreparedGrowable {
            lot: lot_id,
            frontage: lot.parent,
            shape: lot.shape,
            generator,
            building,
        })
    }

    pub(crate) fn commit_prepared_growable(
        &mut self,
        prepared: PreparedGrowable,
    ) -> Result<BuildingID, GrowableLotError> {
        let lot = self.get(prepared.lot).ok_or(GrowableLotError::MissingLot)?;
        if lot.parent != prepared.frontage || lot.shape.corners != prepared.shape.corners {
            return Err(GrowableLotError::MissingRoad);
        }
        lot.check_growable_footprint(self, prepared.generator)?;

        let id = prepared.building.insert(
            &mut self.buildings,
            &mut self.spatial_map,
            &mut self.electricity,
            &mut self.roads,
        );
        self.consume_building_lot(prepared.lot, id, true);
        // Older maps can contain overlapping lots. Consume those replaced by this footprint.
        self.clean_lots_inner(
            self.spatial_map
                .query(prepared.shape, ProjectFilter::LOT)
                .collect(),
        );
        Ok(id)
    }

    /// Apply checked appearance and level changes without moving either role or the frontage.
    /// The simulation's upgrade commit owns the corresponding capacity and job changes.
    pub(crate) fn update_growable_appearance(
        &mut self,
        id: BuildingID,
        appearance: PreparedGrowableAppearance,
    ) -> bool {
        let Some(building) = self.buildings.get_mut(id) else {
            return false;
        };
        let Some(previous) = building.development.as_ref() else {
            return false;
        };
        let next = &appearance.development;
        if appearance.base_z != building.height
            || appearance.door != building.door_pos
            || !appearance.is_valid(&building.obb)
            || next.zone != previous.zone
            || next.site != previous.site
            || next.business != previous.business
            || next.home.is_some() != previous.home.is_some()
            || next.level != previous.level.saturating_add(1)
            || next.level > 5
            || next.floors < previous.floors
            || next.roof_z < previous.roof_z
            || next.planned_jobs < previous.planned_jobs
            || next.approved_value_band < previous.approved_value_band
        {
            return false;
        }
        building.mesh = appearance.mesh;
        building.development = Some(appearance.development);
        self.subscribers.dispatch(UpdateType::Building, building);
        true
    }

    pub(crate) fn set_growable_business_state(
        &mut self,
        id: BuildingID,
        state: GrowableBusinessState,
    ) {
        let Some(building) = self.buildings.get_mut(id) else {
            return;
        };
        if let Some(business) = building
            .development
            .as_mut()
            .and_then(|site| site.business.as_mut())
        {
            if *business != state {
                *business = state;
                self.subscribers.dispatch(UpdateType::Building, building);
            }
        }
    }

    pub(crate) fn mark_growable_abandoned(&mut self, id: BuildingID, elapsed_tick: u64) -> bool {
        let Some(building) = self.buildings.get_mut(id) else {
            return false;
        };
        let Some(site) = building
            .development
            .as_mut()
            .filter(|site| site.site == GrowableSiteState::Active)
        else {
            return false;
        };
        site.site = GrowableSiteState::Abandoned {
            since_elapsed_tick: elapsed_tick,
        };
        self.subscribers.dispatch(UpdateType::Building, building);
        true
    }

    /// Returns the lot origin of a growable. Player builds record an origin for demolition too.
    pub fn lot_origin(&self, building: BuildingID) -> Option<&LotOrigin> {
        self.lot_origins
            .get(&building)
            .filter(|origin| origin.growable)
    }

    pub(crate) fn regenerate_house_mesh(&mut self, id: BuildingID, level: u8) {
        let building = &mut self.buildings[id];
        let center = building.obb.center();
        let edge = building.obb.corners[1] - building.obb.corners[0];
        let seed = common::rand::rand2(center.x, center.y).to_bits();
        let (mut mesh, _) = gen_exterior_house(edge.mag(), u64::from(seed));
        let axis = edge.normalize();
        let height_scale = 1.25_f32.powi(i32::from(level) - 1);
        for (face, _) in &mut mesh.faces {
            for vertex in face {
                vertex.z *= height_scale;
                *vertex = vertex.rotate_z(axis) + center.z(building.height);
            }
        }
        // The last face is the existing walkway to the road-facing edge.
        if let Some(walkway) = building.mesh.faces.pop() {
            mesh.faces.push(walkway);
        }
        building.mesh = mesh;
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            building,
        );
        self.subscribers.dispatch(UpdateType::Building, building);
    }

    fn build_on_lot(
        &mut self,
        lot_id: LotID,
        kind: BuildingKind,
        gen: BuildingGen,
        growable: bool,
    ) -> Option<BuildingID> {
        let lot = self.lots.get(lot_id)?;
        let id = Building::make(
            &mut self.buildings,
            &mut self.spatial_map,
            &mut self.electricity,
            &mut self.roads,
            &self.environment,
            lot.shape,
            kind,
            gen,
            None,
            Some(lot.parent),
        )?;

        self.consume_building_lot(lot_id, id, growable);
        Some(id)
    }

    fn consume_building_lot(&mut self, lot_id: LotID, id: BuildingID, growable: bool) {
        let lot = self.lots.remove(lot_id).expect("building lot still exists");
        self.subscribers.dispatch(UpdateType::Road, &lot);
        self.spatial_map.remove(lot_id);
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &self.buildings[id],
        );
        self.subscribers
            .dispatch(UpdateType::Building, &self.buildings[id]);
        self.lot_origins.insert(
            id,
            LotOrigin {
                parent_road: lot.parent,
                shape: lot.shape,
                growable,
            },
        );
        self.check_invariants();
    }

    pub fn remove_road(&mut self, road_id: RoadID) -> Option<Road> {
        info!("remove_road {:?}", road_id);

        let v = self.remove_road_inner(road_id);
        self.check_invariants();
        v
    }

    pub fn subscribe(&self, filter: UpdateType) -> MapSubscriber {
        self.subscribers.subscribe(filter)
    }

    fn clean_lots_inner(&mut self, mut to_clean: Vec<ProjectKind>) {
        // Slot reuse depends on removal order, but the spatial index is rebuilt on load.
        to_clean.sort_unstable();
        for id in to_clean {
            if let ProjectKind::Lot(id) = id {
                self.spatial_map.remove(id);
                self.subscribers.dispatch(UpdateType::Road, &self.lots[id]);

                unwrap_contlog!(
                    &self.lots.remove(id),
                    "Lot was present in spatial map but not in Lots struct"
                );
            }
        }
    }

    fn remove_road_inner(&mut self, road_id: RoadID) -> Option<Road> {
        let road = self.remove_raw_road(road_id)?;
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &road,
        );
        self.subscribers.dispatch(UpdateType::Road, &road);

        for (id, _) in road.lanes_iter() {
            self.parking.remove_spots(id);
        }

        let smap = &mut self.spatial_map;
        self.lots.retain(|_, lot| {
            let to_remove = lot.parent == road_id;
            if to_remove {
                self.subscribers.dispatch(UpdateType::Road, lot);
                smap.remove(lot.id);
            }
            !to_remove
        });

        self.invalidate(road.src);
        self.invalidate(road.dst);

        for &building_id in &road.connected_buildings {
            self.buildings[building_id].connected_road = None;
        }

        Some(road)
    }

    pub fn lot_layout(&self) -> LotLayout {
        self.lot_layout
    }

    /// Sizes later lots with `layout`. A change replaces the empty lots of every road with
    /// frontage, built or not. Buildings, their lot origins and zoning paint stay unchanged.
    ///
    /// Roads regenerate in `RoadID` order rather than build order, so contested corners
    /// can resolve differently and switching back does not restore the previous lots.
    /// It all happens within the command: about 50 ms for the ~1000 roads of the Lesce
    /// import in a release build, a one-off hitch for an occasional settings change.
    pub fn set_lot_layout(&mut self, layout: LotLayout) {
        if self.lot_layout == layout {
            return;
        }
        info!("set_lot_layout {:?}", layout);
        self.lot_layout = layout;

        // Besides roads holding lots or built lots, take every street on the ground: it
        // can be left without lots when no size it drew fitted, or when a neighbour's lots
        // took its frontage. Roads the importer left without lots keep none.
        let street = |road: &Road| {
            !road.elevated_span()
                && road.has_sidewalks()
                && road.lanes_iter().any(|(_, kind)| kind == LaneKind::Driving)
        };
        let mut frontage: BTreeSet<RoadID> = self.lots.values().map(|lot| lot.parent).collect();
        frontage.extend(self.lot_origins.values().map(|origin| origin.parent_road));
        frontage.extend(
            self.roads
                .values()
                .filter(|road| street(road))
                .map(|road| road.id),
        );
        frontage.retain(|&road| self.roads.contains_key(road) && !self.no_frontage.contains(&road));

        let empty = self.lots.keys().map(ProjectKind::Lot).collect();
        self.clean_lots_inner(empty);
        for road in frontage {
            Lot::generate_along_road(self, road);
        }
        self.check_invariants();
    }

    pub fn set_lot_kind(&mut self, lot: LotID, kind: LotKind) {
        match self.lots.get_mut(lot) {
            Some(lot) => {
                lot.kind = kind;
                self.subscribers.dispatch(UpdateType::Road, lot);
            }
            None => log::warn!("trying to set kind of non-existing lot {:?}", lot),
        }
    }

    pub(crate) fn terraform(
        &mut self,
        tick: Tick,
        kind: TerraformKind,
        center: Vec2,
        radius: f32,
        amount: f32,
        level: f32,
        slope: Option<(Vec3, Vec3)>,
    ) {
        let modified = self
            .environment
            .terraform(tick, kind, center, radius, amount, level, slope);

        for id in modified {
            self.subscribers.dispatch_chunk(UpdateType::Terrain, id);
        }
    }

    pub(crate) fn preview_water_edit(
        &self,
        edit: &WaterEdit,
        zones: &ZoneGrid,
    ) -> Result<WaterEditPreview, WaterEditError> {
        let prepared = self.environment.prepare_water_edit(edit)?;
        let mut remaining = super::MAX_WATER_BATCH_WORK;
        let effects = self.prepare_water_effects(&prepared, zones, &mut remaining)?;
        Ok(WaterEditPreview {
            changed: prepared.changed(),
            dirty: prepared.dirty().clone().union(effects.dirty),
            strict_before: self.environment.strict_water_siting(),
            strict_after: prepared.strict_water_siting(),
            proposed_surface_dm: prepared.proposed_surface_dm(),
            candidate_extent: prepared
                .dirty()
                .road_support()
                .iter()
                .map(|chunk| chunk.bbox())
                .reduce(AABB::union),
        })
    }

    pub(crate) fn apply_water_edit(
        &mut self,
        edit: &WaterEdit,
        zones: &mut ZoneGrid,
        remaining: &mut usize,
    ) -> Result<WaterEditOutcome, WaterEditError> {
        let prepared = self.environment.prepare_water_edit(edit)?;
        self.commit_water_edit(prepared, zones, remaining)
    }

    pub(crate) fn commit_water_edit(
        &mut self,
        prepared: PreparedWaterEdit,
        zones: &mut ZoneGrid,
        remaining: &mut usize,
    ) -> Result<WaterEditOutcome, WaterEditError> {
        let effects = self.prepare_water_effects(&prepared, zones, remaining)?;
        let mut outcome = self.environment.commit_prepared_water(prepared)?;
        self.clean_lots_inner(effects.wet_lots.into_iter().collect());
        zones.clear_prepared_cells(effects.covered_zones);
        for (_, _, _, tree) in effects.wet_trees {
            self.environment.trees.remove_maintain(tree);
        }
        for (chunk, cuts) in effects.cuts {
            self.environment.set_overrides(chunk, cuts);
            self.pending_construction_cuts.remove(&chunk);
        }
        outcome.dirty = outcome.dirty.union(effects.dirty);
        self.subscribers.dispatch_water(&outcome.dirty);
        Ok(outcome)
    }

    fn prepare_water_effects(
        &self,
        prepared: &PreparedWaterEdit,
        zones: &ZoneGrid,
        remaining: &mut usize,
    ) -> Result<WaterEditEffects, WaterEditError> {
        spend_water_work(remaining, prepared.work())?;
        let mut effects = WaterEditEffects {
            wet_lots: BTreeSet::new(),
            covered_zones: Vec::new(),
            wet_trees: Vec::new(),
            dirty: WaterDirtyChunks::default(),
            cuts: Vec::new(),
        };
        let mut road_support = BTreeSet::new();

        let mut candidates = BTreeMap::new();
        for &chunk in prepared.dirty().road_support() {
            let mut objects = BTreeSet::new();
            for object in self
                .spatial_map
                .construction_candidates(chunk.bbox(), ProjectFilter::ALL)
            {
                spend_water_work(remaining, 1)?;
                objects.insert(object);
                if matches!(object, ProjectKind::Road(_) | ProjectKind::Intersection(_)) {
                    let bucket = TerrainChunkID::new(object.canonical_position(self));
                    if self.environment.get_chunk(bucket).is_some() {
                        road_support.insert(bucket);
                    }
                }
            }
            if !objects.is_empty() {
                candidates.insert(chunk, objects);
            }
        }
        let mut footprints = BTreeMap::new();
        for tile in prepared.affected_tiles() {
            let Some(objects) = candidates.get(&tile.chunk()) else {
                continue;
            };
            if prepared.water_at_tile(&self.environment, tile).is_none() {
                continue;
            }
            let bounds = water_tile_bounds(tile);
            for &object in objects {
                spend_water_work(remaining, 1)?;
                let mut overlaps = |points: &[Vec2], part| -> Result<bool, WaterEditError> {
                    // The edge walk is still charged for each tile. Only the bounds
                    // walk and edge allocation are shared across the transaction.
                    spend_water_work(remaining, points.len())?;
                    let entry = footprints.entry((object, part));
                    let shape = match entry {
                        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            spend_water_work(remaining, points.len())?;
                            entry.insert(WaterFootprint::new(points))
                        }
                    };
                    Ok(shape
                        .as_ref()
                        .is_some_and(|shape| shape.overlaps_tile(tile)))
                };
                match object {
                    ProjectKind::Building(id) => {
                        let building = &self.buildings[id];
                        if overlaps(&building.obb.corners, 0)?
                            || building
                                .zone
                                .as_ref()
                                .map(|zone| overlaps(zone.poly.as_slice(), 1))
                                .transpose()?
                                .unwrap_or(false)
                        {
                            return Err(WaterEditError::OccupiedFootprint { object, tile });
                        }
                    }
                    // The crossing leaf replaces this conservative check with final deck,
                    // junction and underside clearance checks.
                    ProjectKind::Road(id) => {
                        let road = &self.roads[id];
                        spend_water_work(remaining, road.points().len())?;
                        if road.boldline().intersects(&bounds) {
                            return Err(WaterEditError::TopologyConflict { object, tile });
                        }
                    }
                    ProjectKind::Intersection(id)
                        if self.intersections[id].bcircle().intersects(&bounds) =>
                    {
                        return Err(WaterEditError::TopologyConflict { object, tile });
                    }
                    ProjectKind::Lot(id) if overlaps(&self.lots[id].shape.corners, 0)? => {
                        effects.wet_lots.insert(object);
                    }
                    _ => {}
                }
            }
        }
        effects.covered_zones = zones.newly_wet_cells(&self.environment, prepared, remaining)?;
        let mut terrain = BTreeSet::new();
        for &chunk in prepared.dirty().water() {
            let bounds = chunk.bbox();
            for (handle, position) in self.environment.trees.query_aabb(bounds.ll, bounds.ur) {
                spend_water_work(remaining, 1)?;
                if TerrainChunkID::new(position) == chunk
                    && prepared.water_at(&self.environment, position).is_some()
                    && !self.environment.is_water(position)
                {
                    terrain.insert(chunk);
                    effects.wet_trees.push((
                        chunk,
                        OrderedFloat(position.y),
                        OrderedFloat(position.x),
                        handle,
                    ));
                }
            }
        }
        effects.wet_trees.sort_unstable();
        for &chunk in prepared.dirty().road_support() {
            spend_water_work(remaining, 1)?;
            if !self.pending_construction_cuts.contains(&chunk)
                && self
                    .spatial_map
                    .cut_candidates(chunk.bbox())
                    .next()
                    .is_none()
            {
                continue;
            }
            spend_water_work(
                remaining,
                TERRAIN_CHUNK_RESOLUTION * TERRAIN_CHUNK_RESOLUTION,
            )?;
            let cuts = self.rebuild_construction_cuts(
                chunk,
                |node| prepared.is_wet_base_node(&self.environment, node),
                &mut Some(remaining),
            )?;
            effects.cuts.push((chunk, cuts));
            terrain.insert(chunk);
        }
        effects.dirty = WaterDirtyChunks::new(BTreeSet::new(), terrain, road_support);
        Ok(effects)
    }

    // Private mutating

    fn lane_topology_changed(&mut self) {
        self.walking_topology_generation += 1;
        self.driving_reachability.take();
    }

    pub(crate) fn add_intersection(&mut self, pos: Vec3) -> IntersectionID {
        let id = Intersection::make(&mut self.intersections, &mut self.spatial_map, pos);
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &self.intersections[id],
        );
        self.subscribers
            .dispatch(UpdateType::Building, &self.intersections[id]);
        self.electricity.add_object(id);
        id
    }

    const IMPORT_LANE_RESERVE: f32 = 10.0;

    fn reserve_import_lane(road: &mut Road) {
        let src = road.interface_from(road.src);
        let dst = road.interface_from(road.dst);
        let available = (road.length() - Self::IMPORT_LANE_RESERVE).max(0.0);
        if src + dst > available {
            let scale = available / (src + dst);
            road.set_interface(road.src, src * scale);
            road.set_interface(road.dst, dst * scale);
        }
    }

    pub(crate) fn finalize_import(&mut self, junctions: &[IntersectionID], roads: &[RoadID]) {
        self.import_lane_reserve = true;
        self.lane_topology_changed();
        for &id in junctions {
            let junction = &mut self.intersections[id];
            junction.update_interface_radius(&mut self.roads);
        }
        for &id in roads {
            let road = &mut self.roads[id];
            Self::reserve_import_lane(road);
            road.update_lanes(&mut self.lanes, &mut self.parking, &self.environment);
            queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, road);
            self.subscribers.dispatch(UpdateType::Road, road);
        }
        for &id in junctions {
            let junction = &mut self.intersections[id];
            junction.update_traffic_control(&mut self.lanes, &self.roads);
            junction.update_turns(&self.lanes, &self.roads);
            self.spatial_map.update(junction);
            queue_construction_cut(
                &mut self.pending_construction_cuts,
                &self.environment,
                junction,
            );
            self.subscribers.dispatch(UpdateType::Road, junction);
        }
    }

    fn invalidate(&mut self, id: IntersectionID) {
        info!("invalidate {:?}", id);

        if !self.intersections.contains_key(id) {
            return;
        }
        self.lane_topology_changed();
        let inter = &mut self.intersections[id];
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            inter,
        );
        self.subscribers.dispatch(UpdateType::Road, inter);

        if inter.roads.is_empty() {
            self.remove_intersection_inner(id);
            return;
        }

        if !self.import_lane_reserve {
            self.invalidate_legacy(id);
            return;
        }
        self.invalidate_imported(id);
    }

    fn invalidate_legacy(&mut self, id: IntersectionID) {
        let inter = &mut self.intersections[id];
        inter.update_interface_radius(&mut self.roads);

        for x in inter.roads.clone() {
            let road = unwrap_contlog!(
                self.roads.get(x),
                "intersection has unexisting road in list"
            );
            queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, road);
            self.subscribers.dispatch(UpdateType::Road, road);

            let oend_id = unwrap_cont!(road.other_end(id));

            let other_end = unwrap_contlog!(
                self.intersections.get_mut(oend_id),
                "road is connected to unexisting intersection"
            );
            queue_construction_cut(
                &mut self.pending_construction_cuts,
                &self.environment,
                other_end,
            );
            other_end.update_interface_radius(&mut self.roads);
            queue_construction_cut(
                &mut self.pending_construction_cuts,
                &self.environment,
                other_end,
            );
            self.spatial_map.update_cut_influence(other_end);

            #[allow(clippy::indexing_slicing)] // borrowed before
            self.roads[x].update_lanes(&mut self.lanes, &mut self.parking, &self.environment);
        }

        #[allow(clippy::indexing_slicing)] // borrowed before
        let inter = &mut self.intersections[id];
        inter.update_traffic_control(&mut self.lanes, &self.roads);
        inter.update_turns(&self.lanes, &self.roads);

        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            inter,
        );
        self.spatial_map.update(inter);
    }

    fn invalidate_imported(&mut self, id: IntersectionID) {
        let direct = self.intersections[id].roads.clone();
        let mut junctions = BTreeSet::from([id]);
        junctions.extend(
            direct
                .iter()
                .filter_map(|road| self.roads[*road].other_end(id)),
        );
        let mut previous = BTreeMap::new();
        for &junction in &junctions {
            for &road in &self.intersections[junction].roads {
                previous.entry(road).or_insert_with(|| {
                    let r = &self.roads[road];
                    (r.interface_from(r.src), r.interface_from(r.dst))
                });
            }
        }
        for junction in junctions {
            let intersection = &mut self.intersections[junction];
            queue_construction_cut(
                &mut self.pending_construction_cuts,
                &self.environment,
                intersection,
            );
            intersection.update_interface_radius(&mut self.roads);
            queue_construction_cut(
                &mut self.pending_construction_cuts,
                &self.environment,
                intersection,
            );
            self.spatial_map.update_cut_influence(intersection);
        }

        let mut turns = BTreeSet::from([id]);
        let road_order = direct.iter().copied().chain(
            previous
                .keys()
                .copied()
                .filter(|road| !direct.contains(road)),
        );
        for road_id in road_order {
            let old_interfaces = previous[&road_id];
            let road = &mut self.roads[road_id];
            Self::reserve_import_lane(road);
            let changed =
                old_interfaces != (road.interface_from(road.src), road.interface_from(road.dst));
            if changed || direct.contains(&road_id) {
                queue_construction_cut(
                    &mut self.pending_construction_cuts,
                    &self.environment,
                    road,
                );
                self.subscribers.dispatch(UpdateType::Road, road);
                road.update_lanes(&mut self.lanes, &mut self.parking, &self.environment);
            }
            if changed {
                turns.extend([road.src, road.dst]);
            }
        }
        for junction in turns {
            let inter = &mut self.intersections[junction];
            if junction == id {
                inter.update_traffic_control(&mut self.lanes, &self.roads);
            }
            inter.update_turns(&self.lanes, &self.roads);
            self.spatial_map.update(inter);
            queue_construction_cut(
                &mut self.pending_construction_cuts,
                &self.environment,
                inter,
            );
            self.subscribers.dispatch(UpdateType::Road, inter);
        }
    }

    /// Only removes road from Roads and spatial map but keeps lots, buildings connection
    /// and potentially empty intersections.
    fn remove_raw_road(&mut self, road_id: RoadID) -> Option<Road> {
        let road = self.roads.remove(road_id)?;
        self.lane_topology_changed();

        self.spatial_map.remove(road_id);
        self.electricity.remove_object(road_id);

        for (id, _) in road.lanes_iter() {
            self.lanes.remove(id);
        }

        if let Some(i) = self.intersections.get_mut(road.src) {
            i.remove_road(road_id);
        }
        if let Some(i) = self.intersections.get_mut(road.dst) {
            i.remove_road(road_id);
        }

        Some(road)
    }

    #[allow(clippy::collapsible_else_if)]
    fn split_road(
        &mut self,
        split_road_id: RoadID,
        pos: Vec3,
        segment: usize,
    ) -> Option<RoadSplit> {
        info!("split_road {:?} {:?}", split_road_id, pos);

        let pat = self.roads.get(split_road_id)?.pattern(&self.lanes);
        let r = self.remove_raw_road(split_road_id)?;
        queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, &r);
        self.subscribers.dispatch(UpdateType::Road, &r);

        for (id, _) in r.lanes_iter() {
            self.parking.remove_to_reuse(id);
        }

        let id = self.add_intersection(pos - Vec3::z(ROAD_Z_OFFSET));

        let mut before: Vec<_> = r.points.iter().copied().take(segment).collect();
        // A split at an existing vertex replaces its height.
        while before.last().is_some_and(|point| point.xy() == pos.xy()) {
            before.pop();
        }
        before.push(pos);
        let after_split = r
            .points
            .iter()
            .copied()
            .skip(segment)
            .skip_while(|point| point.xy() == pos.xy());
        let mut after: Vec<_> = [pos].into_iter().chain(after_split).collect();
        before.dedup();
        after.dedup();
        let before = PolyLine3::new(before);
        let after = PolyLine3::new(after);

        let inherited_span = Some(r.elevated_span());
        let r1 = self.connect_with_span(
            r.src,
            id,
            &pat,
            RoadSegmentKind::Arbitrary(before),
            inherited_span,
        )?;
        let r2 = self.connect_with_span(
            id,
            r.dst,
            &pat,
            RoadSegmentKind::Arbitrary(after),
            inherited_span,
        )?;

        self.invalidate(r.src);
        self.invalidate(r.dst);
        self.invalidate(id);

        log::info!(
            "{} parking spots reused when splitting",
            self.parking.clean_reuse()
        );

        let roads = [r1, r2];
        let r1 = self.roads.get(r1)?;
        let r2 = self.roads.get(r2)?;

        let spatial = &mut self.spatial_map;

        for b in &r.connected_buildings {
            let b = self.buildings.get_mut(*b)?;

            let bpos = b.obb.center().z(b.height);

            let d1 = r1.points.project_dist2(bpos);
            let d2 = r2.points.project_dist2(bpos);

            b.connected_road = Some(match d1 < d2 {
                true => r1.id,
                false => r2.id,
            });
        }

        self.lots.retain(|_, lot| {
            if lot.parent != split_road_id {
                return true;
            }
            let p = lot.shape.corners[0].z(lot.height);
            let d1 = r1.points.project(p).distance(p);
            let d2 = r2.points.project(p).distance(p);
            if d1 < d2 {
                if d1 < r1.width * 0.5 + 1.5 {
                    lot.parent = r1.id;
                } else {
                    spatial.remove(lot.id);
                    return false;
                }
            } else {
                if d2 < r2.width * 0.5 + 1.5 {
                    lot.parent = r2.id;
                } else {
                    spatial.remove(lot.id);
                    return false;
                }
            }
            true
        });

        for b in r.connected_buildings {
            let b = &self.buildings[b];
            let road_id = b.connected_road.unwrap();
            self.roads[road_id].connected_buildings.push(b.id);

            self.electricity.add_edge(b.id, road_id);
        }

        Some(RoadSplit {
            original: split_road_id,
            junction: id,
            roads,
        })
    }

    pub(crate) fn merge_road(&mut self, road1: RoadID, road2: RoadID) -> Option<RoadID> {
        info!("merge_road {:?} {:?}", road1, road2);

        let r1 = self.roads.get(road1)?;
        let r2 = self.roads.get(road2)?;

        let same_inter = if r1.src == r2.src || r1.src == r2.dst {
            r1.src
        } else if r1.dst == r2.src || r1.dst == r2.dst {
            r1.dst
        } else {
            warn!("trying to merge roads that are not connected to each other");
            return None;
        };

        let mut pat1 = r1.pattern(&self.lanes);
        let mut pat2 = r2.pattern(&self.lanes);

        if r1.src == same_inter {
            std::mem::swap(&mut pat1.lanes_backward, &mut pat1.lanes_forward);
        }

        if r2.src != same_inter {
            std::mem::swap(&mut pat2.lanes_backward, &mut pat2.lanes_forward);
        }

        if pat1 != pat2 {
            log::info!("merge refused because patterns don't match");
            return None;
        }

        let r1_extremity = if r1.src == same_inter { r1.dst } else { r1.src };
        let r2_extremity = if r2.src == same_inter { r2.dst } else { r2.src };

        if r1_extremity == r2_extremity {
            log::info!("merge refused because connecting to itself");
            return None;
        }

        let r1 = self.remove_raw_road(road1)?;
        let r2 = self.remove_raw_road(road2)?;

        self.remove_intersection_inner(same_inter);

        queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, &r1);
        self.subscribers.dispatch(UpdateType::Road, &r1);
        queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, &r2);
        self.subscribers.dispatch(UpdateType::Road, &r2);

        for (id, _) in r2.lanes_iter().chain(r1.lanes_iter()) {
            self.parking.remove_to_reuse(id);
        }

        let mut new_polyline = r1.points;
        if r1.src == same_inter {
            new_polyline.reverse();
        }

        if r2.src == same_inter {
            new_polyline.extend(r2.points.iter().skip(1))
        } else {
            new_polyline.extend(r2.points.iter().rev().skip(1))
        }

        let new_r = self.connect(
            r1_extremity,
            r2_extremity,
            &pat1,
            RoadSegmentKind::Arbitrary(new_polyline),
        )?;

        let new_r = &mut self.roads[new_r];
        log::info!("merge_road new road is {:?}", new_r.id);

        log::info!(
            "{} parking spots reused when merging",
            self.parking.clean_reuse()
        );

        for b in r1
            .connected_buildings
            .iter()
            .chain(r2.connected_buildings.iter())
        {
            let b = self.buildings.get_mut(*b)?;
            b.connected_road = Some(new_r.id);
            new_r.connected_buildings.push(b.id);

            self.electricity.add_edge(b.id, new_r.id);
        }

        for lot in &mut self.lots.values_mut() {
            if lot.parent == road1 || lot.parent == road2 {
                lot.parent = new_r.id;
            }
        }

        let new_id = new_r.id;

        self.invalidate(r1_extremity);
        self.invalidate(r2_extremity);

        Some(new_id)
    }

    /// Returns None if one of the intersections don't exist
    pub(crate) fn connect(
        &mut self,
        src_id: IntersectionID,
        dst_id: IntersectionID,
        pattern: &LanePattern,
        segment: RoadSegmentKind,
    ) -> Option<RoadID> {
        self.connect_with_span(src_id, dst_id, pattern, segment, None)
    }

    fn connect_with_span(
        &mut self,
        src_id: IntersectionID,
        dst_id: IntersectionID,
        pattern: &LanePattern,
        segment: RoadSegmentKind,
        inherited_span: Option<bool>,
    ) -> Option<RoadID> {
        let src = self.intersections.get(src_id)?;
        let dst = self.intersections.get(dst_id)?;

        let gen_lots = !matches!(segment, RoadSegmentKind::Arbitrary(_));

        let rid = Road::make_with_span(
            src,
            dst,
            segment,
            pattern,
            &self.environment,
            &mut self.roads,
            &mut self.lanes,
            &mut self.parking,
            &mut self.spatial_map,
            inherited_span,
        );
        self.lane_topology_changed();

        self.electricity.add_object(rid);
        self.electricity.add_edge(src_id, rid);
        self.electricity.add_edge(dst_id, rid);

        #[allow(clippy::indexing_slicing)]
        let r = &self.roads[rid];

        self.intersections.get_mut(src_id)?.add_road(&self.roads, r);
        self.intersections.get_mut(dst_id)?.add_road(&self.roads, r);

        Lot::remove_intersecting_lots(self, rid);
        if gen_lots {
            Lot::generate_along_road(self, rid);
        }

        #[allow(clippy::indexing_slicing)]
        let r = &self.roads[rid];
        let mut b = r.boldline();
        b.expand(40.0);
        self.environment.remove_trees_near(&b, |tree_chunk| {
            self.subscribers
                .dispatch_chunk(UpdateType::Terrain, tree_chunk)
        });

        Some(rid)
    }

    // Public helpers
    pub fn project(&self, pos: Vec3, tolerance: f32, filter: ProjectFilter) -> MapProject {
        let mk_proj = move |kind| MapProject { pos, kind };

        let mut qroad = None;
        for pkind in self.spatial_map.query_around(pos.xy(), tolerance, filter) {
            match pkind {
                ProjectKind::Intersection(id) => {
                    let inter = unwrap_contlog!(self.intersections.get(id),
                        "Inter does not exist anymore, you seem to have forgotten to remove it from the spatial map.");

                    return MapProject {
                        pos: inter.pos,
                        kind: pkind,
                    };
                }
                ProjectKind::Lot(id) => {
                    return mk_proj(ProjectKind::Lot(id));
                }
                ProjectKind::Road(id) => {
                    if qroad.is_some() {
                        continue;
                    }
                    let road = unwrap_contlog!(self.roads.get(id),
                        "Road does not exist anymore, you seem to have forgotten to remove it from the spatial map.");

                    let projected = road.points.project(pos);
                    qroad = Some((id, projected));
                }
                ProjectKind::Building(id) => {
                    return mk_proj(ProjectKind::Building(id));
                }
                ProjectKind::Ground => {}
            }
        }

        if let Some((id, pos)) = qroad {
            return MapProject {
                pos,
                kind: ProjectKind::Road(id),
            };
        }

        mk_proj(ProjectKind::Ground)
    }

    pub fn get<T: MapObj>(&self, id: T) -> Option<&T::Obj> {
        MapObj::get(id, self)
    }

    pub fn is_empty(&self) -> bool {
        self.roads.is_empty() && self.lanes.is_empty() && self.intersections.is_empty()
    }

    pub fn roads(&self) -> &Roads {
        &self.roads
    }
    pub fn lanes(&self) -> &Lanes {
        &self.lanes
    }
    pub fn intersections(&self) -> &Intersections {
        &self.intersections
    }
    pub fn buildings(&self) -> &Buildings {
        &self.buildings
    }
    pub fn lots(&self) -> &Lots {
        &self.lots
    }
    pub fn spatial_map(&self) -> &SpatialMap {
        &self.spatial_map
    }

    pub fn building_overlaps(&self, obb: OBB) -> bool {
        self.spatial_map
            .query(obb, ProjectFilter::BUILDING)
            .next()
            .is_some()
    }

    pub fn find_road(&self, src: IntersectionID, dst: IntersectionID) -> Option<RoadID> {
        for &r in &self.intersections.get(src)?.roads {
            let road = unwrap_cont!(self.roads.get(r));
            if road.src == src && road.dst == dst {
                return Some(road.id);
            }
        }
        None
    }

    pub fn nearest_lane(&self, p: Vec3, kind: LaneKind, cutoff: Option<f32>) -> Option<LaneID> {
        let tryfind = |radius| {
            self.spatial_map()
                .query_around(p.xy(), radius, ProjectFilter::ROAD)
                .filter_map(|x| {
                    if let ProjectKind::Road(id) = x {
                        Some(id)
                    } else {
                        unsafe { std::hint::unreachable_unchecked() }
                    }
                })
                .filter_map(|id| self.roads().get(id))
                .flat_map(|road| road.lanes_iter())
                .filter(|&(_, x)| x == kind)
                .map(|(id, _)| &self.lanes[id])
                // Rebuilding the spatial index on load can reorder equidistant roads.
                .min_by_key(|lane| (OrderedFloat(lane.points.project_dist2(p)), lane.id))
        };

        if let Some(cutoff) = cutoff {
            return tryfind(cutoff).map(|v| v.id);
        }

        if let Some(lane) = tryfind(20.0) {
            return Some(lane.id);
        }

        if let Some(lane) = tryfind(100.0) {
            return Some(lane.id);
        }

        self.lanes
            .iter()
            .filter(|(_, x)| x.kind == kind)
            .min_by_key(|(id, lane)| (OrderedFloat(lane.points.project_dist2(p)), *id))
            .map(|(id, _)| id)
    }

    pub fn parking_to_drive(&self, spot: ParkingSpotID) -> Option<LaneID> {
        let spot = self.parking.get(spot)?;
        let park_lane = self.lanes.get(spot.parent)?;
        let road = self.roads.get(park_lane.parent)?;
        road.outgoing_lanes_from(park_lane.src)
            .iter()
            .rfind(|&&(_, kind)| kind == LaneKind::Driving)
            .map(|&(id, _)| id)
    }

    pub fn parking_to_drive_pos(&self, spot: ParkingSpotID) -> Option<Vec3> {
        let spot = self.parking.get(spot)?;
        let park_lane = self.lanes.get(spot.parent)?;
        let road = self.roads.get(park_lane.parent)?;
        let lane = road
            .outgoing_lanes_from(park_lane.src)
            .iter()
            .rfind(|&&(_, kind)| kind == LaneKind::Driving)
            .map(|&(id, _)| id)?;

        let (pos, _, dir) = self
            .lanes()
            .get(lane)?
            .points
            .project_segment_dir(spot.trans.pos);
        Some(pos - dir * 4.0)
    }

    #[cfg(not(debug_assertions))]
    pub fn check_invariants(&self) {}

    #[cfg(debug_assertions)]
    pub fn check_invariants(&self) {
        /*if std::env::var("MAP_INVARIANT_CHECK").is_err() {
            return;
        }*/
        for inter in self.intersections.values() {
            log::debug!("{:?}", inter.id);
            assert!(!inter.roads.is_empty());

            let mut last_angle = -f32::INFINITY;
            for &road in &inter.roads {
                let road = self.roads.get(road).expect("road does not exist");
                let ang = geom::pseudo_angle(road.dir_from(inter.id));
                assert!(ang >= last_angle);
                last_angle = ang;
            }

            for turn in inter.turns() {
                log::debug!("{:?}", turn.id);
                assert_eq!(turn.id.parent, inter.id);
                assert!(
                    self.lanes.contains_key(turn.id.src),
                    "Turn src doesn't exist for inter:{:?} src:{:?}",
                    inter.id,
                    turn.id.src,
                );
                assert!(
                    self.lanes.contains_key(turn.id.dst),
                    "{:?} {:?}",
                    inter.id,
                    turn.id.dst
                );
                assert!(turn.points.n_points() >= 2);
            }

            assert!(inter.pos.is_finite());
            assert!(self.spatial_map.contains(inter.id));
        }

        for lane in self.lanes.values() {
            log::debug!("{:?}", lane.id);
            assert!(!lane.points.is_empty());
            assert!(self.intersections.contains_key(lane.src), "{:?}", lane.src);
            assert!(self.intersections.contains_key(lane.dst), "{:?}", lane.dst);
            assert!(self.roads.contains_key(lane.parent), "{:?}", lane.parent);

            if matches!(lane.kind, LaneKind::Parking) {
                assert!(
                    self.parking.spots(lane.id).is_some(),
                    "no spots for {:?}",
                    lane.id
                )
            }
        }

        crate::map::electricity_cache::check_electricity_coherency(self);

        for b in self.external_train_stations.iter() {
            assert!(self.buildings.contains_key(*b));
        }

        for road in self.roads.values() {
            log::debug!("{:?}", road.id);
            let src = self.intersections.get(road.src).unwrap();
            assert!(src.roads.contains(&road.id));
            let dst = self.intersections.get(road.dst).unwrap();
            assert!(dst.roads.contains(&road.id));
            assert!(!road.points.is_empty());
            assert!(road.lanes_iter().next().is_some());
            assert!(
                road.points
                    .first()
                    .up(-crate::map::ROAD_Z_OFFSET)
                    .is_close(src.pos, 0.001),
                "First road point should be close to src intersection's position {:?} {:?} {:?} {:?}",
                road.points.first().up(-crate::map::ROAD_Z_OFFSET),
                src.pos,
                road.id,
                src.id,
            );
            assert!(
                road.points
                    .last()
                    .up(-crate::map::ROAD_Z_OFFSET)
                    .is_close(dst.pos, 0.001),
                "Last road point should be close to dst intersection's position {:?} {:?} {:?} {:?}",
                road.points.last().up(-crate::map::ROAD_Z_OFFSET),
                dst.pos,
                road.id,
                dst.id,
            );
            assert!(road.interfaced_points().n_points() >= 2);
            assert!(road.length() > 0.0);
            assert!(self.spatial_map.contains(road.id));

            for (id, _) in road.lanes_iter() {
                let v = self.lanes.get(id).expect("lane child does not exist");
                assert_eq!(v.parent, road.id);
            }

            for b in &road.connected_buildings {
                assert!(self.buildings.contains_key(*b));
                assert_eq!(self.buildings[*b].connected_road, Some(road.id));
            }

            // Road with parking lane has driving lane (incoming)
            let has_parking = road
                .incoming_lanes_to(road.src)
                .iter()
                .any(|(_, kind)| matches!(kind, LaneKind::Parking));

            if has_parking {
                let has_driving = road
                    .incoming_lanes_to(road.src)
                    .iter()
                    .any(|(_, kind)| matches!(kind, LaneKind::Driving));
                assert!(has_driving);
            }

            // Road with parking lane has driving lane (outgoing)
            let has_parking = road
                .outgoing_lanes_from(road.src)
                .iter()
                .any(|(_, kind)| matches!(kind, LaneKind::Parking));

            if has_parking {
                let has_driving = road
                    .outgoing_lanes_from(road.src)
                    .iter()
                    .any(|(_, kind)| matches!(kind, LaneKind::Driving));
                assert!(has_driving);
            }
        }

        for b in self.buildings.values() {
            log::debug!("{:?}", b.id);
            assert!(self.spatial_map.contains(b.id));
            if let Some(r) = b.connected_road {
                assert!(self.roads.contains_key(r));
                assert!(self.roads[r].connected_buildings.contains(&b.id));
            }
        }

        for lot in self.lots.values() {
            log::debug!("{:?}", lot.id);
            assert!(lot.shape.axis().iter().all(|x| x.mag() > 0.0));
            assert!(
                self.roads.contains_key(lot.parent),
                "Lot parent {:?} doesn't exist",
                lot.parent
            );
            assert!(self.spatial_map.contains(lot.id));
        }

        for obj in self.spatial_map.objects() {
            assert!(self.spatial_map.contains(*obj));
            log::debug!("{:?}", obj);
            assert!(obj.check_valid(self));
        }

        assert!(self.parking.reuse_spot.is_empty());

        log::info!("invariants checked");
    }
}

const MAX_WET_LINK_LANES: usize = 32;
const MAX_WATER_PROFILE_SAMPLES: usize = 65_536;

pub(crate) struct WaterTopologyWork {
    pub(crate) remaining: usize,
    profile_samples: usize,
}

impl WaterTopologyWork {
    pub(crate) fn new() -> Self {
        Self {
            remaining: super::MAX_WATER_BATCH_WORK,
            profile_samples: MAX_WATER_PROFILE_SAMPLES,
        }
    }

    fn check_profile_samples(&self, samples: usize) -> Result<(), WaterEditError> {
        if samples > self.profile_samples {
            return Err(WaterEditError::Limit);
        }
        Ok(())
    }

    pub(crate) fn spend_profile_samples(&mut self, samples: usize) {
        self.profile_samples -= samples;
    }
}

fn profile_sample_bound(length: f32) -> usize {
    // The profiler samples once per meter plus the endpoints. The lane clip
    // can extend a road shorter than two meters to a two-meter interface.
    (length.ceil() as usize).max(2).saturating_add(2)
}

pub(crate) fn wet_lane_count_fits(pattern: &LanePattern) -> bool {
    pattern
        .lanes_forward
        .len()
        .saturating_add(pattern.lanes_backward.len())
        <= MAX_WET_LINK_LANES
}

pub(crate) fn spend_water_work(remaining: &mut usize, units: usize) -> Result<(), WaterEditError> {
    if units > *remaining {
        *remaining = 0;
        Err(WaterEditError::Limit)
    } else {
        *remaining -= units;
        Ok(())
    }
}

fn queue_construction_cut(
    pending: &mut BTreeSet<TerrainChunkID>,
    environment: &Environment,
    object: &impl SpatialMapObject,
) {
    if let Some(bounds) = object.cut_influence() {
        // Include nodes exactly on the upper edge, including chunk seams.
        pending.extend(
            environment.covered_chunks(AABB::new_ll_ur(bounds.ll, bounds.ur + Vec2::splat(1.0))),
        );
    }
}

fn water_tile_bounds(tile: WaterTileCoord) -> AABB {
    AABB::new_ll_size(tile.corner(), Vec2::splat(super::terrain::CELL_SIZE))
}

impl MapProject {
    pub fn ground(pos: Vec3) -> Self {
        Self {
            pos,
            kind: ProjectKind::Ground,
        }
    }

    pub fn is_ground(&self) -> bool {
        matches!(self.kind, ProjectKind::Ground)
    }
}

pub trait MapObj {
    type Obj;
    fn get(self, map: &Map) -> Option<&Self::Obj>;
}

impl MapObj for RoadID {
    type Obj = Road;
    fn get(self, map: &Map) -> Option<&Road> {
        map.roads.get(self)
    }
}

impl MapObj for LaneID {
    type Obj = Lane;
    fn get(self, map: &Map) -> Option<&Lane> {
        map.lanes.get(self)
    }
}

impl MapObj for IntersectionID {
    type Obj = Intersection;
    fn get(self, map: &Map) -> Option<&Intersection> {
        map.intersections.get(self)
    }
}

impl MapObj for BuildingID {
    type Obj = Building;
    fn get(self, map: &Map) -> Option<&Building> {
        map.buildings.get(self)
    }
}

impl MapObj for LotID {
    type Obj = Lot;
    fn get(self, map: &Map) -> Option<&Lot> {
        map.lots.get(self)
    }
}
