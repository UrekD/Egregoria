use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use geom::{vec2, Intersect, Polygon, Segment, Shape, Vec2, AABB, OBB};
use ordered_float::OrderedFloat;
use prototypes::{
    try_prototype, ServiceID, ServiceKind, ServicePrototype, DEFAULT_WATER_SERVICE_DISTANCE_M,
    MAX_WATER_SERVICE_DISTANCE_M,
};

use crate::map::{spend_water_work, Building, BuildingID, BuildingKind, Map, RoadID};

use super::{
    intersects_tiles, Environment, Occupant, PreparedWaterEdit, ShoreContact, TerrainChunkID,
    TileBounds, TileMask, WaterBodyId, WaterEditError, WaterKind, WaterTileCoord, CELL_SIZE,
    DIRECTIONS, TILES_PER_CHUNK,
};

pub const MIN_RIVER_SEPARATION_M: u16 = 32;
const SHORELINE_VALUE_REACH_M: f32 = 96.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaterServiceRole {
    Intake,
    Outlet,
}

impl WaterServiceRole {
    fn accepts(self, kind: WaterKind) -> bool {
        matches!(
            (self, kind),
            (Self::Intake, WaterKind::River | WaterKind::Lake)
                | (Self::Outlet, WaterKind::River | WaterKind::Sea)
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaterSitePurpose {
    Placement,
    Live,
}

#[derive(Clone, Copy, Debug)]
pub struct WaterSiteRequest {
    pub role: WaterServiceRole,
    pub footprint: OBB,
    pub connected_road: Option<RoadID>,
    pub max_water_distance_m: f32,
    pub building: Option<BuildingID>,
    pub purpose: WaterSitePurpose,
}

impl WaterSiteRequest {
    pub fn for_placement(
        kind: BuildingKind,
        footprint: OBB,
        connected_road: Option<RoadID>,
    ) -> Option<Self> {
        let service = try_prototype(ServiceID::from(kind.as_goods_company()?))?;
        Self::from_prototype(service, footprint, connected_road)
    }

    fn from_prototype(
        service: &ServicePrototype,
        footprint: OBB,
        connected_road: Option<RoadID>,
    ) -> Option<Self> {
        let role = match service.service_kind {
            ServiceKind::WaterPump => WaterServiceRole::Intake,
            ServiceKind::SewageOutlet => WaterServiceRole::Outlet,
            _ => return None,
        };
        Some(Self {
            role,
            footprint,
            connected_road,
            max_water_distance_m: service
                .max_water_distance_m
                .unwrap_or(DEFAULT_WATER_SERVICE_DISTANCE_M),
            building: None,
            purpose: WaterSitePurpose::Placement,
        })
    }

    fn for_building(building: &Building) -> Option<Self> {
        let mut request =
            Self::for_placement(building.kind, building.obb, building.connected_road)?;
        request.building = Some(building.id);
        request.purpose = WaterSitePurpose::Live;
        Some(request)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WaterSite {
    Legacy {
        road: RoadID,
    },
    Shore {
        contact: ShoreContact,
        distance_m: f32,
        road: RoadID,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaterSiteError {
    InvalidFootprint,
    WetFootprint,
    MissingRoad,
    NoEligibleShore(WaterServiceRole),
    OrderConflict {
        role: WaterServiceRole,
        body: WaterBodyId,
        provider: BuildingID,
        reach_m: u16,
    },
}

impl fmt::Display for WaterSiteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use WaterServiceRole::{Intake, Outlet};

        match self {
            Self::InvalidFootprint => f.write_str("Invalid utility footprint"),
            Self::WetFootprint => f.write_str("Utility footprint must be dry"),
            Self::MissingRoad => f.write_str("Utility needs a road connection"),
            Self::NoEligibleShore(Intake) => f.write_str("Pump needs a river or lake shore within reach"),
            Self::NoEligibleShore(Outlet) => f.write_str("Outlet needs a river bank or coast within reach"),
            Self::OrderConflict { role: Intake, reach_m, .. } => write!(
                f, "Pump must be at least {MIN_RIVER_SEPARATION_M} m upstream of the outlet at river metre {reach_m}"
            ),
            Self::OrderConflict { role: Outlet, reach_m, .. } => write!(
                f, "Outlet must be at least {MIN_RIVER_SEPARATION_M} m downstream of the pump at river metre {reach_m}"
            ),
        }
    }
}

type SiteResult = Result<WaterSite, WaterSiteError>;

#[derive(Clone, Copy)]
struct RiverProvider {
    reach_m: u16,
    id: BuildingID,
}

#[derive(Default)]
struct RiverExtrema {
    intakes: [Option<RiverProvider>; 2],
    outlets: [Option<RiverProvider>; 2],
}

impl RiverExtrema {
    fn insert(&mut self, role: WaterServiceRole, mut provider: RiverProvider) {
        let key = |p: RiverProvider| match role {
            WaterServiceRole::Intake => (u16::MAX - p.reach_m, p.id),
            WaterServiceRole::Outlet => (p.reach_m, p.id),
        };
        let extrema = match role {
            WaterServiceRole::Intake => &mut self.intakes,
            WaterServiceRole::Outlet => &mut self.outlets,
        };
        for slot in extrema {
            if slot.is_none_or(|current| key(provider) < key(current)) {
                let Some(previous) = slot.replace(provider) else {
                    break;
                };
                provider = previous;
            }
        }
    }
}

/// Physical providers across all roads. The top two extrema allow an existing
/// provider to be excluded without resampling the river.
#[derive(Default)]
pub struct RiverSiteSummary {
    rivers: BTreeMap<WaterBodyId, RiverExtrema>,
    sites: BTreeMap<BuildingID, (WaterServiceRole, SiteResult)>,
}

impl RiverSiteSummary {
    fn insert(&mut self, id: BuildingID, role: WaterServiceRole, site: SiteResult) {
        if let Ok(WaterSite::Shore { contact, .. }) = site {
            if let Some(reach_m) = contact.reach_m {
                self.rivers
                    .entry(contact.body)
                    .or_default()
                    .insert(role, RiverProvider { reach_m, id });
            }
        }
        self.sites.insert(id, (role, site));
    }

    pub fn provider_site(&self, id: BuildingID) -> Option<SiteResult> {
        let &(role, site) = self.sites.get(&id)?;
        Some(self.check_order(site, role, Some(id), WaterSitePurpose::Live))
    }

    fn check_order(
        &self,
        site: SiteResult,
        role: WaterServiceRole,
        building: Option<BuildingID>,
        purpose: WaterSitePurpose,
    ) -> SiteResult {
        let WaterSite::Shore { contact, .. } = site? else {
            return site;
        };
        let (Some(reach), Some(river)) = (contact.reach_m, self.rivers.get(&contact.body)) else {
            return site;
        };
        let (extrema, upstream) = match (role, purpose) {
            (WaterServiceRole::Intake, WaterSitePurpose::Live) => return site,
            (WaterServiceRole::Intake, WaterSitePurpose::Placement) => (&river.outlets, true),
            (WaterServiceRole::Outlet, _) => (&river.intakes, false),
        };
        let Some(other) = extrema
            .iter()
            .flatten()
            .find(|other| Some(other.id) != building)
        else {
            return site;
        };
        let (intake, outlet) = if upstream {
            (reach, other.reach_m)
        } else {
            (other.reach_m, reach)
        };
        if u32::from(outlet) < u32::from(intake) + u32::from(MIN_RIVER_SEPARATION_M) {
            return Err(WaterSiteError::OrderConflict {
                role,
                body: contact.body,
                provider: other.id,
                reach_m: other.reach_m,
            });
        }
        site
    }
}

#[derive(Default)]
pub(crate) struct WaterServiceCache {
    shores: Mutex<ShoreIndex>,
    providers: Mutex<Option<ProviderSnapshot>>,
}

struct ProviderSnapshot {
    water_revision: u64,
    topology_generation: u64,
    prototypes: &'static prototypes::Prototypes,
    summary: Arc<RiverSiteSummary>,
}

impl WaterServiceCache {
    pub(crate) fn water_edited(&mut self, env: &Environment, dirty: &super::WaterDirtyChunks) {
        let shores = self.shores.get_mut().unwrap();
        // Commits publish shore dependencies for both sides of an affected seam.
        if shores.revision.checked_add(1) == Some(env.water_revision()) {
            for chunk in dirty.water() {
                shores.chunks.remove(chunk);
            }
            shores.revision = env.water_revision();
        }
    }
}

/// Acquire one physical summary for placement or the scheduled utility pass.
pub fn river_site_summary(map: &Map) -> Arc<RiverSiteSummary> {
    let env = map.environment();
    if !env.strict_water_siting() {
        return Arc::default();
    }
    let water_revision = env.water_revision();
    let topology_generation = map.electricity.generation();
    let prototypes = prototypes::prototypes();
    let mut cached = map.water_services.providers.lock().unwrap();
    if let Some(snapshot) = cached.as_ref().filter(|snapshot| {
        snapshot.water_revision == water_revision
            && snapshot.topology_generation == topology_generation
            && std::ptr::eq(snapshot.prototypes, prototypes)
    }) {
        return Arc::clone(&snapshot.summary);
    }
    let mut summary = RiverSiteSummary::default();
    let mut shores = map.water_services.shores.lock().unwrap();
    shores.sync(env);
    let terrain = WaterTerrain::new(env, None);
    let mut remaining = usize::MAX;
    for building in map.buildings().values() {
        if let Some(request) = WaterSiteRequest::for_building(building) {
            let site = shores
                .geometric_site(map, &terrain, request, &mut remaining)
                .expect("map-bounded water site search");
            summary.insert(building.id, request.role, site);
        }
    }
    let summary = Arc::new(summary);
    *cached = Some(ProviderSnapshot {
        water_revision,
        topology_generation,
        prototypes,
        summary: Arc::clone(&summary),
    });
    summary
}

/// Choose the physical contact before applying river order.
pub fn water_service_site(
    map: &Map,
    request: WaterSiteRequest,
    summary: &RiverSiteSummary,
) -> SiteResult {
    let env = map.environment();
    let mut shores = map.water_services.shores.lock().unwrap();
    shores.sync(env);
    let mut remaining = usize::MAX;
    let site = shores
        .geometric_site(map, &WaterTerrain::new(env, None), request, &mut remaining)
        .expect("map-bounded water site search");
    summary.check_order(site, request.role, request.building, request.purpose)
}

pub fn validate_water_service_placement(
    map: &Map,
    kind: BuildingKind,
    footprint: OBB,
    connected_road: Option<RoadID>,
) -> Result<(), WaterSiteError> {
    if map.environment().strict_water_siting() {
        if let Some(request) = WaterSiteRequest::for_placement(kind, footprint, connected_road) {
            water_service_site(map, request, &river_site_summary(map))?;
        }
    }
    Ok(())
}

/// Current physical site status for an installed provider, irrespective of staffing or power.
pub fn water_provider_site(map: &Map, id: BuildingID) -> Option<SiteResult> {
    if map.environment().strict_water_siting() {
        return river_site_summary(map).provider_site(id);
    }
    let request = WaterSiteRequest::for_building(map.get(id)?)?;
    Some(water_service_site(
        map,
        request,
        &RiverSiteSummary::default(),
    ))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WaterProviderImpact {
    pub count: usize,
    pub sample: Vec<BuildingID>,
}

pub(crate) fn water_provider_impact(
    map: &Map,
    edit: &PreparedWaterEdit,
    remaining: &mut usize,
) -> Result<WaterProviderImpact, WaterEditError> {
    const SAMPLE_LIMIT: usize = 8;
    if !edit.strict_water_siting() || edit.dirty().water().is_empty() {
        return Ok(WaterProviderImpact::default());
    }
    let env = map.environment();
    let live = WaterTerrain::new(env, None);
    let proposed = WaterTerrain::new(env, Some(edit));
    // Use local indexes so work limits and preview results are independent of cache warmth.
    let mut live_shores = ShoreIndex::default();
    let mut proposed_shores = ShoreIndex::default();
    let mut before = RiverSiteSummary::default();
    let mut after = RiverSiteSummary::default();

    spend_water_work(remaining, map.buildings().len())?;
    for building in map.buildings().values() {
        let Some(request) = WaterSiteRequest::for_building(building) else {
            continue;
        };
        let old = live_shores.geometric_site(map, &live, request, remaining)?;
        let new = if !live.strict()
            || env
                .covered_chunks(
                    request
                        .footprint
                        .bbox()
                        .expand(request.max_water_distance_m + CELL_SIZE),
                )
                .any(|chunk| edit.dirty().water().contains(&chunk))
        {
            proposed_shores.geometric_site(map, &proposed, request, remaining)?
        } else {
            old
        };
        before.insert(building.id, request.role, old);
        after.insert(building.id, request.role, new);
    }

    let mut impact = WaterProviderImpact::default();
    for &id in after.sites.keys() {
        if before.sites.get(&id) != after.sites.get(&id)
            || before.provider_site(id) != after.provider_site(id)
        {
            impact.count += 1;
            if impact.sample.len() < SAMPLE_LIMIT {
                impact.sample.push(id);
            }
        }
    }
    Ok(impact)
}

/// Add ten points at an actual wet–dry edge, falling linearly to zero at 96 m.
pub fn shoreline_value_bonus(map: &Map, xy: Vec2) -> f32 {
    let env = map.environment();
    if !env.strict_water_siting() || (env.water.fresh.is_empty() && env.water.sea.is_empty()) {
        return 0.0;
    }
    let terrain = WaterTerrain::new(env, None);
    if terrain
        .bounds
        .position(f64::from(xy.x), f64::from(xy.y))
        .is_none()
    {
        return 0.0;
    }
    let mut shores = map.water_services.shores.lock().unwrap();
    shores.sync(env);
    let mut remaining = usize::MAX;
    let search = AABB::centered(xy, Vec2::splat(2.0 * SHORELINE_VALUE_REACH_M));
    let mut best = SHORELINE_VALUE_REACH_M.powi(2);
    shores
        .visit_edges(&terrain, search, &mut remaining, |edge| {
            best = best.min(edge.segment.project(xy).distance2(xy));
        })
        .expect("map-bounded shoreline search");
    10.0 * (1.0 - best.sqrt() / SHORELINE_VALUE_REACH_M)
}

#[derive(Default)]
struct ShoreIndex {
    revision: u64,
    chunks: BTreeMap<TerrainChunkID, Box<[ShoreEdge]>>,
}

struct WaterTerrain<'a> {
    env: &'a Environment,
    prepared: Option<&'a PreparedWaterEdit>,
    bounds: TileBounds,
}

impl<'a> WaterTerrain<'a> {
    fn new(env: &'a Environment, prepared: Option<&'a PreparedWaterEdit>) -> Self {
        Self {
            env,
            prepared,
            bounds: TileBounds::new(&env.heightmap),
        }
    }

    fn strict(&self) -> bool {
        self.prepared.map_or(
            self.env.strict_water_siting(),
            PreparedWaterEdit::strict_water_siting,
        )
    }

    fn occupant(&self, tile: WaterTileCoord) -> Option<Occupant> {
        match self.prepared {
            Some(prepared) => prepared.occupant(self.env, tile),
            None => self.env.water.occupant(self.bounds, tile),
        }
    }

    fn wet_mask(
        &self,
        chunk: TerrainChunkID,
        remaining: &mut usize,
    ) -> Result<TileMask, WaterEditError> {
        spend_water_work(remaining, TileMask::WORDS)?;
        let state = &self.env.water;
        let mut mask = match self.prepared.and_then(|edit| edit.fresh.chunks.get(&chunk)) {
            Some(Some(fresh)) => {
                spend_water_work(remaining, TILES_PER_CHUNK)?;
                let mut mask = TileMask::default();
                for (i, cell) in fresh.iter().enumerate() {
                    if cell.body != 0 {
                        mask.insert(i);
                    }
                }
                mask
            }
            Some(None) => TileMask::default(),
            None => state.fresh_masks.0.get(&chunk).copied().unwrap_or_default(),
        };
        let sea = self
            .prepared
            .and_then(|edit| edit.sea.as_deref())
            .unwrap_or(&state.sea);
        if let Some(sea) = sea.get(self.bounds.index(chunk)) {
            mask.union(sea);
        }
        Ok(mask)
    }
}

#[derive(Clone, Copy)]
struct ShoreEdge {
    segment: Segment,
    contact: ShoreContact,
}

impl ShoreIndex {
    fn sync(&mut self, env: &Environment) {
        if self.revision != env.water_revision() {
            self.chunks.clear();
            self.revision = env.water_revision();
        }
    }

    fn geometric_site(
        &mut self,
        map: &Map,
        terrain: &WaterTerrain<'_>,
        request: WaterSiteRequest,
        remaining: &mut usize,
    ) -> Result<SiteResult, WaterEditError> {
        let Some(road) = request
            .connected_road
            .filter(|&road| map.get(road).is_some())
        else {
            return Ok(Err(WaterSiteError::MissingRoad));
        };
        if !terrain.strict() {
            return Ok(Ok(WaterSite::Legacy { road }));
        }
        let obb = request.footprint;
        if obb.corners.iter().any(|point| !point.is_finite())
            || obb
                .axis()
                .iter()
                .any(|axis| !axis.mag2().is_finite() || axis.mag2() == 0.0)
        {
            return Ok(Err(WaterSiteError::InvalidFootprint));
        }
        let reach = request
            .max_water_distance_m
            .min(MAX_WATER_SERVICE_DISTANCE_M);
        if !request.max_water_distance_m.is_finite() || reach <= 0.0 {
            return Ok(Err(WaterSiteError::NoEligibleShore(request.role)));
        }

        let bbox = obb.bbox();
        let clamped = bbox.intersection(terrain.env.bounds());
        let tiles = clamped.w() / CELL_SIZE + 2.0;
        let rows = clamped.h() / CELL_SIZE + 2.0;
        spend_water_work(remaining, (tiles * rows).ceil() as usize)?;
        let polygon = Polygon::from(obb.corners.as_slice());
        if intersects_tiles(terrain.bounds, &polygon, |tile| {
            terrain.occupant(tile).is_some()
        }) {
            return Ok(Err(WaterSiteError::WetFootprint));
        }

        let sides = obb.segments();
        let mut best = None;
        self.visit_edges(terrain, bbox.expand(reach), remaining, |edge| {
            if !request.role.accepts(edge.contact.kind) {
                return;
            }
            let (distance2, closest) = footprint_distance(edge.segment, &sides);
            if distance2 > reach.powi(2) {
                return;
            }
            let key = (
                OrderedFloat(distance2),
                edge.contact.body,
                OrderedFloat(edge.segment.src.x),
                OrderedFloat(edge.segment.src.y),
                edge.segment.src.x == edge.segment.dst.x,
            );
            if best.is_none_or(|(previous, _)| key < previous) {
                best = Some((
                    key,
                    ShoreContact {
                        point: closest,
                        ..edge.contact
                    },
                ));
            }
        })?;
        Ok(best.map_or_else(
            || Err(WaterSiteError::NoEligibleShore(request.role)),
            |((distance2, ..), contact)| {
                Ok(WaterSite::Shore {
                    contact,
                    distance_m: distance2.0.sqrt(),
                    road,
                })
            },
        ))
    }

    fn visit_edges(
        &mut self,
        terrain: &WaterTerrain<'_>,
        search: AABB,
        remaining: &mut usize,
        mut visit: impl FnMut(&ShoreEdge),
    ) -> Result<(), WaterEditError> {
        // An edge on a seam is indexed under its wet tile's chunk.
        for chunk in terrain.env.covered_chunks(search.expand(CELL_SIZE)) {
            let edges = match self.chunks.entry(chunk) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(build_shore_chunk(terrain, chunk, remaining)?)
                }
            };
            spend_water_work(remaining, 1 + edges.len())?;
            for edge in edges.iter() {
                if search.intersects(&edge.segment.bbox()) {
                    visit(edge);
                }
            }
        }
        Ok(())
    }
}

fn build_shore_chunk(
    terrain: &WaterTerrain<'_>,
    chunk: TerrainChunkID,
    remaining: &mut usize,
) -> Result<Box<[ShoreEdge]>, WaterEditError> {
    let wet = terrain.wet_mask(chunk, remaining)?;
    let mut edges = Vec::new();
    if !wet.is_empty() {
        for (direction, (dx, dy)) in DIRECTIONS.into_iter().enumerate() {
            spend_water_work(remaining, TileMask::WORDS)?;
            let neighbor = TerrainChunkID::new_i16(chunk.0 + dx, chunk.1 + dy);
            let across = if terrain.bounds.contains_chunk(neighbor) {
                terrain.wet_mask(neighbor, remaining)?
            } else {
                // Map boundaries have no dry tile on the other side.
                TileMask([u64::MAX; TileMask::WORDS])
            };
            let wet_neighbors = wet.shift_from(across, direction);
            let shore = TileMask(std::array::from_fn(|i| wet.0[i] & !wet_neighbors.0[i]));
            for index in shore.indices() {
                // Charge retained edge storage as well as the tile and contact lookup.
                spend_water_work(remaining, 8)?;
                let tile = WaterTileCoord::from_local(chunk, index);
                let water = terrain.occupant(tile).expect("wet tile in shore index");
                let dry = tile.neighbor(direction);
                let lower = tile.corner().max(dry.corner());
                let upper = lower
                    + if dx != 0 {
                        vec2(0.0, CELL_SIZE)
                    } else {
                        vec2(CELL_SIZE, 0.0)
                    };
                edges.push(ShoreEdge {
                    segment: Segment::new(lower, upper),
                    contact: ShoreContact {
                        body: water.body,
                        kind: water.kind,
                        surface_z: f32::from(water.level_dm) / 10.0,
                        point: (lower + upper) * 0.5,
                        dry_tile: dry,
                        wet_tile: tile,
                        reach_m: water.reach,
                    },
                });
            }
        }
    }
    Ok(edges.into_boxed_slice())
}

fn footprint_distance(edge: Segment, sides: &[Segment; 4]) -> (f32, Vec2) {
    let mut best = (f32::INFINITY, edge.src);
    let mut consider = |shore: Vec2, footprint: Vec2| {
        let distance2 = shore.distance2(footprint);
        if (
            OrderedFloat(distance2),
            OrderedFloat(shore.x),
            OrderedFloat(shore.y),
        ) < (
            OrderedFloat(best.0),
            OrderedFloat(best.1.x),
            OrderedFloat(best.1.y),
        ) {
            best = (distance2, shore);
        }
    };
    for side in sides {
        if let Some(point) = edge.intersection_point(side) {
            consider(point, point);
        }
        consider(edge.project(side.src), side.src);
        consider(edge.src, side.project(edge.src));
        consider(edge.dst, side.project(edge.dst));
    }
    best
}

#[cfg(test)]
mod tests {
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;
    use prototypes::{GameTime, GoodsCompanyID, Tick};

    use crate::map::{
        LanePatternBuilder, MapProject, ProjectKind, WaterBasin, WaterEdit, WaterStroke,
    };
    use crate::map_dynamic::{
        electricity_flow_system, env_update, utility_flow_update, utility_network_at_road,
        BuildingInfos, EnvGrid, UtilityNetworkState,
    };
    use crate::souls::{goods_company::company_soul, human::spawn_human, residence};
    use crate::world_command::{Rejection, WorldCommand};
    use crate::{MapPreset, Simulation, SoulID};

    use super::*;

    fn road(map: &mut Map, start: f32, end: f32, y: f32) -> RoadID {
        let project = |x| MapProject {
            kind: ProjectKind::Ground,
            pos: vec3(x, y, 0.0),
        };
        map.make_connection(
            project(start),
            project(end),
            None,
            &LanePatternBuilder::default().build(),
        )
        .unwrap()
        .1
    }

    fn footprint(x: f32, y: f32) -> OBB {
        OBB::new(vec2(x, y), Vec2::X, 20.0, 20.0)
    }

    fn flat_map() -> Map {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::generate(2, 1, MapPreset::Flat, 0, 0.0)));
        map
    }

    fn river() -> WaterEdit {
        WaterEdit::CarveRiver {
            polyline: WaterStroke::new(&[vec2(64.0, 256.0), vec2(448.0, 256.0)]).unwrap(),
            width_m: 32.0,
            depth_m: 2.0,
            level_dm: Some(-10),
        }
    }

    fn place(map: &mut Map, name: &str, obb: OBB, access: RoadID) -> BuildingID {
        let id = GoodsCompanyID::new(name);
        map.build_special_building(
            &obb,
            BuildingKind::GoodsCompany(id),
            try_prototype(id).unwrap().bgen,
            None,
            Some(access),
        )
        .unwrap()
    }

    #[test]
    fn w3_water_direct_placement_checks_freshwater_reach() {
        let _test = crate::tests::TestCtx::new();
        let mut env = Environment::generate(2, 1, MapPreset::Flat, 0, 0.0);
        env.fill_lake(
            &Polygon::centered_rect(vec2(128.0, 256.0), 64.0, 64.0),
            -10,
            2.0,
        )
        .unwrap();
        let mut map = Map::empty();
        assert!(map.install_environment(env));
        let access = road(&mut map, 32.0, 480.0, 176.0);

        let id = GoodsCompanyID::new("water-pump");
        let pump = try_prototype(id).unwrap();
        assert!(map
            .build_special_building(
                &footprint(320.0, 216.0),
                BuildingKind::GoodsCompany(id),
                pump.bgen,
                None,
                Some(access),
            )
            .is_none());
        assert!(map
            .build_special_building(
                &footprint(128.0, 212.0),
                BuildingKind::GoodsCompany(id),
                pump.bgen,
                None,
                Some(access),
            )
            .is_some());
    }

    #[test]
    fn w3_water_site_orders_contacts_across_networks() {
        let mut test = crate::tests::TestCtx::new();
        let mut map = flat_map();
        let south = road(&mut map, 32.0, 480.0, 188.0);
        let north = road(&mut map, 32.0, 480.0, 324.0);
        *test.g.map_mut() = map;
        test.g.edit_water(river()).unwrap();
        let mut map = test.g.map_mut();
        assert_ne!(map.electricity.net_id(south), map.electricity.net_id(north));

        let small = |x, y| OBB::new(vec2(x, y), Vec2::X, 8.0, 8.0);
        let intake = place(&mut map, "water-pump", small(168.0, 212.0), south);
        let outlet_at = |x| WaterSiteRequest {
            role: WaterServiceRole::Outlet,
            footprint: small(x, 300.0),
            connected_road: Some(north),
            max_water_distance_m: 48.0,
            building: None,
            purpose: WaterSitePurpose::Placement,
        };
        let summary = river_site_summary(&map);
        for x in [152.0, 168.0, 184.0] {
            assert!(matches!(
                water_service_site(&map, outlet_at(x), &summary),
                Err(WaterSiteError::OrderConflict { provider, reach_m: 104, .. })
                    if provider == intake
            ));
        }
        let exact = water_service_site(&map, outlet_at(200.0), &summary).unwrap();
        let WaterSite::Shore {
            contact,
            distance_m,
            road,
        } = exact
        else {
            panic!("expected a river contact");
        };
        assert_eq!(contact.reach_m, Some(136));
        assert_eq!(contact.point, vec2(196.0, 272.0));
        assert_eq!(distance_m, 24.0);
        assert_eq!(road, north);

        let outlet = place(&mut map, "sewage-outlet", small(200.0, 300.0), north);
        let next_outlet = place(&mut map, "sewage-outlet", small(216.0, 300.0), north);
        let request = WaterSiteRequest {
            role: WaterServiceRole::Intake,
            footprint: small(184.0, 212.0),
            connected_road: Some(south),
            ..outlet_at(200.0)
        };
        let summary = river_site_summary(&map);
        assert!(matches!(
            water_service_site(&map, request, &summary),
            Err(WaterSiteError::OrderConflict { provider, .. }) if provider == outlet
        ));
        let request = WaterSiteRequest {
            building: Some(outlet),
            ..request
        };
        assert!(water_service_site(&map, request, &summary).is_ok());
        let request = WaterSiteRequest {
            footprint: small(200.0, 212.0),
            ..request
        };
        assert!(matches!(
            water_service_site(&map, request, &summary),
            Err(WaterSiteError::OrderConflict { provider, .. }) if provider == next_outlet
        ));

        map.remove_road(south).unwrap();
        assert_eq!(
            water_provider_site(&map, intake),
            Some(Err(WaterSiteError::MissingRoad))
        );
        assert!(water_service_site(&map, outlet_at(152.0), &river_site_summary(&map)).is_ok());
    }

    #[test]
    fn w3_water_site_uses_eligible_shore_and_true_footprint_distance() {
        let mut test = crate::tests::TestCtx::new();
        let mut env = Environment::generate(2, 1, MapPreset::Flat, 0, 0.0);
        env.terrain_apply(
            env.bounds(),
            |point| if point.x <= 96.0 { -4.0 } else { 0.0 },
        );
        env.set_sea_level(Some(-10)).unwrap();
        let mut map = Map::empty();
        assert!(map.install_environment(env));
        let access = road(&mut map, 200.0, 480.0, 160.0);
        *test.g.map_mut() = map;
        let summary = RiverSiteSummary::default();
        let request = WaterSiteRequest {
            role: WaterServiceRole::Intake,
            footprint: footprint(130.0, 256.0),
            connected_road: Some(access),
            max_water_distance_m: 24.0,
            building: None,
            purpose: WaterSitePurpose::Placement,
        };
        assert_eq!(
            water_service_site(&test.g.map(), request, &summary),
            Err(WaterSiteError::NoEligibleShore(WaterServiceRole::Intake))
        );
        let outlet = WaterSiteRequest {
            role: WaterServiceRole::Outlet,
            ..request
        };
        assert!(matches!(
            water_service_site(&test.g.map(), outlet, &summary),
            Ok(WaterSite::Shore { contact, distance_m: 24.0, .. }) if contact.kind == WaterKind::Sea
        ));
        let too_far = WaterSiteRequest {
            max_water_distance_m: 23.0,
            ..outlet
        };
        assert_eq!(
            water_service_site(&test.g.map(), too_far, &summary),
            Err(WaterSiteError::NoEligibleShore(WaterServiceRole::Outlet))
        );
        let wet = WaterSiteRequest {
            footprint: footprint(96.0, 256.0),
            ..outlet
        };
        assert_eq!(
            water_service_site(&test.g.map(), wet, &summary),
            Err(WaterSiteError::WetFootprint)
        );

        let lake = Polygon::centered_rect(vec2(544.0, 256.0), 64.0, 64.0);
        let id = test.g.fill_lake(&lake, -10, 2.0).unwrap();
        let outlet = WaterSiteRequest {
            footprint: footprint(478.0, 256.0),
            ..outlet
        };
        assert_eq!(
            water_service_site(&test.g.map(), outlet, &summary),
            Err(WaterSiteError::NoEligibleShore(WaterServiceRole::Outlet))
        );
        let intake = WaterSiteRequest {
            role: WaterServiceRole::Intake,
            ..outlet
        };
        assert!(matches!(
            water_service_site(&test.g.map(), intake, &summary),
            Ok(WaterSite::Shore { contact, distance_m: 24.0, .. }) if contact.body == id
        ));
        assert_eq!(
            shoreline_value_bonus(&test.g.map(), vec2(512.0, 256.0)),
            10.0
        );
        assert_eq!(
            shoreline_value_bonus(&test.g.map(), vec2(464.0, 256.0)),
            5.0
        );
        assert_eq!(
            shoreline_value_bonus(&test.g.map(), vec2(416.0, 256.0)),
            0.0
        );
        let boundary_bonus = shoreline_value_bonus(&test.g.map(), vec2(1.0, 256.0));
        assert!((boundary_bonus - 10.0 / 96.0).abs() < 1e-5);
        assert_eq!(
            shoreline_value_bonus(&test.g.map(), vec2(f32::NAN, 256.0)),
            0.0
        );
        test.g
            .edit_water(WaterEdit::RemoveFreshWater { body: id })
            .unwrap();
        assert_eq!(
            shoreline_value_bonus(&test.g.map(), vec2(512.0, 256.0)),
            0.0
        );
    }

    #[test]
    fn w3_water_contact_ties_rebind_after_deletion() {
        let mut test = crate::tests::TestCtx::new();
        let mut map = flat_map();
        let access = road(&mut map, 300.0, 720.0, 192.0);
        *test.g.map_mut() = map;
        let lake = |x| Polygon::centered_rect(vec2(x, 256.0), 32.0, 32.0);
        let right = test.g.fill_lake(&lake(576.0), -10, 2.0).unwrap();
        let left = test.g.fill_lake(&lake(448.0), -10, 2.0).unwrap();
        let provider = place(
            &mut test.g.map_mut(),
            "water-pump",
            footprint(512.0, 240.0),
            access,
        );
        let chosen =
            |sim: &Simulation| match water_provider_site(&sim.map(), provider).unwrap().unwrap() {
                WaterSite::Shore {
                    contact,
                    distance_m,
                    ..
                } => {
                    assert_eq!(distance_m, 38.0);
                    contact.body
                }
                WaterSite::Legacy { .. } => panic!("expected lake contact"),
            };
        assert_eq!(chosen(&test.g), right);
        let remove = WaterEdit::RemoveFreshWater { body: right };
        assert_eq!(
            test.g
                .preview_water_edit(&remove)
                .unwrap()
                .provider_impact
                .sample,
            [provider]
        );
        test.g.edit_water(remove).unwrap();
        assert_eq!(chosen(&test.g), left);
    }

    #[test]
    fn w3_water_edit_updates_site_before_saved_network_totals() {
        let mut test = crate::tests::TestCtx::new();
        let mut map = flat_map();
        let south = road(&mut map, 32.0, 480.0, 188.0);
        let north = road(&mut map, 32.0, 480.0, 324.0);
        let pump = place(&mut map, "water-pump", footprint(232.0, 212.0), south);
        let outlet = place(&mut map, "sewage-outlet", footprint(168.0, 300.0), north);
        let house = map
            .build_special_building(
                &footprint(368.0, 300.0),
                BuildingKind::House,
                prototypes::BuildingGen::House,
                None,
                Some(north),
            )
            .unwrap();
        let sim = &mut test.g;
        *sim.map_mut() = map;
        for id in [pump, outlet, house] {
            sim.write::<BuildingInfos>().insert(id);
        }
        residence::on_house_built(sim, house);
        let Some(SoulID::GoodsCompany(company)) =
            company_soul(sim, outlet, GoodsCompanyID::new("sewage-outlet"))
        else {
            panic!("could not start outlet");
        };
        let worker = spawn_human(sim, house).unwrap();
        sim.world.companies[company].workers.0.push(worker);

        let refresh = |sim: &mut Simulation, tick| {
            *sim.write::<GameTime>() = GameTime::new(Tick(tick));
            let (world, resources) = sim.world_res();
            electricity_flow_system(world, resources);
            utility_flow_update(sim);
        };
        let read = |sim: &Simulation| utility_network_at_road(sim, north).unwrap();
        refresh(sim, 3001);
        *sim.write::<GameTime>() = GameTime::new(Tick(3002));
        env_update(sim);
        let dry_value = sim.read::<EnvGrid>().land_value(house);
        let old_totals = read(sim).utility_totals.unwrap();
        assert_eq!(old_totals.sewage_capacity, 500.0);
        let preview = sim.preview_water_edit(&river()).unwrap();
        assert!(!preview.strict_before);
        assert!(preview.strict_after);
        let mut providers = vec![pump, outlet];
        providers.sort_unstable();
        assert_eq!(
            preview.provider_impact,
            WaterProviderImpact {
                count: 2,
                sample: providers
            }
        );
        let body = sim.edit_water(river()).unwrap().body.unwrap();
        assert!(matches!(
            water_provider_site(&sim.map(), outlet),
            Some(Err(WaterSiteError::OrderConflict { provider, .. })) if provider == pump
        ));
        assert!(water_provider_site(&sim.map(), pump).unwrap().is_ok());
        assert_eq!(read(sim).sewage, UtilityNetworkState::Updating);
        assert_eq!(read(sim).utility_totals, Some(old_totals));
        assert_eq!(sim.read::<EnvGrid>().land_value(house), dry_value);
        let value_bonus = shoreline_value_bonus(&sim.map(), vec2(368.0, 300.0));
        assert!(value_bonus > 0.0);

        let bytes = Bincode::encode(sim).unwrap();
        let mut loaded: Simulation = Bincode::decode(&bytes).unwrap();
        assert_eq!(read(&loaded), read(sim));
        assert_eq!(loaded.read::<EnvGrid>().land_value(house), dry_value);
        refresh(&mut loaded, 6001);
        assert_eq!(read(&loaded).utility_totals.unwrap().sewage_capacity, 0.0);
        assert_ne!(read(&loaded).sewage, UtilityNetworkState::Updating);
        *loaded.write::<GameTime>() = GameTime::new(Tick(6002));
        env_update(&mut loaded);
        assert_eq!(
            loaded.read::<EnvGrid>().land_value(house),
            dry_value + value_bonus as u8
        );

        loaded
            .edit_water(WaterEdit::RemoveFreshWater { body })
            .unwrap();
        assert!(loaded.map().environment().strict_water_siting());
        assert!(matches!(
            water_provider_site(&loaded.map(), outlet),
            Some(Err(WaterSiteError::NoEligibleShore(
                WaterServiceRole::Outlet
            )))
        ));
    }

    #[test]
    fn w3_water_command_preview_and_commit_share_rejection() {
        use crate::map::special_placement::{snap_special_building, PlacementError};
        use crate::world_command::CommandOutcomes;

        let mut test = crate::tests::TestCtx::new();
        let mut map = flat_map();
        road(&mut map, 32.0, 480.0, 188.0);
        *test.g.map_mut() = map;
        let lake =
            WaterBasin::new(Polygon::centered_rect(vec2(128.0, 272.0), 64.0, 64.0).as_slice())
                .unwrap();
        test.g
            .edit_water(WaterEdit::FillLake {
                basin: lake,
                level_dm: -10,
                depth_m: 2.0,
            })
            .unwrap();
        let id = GoodsCompanyID::new("water-pump");
        let pump = try_prototype(id).unwrap();
        let command = |pos, connected_road| WorldCommand::MapBuildSpecialBuilding {
            pos,
            kind: BuildingKind::GoodsCompany(id),
            gen: pump.bgen,
            zone: None,
            connected_road: Some(connected_road),
        };
        let rejection =
            Rejection::WaterServiceSite(WaterSiteError::NoEligibleShore(WaterServiceRole::Intake));
        let (failure, accepted) = {
            let map = test.g.map();
            let validate = |args: &crate::map::special_placement::SpecialBuildArgs| {
                let cmd = command(args.obb, args.connected_road);
                cmd.validate_and_price(&test.g)?;
                Ok(cmd)
            };
            let failure = snap_special_building(&map, vec3(340.0, 208.0, 0.0), pump.size, validate)
                .unwrap_err();
            let (_, accepted) =
                snap_special_building(&map, vec3(128.0, 208.0, 0.0), pump.size, validate).unwrap();
            (failure, accepted)
        };
        assert_eq!(failure.error, PlacementError::CommandRejected(rejection));
        let access = test.g.map().roads().keys().next().unwrap();
        let rejected = command(failure.attempted.unwrap().obb, access);
        test.g.apply_commands([&rejected, &accepted]);
        assert_eq!(test.g.read::<CommandOutcomes>().accepted, [1]);
        assert_eq!(test.g.map().buildings().len(), 1);
    }
}
