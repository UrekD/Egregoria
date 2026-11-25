use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::map::terrain::CELL_SIZE;
use flat_spatial::AABBGrid;
use geom::{
    unpack_height, Intersect, PolyLine3, Polygon, Shape, Vec2, Vec3, AABB, NO_OVERRIDE, OBB,
};

use crate::map::{
    profile_height, spend_water_work, Environment, Intersection, IntersectionID, Intersections,
    LanePattern, Lanes, Lot, LotLayout, Map, MapProject, PointGenerateError, PreparedWaterEdit,
    ProjectKind, Road, RoadID, RoadSegmentKind, Roads, SpatialMap, TerrainChunkID, Turn,
    WaterEditError, WaterFootprint, WaterTileCoord, WaterTopologyWork, MAX_SLOPE, ROAD_Z_OFFSET,
    TERRAIN_CHUNK_RESOLUTION, UNIFORM_LOT_SIZE,
};

pub const WATER_BRIDGE_CLEARANCE: f32 = 2.5;
pub(crate) const MAX_DECK_TILE_TESTS: usize = 2_000_000;
const CLEARANCE_EPSILON: f32 = 0.0001;
const RAMP_GRADE: f32 = MAX_SLOPE - 0.01;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionError {
    InvalidEndpoint,
    OutOfBounds,
    Grade,
    WetAnchor(WaterTileCoord),
    AtGrade(WaterTileCoord),
    WaterClearance(WaterTileCoord),
    WetJunction(WaterTileCoord),
    WorkLimit,
    Stale,
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEndpoint => "Invalid road or rail endpoint",
            Self::OutOfBounds => "The crossing leaves the map",
            Self::Grade => "The deck or a lane is too steep",
            Self::WetAnchor(_) => "Start or end on dry land or attach to an elevated deck",
            Self::AtGrade(_) => "Roads and rails over water require an elevated deck",
            Self::WaterClearance(_) => "The deck needs 2.5 m of clearance above the water",
            Self::WetJunction(_) => "The junction extends over water without enough clearance",
            Self::WorkLimit => "The crossing exceeds the water work limit",
            Self::Stale => "The road network or water changed during placement",
        })
    }
}

impl std::error::Error for ConnectionError {}

impl From<WaterEditError> for ConnectionError {
    fn from(error: WaterEditError) -> Self {
        match error {
            WaterEditError::OutOfBounds => Self::OutOfBounds,
            WaterEditError::Limit => Self::WorkLimit,
            WaterEditError::Stale => Self::Stale,
            _ => Self::InvalidEndpoint,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RoadPreview {
    pub deck: PolyLine3,
    pub junctions: Vec<Vec<Vec3>>,
    pub crosses_water: bool,
}

#[derive(Clone, Debug)]
pub struct ConnectionOutcome {
    pub road: RoadID,
    /// Final junction on the destination side, even after a shallow merge.
    pub destination: IntersectionID,
    pub src: Option<IntersectionID>,
    pub dst: Option<IntersectionID>,
    pub replacements: BTreeMap<RoadID, Vec<RoadID>>,
}

pub(crate) struct CrossingWork<'a> {
    remaining: &'a mut usize,
    tiles: &'a mut usize,
    samples: &'a mut usize,
}

impl<'a> CrossingWork<'a> {
    pub(crate) fn new(
        remaining: &'a mut usize,
        tiles: &'a mut usize,
        samples: &'a mut usize,
    ) -> Self {
        Self {
            remaining,
            tiles,
            samples,
        }
    }

    fn charge(&mut self, work: usize) -> Result<(), ConnectionError> {
        Ok(spend_water_work(self.remaining, work)?)
    }

    fn profile(&mut self, length: f32) -> Result<(), ConnectionError> {
        let samples = (length.ceil() as usize).max(2).saturating_add(2);
        spend_water_work(self.samples, samples)?;
        self.charge(samples)
    }

    fn tile(&mut self) -> Result<(), ConnectionError> {
        spend_water_work(self.tiles, 1)?;
        self.charge(1)
    }
}

pub(crate) struct PreparedTopologyEdit {
    revision: (u64, u64, u64, u64, usize),
    pub(crate) profile: PolyLine3,
    pub(crate) elevated: bool,
    pub(crate) preview: RoadPreview,
    pub(crate) merges: Option<[bool; 2]>,
}

impl PreparedTopologyEdit {
    pub(crate) fn matches(&self, map: &Map) -> bool {
        self.revision == topology_revision(map)
    }
}

fn topology_revision(map: &Map) -> (u64, u64, u64, u64, usize) {
    (
        map.identity(),
        map.walking_topology_generation,
        map.environment().water_revision(),
        map.environment().base_revision,
        map.pending_construction_cuts.len(),
    )
}

#[derive(Clone, Copy)]
pub(crate) struct CrossingEndpoint {
    pub(crate) kind: ProjectKind,
    pub(crate) pos: Vec3,
    pub(crate) split: Option<(Vec3, usize)>,
}

impl Map {
    pub fn road_preview_revision(&self) -> (u64, u64, u64, u64, usize) {
        topology_revision(self)
    }

    pub fn preview_connection(
        &self,
        from: MapProject,
        to: MapProject,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
    ) -> Result<RoadPreview, ConnectionError> {
        let mut work = self
            .environment()
            .has_water_configuration()
            .then(WaterTopologyWork::new);
        let (a, b, prepared) = self.prepare_connection(from, to, elbow, pattern, work.as_mut())?;
        if let Some(prepared) = prepared {
            return Ok(prepared.preview);
        }
        let bounds = self.environment().bounds();
        if !bounds.contains(a.pos.xy())
            || !bounds.contains(b.pos.xy())
            || elbow.is_some_and(|elbow| !bounds.contains(elbow))
        {
            return Err(ConnectionError::OutOfBounds);
        }
        let (deck, error) = Road::generate_points(
            a.pos,
            b.pos,
            segment(a.pos, b.pos, elbow),
            pattern.lanes().any(|(kind, _, _)| kind.is_rail()),
            self.environment(),
        );
        check_profile_error(error)?;
        Ok(RoadPreview {
            deck,
            junctions: Vec::new(),
            crosses_water: false,
        })
    }
}

/// Prove that the new deck and every affected road clip and junction are
/// outside wet chunks before calling the legacy dry builder.
pub(crate) fn connection_is_dry(
    map: &Map,
    from: CrossingEndpoint,
    to: CrossingEndpoint,
    elbow: Option<Vec2>,
    width: f32,
    remaining: &mut usize,
) -> Result<bool, ConnectionError> {
    if map.import_lane_reserve || from.split.is_some() || to.split.is_some() {
        return Ok(false);
    }
    let a = from.pos.xy();
    let b = to.pos.xy();
    let mut min = a.min(b);
    let mut max = a.max(b);
    if let Some(elbow) = elbow {
        let scale = std::f32::consts::FRAC_1_SQRT_2 / 3.0;
        for control in [a + (elbow - a) * scale, b + (elbow - b) * scale] {
            min = min.min(control);
            max = max.max(control);
        }
    }
    let link_bounds = AABB::new_ll_ur(min, max).expand(width * 5.0 + 56.0);
    let env = map.environment();
    let mut dry_chunks = BTreeSet::new();
    let mut check = |bounds: AABB, remaining: &mut usize| -> Result<bool, ConnectionError> {
        for chunk in env.covered_chunks(bounds) {
            spend_water_work(remaining, 1)?;
            if dry_chunks.contains(&chunk) {
                continue;
            }
            let mut wet = env.water_tiles(chunk).peekable();
            if wet.peek().is_none() {
                dry_chunks.insert(chunk);
                continue;
            }
            spend_water_work(remaining, crate::map::TERRAIN_CHUNK_RESOLUTION.pow(2))?;
            for tile in wet {
                let tile = tile_bounds(tile.tile);
                if bounds.ll.x < tile.ur.x
                    && bounds.ur.x > tile.ll.x
                    && bounds.ll.y < tile.ur.y
                    && bounds.ur.y > tile.ll.y
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    };
    if !check(link_bounds, remaining)? {
        return Ok(false);
    }

    let mut roots = BTreeSet::new();
    for endpoint in [from, to] {
        if let ProjectKind::Intersection(id) = endpoint.kind {
            roots.insert(id);
        }
    }
    let mut junctions = roots.clone();
    for &id in &roots {
        for &road in &map.intersections[id].roads {
            spend_water_work(remaining, 1)?;
            junctions.insert(map.roads[road].other_end(id).unwrap());
        }
    }
    let mut road_bounds = BTreeMap::new();
    for id in junctions {
        let inter = &map.intersections[id];
        let xy = inter.pos.xy();
        let mut bounds = AABB::new_ll_ur(xy, xy);
        if roots.contains(&id) {
            bounds = bounds.union(link_bounds);
        }
        for &id in &inter.roads {
            spend_water_work(remaining, 1)?;
            let road = &map.roads[id];
            let bounds_of_road = match road_bounds.entry(id) {
                std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    spend_water_work(remaining, road.points().len())?;
                    *entry.insert(
                        road.points()
                            .bbox()
                            .flatten()
                            .expand(road.width * 5.0 + 2.0),
                    )
                }
            };
            bounds = bounds.union(bounds_of_road);
        }
        if let Some(roundabout) = inter.turn_policy.roundabout {
            let radius = roundabout.radius + 3.0;
            if !radius.is_finite() || radius < 0.0 {
                return Ok(false);
            }
            bounds = bounds.union(AABB::centered(xy, Vec2::splat(radius * 2.0)));
        }
        // Junction turns use cubic tangent lengths below 0.66 times the
        // chord. Their two control points can extend at most 0.22 chords.
        let reach = 0.22 * bounds.size().mag();
        if !check(bounds.expand(reach), remaining)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn segment(from: Vec3, to: Vec3, elbow: Option<Vec2>) -> RoadSegmentKind {
    elbow.map_or(RoadSegmentKind::Straight, |elbow| {
        RoadSegmentKind::from_elbow(from.xy(), to.xy(), elbow)
    })
}

fn check_profile_error(error: Option<PointGenerateError>) -> Result<(), ConnectionError> {
    match error {
        Some(PointGenerateError::OutsideOfMap) => Err(ConnectionError::OutOfBounds),
        Some(PointGenerateError::TooSteep) => Err(ConnectionError::Grade),
        None => Ok(()),
    }
}

pub(crate) fn prepare_topology(
    map: &Map,
    from: CrossingEndpoint,
    to: CrossingEndpoint,
    elbow: Option<Vec2>,
    pattern: &LanePattern,
    stage_merges: bool,
    work: &mut CrossingWork<'_>,
) -> Result<PreparedTopologyEdit, ConnectionError> {
    let env = map.environment();
    let bounds = env.bounds();
    for xy in [Some(from.pos.xy()), Some(to.pos.xy()), elbow]
        .into_iter()
        .flatten()
    {
        if !bounds.contains(xy) {
            return Err(ConnectionError::OutOfBounds);
        }
    }
    for endpoint in [from, to] {
        if endpoint.kind == ProjectKind::Ground && env.is_water(endpoint.pos.xy()) {
            return Err(ConnectionError::WetAnchor(tile_at(endpoint.pos.xy())));
        }
    }
    let a = from.pos.xy();
    let b = to.pos.xy();
    let length = elbow.map_or(a.distance(b), |p| {
        a.distance(b)
            + (a.distance(p) + b.distance(p)) * (2.0 * std::f32::consts::FRAC_1_SQRT_2 / 3.0)
    });
    work.profile(length)?;
    let elevated = Road::is_elevated_span(from.pos, to.pos, env);
    let (profile, error) = Road::generate_points_with_span(
        from.pos,
        to.pos,
        segment(from.pos, to.pos, elbow),
        pattern.lanes().any(|(kind, _, _)| kind.is_rail()),
        env,
        elevated,
    );
    if error == Some(PointGenerateError::OutsideOfMap) {
        return Err(ConnectionError::OutOfBounds);
    }

    let (profile, lifted) = raise_crossing(profile, pattern.width(), env, work)?;
    let elevated = elevated || lifted;
    stage_connection(
        map,
        from,
        to,
        profile,
        pattern,
        elevated,
        stage_merges,
        work,
    )
}

fn stage_connection(
    map: &Map,
    from: CrossingEndpoint,
    to: CrossingEndpoint,
    profile: PolyLine3,
    pattern: &LanePattern,
    elevated: bool,
    stage_merges: bool,
    work: &mut CrossingWork<'_>,
) -> Result<PreparedTopologyEdit, ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    let src = graph.resolve(from)?;
    let dst = graph.resolve(to)?;
    let mut id = graph.add_road(src, dst, profile.clone(), pattern, elevated)?;
    graph.invalidate(src)?;
    graph.invalidate(dst)?;
    let (mut wet, mut junctions) = graph.validate()?;
    let merges = stage_merges.then(|| {
        [src, dst].map(|junction| {
            if let Some((merged, (next_wet, next_junctions))) = graph.stage_shallow_merge(junction)
            {
                id = merged;
                wet = next_wet;
                junctions = next_junctions;
                true
            } else {
                false
            }
        })
    });
    Ok(PreparedTopologyEdit {
        revision: topology_revision(map),
        profile,
        elevated,
        merges,
        preview: RoadPreview {
            deck: graph.roads[id].interfaced_points().clone(),
            junctions,
            crosses_water: wet.contains(&id),
        },
    })
}

fn tile_at(xy: Vec2) -> WaterTileCoord {
    let tile = (xy / CELL_SIZE).floor();
    WaterTileCoord(tile.x as i16, tile.y as i16)
}

fn wet_tiles(
    env: &Environment,
    footprint: &[Vec2],
    work: &mut CrossingWork<'_>,
    mut visit: impl FnMut(WaterTileCoord, f32) -> Result<(), ConnectionError>,
) -> Result<(), ConnectionError> {
    work.charge(footprint.len())?;
    let Some(shape) = WaterFootprint::new(footprint) else {
        return Ok(());
    };
    let bbox = bounds_of(footprint);
    let world = env.bounds();
    let min = bbox.ll.max(world.ll);
    let max = bbox.ur.min(world.ur);
    if max.x <= min.x || max.y <= min.y {
        return Ok(());
    }
    let min = tile_at(min);
    let max = (max / CELL_SIZE).ceil();
    for y in min.1..max.y as i16 {
        for x in min.0..max.x as i16 {
            work.tile()?;
            let tile = WaterTileCoord(x, y);
            if let Some(surface) = env.water_surface_elevation(tile.center()) {
                work.charge(footprint.len())?;
                if shape.overlaps_tile(tile) {
                    visit(tile, surface)?;
                }
            }
        }
    }
    Ok(())
}

fn raise_crossing(
    profile: PolyLine3,
    width: f32,
    env: &Environment,
    work: &mut CrossingWork<'_>,
) -> Result<(PolyLine3, bool), ConnectionError> {
    let profile_pts = profile.as_slice();
    let mut points = vec![profile.first()];
    let mut offsets = vec![deck_normal(profile_pts, 0) * (width * 0.5)];
    for (i, pair) in profile_pts.windows(2).enumerate() {
        let [a, b] = [pair[0], pair[1]];
        let count = (a.xy().distance(b.xy()) / (CELL_SIZE * 0.5))
            .ceil()
            .max(1.0) as usize;
        work.charge(count)?;
        let normal = (b - a)
            .xy()
            .try_normalize()
            .unwrap_or(Vec2::ZERO)
            .perpendicular();
        for part in 1..count {
            points.push(a + (b - a) * (part as f32 / count as f32));
            offsets.push(normal * (width * 0.5));
        }
        points.push(b);
        offsets.push(deck_normal(profile_pts, i + 1) * (width * 0.5));
    }
    let mut needed = vec![f32::NEG_INFINITY; points.len()];
    let mut first_water = None;
    for i in 1..points.len() {
        let a = points[i - 1].xy();
        let b = points[i].xy();
        let [u, v] = [offsets[i - 1], offsets[i]];
        wet_tiles(env, &[a + u, a - u, b - v, b + v], work, |tile, surface| {
            first_water.get_or_insert(tile);
            let z = surface + WATER_BRIDGE_CLEARANCE + ROAD_Z_OFFSET + 0.002;
            needed[i - 1] = needed[i - 1].max(z);
            needed[i] = needed[i].max(z);
            Ok(())
        })?;
    }
    let Some(tile) = first_water else {
        return Ok((profile, false));
    };
    if points.iter().zip(&needed).all(|(p, z)| p.z + 0.002 >= *z) {
        return Ok((profile, true));
    }

    for (point, required) in points.iter().zip(&mut needed) {
        *required = required.max(point.z);
    }
    for i in 1..points.len() {
        let run = points[i - 1].xy().distance(points[i].xy());
        needed[i] = needed[i].max(needed[i - 1] - run * RAMP_GRADE);
    }
    for i in (1..points.len()).rev() {
        let run = points[i - 1].xy().distance(points[i].xy());
        needed[i - 1] = needed[i - 1].max(needed[i] - run * RAMP_GRADE);
    }
    if needed[0] > profile.first().z + CLEARANCE_EPSILON
        || *needed.last().unwrap() > profile.last().z + CLEARANCE_EPSILON
    {
        return Err(ConnectionError::WaterClearance(tile));
    }
    let last = points.len() - 1;
    for i in 1..last {
        points[i].z = needed[i];
    }
    Ok((PolyLine3::new(points), true))
}

// Uses the deck renderer's miter and endpoint normals, including its acute-join cap.
fn deck_normal(points: &[Vec3], i: usize) -> Vec2 {
    if i == 0 || i == points.len() - 1 {
        let a = i.min(points.len() - 2);
        let direction = (points[a + 1] - points[a])
            .try_normalize()
            .unwrap_or(Vec3::ZERO);
        return -direction.xy().perpendicular();
    }
    let a = (points[i] - points[i - 1])
        .xy()
        .try_normalize()
        .unwrap_or(Vec2::ZERO);
    let b = (points[i] - points[i + 1])
        .xy()
        .try_normalize()
        .unwrap_or(Vec2::ZERO);
    let side = a.perp_dot(b);
    let normal = match (a + b).try_normalize() {
        Some(normal) if side >= 0.01 => normal,
        Some(normal) if side <= -0.01 => -normal,
        _ => -a.perpendicular(),
    };
    normal / a.perp_dot(normal).max(0.1)
}

fn deck_triangles(road: &Road) -> impl Iterator<Item = [Vec3; 3]> + '_ {
    let points = road.interfaced_points().as_slice();
    let edge = |i| {
        let offset = (deck_normal(points, i) * (road.width * 0.5)).z0();
        [points[i] + offset, points[i] - offset]
    };
    (1..points.len()).flat_map(move |i| {
        let [a, b] = edge(i - 1);
        let [c, d] = edge(i);
        [[a, b, c], [d, c, b]]
    })
}

// Clip in XY while interpolating Z along the triangles. Both edge-only contact
// and the minimum height over the positive-area intersection are then exact.
fn clipped_deck_height(triangle: [Vec3; 3], bounds: AABB) -> Option<f32> {
    let mut vertices = [Vec3::ZERO; 8];
    vertices[..3].copy_from_slice(&triangle);
    let mut len = 3;
    for (axis, edge, sign) in [
        (0, bounds.ll.x, 1.0),
        (0, bounds.ur.x, -1.0),
        (1, bounds.ll.y, 1.0),
        (1, bounds.ur.y, -1.0),
    ] {
        if len == 0 {
            return None;
        }
        let old = vertices;
        let distance = |point: Vec3| (if axis == 0 { point.x } else { point.y } - edge) * sign;
        let mut a = old[len - 1];
        let mut da = distance(a);
        let mut count = 0;
        for &b in &old[..len] {
            let db = distance(b);
            if (da < 0.0) != (db < 0.0) {
                vertices[count] = a + (b - a) * (da / (da - db));
                count += 1;
            }
            if db >= 0.0 {
                vertices[count] = b;
                count += 1;
            }
            a = b;
            da = db;
        }
        len = count;
    }
    if len < 3 {
        return None;
    }
    let origin = vertices[0].xy();
    let area = vertices[1..len]
        .windows(2)
        .map(|pair| (pair[0].xy() - origin).perp_dot(pair[1].xy() - origin))
        .sum::<f32>();
    (area.abs() > 0.0).then(|| {
        vertices[..len]
            .iter()
            .map(|p| p.z)
            .fold(f32::INFINITY, f32::min)
    })
}

fn tile_bounds(tile: WaterTileCoord) -> AABB {
    AABB::new_ll_size(tile.corner(), Vec2::splat(CELL_SIZE))
}

fn validate_deck_height(
    road: &Road,
    tile: WaterTileCoord,
    surface: f32,
    height: f32,
) -> Result<(), ConnectionError> {
    if !road.elevated_span() {
        return Err(ConnectionError::AtGrade(tile));
    }
    if height - ROAD_Z_OFFSET + CLEARANCE_EPSILON < surface + WATER_BRIDGE_CLEARANCE {
        return Err(ConnectionError::WaterClearance(tile));
    }
    Ok(())
}

fn grade_fits(points: &PolyLine3) -> bool {
    points.as_slice().windows(2).all(|pair| {
        let delta = pair[1] - pair[0];
        delta.is_finite() && delta.z.abs() <= MAX_SLOPE * delta.xy().mag() + CLEARANCE_EPSILON
    })
}

fn bounds_of(points: &[Vec2]) -> AABB {
    let (min, max) = points
        .iter()
        .fold((points[0], points[0]), |(min, max), &point| {
            (min.min(point), max.max(point))
        });
    AABB::new_ll_ur(min, max)
}

struct LocalTopology<'map, 'work, 'budget> {
    map: &'map Map,
    work: &'work mut CrossingWork<'budget>,
    roads: Roads,
    junctions: Intersections,
    junction_keys: BTreeMap<IntersectionID, IntersectionID>,
    live_junctions: BTreeMap<IntersectionID, IntersectionID>,
    road_keys: BTreeMap<RoadID, RoadID>,
    live_roads: BTreeMap<RoadID, RoadID>,
    patterns: BTreeMap<RoadID, LanePattern>,
    loaded_junctions: BTreeSet<IntersectionID>,
    changed_roads: BTreeSet<RoadID>,
    changed_junctions: BTreeSet<IntersectionID>,
    wet_splits: BTreeSet<IntersectionID>,
    spatial: SpatialMap,
}

impl<'map, 'work, 'budget> LocalTopology<'map, 'work, 'budget> {
    fn new(map: &'map Map, work: &'work mut CrossingWork<'budget>) -> Self {
        Self {
            map,
            work,
            roads: Roads::default(),
            junctions: Intersections::default(),
            junction_keys: BTreeMap::new(),
            live_junctions: BTreeMap::new(),
            road_keys: BTreeMap::new(),
            live_roads: BTreeMap::new(),
            patterns: BTreeMap::new(),
            loaded_junctions: BTreeSet::new(),
            changed_roads: BTreeSet::new(),
            changed_junctions: BTreeSet::new(),
            wet_splits: BTreeSet::new(),
            spatial: SpatialMap::default(),
        }
    }

    fn junction_shell(&mut self, original: IntersectionID) -> IntersectionID {
        if let Some(&id) = self.junction_keys.get(&original) {
            return id;
        }
        let live = &self.map.intersections[original];
        let id = self.new_junction(live.pos);
        let junction = &mut self.junctions[id];
        junction.radius = live.radius;
        junction.turn_policy = live.turn_policy;
        junction.light_policy = live.light_policy;
        self.junction_keys.insert(original, id);
        self.live_junctions.insert(id, original);
        id
    }

    fn new_junction(&mut self, pos: Vec3) -> IntersectionID {
        Intersection::make(&mut self.junctions, &mut self.spatial, pos)
    }

    fn load_road(&mut self, original: RoadID) -> Result<RoadID, ConnectionError> {
        if let Some(&id) = self.road_keys.get(&original) {
            return Ok(id);
        }
        let live = &self.map.roads[original];
        self.work
            .charge(live.points().len() + live.interfaced_points().len() + live.n_lanes() + 1)?;
        let src = self.junction_shell(live.src);
        let dst = self.junction_shell(live.dst);
        let id = live.copy_geometry(&mut self.roads, src, dst);
        self.road_keys.insert(original, id);
        self.live_roads.insert(id, original);
        Ok(id)
    }

    fn load_junction(
        &mut self,
        original: IntersectionID,
    ) -> Result<IntersectionID, ConnectionError> {
        let id = self.junction_shell(original);
        if self.loaded_junctions.insert(original) {
            let live = &self.map.intersections[original];
            self.work.charge(live.roads.len())?;
            let arms = live
                .roads
                .iter()
                .map(|&road| self.load_road(road))
                .collect::<Result<_, _>>()?;
            self.junctions[id].roads = arms;
        }
        Ok(id)
    }

    fn ensure_arms(&mut self, id: IntersectionID) -> Result<(), ConnectionError> {
        if let Some(&original) = self.live_junctions.get(&id) {
            self.load_junction(original)?;
        }
        Ok(())
    }

    fn resolve(&mut self, endpoint: CrossingEndpoint) -> Result<IntersectionID, ConnectionError> {
        match endpoint.kind {
            ProjectKind::Ground => Ok(self.new_junction(endpoint.pos)),
            ProjectKind::Intersection(id) => self.load_junction(id),
            ProjectKind::Road(original) => {
                let (point, index) = endpoint.split.ok_or(ConnectionError::InvalidEndpoint)?;
                let live = &self.map.roads[original];
                let src = self.load_junction(live.src)?;
                let dst = self.load_junction(live.dst)?;
                let original = self.road_keys[&original];
                let road = self.remove_road(original);
                let pattern = live.pattern(&self.map.lanes);
                let [before, after] = road.split_profiles(point, index);
                if before.len() < 2 || after.len() < 2 {
                    return Err(ConnectionError::InvalidEndpoint);
                }
                let junction = self.new_junction(endpoint.pos);
                if road.elevated_span() {
                    self.wet_splits.insert(junction);
                }
                self.add_road(src, junction, before, &pattern, road.elevated_span())?;
                self.add_road(junction, dst, after, &pattern, road.elevated_span())?;
                self.invalidate(src)?;
                self.invalidate(dst)?;
                self.invalidate(junction)?;
                Ok(junction)
            }
            _ => Err(ConnectionError::InvalidEndpoint),
        }
    }

    fn add_road(
        &mut self,
        src: IntersectionID,
        dst: IntersectionID,
        points: PolyLine3,
        pattern: &LanePattern,
        elevated: bool,
    ) -> Result<RoadID, ConnectionError> {
        self.work.charge(
            points.len() + self.junctions[src].roads.len() + self.junctions[dst].roads.len(),
        )?;
        let id = Road::insert_profile(&mut self.roads, src, dst, points, pattern.width(), elevated);
        self.patterns.insert(id, pattern.clone());
        self.rebuild_clip(id)?;
        self.junctions[src].add_road(&self.roads, &self.roads[id]);
        self.junctions[dst].add_road(&self.roads, &self.roads[id]);
        // The live lot-clear pass computes interfaces before invalidation.
        self.update_interfaces(src)?;
        self.update_interfaces(dst)?;
        Ok(id)
    }

    fn remove_road(&mut self, id: RoadID) -> Road {
        let road = self.roads.remove(id).expect("staged road exists");
        self.junctions[road.src].remove_road(id);
        self.junctions[road.dst].remove_road(id);
        self.changed_roads.remove(&id);
        road
    }

    fn update_interfaces(&mut self, id: IntersectionID) -> Result<(), ConnectionError> {
        self.ensure_arms(id)?;
        let junction = &mut self.junctions[id];
        let count = junction.roads.len();
        // Near-parallel arms walk their centerlines in the interface builder.
        self.work.charge(count)?;
        if count > 2 {
            for (&a, &b) in junction
                .roads
                .iter()
                .zip(junction.roads.iter().cycle().skip(1))
            {
                let (a, b) = (&self.roads[a], &self.roads[b]);
                if a.dir_from(id).angle(b.dir_from(id)).abs() < 0.17453292 {
                    self.work.profile(a.length())?;
                    self.work.profile(b.length())?;
                }
            }
        }
        junction.update_interface_radius(&mut self.roads);
        self.changed_junctions.insert(id);
        Ok(())
    }

    fn rebuild_clip(&mut self, id: RoadID) -> Result<(), ConnectionError> {
        let road = &mut self.roads[id];
        if !road.elevated_span() {
            self.work.profile(road.length())?;
        }
        self.work.charge(road.points().len())?;
        road.update_interfaced_points(self.map.environment());
        self.changed_roads.insert(id);
        Ok(())
    }

    fn invalidate(&mut self, id: IntersectionID) -> Result<(), ConnectionError> {
        self.ensure_arms(id)?;
        let direct = self.junctions[id].roads.clone();
        if direct.is_empty() {
            self.changed_junctions.remove(&id);
            return Ok(());
        }
        if !self.map.import_lane_reserve {
            self.update_interfaces(id)?;
            for road in direct {
                let other = self.roads[road].other_end(id).unwrap();
                self.update_interfaces(other)?;
                self.rebuild_clip(road)?;
            }
            return Ok(());
        }

        let mut junctions = BTreeSet::from([id]);
        junctions.extend(
            direct
                .iter()
                .filter_map(|road| self.roads[*road].other_end(id)),
        );
        let mut previous = BTreeMap::new();
        for &junction in &junctions {
            self.ensure_arms(junction)?;
            for &road in &self.junctions[junction].roads {
                self.work.charge(1)?;
                let road = &self.roads[road];
                previous.entry(road.id).or_insert(interfaces(road));
            }
        }
        for junction in junctions {
            self.update_interfaces(junction)?;
        }
        for (id, old) in previous {
            let road = &mut self.roads[id];
            reserve_import_lane(road);
            if old != interfaces(road) || direct.contains(&id) {
                self.changed_junctions.extend([road.src, road.dst]);
                self.rebuild_clip(id)?;
            }
        }
        Ok(())
    }

    fn pattern(&self, id: RoadID) -> LanePattern {
        self.patterns
            .get(&id)
            .cloned()
            .unwrap_or_else(|| self.map.roads[self.live_roads[&id]].pattern(&self.map.lanes))
    }

    fn stage_shallow_merge(
        &mut self,
        joint: IntersectionID,
    ) -> Option<(RoadID, (BTreeSet<RoadID>, Vec<Vec<Vec3>>))> {
        let [first, second] = *self.junctions.get(joint)?.roads else {
            return None;
        };
        let a = &self.roads[first];
        let b = &self.roads[second];
        let (src, dst) = (a.other_end(joint)?, b.other_end(joint)?);
        if src == dst || a.dir_from(joint).dot(b.dir_from(joint)) >= -0.9 {
            return None;
        }
        let mut pattern = self.pattern(first);
        let mut other = self.pattern(second);
        if a.src == joint {
            std::mem::swap(&mut pattern.lanes_forward, &mut pattern.lanes_backward);
        }
        if b.src != joint {
            std::mem::swap(&mut other.lanes_forward, &mut other.lanes_backward);
        }
        if pattern != other {
            return None;
        }

        // Reserve the merge's live profiling, lot scans and reassignment before
        // testing geometry. Keep a snapshot of only this bounded local topology.
        let length = a.length() + b.length();
        let samples = (length.ceil() as usize + 2).saturating_mul(3);
        spend_water_work(self.work.samples, samples).ok()?;
        self.work
            .charge(
                samples
                    .saturating_mul((pattern.lanes().count() + 1) * 8)
                    .saturating_add(self.map.lots.len().saturating_mul(2)),
            )
            .ok()?;
        let elevated = a.elevated_span() || b.elevated_span();
        let points = if elevated {
            a.points().len() + b.points().len()
        } else {
            samples.max(a.points().len() + b.points().len())
        };
        for id in [first, second] {
            if let Some(&live) = self.live_roads.get(&id) {
                self.work
                    .charge(reassigned_frontage_work(self.map, live, points))
                    .ok()?;
            } else if pattern
                .lanes()
                .any(|(kind, _, _)| kind == crate::map::LaneKind::Walking)
            {
                // Newly generated vacant lots take at least 22 m of station spacing.
                let lots = (self.roads[id].length() / 22.0).ceil() as usize * 2;
                let per_lot = points.saturating_mul(13) + frontage_attempt_work(a.width, true);
                self.work.charge(lots.saturating_mul(per_lot)).ok()?;
            }
        }
        let snapshot_cost = self
            .roads
            .values()
            .map(|road| road.points().len() + road.interfaced_points().len() + road.n_lanes())
            .sum::<usize>()
            + self
                .junctions
                .values()
                .map(|junction| {
                    junction.roads.len()
                        + junction
                            .turns()
                            .map(|turn| turn.points.len())
                            .sum::<usize>()
                })
                .sum::<usize>();
        self.work.charge(snapshot_cost).ok()?;
        let mut trial = LocalTopology {
            map: self.map,
            work: self.work,
            roads: self.roads.clone(),
            junctions: self.junctions.clone(),
            junction_keys: self.junction_keys.clone(),
            live_junctions: self.live_junctions.clone(),
            road_keys: self.road_keys.clone(),
            live_roads: self.live_roads.clone(),
            patterns: self.patterns.clone(),
            loaded_junctions: self.loaded_junctions.clone(),
            changed_roads: self.changed_roads.clone(),
            changed_junctions: self.changed_junctions.clone(),
            wet_splits: self.wet_splits.clone(),
            spatial: SpatialMap::default(),
        };
        let points = a.merged_profile(b, joint);
        let result = (|| {
            trial.ensure_arms(src)?;
            trial.ensure_arms(dst)?;
            trial.remove_road(first);
            trial.remove_road(second);
            trial.changed_junctions.remove(&joint);
            let id = trial.add_road(src, dst, points, &pattern, elevated)?;
            trial.invalidate(src)?;
            trial.invalidate(dst)?;
            Ok::<_, ConnectionError>((id, trial.validate()?))
        })()
        .ok()?;
        self.roads = trial.roads;
        self.junctions = trial.junctions;
        self.junction_keys = trial.junction_keys;
        self.live_junctions = trial.live_junctions;
        self.road_keys = trial.road_keys;
        self.live_roads = trial.live_roads;
        self.patterns = trial.patterns;
        self.loaded_junctions = trial.loaded_junctions;
        self.changed_roads = trial.changed_roads;
        self.changed_junctions = trial.changed_junctions;
        self.wet_splits = trial.wet_splits;
        Some(result)
    }

    fn validate(&mut self) -> Result<(BTreeSet<RoadID>, Vec<Vec<Vec3>>), ConnectionError> {
        let env = self.map.environment();
        let mut wet = BTreeSet::new();
        for &id in &self.changed_roads {
            if validate_deck(&self.roads[id], env, self.work)? {
                wet.insert(id);
            }
        }

        let junctions = self.changed_junctions.clone();
        let mut lane_roads = wet.clone();
        for &id in &junctions {
            self.ensure_arms(id)?;
            lane_roads.extend(self.junctions[id].roads.iter().copied());
        }
        let mut lanes = Lanes::default();
        for id in lane_roads {
            let pattern = self.pattern(id);
            let road = &mut self.roads[id];
            self.work.charge(
                road.interfaced_points()
                    .len()
                    .saturating_mul(pattern.lanes().count()),
            )?;
            road.clear_staged_lanes();
            road.make_lanes(&pattern, &mut lanes);
            for (lane, _) in road.lanes_iter() {
                let lane = &mut lanes[lane];
                lane.gen_pos(road);
                if wet.contains(&id) && !grade_fits(&lane.points) {
                    return Err(ConnectionError::Grade);
                }
            }
        }

        let mut outlines = Vec::new();
        for junction in junctions {
            let inter = &mut self.junctions[junction];
            let outline = junction_outline(inter, &self.roads, self.work)?;
            let elevated = self.wet_splits.contains(&junction)
                || is_elevated_junction(inter, &self.roads, env);
            wet_tiles(env, &outline.0, self.work, |tile, water| {
                validate_junction_height(inter, elevated, tile, water)
            })?;
            let lane_count: usize = inter.roads.iter().map(|&id| self.roads[id].n_lanes()).sum();
            // A spline's adaptive step is inversely proportional to the
            // square root of its second derivative. Reserve before creating turns.
            let span = junction_support_bounds(inter, &self.roads)
                .map_or(0.0, |bounds| bounds.size().mag());
            let turn_work = 32_usize.saturating_add((span.sqrt() * 20.0) as usize);
            self.work.charge(
                lane_count
                    .saturating_mul(lane_count)
                    .saturating_mul(turn_work),
            )?;
            inter.update_turns(&lanes, &self.roads);
            for turn in inter.turns() {
                let route = JunctionRoute::new(turn, &lanes, self.work)?;
                route.validate(env, self.work)?;
            }
            if !outline.is_empty() {
                outlines.push(
                    outline
                        .iter()
                        .map(|p| p.z(inter.pos.z + ROAD_Z_OFFSET))
                        .collect(),
                );
            }
        }
        Ok((wet, outlines))
    }
}

fn interfaces(road: &Road) -> (f32, f32) {
    (road.interface_from(road.src), road.interface_from(road.dst))
}

pub(crate) fn reserve_import_lane(road: &mut Road) {
    let (src, dst) = interfaces(road);
    let available = (road.length() - 10.0).max(0.0);
    if src + dst > available {
        let scale = available / (src + dst);
        road.set_interface(road.src, src * scale);
        road.set_interface(road.dst, dst * scale);
    }
}

fn validate_deck(
    road: &Road,
    env: &Environment,
    work: &mut CrossingWork<'_>,
) -> Result<bool, ConnectionError> {
    let mut wet = false;
    for triangle in deck_triangles(road) {
        let footprint = triangle.map(Vec3::xy);
        wet_tiles(env, &footprint, work, |tile, surface| {
            if let Some(z) = clipped_deck_height(triangle, tile_bounds(tile)) {
                wet = true;
                validate_deck_height(road, tile, surface, z)?;
            }
            Ok(())
        })?;
    }
    if wet && !grade_fits(road.interfaced_points()) {
        return Err(ConnectionError::Grade);
    }
    Ok(wet)
}

fn is_elevated_junction(inter: &Intersection, roads: &Roads, env: &Environment) -> bool {
    (env.is_water(inter.pos.xy())
        || profile_height(env, inter.pos.xy()).is_some_and(|ground| inter.pos.z >= ground + 0.5))
        && inter.roads.iter().any(|&id| roads[id].elevated_span())
}

fn validate_junction_height(
    inter: &Intersection,
    elevated: bool,
    tile: WaterTileCoord,
    water: f32,
) -> Result<(), ConnectionError> {
    if !elevated || inter.pos.z + CLEARANCE_EPSILON < water + WATER_BRIDGE_CLEARANCE {
        return Err(ConnectionError::WetJunction(tile));
    }
    Ok(())
}

fn segment_window(a: Vec2, b: Vec2, bounds: AABB) -> Option<(f32, f32)> {
    let mut start = 0.0_f32;
    let mut end = 1.0_f32;
    for (src, dst, min, max) in [
        (a.x, b.x, bounds.ll.x, bounds.ur.x),
        (a.y, b.y, bounds.ll.y, bounds.ur.y),
    ] {
        let delta = dst - src;
        if delta == 0.0 {
            if src < min || src > max {
                return None;
            }
        } else {
            let enter = (min - src) / delta;
            let exit = (max - src) / delta;
            start = start.max(enter.min(exit));
            end = end.min(enter.max(exit));
        }
    }
    (start <= end).then_some((start, end))
}

pub(crate) struct JunctionRoute {
    points: PolyLine3,
    fits: bool,
}

impl JunctionRoute {
    fn new(
        turn: &Turn,
        lanes: &Lanes,
        work: &mut CrossingWork<'_>,
    ) -> Result<Self, ConnectionError> {
        work.charge(turn.points.len() + 1)?;
        let mut points = turn.points.clone();
        let destination = lanes[turn.id.dst].get_inter_node_pos(turn.id.parent);
        points.push(destination);
        let fits = grade_fits(&points);
        Ok(Self { points, fits })
    }

    fn validate_segment(
        &self,
        a: Vec3,
        b: Vec3,
        tile: WaterTileCoord,
        water: f32,
    ) -> Result<(), ConnectionError> {
        let Some((lo, hi)) = segment_window(a.xy(), b.xy(), tile_bounds(tile)) else {
            return Ok(());
        };
        // Match half-open water tiles, including zero-length transitions at lane joins.
        let middle = a.xy().lerp(b.xy(), (lo + hi) * 0.5);
        if tile_at(middle) != tile {
            return Ok(());
        }
        let z = a.z + (b.z - a.z) * if b.z < a.z { hi } else { lo };
        if z - ROAD_Z_OFFSET + CLEARANCE_EPSILON < water + WATER_BRIDGE_CLEARANCE {
            return Err(ConnectionError::WetJunction(tile));
        }
        if !self.fits {
            return Err(ConnectionError::Grade);
        }
        Ok(())
    }

    fn validate(
        &self,
        env: &Environment,
        work: &mut CrossingWork<'_>,
    ) -> Result<(), ConnectionError> {
        let world = env.bounds();
        for &[a, b] in self.points.array_windows::<2>() {
            let min = tile_at(a.xy().min(b.xy()).max(world.ll));
            let max = tile_at(a.xy().max(b.xy()).min(world.ur));
            for y in min.1..=max.1 {
                for x in min.0..=max.0 {
                    work.tile()?;
                    let tile = WaterTileCoord(x, y);
                    if let Some(water) = env.water_surface_elevation(tile.center()) {
                        self.validate_segment(a, b, tile, water)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_tile(
        &self,
        tile: WaterTileCoord,
        water: f32,
        work: &mut CrossingWork<'_>,
    ) -> Result<(), ConnectionError> {
        for &[a, b] in self.points.array_windows::<2>() {
            work.tile()?;
            self.validate_segment(a, b, tile, water)?;
        }
        Ok(())
    }
}

// The outer footprint includes the sidewalks at the full road width. Use the
// same clipped arm positions and turns as the junction renderer.
fn junction_outline(
    junction: &Intersection,
    roads: &Roads,
    work: &mut CrossingWork<'_>,
) -> Result<Polygon, ConnectionError> {
    let mut arms = Vec::new();
    for &id in &junction.roads {
        work.charge(1)?;
        let road = &roads[id];
        let points = road.interfaced_points();
        let pose = if road.src == junction.id {
            points.first_dir().map(|dir| (points.first(), dir.xy()))
        } else {
            points.last_dir().map(|dir| (points.last(), -dir.xy()))
        };
        if let Some((point, dir)) = pose {
            let normal = dir.perpendicular() * (road.width * 0.5);
            arms.push((point.xy() - normal, point.xy() + normal, dir));
        }
    }
    let mut outline = Polygon::default();
    let center = junction.pos.xy();
    for ((left, _, src), (_, right, dst)) in arms.iter().zip(arms.iter().cycle().skip(1)) {
        if junction.is_roundabout() {
            if let Some(policy) = junction.turn_policy.roundabout {
                let a = (*left - center).try_normalize();
                let b = (*right - center).try_normalize();
                if a.zip(b)
                    .is_some_and(|(a, b)| a.angle(b).abs() >= 21.0_f32.to_radians())
                {
                    work.charge((policy.radius.abs() / 10.0).ceil() as usize + 1)?;
                    for point in Turn::gen_roundabout(
                        left.z0(),
                        right.z0(),
                        -*src,
                        *dst,
                        policy.radius + 3.0,
                        center,
                    ) {
                        work.charge(1)?;
                        outline.push(point);
                    }
                    continue;
                }
            }
        }
        let spline = Turn::spline(*left, *right, -*src, *dst);
        for point in spline.smart_points(1.0, 0.0, 1.0) {
            work.charge(1)?;
            outline.push(point);
        }
    }
    work.charge(outline.len())?;
    outline.simplify();
    Ok(outline)
}

pub(crate) fn prepare_merge(
    map: &Map,
    first: RoadID,
    second: RoadID,
    src: IntersectionID,
    dst: IntersectionID,
    pattern: &LanePattern,
    profile: &PolyLine3,
    elevated: bool,
    work: &mut CrossingWork<'_>,
) -> Result<(), ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    let src = graph.load_junction(src)?;
    let dst = graph.load_junction(dst)?;
    let first = graph.road_keys[&first];
    let second = graph.road_keys[&second];
    graph.remove_road(first);
    graph.remove_road(second);
    graph.add_road(src, dst, profile.clone(), pattern, elevated)?;
    graph.invalidate(src)?;
    graph.invalidate(dst)?;
    graph.validate()?;
    Ok(())
}

pub(crate) fn prepare_direct_road(
    map: &Map,
    src: IntersectionID,
    dst: IntersectionID,
    pattern: &LanePattern,
    segment: RoadSegmentKind,
    work: &mut CrossingWork<'_>,
) -> Result<PreparedTopologyEdit, ConnectionError> {
    let a = map
        .intersections
        .get(src)
        .ok_or(ConnectionError::InvalidEndpoint)?
        .pos;
    let b = map
        .intersections
        .get(dst)
        .ok_or(ConnectionError::InvalidEndpoint)?
        .pos;
    if src == dst || a.xy().distance(b.xy()) < 1.0 {
        return Err(ConnectionError::InvalidEndpoint);
    }
    let env = map.environment();
    for id in [src, dst] {
        let inter = &map.intersections[id];
        if env.is_water(inter.pos.xy()) && !is_elevated_junction(inter, &map.roads, env) {
            return Err(ConnectionError::WetAnchor(tile_at(inter.pos.xy())));
        }
    }
    match segment {
        RoadSegmentKind::Straight => {
            let from = CrossingEndpoint {
                kind: ProjectKind::Intersection(src),
                pos: a,
                split: None,
            };
            let to = CrossingEndpoint {
                kind: ProjectKind::Intersection(dst),
                pos: b,
                split: None,
            };
            prepare_topology(map, from, to, None, pattern, false, work)
        }
        explicit => {
            let length = match &explicit {
                RoadSegmentKind::Curved((u, v)) => {
                    a.xy().distance(b.xy()) + (u.mag() + v.mag()) * (2.0 / 3.0)
                }
                RoadSegmentKind::Arbitrary(points) => {
                    if points.len() < 2 || points.len() > 65_536 {
                        return Err(ConnectionError::WorkLimit);
                    }
                    work.charge(points.len())?;
                    points.length()
                }
                RoadSegmentKind::Straight => unreachable!(),
            };
            work.profile(length)?;
            let env = map.environment();
            let elevated = Road::is_elevated_span(a, b, env);
            let allow_lift = !matches!(explicit, RoadSegmentKind::Arbitrary(_));
            let (profile, error) = Road::generate_points_with_span(
                a,
                b,
                explicit,
                pattern.lanes().any(|(kind, _, _)| kind.is_rail()),
                env,
                elevated,
            );
            if error == Some(PointGenerateError::OutsideOfMap)
                || profile
                    .iter()
                    .any(|point| !point.is_finite() || !env.bounds().contains(point.xy()))
            {
                return Err(ConnectionError::OutOfBounds);
            }
            let (profile, lifted) = if allow_lift {
                raise_crossing(profile, pattern.width(), env, work)?
            } else {
                (profile, false)
            };
            let elevated = elevated || lifted;
            let from = CrossingEndpoint {
                kind: ProjectKind::Intersection(src),
                pos: a,
                split: None,
            };
            let to = CrossingEndpoint {
                kind: ProjectKind::Intersection(dst),
                pos: b,
                split: None,
            };
            stage_connection(map, from, to, profile, pattern, elevated, false, work)
        }
    }
}

pub(crate) fn prepare_road_removal(
    map: &Map,
    id: RoadID,
    work: &mut CrossingWork<'_>,
) -> Result<(), ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    let src = graph.load_junction(map.roads[id].src)?;
    let dst = graph.load_junction(map.roads[id].dst)?;
    let id = graph.road_keys[&id];
    graph.remove_road(id);
    graph.invalidate(src)?;
    graph.invalidate(dst)?;
    graph.validate()?;
    Ok(())
}

pub(crate) fn prepare_junction_removal(
    map: &Map,
    id: IntersectionID,
    work: &mut CrossingWork<'_>,
) -> Result<(), ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    let junction = graph.load_junction(id)?;
    for id in graph.junctions[junction].roads.clone() {
        let src = graph.roads[id].src;
        let dst = graph.roads[id].dst;
        graph.ensure_arms(src)?;
        graph.ensure_arms(dst)?;
        graph.remove_road(id);
        let other = if src == junction { dst } else { src };
        graph.invalidate(other)?;
        graph.invalidate(other)?;
    }
    graph.changed_junctions.remove(&junction);
    graph.validate()?;
    Ok(())
}

pub(crate) fn prepare_junction_update(
    map: &Map,
    next: &Intersection,
    work: &mut CrossingWork<'_>,
) -> Result<(), ConnectionError> {
    if next.turn_policy.roundabout.is_some_and(|policy| {
        !policy.radius.is_finite() || !(0.0..=40_960.0).contains(&policy.radius)
    }) {
        return Err(ConnectionError::WorkLimit);
    }
    let current = &map.intersections[next.id];
    if current.pos != next.pos || current.roads != next.roads || current.radius != next.radius {
        return Err(ConnectionError::InvalidEndpoint);
    }
    let mut graph = LocalTopology::new(map, work);
    let id = graph.load_junction(next.id)?;
    graph.junctions[id].turn_policy = next.turn_policy;
    graph.junctions[id].light_policy = next.light_policy;
    graph.invalidate(id)?;
    graph.validate()?;
    Ok(())
}

pub(crate) enum ExistingCrossingGeometry {
    Road {
        triangles: Vec<[Vec3; 3]>,
        grade_fits: bool,
    },
    Junction {
        outline: Option<WaterFootprint>,
        elevated: bool,
        routes: Vec<JunctionRoute>,
    },
}

impl ExistingCrossingGeometry {
    pub(crate) fn new(
        map: &Map,
        object: ProjectKind,
        work: &mut CrossingWork<'_>,
    ) -> Result<Self, ConnectionError> {
        match object {
            ProjectKind::Road(id) => {
                let road = &map.roads[id];
                work.charge(road.interfaced_points().len().saturating_mul(2))?;
                let mut fits = grade_fits(road.interfaced_points());
                for (id, _) in road.lanes_iter() {
                    let lane = &map.lanes[id];
                    work.charge(lane.points.len())?;
                    fits &= grade_fits(&lane.points);
                }
                Ok(Self::Road {
                    triangles: deck_triangles(road).collect(),
                    grade_fits: fits,
                })
            }
            ProjectKind::Intersection(id) => {
                let inter = &map.intersections[id];
                let outline = junction_outline(inter, &map.roads, work)?;
                Ok(Self::Junction {
                    outline: WaterFootprint::new(&outline.0),
                    elevated: is_elevated_junction(inter, &map.roads, map.environment()),
                    routes: inter
                        .turns()
                        .map(|turn| JunctionRoute::new(turn, &map.lanes, work))
                        .collect::<Result<_, _>>()?,
                })
            }
            _ => unreachable!("crossing geometry requires a road or intersection"),
        }
    }

    pub(crate) fn validate_tile(
        &self,
        map: &Map,
        object: ProjectKind,
        tile: WaterTileCoord,
        water: f32,
        work: &mut CrossingWork<'_>,
    ) -> Result<(), WaterEditError> {
        let error = match (self, object) {
            (
                Self::Road {
                    triangles,
                    grade_fits,
                },
                ProjectKind::Road(id),
            ) => {
                let road = &map.roads[id];
                let bounds = tile_bounds(tile);
                let mut error = Ok(());
                for &triangle in triangles {
                    work.tile().map_err(|_| WaterEditError::Limit)?;
                    if let Some(z) = clipped_deck_height(triangle, bounds) {
                        error = validate_deck_height(road, tile, water, z);
                        if error.is_ok() && !grade_fits {
                            error = Err(ConnectionError::Grade);
                        }
                        if error.is_err() {
                            break;
                        }
                    }
                }
                error
            }
            (
                Self::Junction {
                    outline,
                    elevated,
                    routes,
                },
                ProjectKind::Intersection(id),
            ) => (|| {
                work.tile()?;
                if outline
                    .as_ref()
                    .is_some_and(|shape| shape.overlaps_tile(tile))
                {
                    validate_junction_height(&map.intersections[id], *elevated, tile, water)?;
                }
                for route in routes {
                    route.validate_tile(tile, water, work)?;
                }
                Ok(())
            })(),
            _ => unreachable!("crossing geometry and object kind agree"),
        };
        error.map_err(|reason| match reason {
            ConnectionError::WorkLimit => WaterEditError::Limit,
            ConnectionError::WaterClearance(_) => {
                WaterEditError::InsufficientClearance { object, tile }
            }
            _ => WaterEditError::TopologyConflict { object, tile },
        })
    }
}

pub(crate) fn road_support_bounds(road: &Road) -> AABB {
    let points = road.interfaced_points().as_slice();
    let first = points[0].xy();
    if points.len() < 2 {
        return AABB::centered(first, Vec2::splat(road.width));
    }
    let (min, max) = points
        .iter()
        .enumerate()
        .fold((first, first), |(min, max), (i, point)| {
            let normal = deck_normal(points, i);
            let offset = Vec2::new(normal.x.abs(), normal.y.abs()) * (road.width * 0.5);
            (min.min(point.xy() - offset), max.max(point.xy() + offset))
        });
    AABB::new_ll_ur(min, max)
}

pub(crate) fn junction_support_bounds(junction: &Intersection, roads: &Roads) -> Option<AABB> {
    let center = junction.pos.xy();
    let mut radius = 0.0_f32;
    for &id in &junction.roads {
        let road = &roads[id];
        let reach = center.distance(road.interface_point(junction.id).xy()) + road.width * 0.5;
        radius = radius.max(reach);
    }
    if junction.is_roundabout() {
        if let Some(roundabout) = junction.turn_policy.roundabout {
            radius = radius.max(roundabout.radius + 3.0);
        }
    }
    // Turn splines use at most 0.66 times the endpoint chord as a tangent.
    // Their control hull, the roundabout arc and the sidewalks fit in this bound.
    (radius > 0.0).then(|| AABB::centered(center, Vec2::splat(3.0 * radius)))
}

impl Map {
    /// Water-aware frontage rule for lot creation and reassignment.
    pub fn has_dry_frontage(&self, id: RoadID, lot: &geom::OBB) -> bool {
        if self.no_frontage.contains(&id) {
            return false;
        }
        if !self.environment().strict_water_siting() {
            return true;
        }
        let Some(road) = self.get(id) else {
            return false;
        };
        let env = self.environment();
        dry_frontage(
            road.width,
            lot,
            |xy, _| Some(road.interfaced_points().project_2d(xy)),
            |xy| profile_height(env, xy),
            |xy| env.is_water(xy),
            |polygon| env.intersects_water(polygon),
        )
    }
}

fn frontage_samples(shape: &geom::OBB) -> usize {
    (shape.corners[0].distance(shape.corners[1]) / (CELL_SIZE * 0.25))
        .ceil()
        .clamp(1.0, 256.0) as usize
}

/// The global scan is reserved by the caller. Only lots on this road need the
/// split projection and the subsequent sample-by-sample frontage check.
pub(crate) fn reassigned_frontage_work(map: &Map, id: RoadID, points: usize) -> usize {
    let tile_work = frontage_attempt_work(map.roads[id].width, true);
    map.lots
        .values()
        .filter(|lot| lot.parent == id)
        .map(|lot| {
            let samples = frontage_samples(&lot.shape);
            points
                .saturating_mul(samples + 3)
                .saturating_add(tile_work.saturating_mul(samples.div_ceil(10)))
        })
        .fold(0, usize::saturating_add)
}

pub(crate) fn generated_frontage_work(length: f32, pattern: &LanePattern, wet: bool) -> usize {
    let sides = pattern
        .lanes()
        .filter(|(kind, _, _)| *kind == crate::map::LaneKind::Walking)
        .count()
        .min(2);
    if sides == 0 {
        return 0;
    }
    // Allow a retry every two meters, including steep profiles. At each
    // station only three segments per frontage sample can be visited.
    let attempts = ((length * (1.0 + MAX_SLOPE * MAX_SLOPE).sqrt() / 2.0).ceil() as usize + 2)
        .saturating_mul(sides);
    attempts.saturating_mul(frontage_attempt_work(pattern.width(), wet) + 10)
}

pub(crate) fn frontage_attempt_work(width: f32, wet: bool) -> usize {
    let tiles_across = ((width * 2.0 + 8.0) / CELL_SIZE).ceil() as usize + 2;
    64 + if wet { 20 * tiles_across.pow(2) } else { 0 }
}

pub(crate) struct FrontageStations {
    ends: Vec<f32>,
}

impl FrontageStations {
    pub(crate) fn new(points: &PolyLine3) -> Self {
        let ends = points
            .segments()
            .scan(0.0, |end, segment| {
                *end += segment.src.distance(segment.dst);
                Some(*end)
            })
            .collect();
        Self { ends }
    }

    pub(crate) fn project(&self, points: &PolyLine3, xy: Vec2, station: f32) -> Option<Vec3> {
        if self.ends.is_empty() {
            return None;
        }
        let index = self
            .ends
            .partition_point(|&end| end < station)
            .min(self.ends.len() - 1);
        let low = index.saturating_sub(1);
        let high = (index + 2).min(self.ends.len());
        (low..high)
            .map(|i| {
                let (a, b) = (points[i], points[i + 1]);
                let segment = geom::Segment {
                    src: a.xy(),
                    dst: b.xy(),
                };
                a.lerp(b, segment.project_t(xy))
            })
            .min_by(|a, b| xy.distance2(a.xy()).total_cmp(&xy.distance2(b.xy())))
    }
}

pub(crate) fn dry_frontage(
    width: f32,
    lot: &geom::OBB,
    mut project: impl FnMut(Vec2, f32) -> Option<Vec3>,
    mut height: impl FnMut(Vec2) -> Option<f32>,
    mut is_water: impl FnMut(Vec2) -> bool,
    mut intersects_water: impl FnMut(&Polygon) -> bool,
) -> bool {
    if intersects_water(&Polygon::from(lot.corners.as_slice())) {
        return false;
    }
    let a = lot.corners[0];
    let b = lot.corners[1];
    let count = frontage_samples(lot);
    let mut previous: Option<[Vec2; 2]> = None;
    for i in 0..=count {
        let fraction = i as f32 / count as f32;
        let xy = a.lerp(b, fraction);
        let Some(point) = project(xy, fraction) else {
            return false;
        };
        if height(xy).is_none_or(|ground| (point.z - ground).abs() > 1.0)
            || xy.distance(point.xy()) > width * 0.5 + 2.0
            || is_water(point.xy())
        {
            return false;
        }
        let edge = [xy, point.xy()];
        if let Some([lot_edge, center]) = previous {
            if intersects_water(&Polygon(vec![lot_edge, center, edge[1], edge[0]])) {
                return false;
            }
            if let Some(direction) = (edge[1] - center).try_normalize() {
                let normal = direction.perpendicular() * (width * 0.5);
                let deck = Polygon(vec![
                    center - normal,
                    center + normal,
                    edge[1] + normal,
                    edge[1] - normal,
                ]);
                if intersects_water(&deck) {
                    return false;
                }
            }
        }
        previous = Some(edge);
    }
    true
}

pub(crate) struct DryingFrontage<'a> {
    pub(crate) map: &'a Map,
    pub(crate) prepared: &'a PreparedWaterEdit,
    pub(crate) cuts: BTreeMap<TerrainChunkID, &'a crate::map::height_override::CutHeights>,
    pub(crate) removed: &'a BTreeSet<ProjectKind>,
    pub(crate) tiles: BTreeMap<TerrainChunkID, BTreeSet<WaterTileCoord>>,
}

pub(crate) struct StagedFrontageLot {
    pub(crate) road: RoadID,
    pub(crate) shape: OBB,
    pub(crate) height: f32,
}

impl DryingFrontage<'_> {
    pub(crate) fn stage(
        &self,
        roads: BTreeSet<RoadID>,
        work: &mut usize,
    ) -> Result<Vec<StagedFrontageLot>, WaterEditError> {
        let mut lots = Vec::<StagedFrontageLot>::new();
        let mut staged = AABBGrid::<usize, AABB>::new(50);
        for id in roads {
            let road = &self.map.roads[id];
            if self.map.no_frontage.contains(&id) || !road.has_sidewalks() {
                continue;
            }
            let points = road.interfaced_points();
            let windows = self.windows(road, work)?;
            if windows.is_empty() {
                continue;
            }
            spend_water_work(work, 3 * points.len())?;
            let stations = FrontageStations::new(points);
            let sidewalks = road.sidewalks(road.src);
            for (side, sidewalk) in [(1.0, sidewalks.outgoing), (-1.0, sidewalks.incoming)] {
                if sidewalk.is_none() {
                    continue;
                }
                let mut along = points.points_dirs_manual();
                for &(start, end) in &windows {
                    let mut station = (start / 2.0).ceil() * 2.0;
                    while station <= end {
                        spend_water_work(work, 1)?;
                        let Some((pos, dir)) = along.next(station) else {
                            break;
                        };
                        let axis = side * dir.perp_up().xy().normalize();
                        let at = pos + (axis * (road.width * 0.5 + 1.0)).z0();
                        let mut distance = 2.0;
                        if let Some(height) =
                            self.height(at.xy()).filter(|z| (z - at.z).abs() <= 1.0)
                        {
                            let sizes: &[f32] = match self.map.lot_layout {
                                LotLayout::Uniform => &[UNIFORM_LOT_SIZE],
                                LotLayout::Varied => &[40.0, 30.0, 20.0],
                            };
                            for &size in sizes {
                                spend_water_work(work, frontage_attempt_work(road.width, true))?;
                                let shape = OBB::new(at.xy() + axis * size * 0.5, axis, size, size);
                                if !self.touches_dried(shape, road.width, work)?
                                    || !dry_frontage(
                                        road.width,
                                        &shape,
                                        |xy, t| {
                                            stations.project(
                                                points,
                                                xy,
                                                station + side * (0.5 - t) * size,
                                            )
                                        },
                                        |xy| self.height(xy),
                                        |xy| {
                                            self.prepared
                                                .water_at(self.map.environment(), xy)
                                                .is_some()
                                        },
                                        |polygon| {
                                            self.prepared
                                                .intersects_water(self.map.environment(), polygon)
                                        },
                                    )
                                    || Lot::frontage_obstructed(
                                        self.map,
                                        shape,
                                        self.removed,
                                        work,
                                    )?
                                {
                                    continue;
                                }
                                let mut overlaps = false;
                                for (_, _, &index) in staged.query(shape.bbox()) {
                                    spend_water_work(work, 1)?;
                                    overlaps |= shape.intersects(&lots[index].shape);
                                }
                                if !overlaps {
                                    staged.insert(shape.bbox(), lots.len());
                                    lots.push(StagedFrontageLot {
                                        road: id,
                                        shape,
                                        height,
                                    });
                                    distance += size * 0.5 + 10.0;
                                    break;
                                }
                            }
                        }
                        station += distance;
                    }
                }
            }
        }
        Ok(lots)
    }

    fn windows(&self, road: &Road, work: &mut usize) -> Result<Vec<(f32, f32)>, WaterEditError> {
        let mut station = 0.0;
        let radius = road.width + 48.0;
        let mut windows = Vec::<(f32, f32)>::new();
        for segment in road.interfaced_points().segments() {
            spend_water_work(work, 1)?;
            let (a, b) = (segment.src.xy(), segment.dst.xy());
            let len = segment.src.distance(segment.dst);
            let bounds = AABB::new_ll_ur(a.min(b), a.max(b)).expand(radius);
            for chunk in self.map.environment().covered_chunks(bounds) {
                spend_water_work(work, 1)?;
                for &tile in self.tiles.get(&chunk).into_iter().flatten() {
                    spend_water_work(work, 1)?;
                    if let Some((lo, hi)) = segment_window(a, b, tile_bounds(tile).expand(radius)) {
                        windows.push((station + lo * len, station + hi * len));
                    }
                }
            }
            station += len;
        }
        spend_water_work(
            work,
            windows
                .len()
                .saturating_mul(windows.len().max(1).ilog2() as usize + 1),
        )?;
        windows.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged = Vec::<(f32, f32)>::new();
        for (lo, hi) in windows {
            if let Some(last) = merged.last_mut().filter(|last| lo <= last.1) {
                last.1 = last.1.max(hi);
            } else {
                merged.push((lo, hi));
            }
        }
        Ok(merged)
    }

    fn touches_dried(
        &self,
        mut shape: OBB,
        width: f32,
        work: &mut usize,
    ) -> Result<bool, WaterEditError> {
        let back = (shape.corners[3] - shape.corners[0]).normalize() * (width + 1.0);
        shape.corners[0] -= back;
        shape.corners[1] -= back;
        let footprint = WaterFootprint::new(&shape.corners).unwrap();
        for chunk in self.map.environment().covered_chunks(shape.bbox()) {
            spend_water_work(work, 1)?;
            for &tile in self.tiles.get(&chunk).into_iter().flatten() {
                spend_water_work(work, 1)?;
                if footprint.overlaps_tile(tile) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn height(&self, point: Vec2) -> Option<f32> {
        let env = self.map.environment();
        let node_height = |point: Vec2| {
            let chunk = TerrainChunkID::new(point);
            if !self.prepared.dirty().terrain().contains(&chunk) && !self.cuts.contains_key(&chunk)
            {
                return env.heightmap.height_nearest(point);
            }
            let node = (point / CELL_SIZE).floor() * CELL_SIZE;
            let base = self.prepared.base_height(env, node)?;
            let index = tile_at(node).local_index();
            let cut = self.cuts.get(&chunk).map(|cuts| {
                cuts[index / TERRAIN_CHUNK_RESOLUTION][index % TERRAIN_CHUNK_RESOLUTION]
            });
            Some(
                cut.filter(|&cut| cut != NO_OVERRIDE)
                    .map_or(base, |cut| base.min(unpack_height(cut))),
            )
        };
        let ll = node_height(point)?;
        let height = match (
            node_height(point + Vec2::x(CELL_SIZE)),
            node_height(point + Vec2::y(CELL_SIZE)),
            node_height(point + Vec2::splat(CELL_SIZE)),
        ) {
            (Some(lr), Some(ul), Some(ur)) => {
                let p = (point / CELL_SIZE).fract();
                let lower = ll + p.x * (lr - ll);
                let upper = ul + p.x * (ur - ul);
                lower + p.y * (upper - lower)
            }
            _ => ll,
        };
        Some(height.max(0.0))
    }
}

pub(crate) fn remap_merged_roads(
    replacements: &mut BTreeMap<RoadID, Vec<RoadID>>,
    old: [RoadID; 2],
    new: RoadID,
) {
    for roads in replacements.values_mut() {
        for id in roads.iter_mut() {
            if old.contains(id) {
                *id = new;
            }
        }
        roads.sort_unstable();
        roads.dedup();
    }
    replacements.extend(old.into_iter().map(|id| (id, vec![new])));
}
