use super::height_override::{ConstructionCut, CutHeights};
use super::terrain::TERRAIN_CHUNK_RESOLUTION;
use crate::map::electricity_cache::ElectricityCache;
use crate::map::pathfinding::WalkingRoutes;
use crate::map::procgen::gen_exterior_house;
use crate::map::reachability::DrivingReachability;
use crate::map::{
    Building, BuildingID, BuildingKind, ConnectionError, ConnectionOutcome, CrossingEndpoint,
    CrossingWork, Environment, GrowableBusinessState, GrowableLotError, GrowableSiteState,
    Intersection, IntersectionID, JunctionRules, Lane, LaneID, LaneKind, LanePattern,
    LaneScoringPilot, Lot, LotID, LotKind, LotLayout, MapSubscriber, MapSubscribers, ParkingSpotID,
    ParkingSpots, PreparedBuilding, PreparedEffects, PreparedGrowableAppearance,
    PreparedTopologyEdit, ProjectFilter, ProjectKind, Road, RoadID, RoadSegmentKind, SpatialMap,
    SpatialMapObject, TerraformKind, TerrainChunkID, UpdateType, WaterEditError, Zone,
    MAX_DECK_TILE_TESTS, ROAD_Z_OFFSET,
};
use geom::OBB;
use geom::{Intersect, PolyLine3, Shape, Vec2, Vec3, AABB};
use ordered_float::OrderedFloat;
use prototypes::{BuildingGen, GrowableCompanyID, Tick};
use serde::{Deserialize, Serialize};
use slotmapd::{HopSlotMap, Key};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[path = "water_edit.rs"]
mod water_edit;
pub use water_edit::WaterEditPreview;

pub type Roads = HopSlotMap<RoadID, Road>;
pub type Lanes = HopSlotMap<LaneID, Lane>;
pub type Intersections = HopSlotMap<IntersectionID, Intersection>;
pub type Buildings = HopSlotMap<BuildingID, Building>;
pub type Lots = HopSlotMap<LotID, Lot>;

pub(crate) const MAX_CONFIGURED_JUNCTIONS: usize = 1024;

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SavedJunctionPolicies {
    pub(crate) revision: u64,
    pub(crate) driving_revision: u64,
    #[serde(
        deserialize_with = "crate::world_command::bounded_command_vec::<_, _, MAX_CONFIGURED_JUNCTIONS>"
    )]
    pub(crate) rules: Vec<(IntersectionID, JunctionRules)>,
}

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
    pub(crate) roads: [RoadID; 2],
}

pub(crate) struct ConnectedRoad {
    pub src: IntersectionID,
    pub dst: IntersectionID,
    pub road: RoadID,
    pub splits: [Option<RoadSplit>; 2],
}

pub(crate) struct PreparedSpecialBuilding {
    building: super::PreparedBuilding,
    shape: OBB,
    kind: BuildingKind,
}

/// Map-side placement validated against a specific live lot and frontage.
pub(crate) struct PreparedGrowable {
    lot: LotID,
    frontage: RoadID,
    shape: OBB,
    generator: BuildingGen,
    attachment: Vec3,
    map_identity: u64,
    grade_revision: (u64, u64, u64),
    building: PreparedBuilding,
}

impl PreparedSpecialBuilding {
    pub(crate) fn fire_frontage(&self) -> (Option<RoadID>, Vec3, OBB) {
        (self.building.connected_road(), self.building.door(), self.shape)
    }
}

pub struct Map {
    pub(crate) service_frontages: BTreeMap<RoadID, Vec<(BuildingID, OBB)>>,
    pub(crate) active_service_frontages: BTreeSet<(RoadID, BuildingID)>,
    pub(super) land_ownership: super::LandOwnership,
    pub(super) land_frontage: super::land_frontage::LandFrontage,
    pub(crate) roads: Roads,
    pub(crate) lanes: Lanes,
    pub(crate) intersections: Intersections,
    pub(crate) buildings: Buildings,
    pub(crate) fire_cohorts: BTreeSet<(u8, BuildingID)>,
    pub(super) ruins: BTreeSet<BuildingID>,
    pub(super) ruin_land_value: BTreeMap<(i32, i32), u32>,
    pub(crate) lots: Lots,
    pub(super) lots_by_road: BTreeMap<RoadID, BTreeSet<LotID>>,
    pub(crate) spatial_map: SpatialMap,
    pub(crate) external_train_stations: Vec<BuildingID>,
    pub(crate) lot_origins: BTreeMap<BuildingID, LotOrigin>,
    // Derived identity for caches whose lifetime can exceed this map's.
    pub(super) identity: u64,
    pub(crate) walking_topology_generation: u64,
    pub(crate) driving_policy_generation: u64,
    pub(super) junction_policy_revision: u64,
    pub(super) roundabouts: BTreeSet<IntersectionID>,
    // Old paths changed during map decoding, consumed at the full snapshot boundary.
    pub(crate) migrated_roundabout_paths: BTreeMap<super::TraverseKind, Vec<Vec3>>,
    pub(super) junction_rules: BTreeMap<IntersectionID, JunctionRules>,
    pub(super) lane_scoring: Option<LaneScoringPilot>,
    // Derived from lanes and turns. Map deserialization starts with an empty cache.
    pub(super) driving_reachability: OnceLock<DrivingReachability>,
    pub(super) walking_routes: OnceLock<WalkingRoutes>,
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
    pub(super) construction_cut_revision: u64,
    pub(super) water_services: super::WaterServiceCache,
}

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
            spatial_map.set_additional_occupancy(
                intersection.id.into(),
                super::junction_support_bounds(intersection, &self.roads),
            );
        }
        for lot in self.lots.values() {
            spatial_map.insert(lot);
        }

        // Like a loaded map, the clone starts with its own subscriptions and identity.
        Self {
            land_ownership: self.land_ownership.clone(),
            land_frontage: self.land_frontage.clone(),
            lots_by_road: self.lots_by_road.clone(),
            roads: self.roads.clone(),
            lanes: self.lanes.clone(),
            intersections: self.intersections.clone(),
            buildings: self.buildings.clone(),
            fire_cohorts: self.fire_cohorts.clone(),
            ruins: self.ruins.clone(),
            ruin_land_value: self.ruin_land_value.clone(),
            lots: self.lots.clone(),
            spatial_map,
            external_train_stations: self.external_train_stations.clone(),
            lot_origins: self.lot_origins.clone(),
            walking_topology_generation: self.walking_topology_generation,
            service_frontages: self.service_frontages.clone(),
            active_service_frontages: self.active_service_frontages.clone(),
            driving_policy_generation: self.driving_policy_generation,
            junction_policy_revision: self.junction_policy_revision,
            roundabouts: self.roundabouts.clone(),
            junction_rules: self.junction_rules.clone(),
            lane_scoring: self.lane_scoring.clone(),
            import_lane_reserve: self.import_lane_reserve,
            lot_layout: self.lot_layout,
            no_frontage: self.no_frontage.clone(),
            pending_construction_cuts: self.pending_construction_cuts.clone(),
            electricity: self.electricity.clone(),
            environment: self.environment.clone(),
            parking: self.parking.clone(),
            water_services: self.water_services.clone(),
            ..Self::empty()
        }
    }
}

impl Map {
    // Public API
    pub fn empty() -> Self {
        static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);
        Self {
            land_ownership: super::LandOwnership::Unrestricted,
            land_frontage: Default::default(),
            lots_by_road: BTreeMap::new(),
            roads: Roads::default(),
            lanes: Lanes::default(),
            intersections: Intersections::default(),
            parking: ParkingSpots::default(),
            buildings: Buildings::default(),
            fire_cohorts: BTreeSet::new(),
            ruins: BTreeSet::new(),
            ruin_land_value: BTreeMap::new(),
            lots: Lots::default(),
            environment: Environment::default(),
            spatial_map: SpatialMap::default(),
            external_train_stations: Default::default(),
            lot_origins: BTreeMap::new(),
            identity: NEXT_IDENTITY
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("map identities exhausted"),
            walking_topology_generation: 0,
            service_frontages: BTreeMap::new(),
            active_service_frontages: BTreeSet::new(),
            driving_policy_generation: 0,
            junction_policy_revision: 0,
            roundabouts: BTreeSet::new(),
            migrated_roundabout_paths: BTreeMap::new(),
            junction_rules: BTreeMap::new(),
            lane_scoring: None,
            driving_reachability: OnceLock::new(),
            walking_routes: OnceLock::new(),
            import_lane_reserve: false,
            lot_layout: LotLayout::default(),
            no_frontage: BTreeSet::new(),
            electricity: Default::default(),
            pending_construction_cuts: BTreeSet::new(),
            construction_cut_revision: 0,
            water_services: super::WaterServiceCache::default(),
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
    pub fn land_ownership(&self) -> &super::LandOwnership {
        &self.land_ownership
    }

    pub(crate) fn install_land_ownership(&mut self, ownership: super::LandOwnership) {
        assert!(!self.land_ownership.is_tiled());
        self.land_ownership = ownership;
    }

    /// Check the commissioned roots against their live geometry and relationships.
    pub(crate) fn validate_commissioned_reserves(
        &self,
        recipe: super::LandRecipe,
    ) -> Result<(), super::LandSetupError> {
        use super::{GatewayRole, LandRecipe, LandSetupError, Pathfinder, ProtectedRoot};
        let land = self.land_ownership();
        let Some(extent) = land.extent() else {
            return Ok(());
        };
        let bounds = extent.bounds();
        let contained = |point: Vec3| point.is_finite() && bounds.contains(point.xy());
        let roots: BTreeSet<_> = land.reserves().collect();
        for role in [GatewayRole::Highway, GatewayRole::Rail] {
            let invalid = LandSetupError::InvalidGateway(role);
            let mut graph = BTreeMap::<_, BTreeSet<_>>::new();
            for &root in &roots {
                let ProtectedRoot::Road(root_role, id) = root else {
                    continue;
                };
                if root_role != role {
                    continue;
                }
                let road = self.get(id).ok_or(invalid)?;
                if !road.width.is_finite()
                    || road.width <= 0.0
                    || !road.points().iter().copied().all(contained)
                    || !super::land_deck_triangles(road).flatten().all(contained)
                {
                    return Err(invalid);
                }
                let mut has_role_lane = false;
                for (id, kind) in road.lanes_iter() {
                    let lane = self.get(id).ok_or(invalid)?;
                    if lane.parent != road.id
                        || lane.kind != kind
                        || !lane.points.iter().copied().all(contained)
                        || (role == GatewayRole::Rail && !kind.is_rail())
                        || (role == GatewayRole::Highway && kind.is_rail())
                    {
                        return Err(invalid);
                    }
                    has_role_lane |= if role == GatewayRole::Rail {
                        kind.is_rail()
                    } else {
                        kind == LaneKind::Driving
                    };
                }
                if !has_role_lane {
                    return Err(invalid);
                }
                for (end, other) in [(road.src, road.dst), (road.dst, road.src)] {
                    if !roots.contains(&ProtectedRoot::Junction(role, end))
                        || !self
                            .get(end)
                            .is_some_and(|junction| junction.roads.contains(&road.id))
                    {
                        return Err(invalid);
                    }
                    graph.entry(end).or_default().insert(other);
                }
            }
            let Some((&first, _)) = graph.first_key_value() else {
                return Err(LandSetupError::MissingGateway(role));
            };
            let mut visited = BTreeSet::new();
            let mut queue = vec![first];
            while let Some(id) = queue.pop() {
                if visited.insert(id) {
                    queue.extend(graph[&id].iter().copied());
                }
            }
            if visited.len() != graph.len() {
                return Err(invalid);
            }
            let mut terminals = Vec::new();
            for (id, adjacent) in graph {
                let junction = self.get(id).ok_or(invalid)?;
                let outline = self.land_junction_outline(id).map_err(|_| invalid)?;
                if !contained(junction.pos) || outline.iter().any(|&point| !bounds.contains(point))
                {
                    return Err(invalid);
                }
                if adjacent.len() == 1 {
                    terminals.push(junction.pos.xy());
                }
            }
            // Native V1 commissions the road and rail end interfaces eight
            // metres inside the edge. Authored V1 also supports flush ends.
            const TERMINAL_INSET: f32 = 8.01;
            let west = terminals.iter().any(|point| point.x <= TERMINAL_INSET);
            let east = terminals
                .iter()
                .any(|point| point.x >= bounds.ur.x - TERMINAL_INSET);
            let commissioned = if matches!(recipe, LandRecipe::AuthoredV1 { .. }) {
                let faces = |point: &Vec2| {
                    u8::from(point.x <= TERMINAL_INSET)
                        | (u8::from(point.x >= bounds.ur.x - TERMINAL_INSET) << 1)
                        | (u8::from(point.y <= TERMINAL_INSET) << 2)
                        | (u8::from(point.y >= bounds.ur.y - TERMINAL_INSET) << 3)
                };
                terminals.iter().enumerate().any(|(index, first)| {
                    let first = faces(first);
                    first != 0
                        && terminals[index + 1..].iter().any(|second| {
                            let second = faces(second);
                            second != 0 && (first | second).count_ones() >= 2
                        })
                })
            } else {
                west && east
            };
            if !commissioned {
                return Err(invalid);
            }
        }

        let role = GatewayRole::Freight;
        let invalid = LandSetupError::InvalidGateway(role);
        let mut external = false;
        let mut local = false;
        for root in roots {
            let ProtectedRoot::Building(GatewayRole::Freight, id) = root else {
                continue;
            };
            let building = self.get(id).ok_or(invalid)?;
            if !contained(building.door_pos)
                || !building.height.is_finite()
                || building
                    .obb
                    .corners
                    .iter()
                    .any(|&point| !bounds.contains(point))
                || building
                    .zone
                    .as_ref()
                    .is_some_and(|zone| zone.poly.iter().any(|&point| !bounds.contains(point)))
            {
                return Err(invalid);
            }
            let center = building.obb.center().z(building.height);
            let boarding = match building.kind {
                BuildingKind::ExternalTrading if self.external_train_stations.contains(&id) => {
                    center
                }
                BuildingKind::RailFreightStation(_) => crate::souls::freight_station::train_stop(
                    &geom::Transform::new_dir(center, building.obb.axis()[1].z(0.0).normalize()),
                ),
                _ => return Err(invalid),
            };
            let lane = super::PathKind::Rail
                .nearest_lane(self, boarding)
                .and_then(|id| self.get(id))
                .filter(|lane| {
                    land.reserves()
                        .any(|root| root == ProtectedRoot::Road(GatewayRole::Rail, lane.parent))
                })
                .ok_or(invalid)?;
            let rail = lane.points.project(boarding);
            match building.kind {
                BuildingKind::ExternalTrading if building.obb.contains(rail.xy()) => {
                    external = true
                }
                BuildingKind::RailFreightStation(_) if rail.is_close(boarding, 120.0) => {
                    local = true
                }
                _ => return Err(invalid),
            }
        }
        if !external || !local {
            return Err(LandSetupError::MissingGateway(role));
        }
        Ok(())
    }

    pub(crate) fn buy_prepared_tile(&mut self, tile: super::LandTileId) {
        self.land_ownership.purchase(tile);
    }

    pub(crate) fn needs_topology_preparation(&self) -> bool {
        self.land_ownership.is_tiled() || self.environment.has_water_configuration()
    }

    pub fn junction_rules(&self, id: IntersectionID) -> Option<&JunctionRules> {
        self.junction_rules.get(&id)
    }

    pub fn junction_policy_revision(&self) -> u64 {
        self.junction_policy_revision
    }

    pub fn allows_turn(&self, turn: crate::map::TurnID) -> bool {
        self.get(turn.parent)
            .is_some_and(|junction| junction.find_turn(turn).is_some())
    }

    pub(crate) fn has_junction_rules(&self) -> bool {
        !self.junction_rules.is_empty()
    }

    /// Whether every junction uses stock traffic. Rules only affect their own junction.
    pub fn uses_stock_traffic(&self) -> bool {
        !self.has_junction_rules()
    }

    pub(crate) fn needs_junction_entry_checks(&self) -> bool {
        self.has_junction_rules() || !self.roundabouts.is_empty()
    }

    pub(crate) fn junction_rules_near(&self, position: Vec3, margin: f32) -> bool {
        self.has_junction_rules()
            && self.spatial_map
                .query_around(position.xy(), margin, ProjectFilter::INTER)
                .any(|candidate| matches!(candidate, ProjectKind::Intersection(id)
                    if self.junction_rules(id).is_some()
                        && self.get(id).is_some_and(|junction| (position.z - junction.pos.z).abs() < 1.0)))
    }

    pub(crate) fn has_junction_controller(&self) -> bool {
        !self.roundabouts.is_empty()
            || self
                .junction_rules
                .values()
                .any(|rules| !rules.approaches().is_empty())
    }

    pub(crate) fn is_yield_roundabout(&self, junction: IntersectionID) -> bool {
        self.roundabouts.contains(&junction)
    }

    pub(crate) fn configured_junctions(&self) -> impl Iterator<Item = IntersectionID> + '_ {
        self.roundabouts
            .iter()
            .copied()
            .chain(self.junction_rules.iter().filter_map(|(&id, rules)| {
                (!rules.approaches().is_empty() && !self.roundabouts.contains(&id)).then_some(id)
            }))
    }

    pub(crate) fn junction_rule_count(&self) -> usize {
        self.junction_rules.len()
    }

    fn check_junction_edit(&self, id: IntersectionID, changes_geometry: bool) -> Result<(), ConnectionError> {
        if !self.has_junction_rules() && self.active_service_frontages.is_empty() {
            return Ok(());
        }
        let junction = self.get(id).ok_or(ConnectionError::InvalidEndpoint)?;
        if junction.roads.len() > 128 {
            return Err(ConnectionError::WorkLimit);
        }
        if changes_geometry && junction.roads.iter().any(|&road| self.active_service_road(road)) {
            return Err(ConnectionError::OccupiedServiceAccess);
        }
        if self.junction_rules(id).is_some()
            || junction.roads.iter().any(|&road| {
                self.get(road)
                    .and_then(|road| road.other_end(id))
                    .is_some_and(|end| self.junction_rules(end).is_some())
            })
        {
            return Err(ConnectionError::Stale);
        }
        Ok(())
    }

    fn check_junction_topology_edit(&self, id: IntersectionID) -> Result<(), ConnectionError> {
        self.check_junction_edit(id, true)
    }

    pub(crate) fn active_service_road(&self, road: RoadID) -> bool {
        self.active_service_frontages
            .range((road, BuildingID::from(slotmapd::KeyData::from_ffi(0)))..)
            .next()
            .is_some_and(|&(found, _)| found == road)
    }

    fn check_connection_junctions(
        &self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
    ) -> Result<(), ConnectionError> {
        if !self.has_junction_rules() && self.active_service_frontages.is_empty() {
            return Ok(());
        }
        for endpoint in [from, to] {
            match endpoint.kind {
                ProjectKind::Intersection(id) => self.check_junction_topology_edit(id)?,
                ProjectKind::Road(id) => {
                    let road = self.get(id).ok_or(ConnectionError::InvalidEndpoint)?;
                    self.check_junction_topology_edit(road.src)?;
                    self.check_junction_topology_edit(road.dst)?;
                }
                _ => (),
            }
        }
        Ok(())
    }

    pub(crate) fn install_junction_rules(&mut self, id: IntersectionID, rules: JunctionRules) {
        let previous = self.junction_rules.get(&id);
        let old_mask = previous.map_or(&[][..], JunctionRules::forbidden);
        if old_mask != rules.forbidden() {
            self.driving_graph_changed();
            self.intersections[id].apply_turn_mask(Some(&rules));
        }
        if rules.is_empty() {
            self.junction_rules.remove(&id);
        } else {
            self.junction_rules.insert(id, rules);
        }
        self.junction_policy_revision += 1;
    }

    pub(crate) fn save_junction_policies(&self) -> SavedJunctionPolicies {
        SavedJunctionPolicies {
            revision: self.junction_policy_revision,
            driving_revision: self.driving_policy_generation,
            rules: self
                .junction_rules
                .iter()
                .map(|(&id, rules)| (id, rules.clone()))
                .collect(),
        }
    }

    pub(crate) fn load_junction_policies(
        &mut self,
        state: SavedJunctionPolicies,
    ) -> std::io::Result<()> {
        let invalid =
            || std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid junction policies");
        if state.revision == u64::MAX
            || state.driving_revision == u64::MAX
            || state.rules.windows(2).any(|pair| pair[0].0 >= pair[1].0)
        {
            return Err(invalid());
        }
        for (id, rules) in &state.rules {
            if rules.is_empty() {
                return Err(invalid());
            }
            rules.validate(self, *id).map_err(|_| invalid())?;
            let mut geometry = crate::world_command::PrepareBudget::for_preview(280_000);
            super::road_edit::reserve_junction(self, *id, &mut geometry)
                .map_err(|_| invalid())?;
            let mut budget = crate::world_command::PrepareBudget::for_preview(16_384);
            rules
                .check_rail_sweeps(self, *id, &mut budget)
                .map_err(|_| invalid())?;
        }
        for (id, rules) in state.rules {
            self.intersections[id].apply_turn_mask(Some(&rules));
            self.junction_rules.insert(id, rules);
        }
        self.junction_policy_revision = state.revision;
        self.driving_policy_generation = state.driving_revision;
        Ok(())
    }

    pub(crate) fn validate_junction_policy_land(&self) -> Result<(), super::LandDenial> {
        for (&id, rules) in &self.junction_rules {
            self.authorize_land_effects(PreparedEffects::Junction(id))?;
            if !rules.approaches().is_empty() {
                for &road in &self.intersections[id].roads {
                    self.mutable_object(road.into())?;
                }
            }
            for turn in rules.forbidden() {
                for id in [turn.src, turn.dst] {
                    let lane = self.get(id).ok_or(super::LandDenial::RequiresMutableAccess)?;
                    self.mutable_object(lane.parent.into())?;
                }
            }
        }
        Ok(())
    }

    pub fn environment(&self) -> &Environment {
        &self.environment
    }

    pub(crate) fn has_imported_lane_reserves(&self) -> bool {
        self.import_lane_reserve
    }

    #[cfg(test)]
    pub(crate) fn environment_mut_for_test(&mut self) -> &mut Environment {
        &mut self.environment
    }

    /// Install authored terrain into a new, empty map.
    pub fn install_environment(&mut self, environment: Environment) -> bool {
        if self.land_ownership.is_tiled() {
            return false;
        }
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
        if !self.pending_construction_cuts.is_empty() {
            self.construction_cut_revision += 1;
        }
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

    pub fn update_intersection(&mut self, id: IntersectionID, f: impl Fn(&mut Intersection)) {
        let _ = self.try_update_intersection(id, f);
    }

    pub(crate) fn try_update_intersection(
        &mut self,
        id: IntersectionID,
        f: impl FnOnce(&mut Intersection),
    ) -> Result<(), ConnectionError> {
        let inter = self
            .intersections
            .get(id)
            .ok_or(ConnectionError::InvalidEndpoint)?;
        let mut next = inter.clone();
        f(&mut next);
        if next.id != id || next.pos != inter.pos || next.roads != inter.roads {
            return Err(ConnectionError::InvalidEndpoint);
        }
        let changes_geometry = next.turn_policy.roundabout != inter.turn_policy.roundabout;
        self.check_junction_edit(id, changes_geometry)?;
        if !changes_geometry && inter.roads.iter().any(|road| self.service_frontages.contains_key(road)) {
            self.intersections[id] = next;
            self.refresh_junction_paths(id, true);
            self.driving_graph_changed();
            self.subscribers.dispatch(UpdateType::Road, &self.intersections[id]);
            self.check_invariants();
            return Ok(());
        }
        if self.environment.strict_water_siting()
            || self.land_ownership.is_tiled()
            || changes_geometry
        {
            super::prepare_junction_update(self, &next, &mut WaterTopologyWork::new().geometry())?;
        }
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            inter,
        );
        self.intersections[id] = next;
        self.invalidate(id);
        self.check_invariants();
        Ok(())
    }

    pub(crate) fn prepare_intersection_policy(
        &self,
        id: IntersectionID,
        turn: super::TurnPolicy,
        light: super::LightPolicy,
    ) -> Result<(), ConnectionError> {
        let mut next = self
            .get(id)
            .ok_or(ConnectionError::InvalidEndpoint)?
            .clone();
        let changed_roundabout = next.turn_policy.roundabout != turn.roundabout;
        self.check_junction_edit(id, changed_roundabout)?;
        next.turn_policy = turn;
        next.light_policy = light;
        if changed_roundabout
            || self.environment.strict_water_siting()
            || self.land_ownership.is_tiled()
        {
            super::prepare_junction_update(self, &next, &mut WaterTopologyWork::new().geometry())?;
        }
        Ok(())
    }

    pub fn remove_intersection(&mut self, src: IntersectionID) {
        let _ = self.try_remove_intersection(src);
    }

    pub(crate) fn prepare_remove_intersection(
        &self,
        src: IntersectionID,
    ) -> Result<(), ConnectionError> {
        if !self.intersections.contains_key(src) {
            return Err(ConnectionError::InvalidEndpoint);
        }
        self.check_junction_topology_edit(src)?;
        if self.has_junction_rules() {
            for &road in &self.intersections[src].roads {
                if let Some(dst) = self.get(road).and_then(|road| road.other_end(src)) {
                    self.check_junction_topology_edit(dst)?;
                }
            }
        }
        if self.environment.strict_water_siting() || self.land_ownership.is_tiled() {
            super::prepare_junction_removal(self, src, &mut WaterTopologyWork::new().geometry())?;
        }
        Ok(())
    }

    pub(crate) fn try_remove_intersection(
        &mut self,
        src: IntersectionID,
    ) -> Result<(), ConnectionError> {
        self.prepare_remove_intersection(src)?;
        self.remove_intersection_inner(src);
        self.check_invariants();
        Ok(())
    }

    fn remove_intersection_inner(&mut self, src: IntersectionID) {
        let inter = unwrap_ret!(self.intersections.remove(src));
        self.roundabouts.remove(&src);
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

    pub(crate) fn authorize_building_removal(
        &self,
        id: BuildingID,
    ) -> Result<(), super::LandDenial> {
        let mut remaining = super::MAX_WATER_BATCH_WORK;
        self.authorize_building_removal_budgeted(id, &mut remaining)
    }

    pub(crate) fn authorize_building_removal_budgeted(
        &self,
        id: BuildingID,
        remaining: &mut usize,
    ) -> Result<(), super::LandDenial> {
        let building = self.get(id).ok_or(super::LandDenial::ForbiddenOperation)?;
        super::spend_water_work(remaining, 1 + building.zone.as_ref().map_or(0, |zone| zone.poly.len()))
            .map_err(|_| super::LandDenial::WorkLimit)?;
        self.authorize_land_effects(super::PreparedEffects::Building(id))?;
        self.authorize_projected_construction_cuts(
            &BTreeSet::from([id.into()]),
            &[],
            remaining,
        )
        .map_err(|error| match error {
            WaterEditError::LandDenied(reason) => reason,
            _ => super::LandDenial::WorkLimit,
        })
    }

    pub(crate) fn prepare_demolition_clearance(
        &self,
        id: BuildingID,
        remaining: &mut usize,
    ) -> Result<(), crate::souls::demolition::ClearanceBlocker> {
        use crate::souls::demolition::ClearanceBlocker;
        let building = self.get(id).ok_or(ClearanceBlocker::Land(super::LandDenial::ForbiddenOperation))?;
        super::spend_water_work(remaining, 1 + building.zone.as_ref().map_or(0, |zone| zone.poly.len()))
            .map_err(|_| ClearanceBlocker::WorkLimit)?;
        self.authorize_land_effects(super::PreparedEffects::Building(id))
            .map_err(ClearanceBlocker::Land)?;
        if !Building::dry_footprint(&self.environment, &building.obb, building.zone.as_ref()) {
            return Err(ClearanceBlocker::Water);
        }
        self.authorize_projected_construction_cuts(&BTreeSet::from([id.into()]), &[], remaining)
            .map_err(|error| match error {
                WaterEditError::LandDenied(reason) => ClearanceBlocker::Land(reason),
                WaterEditError::Limit => ClearanceBlocker::WorkLimit,
                _ => ClearanceBlocker::Water,
            })
    }

    pub(crate) fn authorize_building_grade(
        &self,
        shape: OBB,
        height: f32,
    ) -> Result<(), super::LandDenial> {
        let mut remaining = super::MAX_WATER_BATCH_WORK;
        self.authorize_projected_construction_cuts(
            &BTreeSet::new(),
            &[ConstructionCut::Building(shape, height)],
            &mut remaining,
        )
        .map_err(|error| match error {
            WaterEditError::LandDenied(reason) => reason,
            _ => super::LandDenial::WorkLimit,
        })
    }

    pub(crate) fn close_building(&mut self, id: BuildingID) -> Option<Building> {
        let building = self.buildings.get_mut(id)?;
        if building.kind == BuildingKind::SiteRemnant {
            return None;
        }
        let original = building.clone();
        building.kind = BuildingKind::SiteRemnant;
        self.fire_cohorts.remove(&crate::souls::events::cohort_key(id));
        self.water_services.building_removed(&self.environment, id);
        self.electricity.remove_object(id);
        self.external_train_stations.retain(|target| *target != id);
        self.subscribers.dispatch(UpdateType::Building, building);
        Some(original)
    }

    pub fn remove_building(&mut self, b: BuildingID) -> Option<Building> {
        self.authorize_building_removal(b).ok()?;
        self.clear_prepared_building(b)
    }

    pub(crate) fn clear_prepared_building(&mut self, id: BuildingID) -> Option<Building> {
        let b = self.buildings.remove(id)?;
        if self.ruins.remove(&id) {
            self.update_ruin_land_value(b.obb.center(), false);
        }
        self.fire_cohorts.remove(&crate::souls::events::cohort_key(id));
        self.water_services.building_removed(&self.environment, b.id);
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
        let connected = self.try_connect(from, to, interpoint, pattern).ok()?;
        Some((connected.destination, connected.road))
    }

    pub fn try_connect(
        &mut self,
        from: MapProject,
        to: MapProject,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
    ) -> Result<ConnectionOutcome, ConnectionError> {
        let mut work = self
            .needs_topology_preparation()
            .then(WaterTopologyWork::new);
        let (from, to, prepared) =
            self.prepare_connection(from, to, elbow, pattern, work.as_mut())?;
        self.make_resolved_connection(from, to, elbow, pattern, prepared, work.as_mut())
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
        mut work: Option<&mut WaterTopologyWork>,
    ) -> Result<
        (
            ConnectionEndpoint,
            ConnectionEndpoint,
            Option<PreparedTopologyEdit>,
        ),
        ConnectionError,
    > {
        let mut local;
        if work.is_none()
            && (self.land_ownership.is_tiled()
                || self.checks_rail_sweeps(pattern)
                || !self.active_service_frontages.is_empty())
        {
            local = WaterTopologyWork::new();
            let (from, to, prepared) =
                self.prepare_connection(from, to, elbow, pattern, Some(&mut local))?;
            return Ok((from, to, prepared));
        }
        let endpoints = if let Some(work) = work.as_deref_mut() {
            if !wet_lane_count_fits(pattern) {
                return Err(ConnectionError::WorkLimit);
            }
            self.resolve_water_connection_endpoint(from, &mut work.remaining)?
                .zip(self.resolve_water_connection_endpoint(to, &mut work.remaining)?)
        } else {
            self.resolve_connection_endpoint(from)
                .zip(self.resolve_connection_endpoint(to))
        };
        let (from, to) = endpoints.ok_or(ConnectionError::InvalidEndpoint)?;
        self.check_connection_junctions(from, to)?;
        if !Self::can_connect(from, to, elbow, pattern) {
            return Err(ConnectionError::InvalidEndpoint);
        }
        let prepared = if let Some(work) = work {
            let (samples, prepared) =
                self.prepare_resolved_connection_inner(from, to, elbow, pattern, true, work)?;
            work.spend_profile_samples(samples);
            prepared
        } else {
            None
        };
        Ok((from, to, prepared))
    }

    pub(crate) fn prepare_resolved_connection(
        &self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
        work: &mut WaterTopologyWork,
    ) -> Result<(usize, Option<PreparedTopologyEdit>), ConnectionError> {
        self.prepare_resolved_connection_inner(from, to, elbow, pattern, false, work)
    }

    fn prepare_resolved_connection_inner(
        &self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
        stage_merges: bool,
        work: &mut WaterTopologyWork,
    ) -> Result<(usize, Option<PreparedTopologyEdit>), ConnectionError> {
        self.check_connection_junctions(from, to)?;
        let map_bounds = self.environment.bounds();
        let a = from.pos.xy();
        let b = to.pos.xy();
        let mut length = a.distance(b);
        if let Some(elbow) = elbow {
            if !map_bounds.contains(elbow) {
                return Err(ConnectionError::OutOfBounds);
            }
            length = a.distance(elbow) + elbow.distance(b);
        }
        if !map_bounds.contains(a) || !map_bounds.contains(b) {
            return Err(ConnectionError::OutOfBounds);
        }
        if !wet_lane_count_fits(pattern) || length > 65_536.0 {
            return Err(ConnectionError::WorkLimit);
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
        let path_length = connection_profile_length_bound(a, b, elbow);
        // Initial profiling, lane creation, and invalidation of the two endpoints.
        let mut profile_samples = profile_sample_bound(path_length).saturating_mul(4);
        let invalidations = 2 + 3 * splits.len();
        for &id in &splits {
            let road = &self.roads[id];
            if !road.elevated_span() {
                // Splitting partitions the horizontal path. Allow for rounding
                // and endpoint samples of both pieces, plus their lane creation.
                let samples = profile_sample_bound(road.length()).saturating_add(5);
                profile_samples =
                    profile_samples.saturating_add(samples.saturating_mul(1 + invalidations));
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
        let endpoint = |resolved: ConnectionEndpoint| CrossingEndpoint {
            kind: resolved.kind,
            pos: resolved.pos,
            split: resolved.split_point,
        };
        let check_geometry = self.environment.strict_water_siting()
            && !super::connection_is_dry(
                self,
                endpoint(from),
                endpoint(to),
                elbow,
                pattern.width(),
                &mut work.remaining,
            )?;
        if self.environment.strict_water_siting() {
            spend_water_work(
                &mut work.remaining,
                super::generated_frontage_work(path_length, pattern, check_geometry),
            )?;
        }
        work.spend_profile_samples(profile_samples);
        let prepared = super::prepare_topology(
            self,
            endpoint(from),
            endpoint(to),
            elbow,
            pattern,
            check_geometry,
            stage_merges,
            &mut work.geometry(),
        );
        work.profile_samples += profile_samples;
        let prepared = prepared?;
        if self.checks_rail_sweeps(pattern) {
            self.authorize_rail_deck(&prepared.preview.deck, pattern, &mut work.remaining)?;
        }
        self.authorize_service_frontage(&prepared.preview.deck, pattern.width(), &mut work.remaining)?;
        Ok((profile_samples, Some(prepared)))
    }

    fn authorize_service_frontage(
        &self,
        deck: &PolyLine3,
        width: f32,
        work: &mut usize,
    ) -> Result<(), ConnectionError> {
        for &(road, site) in &self.active_service_frontages {
            let Some(building) = self.get(site) else { continue };
            let Some(frontages) = self.service_frontages.get(&road) else { continue };
            for &(owner, shape) in frontages {
                if site != owner {
                    continue;
                }
                spend_water_work(work, 1)?;
                let envelope = shape.expand(width * 0.5);
                if !envelope.bbox().intersects(&deck.bbox().flatten()) {
                    continue;
                }
                spend_water_work(work, deck.len())?;
                if deck.as_slice().windows(2).any(|pair| {
                    let low = pair[0].z.min(pair[1].z) - 1.25;
                    let high = pair[0].z.max(pair[1].z) + 1.25;
                    (low..=high).contains(&building.door_pos.z)
                        && envelope.intersects(&geom::Segment::new(pair[0].xy(), pair[1].xy()))
                }) {
                    return Err(ConnectionError::OccupiedServiceAccess);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn checks_rail_sweeps(&self, pattern: &LanePattern) -> bool {
        self.has_junction_controller() && pattern.lanes().any(|(kind, _, _)| kind.is_rail())
    }

    fn authorize_rail_deck(
        &self,
        deck: &PolyLine3,
        pattern: &LanePattern,
        remaining: &mut usize,
    ) -> Result<(), ConnectionError> {
        let allowance = (*remaining).min(16_384);
        let mut budget = crate::world_command::PrepareBudget::for_preview(allowance);
        let width = pattern.width();
        let result = super::traffic_control::check_new_rail_sweeps(self, deck, width, &mut budget)
            .map_err(|reason| match reason {
                crate::world_command::Rejection::RoadWorkLimit => ConnectionError::WorkLimit,
                _ => ConnectionError::InvalidEndpoint,
            })
            .and_then(|()| self.authorize_roundabout_islands(deck, width, &mut budget));
        *remaining -= allowance - *budget.road_work_remaining();
        result
    }

    fn authorize_roundabout_islands(
        &self,
        deck: &PolyLine3,
        width: f32,
        budget: &mut crate::world_command::PrepareBudget,
    ) -> Result<(), ConnectionError> {
        if self.roundabouts.is_empty() {
            return Ok(());
        }
        let mut spend = |work| {
            budget
                .spend_road_work(work)
                .map_err(|_| ConnectionError::WorkLimit)
        };
        spend(deck.len())?;
        let bounds = deck.bbox().flatten();
        let half_width = width * std::f32::consts::FRAC_1_SQRT_2;
        for &id in &self.roundabouts {
            spend(1)?;
            let Some(ring) = self.intersections[id].roundabout_geometry() else {
                continue;
            };
            if !bounds
                .expand(ring.island_radius() + half_width)
                .contains(ring.center.xy())
            {
                continue;
            }
            for track in deck.segments() {
                spend(1)?;
                if ring.track_intersects_island(track, half_width) {
                    return Err(ConnectionError::RoundaboutRailConflict);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn make_resolved_connection(
        &mut self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        interpoint: Option<Vec2>,
        pattern: &LanePattern,
        prepared: Option<PreparedTopologyEdit>,
        mut work: Option<&mut WaterTopologyWork>,
    ) -> Result<ConnectionOutcome, ConnectionError> {
        let merges = prepared.as_ref().and_then(|edit| {
            edit.merges
                .map(|ends| [0, 1].map(|end| ends[end].then_some(edit.merge_frontage_work[end])))
        });
        let connected = self.commit_prepared_connection(from, to, interpoint, pattern, prepared)?;
        let mut road = connected.road;
        let mut source_side = connected.src;
        let mut destination = connected.dst;
        let mut replacements = BTreeMap::new();
        for split in connected.splits.into_iter().flatten() {
            replacements.insert(split.original, split.roads.to_vec());
        }
        for (end, junction) in [connected.src, connected.dst].into_iter().enumerate() {
            let merged = if let Some(merges) = merges {
                if let Some(frontage_work) = merges[end] {
                    let [first, second] = *self.intersections[junction].roads else {
                        unreachable!("prepared merge has two arms");
                    };
                    self.merge_road_checked(first, second, None, Some(frontage_work))
                        .map(|road| road.map(|road| ([first, second], road)))
                } else {
                    continue;
                }
            } else {
                self.merge_shallow_junction(junction, work.as_deref_mut())
            };
            if let Ok(Some((old, new))) = merged {
                super::remap_merged_roads(&mut replacements, old, new);
                if old.contains(&road) {
                    road = new;
                    if junction == source_side {
                        source_side = self.roads[new].other_end(destination).unwrap();
                    } else {
                        destination = self.roads[new].other_end(source_side).unwrap();
                    }
                }
            }
        }
        self.check_invariants();
        Ok(ConnectionOutcome {
            road,
            destination,
            src: self
                .intersections
                .contains_key(connected.src)
                .then_some(connected.src),
            dst: self
                .intersections
                .contains_key(connected.dst)
                .then_some(connected.dst),
            replacements,
        })
    }

    #[cfg(test)]
    pub(crate) fn connect_projects(
        &mut self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
    ) -> ConnectedRoad {
        let prepared = self
            .environment
            .has_water_configuration()
            .then(WaterTopologyWork::new)
            .and_then(|mut work| {
                self.prepare_resolved_connection(from, to, elbow, pattern, &mut work)
                    .expect("valid fixture connection")
                    .1
            });
        self.commit_prepared_connection(from, to, elbow, pattern, prepared)
            .expect("prepared fixture belongs to this map")
    }

    pub(crate) fn commit_prepared_connection(
        &mut self,
        from: ConnectionEndpoint,
        to: ConnectionEndpoint,
        interpoint: Option<Vec2>,
        pattern: &LanePattern,
        prepared: Option<PreparedTopologyEdit>,
    ) -> Result<ConnectedRoad, ConnectionError> {
        self.check_connection_junctions(from, to)?;
        if prepared.is_none() && self.land_ownership.is_tiled() {
            return Err(ConnectionError::Stale);
        }
        if prepared
            .as_ref()
            .is_some_and(|prepared| !prepared.matches(self))
        {
            return Err(ConnectionError::Stale);
        }
        // Unrestricted bulk connection callers can arrive without a topology
        // projection. Still authorize track geometry before the first map write.
        let prepared = if prepared.is_none()
            && (self.checks_rail_sweeps(pattern) || !self.active_service_frontages.is_empty())
        {
            self.prepare_resolved_connection_inner(
                from,
                to,
                interpoint,
                pattern,
                false,
                &mut WaterTopologyWork::new(),
            )?
            .1
        } else {
            prepared
        };
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
        let check_water = prepared.as_ref().is_some_and(|edit| edit.check_water);
        let frontage_work = self.environment.has_water_configuration().then(|| {
            let length = connection_profile_length_bound(from.pos.xy(), to.pos.xy(), interpoint);
            super::generated_frontage_work(length, pattern, check_water)
        });
        let mut tiled_frontage = prepared.as_ref().map_or(0, |edit| edit.frontage_work);
        let profile = prepared.map(|prepared| (prepared.profile, prepared.elevated));
        let road = self
            .connect_with_span(src, dst, pattern, connection_segment, None, profile)
            .expect("validated connection intersections");

        self.invalidate(src);
        self.invalidate(dst);
        // Frontage checks use the deck clipped by the finalized junction interfaces.
        if self.land_ownership.is_tiled() {
            let children = [&from_split, &to_split]
                .into_iter()
                .filter_map(Option::as_ref)
                .flat_map(|split| split.roads);
            for road in children.chain(std::iter::once(road)) {
                self.generate_land_frontage(road, &mut tiled_frontage)
                    .expect("prepared connection reserved its frontage");
            }
        } else {
            Lot::generate_along_final_road(self, road, frontage_work);
        }

        info!("connected {:?} {:?}: {:?}", src, dst, road);
        Ok(ConnectedRoad {
            src,
            dst,
            road,
            splits: [from_split, to_split],
        })
    }

    pub(crate) fn merge_shallow_junction(
        &mut self,
        id: IntersectionID,
        mut work: Option<&mut WaterTopologyWork>,
    ) -> Result<Option<([RoadID; 2], RoadID)>, ConnectionError> {
        let Some(intersection) = self.intersections.get(id) else {
            return Ok(None);
        };
        if let [first, second] = *intersection.roads {
            let angle = self.roads[first]
                .dir_from(id)
                .dot(self.roads[second].dir_from(id));
            if angle < -0.9 {
                if let Some(work) = work.as_deref_mut() {
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
                return self
                    .merge_road_checked(first, second, work, None)
                    .map(|merged| merged.map(|new| ([first, second], new)));
            }
        }
        Ok(None)
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
        let replacement_points = replaced_roads
            .iter()
            .map(|id| self.roads[*id].points().len())
            .sum::<usize>();
        let mut points = replacement_points.saturating_add(profile_sample_bound(new_length));
        for &id in replaced_roads {
            let road = &self.roads[id];
            added_lanes = added_lanes.saturating_add(road.n_lanes());
            if self.environment.strict_water_siting() {
                points = points.max(profile_sample_bound(road.length() + new_length));
                super::reserve_reassigned_frontage(self, id, points, remaining)?;
            }
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
                        profile_samples =
                            profile_samples.saturating_add(samples.saturating_mul(rebuilds));
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
        self.authorize_zone_change(id, &zone).ok()?;
        let b = self.buildings.get_mut(id).filter(|building| building.kind != BuildingKind::SiteRemnant)?;
        if !Building::dry_footprint(&self.environment, &b.obb, Some(&zone)) {
            return None;
        }
        *b.zone.as_mut()? = zone;
        queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, b);
        self.subscribers.dispatch(UpdateType::Building, b);

        let z = b.zone.as_ref().unwrap();
        let trees = z.poly.clone();
        let toclean = self.spatial_map.query(&trees, ProjectFilter::LOT).collect();
        self.spatial_map.update(b);
        self.clear_construction_trees(&trees);
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
        let prepared = self
            .prepare_special_building(*obb, kind, gen, zone, connected_road)
            .ok()?;
        Some(self.commit_special_building(prepared))
    }

    pub(crate) fn prepare_special_building(
        &self,
        obb: OBB,
        kind: BuildingKind,
        gen: BuildingGen,
        zone: Option<Zone>,
        connected_road: Option<RoadID>,
    ) -> Result<PreparedSpecialBuilding, crate::world_command::Rejection> {
        use crate::world_command::Rejection;
        let zone = zone
            .map(|zone| zone.normalized().ok_or(Rejection::InvalidZone))
            .transpose()?;
        if !Building::valid_footprint(&obb, gen) {
            return Err(Rejection::InvalidBuilding);
        }
        self.authorize_new_building(&obb, gen, zone.as_ref(), connected_road)?;
        if !Building::dry_footprint(&self.environment, &obb, zone.as_ref()) {
            return Err(Rejection::WetFootprint);
        }
        super::validate_water_service_placement(self, kind, obb, connected_road)
            .map_err(Rejection::WaterServiceSite)?;
        if connected_road.is_some_and(|road| !self.roads.contains_key(road)) {
            return Err(Rejection::MissingReference);
        }
        if self.building_overlaps(obb) {
            return Err(Rejection::BuildingOverlap);
        }
        let building = Building::prepare(
            &self.environment,
            obb,
            kind,
            gen,
            zone,
            connected_road,
            None,
        )
        .ok_or(Rejection::InvalidBuilding)?;
        self.authorize_building_grade(obb, building.base_height())?;
        Ok(PreparedSpecialBuilding {
            building,
            shape: obb,
            kind,
        })
    }

    pub(crate) fn commit_special_building(
        &mut self,
        prepared: PreparedSpecialBuilding,
    ) -> BuildingID {
        let PreparedSpecialBuilding {
            building,
            shape,
            kind,
        } = prepared;
        let id = building.insert(
            &mut self.buildings,
            &mut self.spatial_map,
            &mut self.electricity,
            &mut self.roads,
        );
        self.index_fire_candidate(id);
        self.clean_lots_inner(self.spatial_map.query(shape, ProjectFilter::LOT).collect());
        self.clear_construction_trees(shape.expand(10.0));
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &self.buildings[id],
        );
        self.water_services
            .building_changed(&self.environment, &self.roads, &self.buildings[id]);
        self.subscribers
            .dispatch(UpdateType::Building, &self.buildings[id]);

        if kind == BuildingKind::ExternalTrading {
            self.external_train_stations.push(id);
        }

        self.check_invariants();
        id
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
        let lot = self.get(lot_id)?;
        lot.check_growable_footprint(self, gen).ok()?;
        let prepared = self.prepare_checked_growable(lot, kind, gen, None).ok()?;
        self.commit_prepared_growable(prepared).ok()
    }

    fn construction_grade_revision(&self) -> (u64, u64, u64) {
        (
            self.environment.base_revision,
            self.environment.water_revision(),
            self.construction_cut_revision,
        )
    }

    fn growable_attachment(&self, lot: &Lot) -> Result<Vec3, GrowableLotError> {
        let road = self.get(lot.parent).ok_or(GrowableLotError::MissingRoad)?;
        let front = (lot.shape.corners[0] + lot.shape.corners[1]) * 0.5;
        Ok(road.points.project_2d(front))
    }

    /// The caller supplies a checked footprint. Commit validates live fit again.
    pub(crate) fn prepare_checked_growable(
        &self,
        lot: &Lot,
        kind: BuildingKind,
        generator: BuildingGen,
        appearance: Option<PreparedGrowableAppearance>,
    ) -> Result<PreparedGrowable, GrowableLotError> {
        let attachment = self.growable_attachment(lot)?;
        self.authorize_new_building(&lot.shape, generator, None, Some(lot.parent))
            .map_err(GrowableLotError::LandDenied)?;
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
        self.authorize_building_grade(lot.shape, building.base_height())
            .map_err(GrowableLotError::LandDenied)?;
        Ok(PreparedGrowable {
            lot: lot.id,
            frontage: lot.parent,
            shape: lot.shape,
            generator,
            attachment,
            map_identity: self.identity(),
            grade_revision: self.construction_grade_revision(),
            building,
        })
    }

    pub(crate) fn commit_prepared_growable(
        &mut self,
        prepared: PreparedGrowable,
    ) -> Result<BuildingID, GrowableLotError> {
        let lot = self.get(prepared.lot).ok_or(GrowableLotError::MissingLot)?;
        if self.identity() != prepared.map_identity
            || lot.parent != prepared.frontage
            || lot.shape.corners != prepared.shape.corners
            || self.growable_attachment(lot)? != prepared.attachment
        {
            return Err(GrowableLotError::MissingRoad);
        }
        lot.check_growable_footprint(self, prepared.generator)?;
        self.authorize_new_building(&lot.shape, prepared.generator, None, Some(lot.parent))
            .map_err(GrowableLotError::LandDenied)?;
        self.authorize_building_grade(lot.shape, prepared.building.base_height())
            .map_err(GrowableLotError::LandDenied)?;
        if self.construction_grade_revision() != prepared.grade_revision {
            return Err(GrowableLotError::InfeasibleGrade);
        }

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
        if self
            .authorize_land_effects(super::PreparedEffects::Building(id))
            .is_err()
        {
            return false;
        }
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
            || next.frozen_shell != previous.frozen_shell
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
        if self.service_apron_overlaps(&lot.shape) {
            return None;
        }
        self.authorize_new_building(&lot.shape, gen, None, Some(lot.parent))
            .ok()?;
        let road = self.roads.get(lot.parent)?;
        if !Lot::allows_frontage(self, road)
            || !Lot::has_dry_frontage(&self.environment, road, &lot.shape)
        {
            return None;
        }
        if !Building::valid_footprint(&lot.shape, gen)
            || !Building::dry_footprint(&self.environment, &lot.shape, None)
        {
            return None;
        }
        let building = Building::prepare(
            &self.environment,
            lot.shape,
            kind,
            gen,
            None,
            Some(lot.parent),
            None,
        )?;
        self.authorize_building_grade(lot.shape, building.base_height())
            .ok()?;
        let id = building.insert(
            &mut self.buildings,
            &mut self.spatial_map,
            &mut self.electricity,
            &mut self.roads,
        );

        self.consume_building_lot(lot_id, id, growable);
        Some(id)
    }

    fn consume_building_lot(&mut self, lot_id: LotID, id: BuildingID, growable: bool) {
        self.index_fire_candidate(id);
        let lot = self
            .remove_lot_record(lot_id)
            .expect("building lot still exists");
        self.subscribers.dispatch(UpdateType::Road, &lot);
        self.spatial_map.remove(lot_id);
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &self.buildings[id],
        );
        self.water_services
            .building_changed(&self.environment, &self.roads, &self.buildings[id]);
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

        self.try_remove_road(road_id).ok()
    }

    pub(crate) fn prepare_remove_road(&self, road_id: RoadID) -> Result<(), ConnectionError> {
        let road = self.get(road_id).ok_or(ConnectionError::InvalidEndpoint)?;
        self.check_junction_topology_edit(road.src)?;
        self.check_junction_topology_edit(road.dst)?;
        if self.environment.strict_water_siting() || self.land_ownership.is_tiled() {
            super::prepare_road_removal(self, road_id, &mut WaterTopologyWork::new().geometry())?;
        }
        Ok(())
    }

    pub(crate) fn try_remove_road(&mut self, road_id: RoadID) -> Result<Road, ConnectionError> {
        self.prepare_remove_road(road_id)?;
        let removed = self
            .remove_road_inner(road_id)
            .expect("road preflight checked reference");
        self.check_invariants();
        Ok(removed)
    }

    pub fn subscribe(&self, filter: UpdateType) -> MapSubscriber {
        self.subscribers.subscribe(filter)
    }

    pub(super) fn clear_authored_vacant_lots(&mut self) {
        self.clean_lots_inner(self.lots.keys().map(ProjectKind::Lot).collect());
    }

    pub(super) fn road_lot_ids(&self, road: RoadID) -> Vec<LotID> {
        self.road_lots(road).collect()
    }

    /// The lots removed with this road, in lot ID order.
    pub fn road_lots(&self, road: RoadID) -> impl Iterator<Item = LotID> + '_ {
        self.lots_by_road.get(&road).into_iter().flatten().copied()
    }

    pub(super) fn road_lot_count(&self, road: RoadID) -> usize {
        self.lots_by_road.get(&road).map_or(0, BTreeSet::len)
    }

    pub(super) fn track_road_lot(&mut self, road: RoadID, id: LotID) {
        self.lots_by_road.entry(road).or_default().insert(id);
    }

    fn untrack_road_lot(&mut self, road: RoadID, id: LotID) {
        if let Some(lots) = self.lots_by_road.get_mut(&road) {
            lots.remove(&id);
            if lots.is_empty() {
                self.lots_by_road.remove(&road);
            }
        }
    }

    pub(crate) fn reparent_lot(&mut self, id: LotID, road: RoadID) {
        let previous = self.lots[id].parent;
        self.lots[id].parent = road;
        self.untrack_road_lot(previous, id);
        self.track_road_lot(road, id);
    }

    pub(super) fn remove_lot_record(&mut self, id: LotID) -> Option<Lot> {
        let lot = self.lots.remove(id)?;
        self.untrack_road_lot(lot.parent, id);
        Some(lot)
    }

    fn clean_lots_inner(&mut self, mut to_clean: Vec<ProjectKind>) {
        // Slot reuse depends on removal order, but the spatial index is rebuilt on load.
        to_clean.sort_unstable();
        for id in to_clean {
            if let ProjectKind::Lot(id) = id {
                self.spatial_map.remove(id);
                self.subscribers.dispatch(UpdateType::Road, &self.lots[id]);

                unwrap_contlog!(
                    &self.remove_lot_record(id),
                    "Lot was present in spatial map but not in Lots struct"
                );
            }
        }
    }

    fn clean_reassigned_lots(&mut self, lots: Vec<LotID>) {
        self.spatial_map.remove_lots(lots.iter().copied());
        // Keep the original subscriber and slot-reuse order.
        for id in lots {
            self.subscribers.dispatch(UpdateType::Road, &self.lots[id]);
            self.remove_lot_record(id);
        }
    }

    fn remove_road_inner(&mut self, road_id: RoadID) -> Option<Road> {
        let road = self.remove_raw_road(road_id)?;
        self.no_frontage.remove(&road_id);
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            &road,
        );
        self.subscribers.dispatch(UpdateType::Road, &road);

        for (id, _) in road.lanes_iter() {
            self.parking.remove_spots(id);
        }

        let lots = self
            .road_lot_ids(road_id)
            .into_iter()
            .map(ProjectKind::Lot)
            .collect();
        self.clean_lots_inner(lots);

        self.invalidate(road.src);
        self.invalidate(road.dst);

        for &building_id in &road.connected_buildings {
            let building = &mut self.buildings[building_id];
            building.connected_road = None;
            self.water_services
                .building_changed(&self.environment, &self.roads, building);
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
        if self.lot_layout == layout || self.land_ownership.is_tiled() {
            return;
        }
        info!("set_lot_layout {:?}", layout);
        self.lot_layout = layout;

        // Besides roads holding lots or built lots, take every street on the ground: it
        // can be left without lots when no size it drew fitted, or when a neighbour's lots
        // took its frontage. Roads the importer left without lots keep none.
        let street = |road: &Road| {
            (self.environment.strict_water_siting() || !road.elevated_span())
                && Lot::allows_frontage(self, road)
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
        if self
            .authorize_land_effects(super::PreparedEffects::Lot(lot))
            .is_err()
        {
            return;
        }
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

    // Private mutating

    pub(super) fn lane_topology_changed(&mut self) {
        self.walking_topology_generation += 1;
        self.driving_reachability.take();
        self.walking_routes.take();
        self.lane_scoring = None;
    }

    pub(crate) fn driving_graph_changed(&mut self) {
        self.driving_policy_generation += 1;
        self.driving_reachability.take();
    }

    pub(super) fn refresh_junction_paths(&mut self, id: IntersectionID, update_signals: bool) {
        self.intersections[id].update_turns(&self.lanes, &self.roads);
        if self.intersections[id].roundabout_geometry().is_some() {
            let mut work = WaterTopologyWork::new();
            if super::check_roundabout_rail(
                self,
                &self.intersections[id],
                &mut work.geometry(),
                |_| true,
            )
            .is_err()
            {
                self.intersections[id].use_stock_turns(&self.lanes, &self.roads);
            }
        }
        let junction = &self.intersections[id];
        if update_signals || junction.is_roundabout() {
            junction.update_traffic_control(&mut self.lanes, &self.roads);
        }
        if junction.roundabout_geometry().is_some() {
            self.roundabouts.insert(id);
        } else {
            self.roundabouts.remove(&id);
        }
    }

    pub(crate) fn refresh_driving_road(&mut self, id: RoadID) {
        self.driving_graph_changed();
        self.lane_scoring = None;
        let road = &self.roads[id];
        self.subscribers.dispatch(UpdateType::Road, road);
        for endpoint in [road.src, road.dst] {
            self.refresh_junction_paths(endpoint, true);
            let junction = &mut self.intersections[endpoint];
            junction.apply_turn_mask(self.junction_rules.get(&endpoint));
            self.subscribers.dispatch(UpdateType::Road, junction);
        }
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

    pub(crate) fn finalize_import(&mut self, junctions: &[IntersectionID], roads: &[RoadID]) {
        self.import_lane_reserve = true;
        self.lane_topology_changed();
        for &id in junctions {
            let junction = &mut self.intersections[id];
            junction.update_interface_radius(&mut self.roads);
        }
        for &id in roads {
            let road = &mut self.roads[id];
            super::reserve_import_lane(road);
            road.update_lanes(&mut self.lanes, &mut self.parking, &self.environment);
            queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, road);
            self.subscribers.dispatch(UpdateType::Road, road);
        }
        for &id in junctions {
            self.refresh_junction_paths(id, true);
            let junction = &self.intersections[id];
            self.spatial_map.update(junction);
            self.spatial_map.set_additional_occupancy(
                junction.id.into(),
                super::junction_support_bounds(junction, &self.roads),
            );
            queue_construction_cut(
                &mut self.pending_construction_cuts,
                &self.environment,
                junction,
            );
            self.subscribers.dispatch(UpdateType::Road, junction);
        }
    }

    pub(super) fn invalidate(&mut self, id: IntersectionID) {
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

        let changed_roundabout = inter.is_roundabout() || self.roundabouts.contains(&id);
        let mut nearby_junctions = BTreeSet::new();
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
            if changed_roundabout || other_end.is_roundabout() {
                nearby_junctions.insert(oend_id);
            }

            #[allow(clippy::indexing_slicing)] // borrowed before
            self.roads[x].update_lanes(&mut self.lanes, &mut self.parking, &self.environment);
            self.spatial_map.update_auxiliary(&self.roads[x]);
            self.spatial_map.set_additional_occupancy(
                oend_id.into(),
                super::junction_support_bounds(other_end, &self.roads),
            );
        }

        for neighbor in nearby_junctions {
            self.refresh_junction_paths(neighbor, false);
            let junction = &mut self.intersections[neighbor];
            junction.apply_turn_mask(self.junction_rules.get(&neighbor));
            self.spatial_map.update(junction);
            self.subscribers.dispatch(UpdateType::Road, junction);
        }

        self.refresh_junction_paths(id, true);
        let inter = &self.intersections[id];
        queue_construction_cut(
            &mut self.pending_construction_cuts,
            &self.environment,
            inter,
        );
        self.spatial_map.update(inter);
        self.spatial_map.set_additional_occupancy(
            inter.id.into(),
            super::junction_support_bounds(inter, &self.roads),
        );
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
            super::reserve_import_lane(road);
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
                self.spatial_map.update_auxiliary(road);
            }
            if changed {
                turns.extend([road.src, road.dst]);
            }
        }
        for junction in turns {
            self.refresh_junction_paths(junction, junction == id);
            let inter = &self.intersections[junction];
            self.spatial_map.update(inter);
            self.spatial_map.set_additional_occupancy(
                inter.id.into(),
                super::junction_support_bounds(inter, &self.roads),
            );
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
        self.retire_land_frontage(road_id);
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

        let [before, after] = r.split_profiles(pos, segment);

        let inherited_span = Some(r.elevated_span());
        let r1 = self.connect_with_span(
            r.src,
            id,
            &pat,
            RoadSegmentKind::Arbitrary(before),
            inherited_span,
            None,
        )?;
        let r2 = self.connect_with_span(
            id,
            r.dst,
            &pat,
            RoadSegmentKind::Arbitrary(after),
            inherited_span,
            None,
        )?;

        let frontage_blocked = self.no_frontage.remove(&split_road_id);
        if frontage_blocked {
            self.no_frontage.extend([r1, r2]);
        }

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

        for b in &r.connected_buildings {
            let b = self.buildings.get_mut(*b)?;

            let bpos = b.obb.center().z(b.height);

            let d1 = r1.points.project_dist2(bpos);
            let d2 = r2.points.project_dist2(bpos);

            let parent = if d1 < d2 { r1.id } else { r2.id };
            b.connected_road = Some(parent);
            if let Some(origin) = self.lot_origins.get_mut(&b.id) {
                origin.parent_road = parent;
            }
            self.water_services
                .building_changed(&self.environment, &self.roads, b);
        }

        let mut rejected_lots = Vec::new();
        for id in self.road_lot_ids(split_road_id) {
            let lot = &self.lots[id];
            let p = lot.shape.corners[0].z(lot.height);
            let d1 = self.roads[roads[0]].points.project(p).distance(p);
            let d2 = self.roads[roads[1]].points.project(p).distance(p);
            let (parent, distance) = if d1 < d2 {
                (roads[0], d1)
            } else {
                (roads[1], d2)
            };
            let road = &self.roads[parent];
            if frontage_blocked
                || distance >= road.width * 0.5 + 1.5
                || !Lot::has_dry_frontage(&self.environment, road, &lot.shape)
            {
                rejected_lots.push(id);
            } else {
                self.reparent_lot(id, parent);
            }
        }
        self.clean_reassigned_lots(rejected_lots);

        for b in r.connected_buildings {
            let b = &self.buildings[b];
            let road_id = b.connected_road.unwrap();
            self.roads[road_id].connected_buildings.push(b.id);

            if b.kind != BuildingKind::SiteRemnant {
                self.electricity.add_edge(b.id, road_id);
            }
        }

        Some(RoadSplit {
            original: split_road_id,
            junction: id,
            roads,
        })
    }

    pub(crate) fn merge_road(&mut self, road1: RoadID, road2: RoadID) -> Option<RoadID> {
        let mut work = self
            .needs_topology_preparation()
            .then(WaterTopologyWork::new);
        self.merge_road_checked(road1, road2, work.as_mut(), None)
            .ok()
            .flatten()
    }

    fn merge_road_checked(
        &mut self,
        road1: RoadID,
        road2: RoadID,
        work: Option<&mut WaterTopologyWork>,
        prepared_frontage: Option<usize>,
    ) -> Result<Option<RoadID>, ConnectionError> {
        let r1 = self
            .roads
            .get(road1)
            .ok_or(ConnectionError::InvalidEndpoint)?;
        let r2 = self
            .roads
            .get(road2)
            .ok_or(ConnectionError::InvalidEndpoint)?;
        for id in [r1.src, r1.dst, r2.src, r2.dst] {
            self.check_junction_topology_edit(id)?;
        }
        let frontage_blocked = self.no_frontage.contains(&road1);
        if frontage_blocked != self.no_frontage.contains(&road2) {
            return Ok(None);
        }

        let same_inter = if r1.src == r2.src || r1.src == r2.dst {
            r1.src
        } else if r1.dst == r2.src || r1.dst == r2.dst {
            r1.dst
        } else {
            warn!("trying to merge roads that are not connected to each other");
            return Ok(None);
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
            return Ok(None);
        }

        let r1_extremity = if r1.src == same_inter { r1.dst } else { r1.src };
        let r2_extremity = if r2.src == same_inter { r2.dst } else { r2.src };

        if r1_extremity == r2_extremity {
            log::info!("merge refused because connecting to itself");
            return Ok(None);
        }

        let new_polyline = r1.merged_profile(r2, same_inter);
        if self.checks_rail_sweeps(&pat1) {
            self.authorize_rail_deck(&new_polyline, &pat1, &mut 16_384)?;
        }
        let elevated = r1.elevated_span()
            || r2.elevated_span()
            || Road::is_elevated_span(
                self.intersections[r1_extremity].pos,
                self.intersections[r2_extremity].pos,
                &self.environment,
            );

        let check_water = self.environment.strict_water_siting();
        let mut frontage_work = prepared_frontage.unwrap_or_default();
        if prepared_frontage.is_none()
            && (self.land_ownership.is_tiled() || check_water || work.is_some())
        {
            if self.intersections[same_inter].roads.len() != 2 {
                return Ok(None);
            }
            let work = work.ok_or(ConnectionError::WorkLimit)?;
            frontage_work = super::prepare_merge(
                self,
                road1,
                road2,
                r1_extremity,
                r2_extremity,
                &pat1,
                &new_polyline,
                elevated,
                check_water,
                &mut work.geometry(),
            )?;
        }

        let r1 = self.remove_raw_road(road1).expect("validated merge road");
        let r2 = self.remove_raw_road(road2).expect("validated merge road");
        self.remove_intersection_inner(same_inter);
        for road in [&r1, &r2] {
            queue_construction_cut(&mut self.pending_construction_cuts, &self.environment, road);
            self.subscribers.dispatch(UpdateType::Road, road);
        }
        for (id, _) in r2.lanes_iter().chain(r1.lanes_iter()) {
            self.parking.remove_to_reuse(id);
        }
        let new_r = self
            .connect_with_span(
                r1_extremity,
                r2_extremity,
                &pat1,
                RoadSegmentKind::Arbitrary(new_polyline),
                Some(elevated),
                None,
            )
            .expect("validated merge endpoints");

        if frontage_blocked {
            self.no_frontage.remove(&road1);
            self.no_frontage.remove(&road2);
            self.no_frontage.insert(new_r);
        }
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
            let b = self.buildings.get_mut(*b).expect("road building exists");
            b.connected_road = Some(new_r.id);
            if let Some(origin) = self.lot_origins.get_mut(&b.id) {
                origin.parent_road = new_r.id;
            }
            new_r.connected_buildings.push(b.id);

            if b.kind != BuildingKind::SiteRemnant {
                self.electricity.add_edge(b.id, new_r.id);
            }
        }

        let new_id = new_r.id;
        self.invalidate(r1_extremity);
        self.invalidate(r2_extremity);

        let mut reassigned = self.road_lot_ids(road1);
        reassigned.extend(self.road_lot_ids(road2));
        reassigned.sort_unstable();
        let mut rejected_lots = Vec::new();
        for id in reassigned {
            if frontage_blocked
                || !Lot::has_dry_frontage(
                    &self.environment,
                    &self.roads[new_id],
                    &self.lots[id].shape,
                )
            {
                rejected_lots.push(id);
            } else {
                self.reparent_lot(id, new_id);
            }
        }
        self.clean_reassigned_lots(rejected_lots);
        let new_r = &self.roads[new_id];
        for &id in &new_r.connected_buildings {
            self.water_services
                .building_changed(&self.environment, &self.roads, &self.buildings[id]);
        }

        if self.land_ownership.is_tiled() {
            self.generate_land_frontage(new_id, &mut frontage_work)
                .expect("prepared merge reserved its frontage");
        }
        Ok(Some(new_id))
    }

    /// Returns None if one of the intersections don't exist
    pub(crate) fn connect(
        &mut self,
        src_id: IntersectionID,
        dst_id: IntersectionID,
        pattern: &LanePattern,
        segment: RoadSegmentKind,
    ) -> Option<RoadID> {
        let rail_guard = self.checks_rail_sweeps(pattern);
        if rail_guard {
            self.check_junction_topology_edit(src_id).ok()?;
            self.check_junction_topology_edit(dst_id).ok()?;
        }
        if !self.environment.strict_water_siting() && !self.land_ownership.is_tiled() && !rail_guard
        {
            let gen_lots = !matches!(segment, RoadSegmentKind::Arbitrary(_));
            let road = self.connect_with_span(src_id, dst_id, pattern, segment, None, None)?;
            if gen_lots {
                Lot::generate_along_road(self, road);
            }
            return Some(road);
        }
        if !wet_lane_count_fits(pattern) || pattern.lanes().next().is_none() {
            return None;
        }
        let gen_lots = !matches!(segment, RoadSegmentKind::Arbitrary(_));
        let mut work = WaterTopologyWork::new();
        let frontage_work = if gen_lots {
            let length = self
                .intersections
                .get(src_id)?
                .pos
                .xy()
                .distance(self.intersections.get(dst_id)?.pos.xy());
            let length = match &segment {
                RoadSegmentKind::Curved((u, v)) => length + (u.mag() + v.mag()) * (2.0 / 3.0),
                _ => length,
            };
            let allowance = super::generated_frontage_work(length, pattern, true);
            spend_water_work(&mut work.remaining, allowance).ok()?;
            Some(allowance)
        } else {
            None
        };
        let prepared = super::prepare_direct_road(
            self,
            src_id,
            dst_id,
            pattern,
            segment.clone(),
            &mut work.geometry(),
        )
        .ok()?;
        if rail_guard {
            self.authorize_rail_deck(&prepared.preview.deck, pattern, &mut work.remaining)
                .ok()?;
        }
        let mut tiled_frontage = prepared.frontage_work;
        let id = self.connect_with_span(
            src_id,
            dst_id,
            pattern,
            segment,
            None,
            Some((prepared.profile, prepared.elevated)),
        )?;
        self.invalidate(src_id);
        self.invalidate(dst_id);
        if gen_lots {
            if self.land_ownership.is_tiled() {
                self.generate_land_frontage(id, &mut tiled_frontage)
                    .expect("prepared road reserved its frontage");
            } else {
                Lot::generate_along_final_road(self, id, frontage_work);
            }
        }
        Some(id)
    }

    fn connect_with_span(
        &mut self,
        src_id: IntersectionID,
        dst_id: IntersectionID,
        pattern: &LanePattern,
        segment: RoadSegmentKind,
        inherited_span: Option<bool>,
        prepared: Option<(PolyLine3, bool)>,
    ) -> Option<RoadID> {
        let src = self.intersections.get(src_id)?;
        let dst = self.intersections.get(dst_id)?;

        let (segment, inherited_span) = prepared
            .map_or((segment, inherited_span), |(profile, elevated)| {
                (RoadSegmentKind::Arbitrary(profile), Some(elevated))
            });

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
        for id in [src_id, dst_id] {
            self.spatial_map.set_additional_occupancy(
                id.into(),
                super::junction_support_bounds(&self.intersections[id], &self.roads),
            );
        }

        #[allow(clippy::indexing_slicing)]
        let r = &self.roads[rid];
        let mut b = r.boldline();
        b.expand(40.0);
        self.clear_construction_trees(&b);

        Some(rid)
    }

    fn clear_construction_trees(&mut self, shape: impl geom::Intersect<Vec2>) {
        let land = &self.land_ownership;
        let spatial = &self.spatial_map;
        self.environment.remove_trees_near_filtered(
            shape,
            |point| {
                land.check_point(point).is_ok()
                    && (!land.is_tiled()
                        || !spatial
                            .query(
                                point,
                                ProjectFilter::ROAD
                                    | ProjectFilter::INTER
                                    | ProjectFilter::BUILDING,
                            )
                            .any(|object| land.is_protected(object)))
            },
            |chunk| self.subscribers.dispatch_chunk(UpdateType::Terrain, chunk),
        );
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
    pub fn is_ruined(&self, id: BuildingID) -> bool {
        self.ruins.contains(&id)
    }

    pub(crate) fn mark_ruined(&mut self, id: BuildingID) {
        if self.ruins.insert(id) {
            let building = &self.buildings[id];
            let center = building.obb.center();
            self.subscribers.dispatch(UpdateType::Building, building);
            self.update_ruin_land_value(center, true);
        }
    }

    pub fn ruin_land_value_penalty(&self, at: Vec2) -> f32 {
        if self.ruin_land_value.is_empty() {
            return 0.0;
        }
        let cell = Self::ruin_cell(at);
        self.ruin_land_value.get(&cell).copied().unwrap_or(0).min(1600) as f32 / 100.0
    }

    fn ruin_cell(at: Vec2) -> (i32, i32) {
        let sample = (at / 16.0).round();
        (sample.x as i32, sample.y as i32)
    }

    fn update_ruin_land_value(&mut self, center: Vec2, add: bool) {
        let (column, row) = Self::ruin_cell(center);
        for y in row - 6..=row + 6 {
            for x in column - 6..=column + 6 {
                let sample = Vec2::new(x as f32 * 16.0, y as f32 * 16.0);
                let contribution = (800.0 * (1.0 - sample.distance(center) / 96.0)).max(0.0) as u32;
                if contribution == 0 {
                    continue;
                }
                let cell = (x, y);
                if add {
                    *self.ruin_land_value.entry(cell).or_default() += contribution;
                } else {
                    let value = self.ruin_land_value.get_mut(&cell).expect("existing ruin contribution");
                    *value -= contribution;
                    if *value == 0 {
                        self.ruin_land_value.remove(&cell);
                    }
                }
            }
        }
    }

    fn index_fire_candidate(&mut self, id: BuildingID) {
        if crate::souls::events::can_burn(self.buildings[id].kind) {
            self.fire_cohorts.insert(crate::souls::events::cohort_key(id));
        }
    }

    pub fn buildings(&self) -> &Buildings {
        &self.buildings
    }

    pub fn external_train_station(&self) -> Option<BuildingID> {
        self.external_train_stations.first().copied()
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
            || self.service_apron_overlaps(&obb)
    }

    pub(crate) fn service_apron_overlaps(&self, shape: &OBB) -> bool {
        self.service_frontages.values().flatten().any(|(_, apron)| apron.intersects(shape))
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

    pub(crate) fn apron_spots(&self, lane: LaneID) -> impl Iterator<Item = ParkingSpotID> + '_ {
        self.parking.lane_spots.get(lane).into_iter().flatten().copied()
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

        // A retained site remains a road-frontage object, but closing it
        // removes its electrical node. Compare the live network projection.
        let mut expected = ElectricityCache::build(self);
        for (site, building) in &self.buildings {
            if building.kind == BuildingKind::SiteRemnant {
                expected.remove_object(site);
            }
        }
        assert_eq!(self.electricity.ids, expected.ids);
        assert_eq!(self.electricity.networks, expected.networks);
        let mut live_edges = self.electricity.graph.clone();
        for edges in live_edges.values_mut().chain(expected.graph.values_mut()) {
            edges.sort_unstable();
        }
        assert_eq!(live_edges, expected.graph);

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
    deck_tiles: usize,
}

impl WaterTopologyWork {
    pub(crate) fn new() -> Self {
        Self {
            remaining: super::MAX_WATER_BATCH_WORK,
            profile_samples: MAX_WATER_PROFILE_SAMPLES,
            deck_tiles: MAX_DECK_TILE_TESTS,
        }
    }

    pub(crate) fn geometry(&mut self) -> CrossingWork<'_> {
        CrossingWork::new(
            &mut self.remaining,
            &mut self.deck_tiles,
            &mut self.profile_samples,
        )
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

fn connection_profile_length_bound(a: Vec2, b: Vec2, elbow: Option<Vec2>) -> f32 {
    // The Hermite control polygon bounds the sampled length. Each tangent
    // has length 1/sqrt(2) times its elbow leg, with a 1/3 control offset.
    let chord = a.distance(b);
    elbow.map_or(chord, |point| {
        chord
            + (a.distance(point) + b.distance(point))
                * (2.0 * std::f32::consts::FRAC_1_SQRT_2 / 3.0)
    })
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
