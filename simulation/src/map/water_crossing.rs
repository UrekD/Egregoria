use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::map::height_override::ConstructionCut;
use crate::map::terrain::CELL_SIZE;
use geom::{Intersect, PolyLine3, Polygon, Shape, Vec2, Vec3, AABB};

use crate::map::{
    profile_height, spend_water_work, Environment, Intersection, IntersectionID, Intersections,
    LaneKind, LanePattern, Lanes, Lot, Map, MapProject, PointGenerateError, ProjectFilter,
    ProjectKind, Road, RoadID, RoadSegmentKind, Roads, SpatialMap, Turn, WaterEditError,
    WaterFootprint, WaterTileCoord, WaterTopologyWork, MAX_SLOPE, ROAD_Z_OFFSET,
};

pub const WATER_BRIDGE_CLEARANCE: f32 = 2.5;
pub(crate) const MAX_DECK_TILE_TESTS: usize = 2_000_000;
const CLEARANCE_EPSILON: f32 = 0.0001;
const RAMP_GRADE: f32 = MAX_SLOPE - 0.01;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionError {
    InvalidEndpoint,
    RoundaboutRailConflict,
    OutOfBounds,
    Grade,
    WetAnchor(WaterTileCoord),
    AtGrade(WaterTileCoord),
    WaterClearance(WaterTileCoord),
    WetJunction(WaterTileCoord),
    WorkLimit,
    Stale,
    LandDenied(crate::map::LandDenial),
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::LandDenied(reason) = self {
            return write!(f, "Land edit denied: {reason:?}");
        }
        f.write_str(match self {
            Self::InvalidEndpoint => "Invalid road or rail endpoint",
            Self::RoundaboutRailConflict => "A same-level railway conflicts with this roundabout",
            Self::OutOfBounds => "The crossing leaves the map",
            Self::Grade => "The deck or a lane is too steep",
            Self::WetAnchor(_) => "Start or end on dry land or attach to an elevated deck",
            Self::AtGrade(_) => "Roads and rails over water require an elevated deck",
            Self::WaterClearance(_) => "The deck needs 2.5 m of clearance above the water",
            Self::WetJunction(_) => "The junction extends over water without enough clearance",
            Self::WorkLimit => "The crossing exceeds the water work limit",
            Self::Stale => "The road network or water changed during placement",
            Self::LandDenied(_) => unreachable!(),
        })
    }
}

impl std::error::Error for ConnectionError {}

impl From<crate::map::LandDenial> for ConnectionError {
    fn from(reason: crate::map::LandDenial) -> Self {
        Self::LandDenied(reason)
    }
}

impl From<WaterEditError> for ConnectionError {
    fn from(error: WaterEditError) -> Self {
        match error {
            WaterEditError::OutOfBounds => Self::OutOfBounds,
            WaterEditError::Limit => Self::WorkLimit,
            WaterEditError::Stale => Self::Stale,
            WaterEditError::LandDenied(reason) => Self::LandDenied(reason),
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
    pub(crate) check_water: bool,
    profile_error: Option<PointGenerateError>,
    pub(crate) preview: RoadPreview,
    pub(crate) merges: Option<[bool; 2]>,
    pub(crate) frontage_work: usize,
    pub(crate) walking: Option<PreparedWalkingTopology>,
    pub(crate) merge_frontage_work: [usize; 2],
    #[cfg(test)]
    merge_lot_allowances: [usize; 2],
}

pub(crate) struct PreparedWalkingTopology {
    pub(crate) sides: [Option<crate::map::RoadSide>; 2],
    pub(crate) approaches: Vec<PreparedApproach>,
}

pub(crate) struct PreparedApproach {
    pub(crate) road: RoadID,
    pub(crate) child: Option<usize>,
    pub(crate) ordinal: usize,
    pub(crate) points: PolyLine3,
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
        (map.land_ownership().revision() << 16) | map.pending_construction_cuts.len(),
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

    pub(crate) fn land_junction_outline(
        &self,
        id: IntersectionID,
    ) -> Result<Polygon, ConnectionError> {
        let junction = self.get(id).ok_or(ConnectionError::InvalidEndpoint)?;
        junction_outline(
            junction,
            &self.roads,
            &mut WaterTopologyWork::new().geometry(),
        )
    }

    pub fn preview_connection(
        &self,
        from: MapProject,
        to: MapProject,
        elbow: Option<Vec2>,
        pattern: &LanePattern,
    ) -> Result<RoadPreview, ConnectionError> {
        let mut work = self
            .needs_topology_preparation()
            .then(WaterTopologyWork::new);
        let (a, b, prepared) = self.prepare_connection(from, to, elbow, pattern, work.as_mut())?;
        if let Some(prepared) = prepared {
            check_profile_error(prepared.profile_error)?;
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

/// Prove that the new deck and every affected road clip and junction can
/// use the dry connection path.
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
            let radius = roundabout.radius + 4.0 + LaneKind::Walking.width();
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
    check_water: bool,
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
    if check_water {
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
    }
    let elevated = Road::is_elevated_span(from.pos, to.pos, env);
    let (profile, error) = Road::generate_points_with_span(
        from.pos,
        to.pos,
        segment(from.pos, to.pos, elbow),
        pattern.lanes().any(|(kind, _, _)| kind.is_rail()),
        env,
        elevated,
    );
    if map.land_ownership().is_tiled() || pattern.is_walking_path() {
        check_profile_error(error)?;
    }
    if check_water && error == Some(PointGenerateError::OutsideOfMap) {
        return Err(ConnectionError::OutOfBounds);
    }

    let (profile, lifted) = if check_water {
        raise_crossing(profile, pattern.width(), env, work)?
    } else {
        (profile, false)
    };
    let elevated = (elevated || lifted) && error != Some(PointGenerateError::OutsideOfMap);
    let mut prepared = stage_connection(
        map,
        from,
        to,
        profile,
        pattern,
        elevated,
        check_water,
        stage_merges,
        work,
    )?;
    if !check_water {
        prepared.profile_error = error;
    }
    Ok(prepared)
}

fn stage_connection(
    map: &Map,
    from: CrossingEndpoint,
    to: CrossingEndpoint,
    profile: PolyLine3,
    pattern: &LanePattern,
    elevated: bool,
    check_water: bool,
    stage_merges: bool,
    work: &mut CrossingWork<'_>,
) -> Result<PreparedTopologyEdit, ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    let src = graph.resolve(from)?;
    let dst = graph.resolve(to)?;
    let mut id = graph.add_road(src, dst, profile.clone(), pattern, elevated, 0)?;
    let tiled = map.land_ownership().is_tiled();
    if check_water || tiled || pattern.is_walking_path() {
        graph.invalidate(src)?;
        graph.invalidate(dst)?;
    }
    let walking = if pattern.is_walking_path() {
        if elevated {
            return Err(ConnectionError::Grade);
        }
        let road = &graph.roads[id];
        if road.interfaced_points().length() < 2.0 {
            return Err(ConnectionError::InvalidEndpoint);
        }
        Some(graph.verify_path_correspondence([src, dst], id)?)
    } else {
        None
    };
    let land_outlines = graph.validate_land()?;
    let frontage_work = graph.frontage_work;
    let mut merge_frontage_work = [0; 2];
    #[cfg(test)]
    let mut merge_lot_allowances = [0; 2];
    let (mut wet, mut junctions) = if check_water {
        graph.validate()?
    } else {
        (BTreeSet::new(), land_outlines)
    };
    let merges = stage_merges.then(|| {
        if !check_water
            && (![src, dst].into_iter().any(|joint| graph.shallow_merge_roads(joint).is_some())
                || graph.invalidate(src).and_then(|_| graph.invalidate(dst)).is_err())
        {
            return [false; 2];
        }
        // Tiled commit generates the split children and the connecting road
        // before the first merge. Unrestricted commit generates only the link.
        let mut frontage: Vec<_> = graph
            .patterns
            .iter()
            .filter(|(road, _)| tiled || **road == id)
            .filter_map(|(&road, pattern)| {
                FrontageGeneration::bound(&graph.roads[road], pattern, tiled)
            })
            .collect();
        [0, 1].map(|end| {
            let junction = [src, dst][end];
            let new_lots = frontage.iter().map(|generation| generation.count).sum();
            if let Some((merged, (next_wet, next_junctions))) =
                graph.stage_shallow_merge(junction, &frontage, new_lots)
            {
                id = merged;
                wet = next_wet;
                junctions = next_junctions;
                merge_frontage_work[end] = graph.frontage_work;
                #[cfg(test)]
                {
                    merge_lot_allowances[end] = new_lots;
                }
                if tiled {
                    // The first successful merge can generate more frontage
                    // before the second clearance and reassignment.
                    frontage.extend(FrontageGeneration::bound(
                        &graph.roads[merged],
                        &graph.patterns[&merged],
                        true,
                    ));
                }
                true
            } else {
                false
            }
        })
    });
    let deck = if check_water || tiled || merges.is_some_and(|ends| ends.contains(&true)) {
        graph.roads[id].interfaced_points().clone()
    } else {
        profile.clone()
    };
    Ok(PreparedTopologyEdit {
        revision: topology_revision(map),
        profile,
        elevated,
        check_water,
        profile_error: None,
        merges,
        frontage_work,
        walking,
        merge_frontage_work,
        #[cfg(test)]
        merge_lot_allowances,
        preview: RoadPreview {
            deck,
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

pub(crate) fn land_deck_triangles(road: &Road) -> impl Iterator<Item = [Vec3; 3]> + '_ {
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

pub(crate) fn grade_fits(points: &PolyLine3) -> bool {
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

struct FrontageGeneration {
    count: usize,
    bounds: AABB,
}

impl FrontageGeneration {
    fn bound(road: &Road, pattern: &LanePattern, tiled: bool) -> Option<Self> {
        if pattern.is_walking_path()
            || (tiled && road.elevated_span())
            || !pattern
                .lanes()
                .any(|(kind, _, _)| kind == crate::map::LaneKind::Walking)
        {
            return None;
        }
        let count = if tiled {
            crate::map::land_frontage::max_tiled_frontage_lots(road)
        } else {
            // Each successful legacy frontage station advances at least 22 m
            // per side. Wet maps place it along the clipped road.
            let length = road.length().max(road.interfaced_points().length());
            (length / 22.0).ceil() as usize * 2
        };
        let bounds = road
            .points()
            .bbox()
            .flatten()
            .union(road.interfaced_points().bbox().flatten())
            .expand(road.width * 0.5 + 1.0 + crate::map::MAX_LOT_SIZE * 1.5);
        (count > 0).then_some(Self { count, bounds })
    }
}

/// Preserve the live lane port and tangent while joining the freshly clipped
/// interior. The staged plan publishes this exact polyline after the split.
fn retain_lane_approach(
    original: &crate::map::Lane,
    live: IntersectionID,
    staged: &mut crate::map::Lane,
    joint: IntersectionID,
) -> Result<(), ConnectionError> {
    let origin = original.get_inter_node_pos(live);
    let direction = if original.src == live {
        original.points.first_dir()
    } else {
        original.points.last_dir().map(|direction| -direction)
    }
    .ok_or(ConnectionError::Stale)?;
    let backwards = staged.dst == joint;
    if backwards {
        staged.points.reverse();
    }
    if staged.points.length() < 4.0 {
        return Err(ConnectionError::Stale);
    }
    let interior = staged.points.cut_start(2.0);
    let next = interior.first();
    let advance = (next - origin).dot(direction);
    if advance <= 1.0 || (next - origin - direction * advance).mag() > 0.7 {
        return Err(ConnectionError::Stale);
    }
    let neighbor = if original.src == live {
        1
    } else {
        original.points.len() - 2
    };
    let tangent = original
        .points
        .get(neighbor)
        .copied()
        .filter(|&point| {
            let distance = origin.distance(point);
            distance > 0.05 && distance < advance - 0.1
        })
        .unwrap_or(origin + direction * 0.5);
    let mut points = PolyLine3::new(
        [origin, tangent].into_iter().chain(interior.into_vec()).collect()
    );
    if backwards {
        points.reverse();
    }
    staged.points = points;
    Ok(())
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
    // Original road lots can survive a split and either staged merge.
    lot_sources: BTreeMap<RoadID, BTreeSet<RoadID>>,
    patterns: BTreeMap<RoadID, LanePattern>,
    splits: BTreeMap<RoadID, [RoadID; 2]>,
    loaded_junctions: BTreeSet<IntersectionID>,
    changed_roads: BTreeSet<RoadID>,
    changed_junctions: BTreeSet<IntersectionID>,
    removed_junctions: BTreeSet<IntersectionID>,
    frontage_work: usize,
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
            lot_sources: BTreeMap::new(),
            patterns: BTreeMap::new(),
            splits: BTreeMap::new(),
            loaded_junctions: BTreeSet::new(),
            changed_roads: BTreeSet::new(),
            changed_junctions: BTreeSet::new(),
            removed_junctions: BTreeSet::new(),
            frontage_work: 0,
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
        let [forward, backward] = live.directed_lanes();
        use crate::map::LaneDirection::{Backward, Forward};
        self.roads[id].install_lane_keys(
            forward
                .iter()
                .rev()
                .map(|&(id, kind)| (id, kind, Forward))
                .chain(backward.iter().map(|&(id, kind)| (id, kind, Backward))),
        );
        if live.is_walking_path() {
            self.roads[id].mark_staged_path();
        }
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
                let before =
                    self.add_road(src, junction, before, &pattern, road.elevated_span(), 0)?;
                let after =
                    self.add_road(junction, dst, after, &pattern, road.elevated_span(), 0)?;
                let live = self.live_roads[&original];
                self.splits.insert(live, [before, after]);
                self.lot_sources.insert(before, BTreeSet::from([live]));
                self.lot_sources.insert(after, BTreeSet::from([live]));
                self.invalidate(src)?;
                self.invalidate(dst)?;
                self.invalidate(junction)?;
                // Commit projects each surviving lot against both raw child profiles.
                let segments = [before, after]
                    .into_iter()
                    .map(|child| self.roads[child].points().len() - 1)
                    .sum::<usize>();
                self.work
                    .charge(self.map.road_lot_count(live).saturating_mul(segments))?;
                self.reserve_reassigned_lots([live], None, &[])?;
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
        new_lots: usize,
    ) -> Result<RoadID, ConnectionError> {
        self.work.charge(
            points.len() + self.junctions[src].roads.len() + self.junctions[dst].roads.len(),
        )?;
        let id = Road::insert_profile(&mut self.roads, src, dst, points, pattern.width(), elevated);
        if pattern.is_walking_path() {
            self.roads[id].mark_staged_path();
        }
        self.patterns.insert(id, pattern.clone());
        self.junctions[src].add_road(&self.roads, &self.roads[id]);
        self.junctions[dst].add_road(&self.roads, &self.roads[id]);
        // The live lot-clear pass computes interfaces before invalidation.
        self.update_interfaces(src)?;
        self.update_interfaces(dst)?;
        self.reserve_lot_clearance(id, new_lots)?;
        Ok(id)
    }

    /// Resolve each old arm to the staged child that still touches that
    /// junction. Compare lane identities by cross-section order before commit.
    fn verify_path_correspondence(
        &mut self,
        endpoints: [IntersectionID; 2],
        path: RoadID,
    ) -> Result<PreparedWalkingTopology, ConnectionError> {
        use crate::map::{LaneKind, TurnKind};

        let mut sides = [None; 2];
        let mut approaches = BTreeMap::new();
        let mut affected = BTreeSet::from(endpoints);
        let splits: Vec<_> = self.splits.keys().copied().collect();
        for id in splits {
            let road = &self.map.roads[id];
            affected.extend([self.junction_keys[&road.src], self.junction_keys[&road.dst]]);
        }
        let mut arms = BTreeSet::new();
        for &joint in &affected {
            self.ensure_arms(joint)?;
            let roads = &self.junctions[joint].roads;
            self.work
                .charge(roads.len().saturating_mul(roads.len()).saturating_mul(32))?;
            arms.extend(roads.iter().copied());
        }

        let mut lanes = Lanes::default();
        for arm in arms {
            let pattern = self.pattern(arm);
            let road = &mut self.roads[arm];
            self.work
                .charge(road.points().len().saturating_mul(pattern.lanes().count()))?;
            road.clear_staged_lanes();
            road.make_lanes(&pattern, &mut lanes);
            for (id, _) in road.lanes_iter() {
                lanes[id].gen_pos(road);
            }
        }
        for joint in affected {
            let staged = &self.junctions[joint];
            let Some(&live) = self.live_junctions.get(&joint) else {
                let Some(endpoint) = endpoints.iter().position(|&id| id == joint) else {
                    continue;
                };
                let other: Vec<_> = staged
                    .roads
                    .iter()
                    .copied()
                    .filter(|&id| id != path)
                    .collect();
                if other.iter().all(|id| self.roads[*id].is_walking_path()) {
                    continue;
                }
                let [before, after] = other.as_slice() else {
                    return Err(ConnectionError::InvalidEndpoint);
                };
                let turns = staged.turn_policy.generate_turns(staged, &lanes, &self.roads);
                let dir = self.roads[path].dir_from(joint);
                let side =
                    |road| crate::map::turn_policy::facing_side(&self.roads[road], joint, dir);
                sides[endpoint] = side(*before);
                if sides[endpoint].is_none() || sides[endpoint] != side(*after) {
                    return Err(ConnectionError::InvalidEndpoint);
                }
                let through = |src: RoadID, dst: RoadID| {
                    self.roads[src]
                        .incoming_lanes_to(joint)
                        .iter()
                        .filter(|(_, kind)| kind.vehicles())
                        .zip(
                            self.roads[dst]
                                .outgoing_lanes_from(joint)
                                .iter()
                                .filter(|(_, kind)| kind.vehicles()),
                        )
                        .map(|(&(a, _), &(b, _))| (a, b))
                        .collect::<BTreeSet<_>>()
                };
                let expected: BTreeSet<_> = through(*before, *after)
                    .into_iter()
                    .chain(through(*after, *before))
                    .collect();
                let driving: BTreeSet<_> = turns
                    .iter()
                    .filter(|(_, kind)| *kind == TurnKind::Driving)
                    .map(|(turn, _)| (turn.src, turn.dst))
                    .collect();
                let path_joins = turns
                    .iter()
                    .filter(|(turn, kind)| {
                        *kind == TurnKind::WalkingCorner && lanes[turn.src].parent == path
                    })
                    .count();
                let continuous = turns
                    .iter()
                    .filter(|(turn, kind)| {
                        *kind == TurnKind::WalkingCorner && lanes[turn.src].parent != path
                    })
                    .count();
                if expected != driving
                    || path_joins != 2
                    || continuous != 2
                    || turns.iter().any(|(_, kind)| *kind == TurnKind::Crosswalk)
                {
                    return Err(ConnectionError::InvalidEndpoint);
                }
                continue;
            };

            let original = &self.map.intersections[live];
            let mut mapped_arms = Vec::new();
            let mut mapped_lanes = BTreeMap::new();
            for &old in &original.roads {
                let old_road = &self.map.roads[old];
                let child = self.splits.get(&old).map(|_| usize::from(old_road.dst == live));
                let new = child.map_or_else(|| self.road_keys[&old], |child| self.splits[&old][child]);
                if !staged.roads.contains(&new) {
                    log::debug!("Walking split changed an arm at {live:?}: {old:?}");
                    return Err(ConnectionError::Stale);
                }
                if old_road
                    .incoming_lanes_to(live)
                    .iter()
                    .any(|(_, k)| k.needs_light())
                {
                    mapped_arms.push(new);
                }
                for (ordinal, ((old_id, kind), (new_id, new_kind))) in
                    old_road.lanes_iter().zip(self.roads[new].lanes_iter()).enumerate()
                {
                    if kind != new_kind {
                        log::debug!("Walking split changed a lane on {old:?}: {kind:?} to {new_kind:?}");
                        return Err(ConnectionError::Stale);
                    }
                    if kind.vehicles() || kind == LaneKind::Walking || kind.is_rail() {
                        let original = &self.map.lanes[old_id];
                        let replacement = &mut lanes[new_id];
                        let displacement = original.get_inter_node_pos(live)
                            .distance(replacement.get_inter_node_pos(joint));
                        let rotation = original.orientation_from(live)
                            .distance(replacement.orientation_from(joint));
                        if displacement > 0.35 || rotation > 0.05 {
                            log::debug!("Walking split cannot retain approach {old_id:?} at {live:?}");
                            return Err(ConnectionError::Stale);
                        }
                        if displacement > 0.0001 || rotation > 0.0001 {
                            self.work.charge(
                                original.points.len().saturating_add(replacement.points.len().saturating_mul(2)),
                            )?;
                            if child.is_some() {
                                retain_lane_approach(original, live, replacement, joint)?;
                            } else {
                                replacement.points = original.points.clone();
                            }
                            approaches.insert((old, child, ordinal), new_id);
                        }
                    }
                    mapped_lanes.insert(old_id, new_id);
                }
            }
            let signals = staged.roads.iter().copied().filter(|&arm| {
                self.roads[arm]
                    .incoming_lanes_to(joint)
                    .iter()
                    .any(|(_, k)| k.needs_light())
            });
            let expected: Option<BTreeSet<_>> = original
                .raw_turns()
                .map(|turn| {
                    Some((
                        crate::map::TurnID::new(
                            joint,
                            *mapped_lanes.get(&turn.id.src)?,
                            *mapped_lanes.get(&turn.id.dst)?,
                            turn.id.bidirectional,
                        ),
                        turn.kind,
                    ))
                })
                .collect();
            if !signals.eq(mapped_arms) {
                log::debug!("Walking split changed the signal order at {live:?}");
                return Err(ConnectionError::Stale);
            }
            let turns = staged.turn_policy.generate_turns(staged, &lanes, &self.roads);
            if expected != Some(turns.into_iter().collect()) {
                log::debug!("Walking split changed the movement set at {live:?}");
                return Err(ConnectionError::Stale);
            }
        }
        let approaches = approaches.into_iter().map(|((road, child, ordinal), id)| {
            PreparedApproach { road, child, ordinal, points: lanes[id].points.clone() }
        }).collect();
        Ok(PreparedWalkingTopology { sides, approaches })
    }

    fn reserve_lot_clearance(
        &mut self,
        id: RoadID,
        new_lots: usize,
    ) -> Result<(), ConnectionError> {
        let road = &self.roads[id];
        let points = road.points();
        // The raw road tests each candidate's four edges against its segments.
        // The endpoint queries perform a constant-cost circle/lot test.
        let per_lot = 4_usize.saturating_add(points.len().saturating_sub(1).saturating_mul(5));
        let changed_objects = self.changed_junctions.len() + self.patterns.len();
        let bounds = points.bbox().flatten().expand(road.width * 0.5);
        let mut reserve = |bounds, per_lot| {
            self.map.spatial_map.reserve_lot_query(
                bounds,
                per_lot,
                changed_objects,
                new_lots,
                self.work.remaining,
            )
        };
        reserve(bounds, per_lot)?;
        for junction in [road.src, road.dst] {
            reserve(self.junctions[junction].bcircle().bbox(), 4)?;
        }
        Ok(())
    }

    fn reserve_reassigned_lots(
        &mut self,
        originals: impl IntoIterator<Item = RoadID>,
        profile_points: Option<usize>,
        frontage: &[FrontageGeneration],
    ) -> Result<(), ConnectionError> {
        let mut lots = Vec::new();
        for road in originals {
            self.work.charge(self.map.road_lot_count(road))?;
            let ids = self.map.road_lots(road);
            if let Some(points) = profile_points {
                let tile_work = frontage_attempt_work(self.map.roads[road].width, true);
                for id in self.map.road_lots(road) {
                    let samples = frontage_samples(&self.map.lots[id].shape);
                    self.work.charge(reassigned_lot_work(points, samples, tile_work))?;
                }
            }
            lots.extend(ids);
        }
        let changed_objects = self.changed_junctions.len() + self.patterns.len();
        self.map.spatial_map.reserve_lot_removals(
            lots,
            frontage.iter().map(|generation| generation.bounds),
            frontage.iter().map(|generation| generation.count).sum(),
            changed_objects,
            self.work.remaining,
        )?;
        Ok(())
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

    fn shallow_merge_roads(&self, joint: IntersectionID) -> Option<[RoadID; 2]> {
        let [first, second] = *self.junctions.get(joint)?.roads else {
            return None;
        };
        let a = &self.roads[first];
        let b = &self.roads[second];
        let no_frontage = |id| {
            self.live_roads
                .get(&id)
                .is_some_and(|id| self.map.no_frontage.contains(id))
        };
        (no_frontage(first) == no_frontage(second)
            && a.other_end(joint)? != b.other_end(joint)?
            && a.dir_from(joint).dot(b.dir_from(joint)) < -0.9)
            .then_some([first, second])
    }

    fn stage_shallow_merge(
        &mut self,
        joint: IntersectionID,
        frontage: &[FrontageGeneration],
        new_lots: usize,
    ) -> Option<(RoadID, (BTreeSet<RoadID>, Vec<Vec<Vec3>>))> {
        let [first, second] = self.shallow_merge_roads(joint)?;
        let a = &self.roads[first];
        let b = &self.roads[second];
        let (src, dst) = (a.other_end(joint)?, b.other_end(joint)?);
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
                    .saturating_add(if self.map.land_ownership().is_tiled() {
                        0
                    } else {
                        self.map.lots.len().saturating_add(new_lots).saturating_mul(2)
                    }),
            )
            .ok()?;
        let elevated = a.elevated_span()
            || b.elevated_span()
            || Road::is_elevated_span(
                self.junctions[src].pos,
                self.junctions[dst].pos,
                self.map.environment(),
            );
        let profile_points = if elevated {
            a.points().len() + b.points().len()
        } else {
            samples.max(a.points().len() + b.points().len())
        };
        let mut lot_sources = BTreeSet::new();
        for id in [first, second] {
            lot_sources.extend(self.live_roads.get(&id).copied());
            if let Some(sources) = self.lot_sources.get(&id) {
                lot_sources.extend(sources.iter().copied());
            }
        }
        // Frontage generated earlier in the live edit can also be reassigned.
        // Round up once more for the generated corners' f32 rounding.
        let generated_samples = (crate::map::MAX_LOT_SIZE / (CELL_SIZE * 0.25)).ceil() as usize + 1;
        let per_lot = reassigned_lot_work(
            profile_points,
            generated_samples,
            frontage_attempt_work(a.width, true),
        );
        self.work.charge(new_lots.saturating_mul(per_lot)).ok()?;
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
            lot_sources: self.lot_sources.clone(),
            patterns: self.patterns.clone(),
            splits: self.splits.clone(),
            loaded_junctions: self.loaded_junctions.clone(),
            changed_roads: self.changed_roads.clone(),
            changed_junctions: self.changed_junctions.clone(),
            removed_junctions: self.removed_junctions.clone(),
            frontage_work: 0,
            wet_splits: self.wet_splits.clone(),
            spatial: SpatialMap::default(),
        };
        let points = a.merged_profile(b, joint);
        let result = (|| {
            trial.ensure_arms(src)?;
            trial.ensure_arms(dst)?;
            trial.remove_road(first);
            trial.remove_road(second);
            trial.validate_removed_junction(joint)?;
            trial.changed_junctions.remove(&joint);
            let id = trial.add_road(src, dst, points, &pattern, elevated, new_lots)?;
            trial.invalidate(src)?;
            trial.invalidate(dst)?;
            trial.reserve_reassigned_lots(
                lot_sources.iter().copied(),
                Some(profile_points),
                frontage,
            )?;
            trial.lot_sources.insert(id, lot_sources);
            let outlines = trial.validate_land()?;
            let validation = if trial.map.environment().strict_water_siting() {
                trial.validate()?
            } else {
                (BTreeSet::new(), outlines)
            };
            Ok::<_, ConnectionError>((id, validation))
        })()
        .ok()?;
        self.roads = trial.roads;
        self.junctions = trial.junctions;
        self.junction_keys = trial.junction_keys;
        self.live_junctions = trial.live_junctions;
        self.road_keys = trial.road_keys;
        self.live_roads = trial.live_roads;
        self.lot_sources = trial.lot_sources;
        self.patterns = trial.patterns;
        self.loaded_junctions = trial.loaded_junctions;
        self.changed_roads = trial.changed_roads;
        self.changed_junctions = trial.changed_junctions;
        self.removed_junctions = trial.removed_junctions;
        self.frontage_work = trial.frontage_work;
        self.wet_splits = trial.wet_splits;
        Some(result)
    }

    fn validate_removed_junction(&mut self, id: IntersectionID) -> Result<(), ConnectionError> {
        use crate::map::PreparedEffects;
        if self.map.land_ownership().is_tiled() {
            if let Some(&live) = self.live_junctions.get(&id) {
                self.map
                    .authorize_land_effects(PreparedEffects::Junction(live))?;
                self.removed_junctions.insert(live);
            }
        }
        Ok(())
    }

    fn stage_roundabout_layouts(&mut self) -> Result<(), ConnectionError> {
        let junctions = self
            .changed_junctions
            .iter()
            .copied()
            .filter(|&id| self.junctions[id].is_roundabout())
            .collect::<Vec<_>>();
        let mut arms = BTreeSet::new();
        for &id in &junctions {
            self.ensure_arms(id)?;
            arms.extend(self.junctions[id].roads.iter().copied());
        }
        let mut lanes = Lanes::default();
        for id in arms {
            let pattern = self.pattern(id);
            let road = &mut self.roads[id];
            self.work.charge(
                road.interfaced_points()
                    .len()
                    .saturating_mul(pattern.lanes().count()),
            )?;
            road.clear_staged_lanes();
            road.make_lanes(&pattern, &mut lanes);
            for (id, _) in road.lanes_iter() {
                lanes[id].gen_pos(road);
            }
        }
        for id in junctions {
            let junction = &mut self.junctions[id];
            let lane_count: usize = junction
                .roads
                .iter()
                .map(|&id| self.roads[id].n_lanes())
                .sum();
            self.work
                .charge(lane_count.saturating_mul(lane_count).saturating_mul(32))?;
            junction.update_turns(&lanes, &self.roads);
            check_roundabout_rail(self.map, junction, self.work, |road| {
                self.road_keys
                    .get(&road)
                    .is_none_or(|id| self.roads.contains_key(*id))
            })?;
        }
        Ok(())
    }

    fn validate_land(&mut self) -> Result<Vec<Vec<Vec3>>, ConnectionError> {
        use crate::map::PreparedEffects;
        self.stage_roundabout_layouts()?;
        if !self.map.land_ownership().is_tiled() {
            return Ok(Vec::new());
        }
        let before = *self.work.remaining;
        self.map.check_land_frontage_capacity(
            self.patterns.iter().filter_map(|(&id, pattern)| {
                pattern
                    .lanes()
                    .any(|(kind, _, _)| kind == crate::map::LaneKind::Walking)
                    .then(|| self.roads.get(id))
                    .flatten()
            }),
            self.work.remaining,
        )?;
        self.frontage_work = before - *self.work.remaining;
        let mut removed: BTreeSet<_> = self
            .removed_junctions
            .iter()
            .copied()
            .map(ProjectKind::Intersection)
            .collect();
        let mut added = Vec::new();
        for (&local, &live) in &self.live_roads {
            if !self.roads.contains_key(local) {
                self.map.authorize_existing_road_deck(live)?;
                let old = &self.map.roads[live];
                removed.insert(live.into());
                for &building in &old.connected_buildings {
                    self.map
                        .authorize_land_effects(PreparedEffects::Building(building))?;
                }
                self.work.charge(self.map.road_lot_count(live))?;
                for id in self.map.road_lots(live) {
                    self.map.authorize_land_effects(PreparedEffects::Lot(id))?;
                }
            }
        }
        for &id in &self.changed_roads {
            let road = &self.roads[id];
            let old = self.live_roads.get(&id).map(|&id| &self.map.roads[id]);
            for triangle in land_deck_triangles(road) {
                self.work.charge(1)?;
                if let Some(old) = old {
                    self.map.authorize_existing_road_triangle(old, triangle)?;
                } else {
                    self.map.authorize_land_effects(PreparedEffects::Footprint(
                        &triangle.map(Vec3::xy),
                    ))?;
                }
            }
            if let Some(old) = old {
                self.map.authorize_existing_road_deck(old.id)?;
                if old.points().as_slice() != road.points().as_slice() {
                    removed.insert(old.id.into());
                    added.push(ConstructionCut::Road(road));
                }
                if old.width != road.width {
                    self.map.authorize_displaced_lots(road.boldline())?;
                }
            } else {
                added.push(ConstructionCut::Road(road));
                let strip = road.boldline();
                self.map.authorize_displaced_lots(strip)?;
            }
        }
        let mut outlines = Vec::new();
        for &id in &self.changed_junctions {
            let joint = &self.junctions[id];
            if let Some(&live) = self.live_junctions.get(&id) {
                self.map
                    .authorize_land_effects(PreparedEffects::Junction(live))?;
                let old = &self.map.intersections[live];
                let outline = junction_outline(old, &self.map.roads, self.work)?;
                if outline.len() >= 3 {
                    self.map
                        .authorize_land_effects(PreparedEffects::Footprint(outline.as_slice()))?;
                }
                if !self.removed_junctions.contains(&live)
                    && (joint.pos != old.pos || joint.radius != old.radius)
                {
                    removed.insert(live.into());
                    added.push(ConstructionCut::Junction(joint));
                }
            } else {
                self.map
                    .authorize_land_effects(PreparedEffects::Point(joint.pos.xy()))?;
                added.push(ConstructionCut::Junction(joint));
            }
            self.map.authorize_displaced_lots(joint.bcircle())?;
            let outline = junction_outline(joint, &self.roads, self.work)?;
            if outline.len() >= 3 {
                self.map
                    .authorize_land_effects(PreparedEffects::Footprint(outline.as_slice()))?;
                outlines.push(
                    outline
                        .iter()
                        .map(|point| point.z(joint.pos.z + ROAD_Z_OFFSET))
                        .collect(),
                );
            }
        }
        self.map
            .authorize_projected_construction_cuts(&removed, &added, self.work.remaining)?;
        Ok(outlines)
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
            let outline = junction_outline(inter, &self.roads, self.work)?;
            let elevated = self.wet_splits.contains(&junction)
                || is_elevated_junction(inter, &self.roads, env);
            wet_tiles(env, &outline.0, self.work, |tile, water| {
                validate_junction_height(inter, elevated, tile, water)
            })?;
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

pub(crate) struct RoadReplacementProjection {
    pub(crate) preview: RoadPreview,
    pub(crate) affected_roads: Vec<RoadID>,
    pub(crate) affected_roads_omitted: usize,
    pub(crate) material: u64,
}

/// Projects the replacement through the same junction interfaces and land/cut
/// authority as construction. The graph and all allocations belong to preparation.
pub(crate) fn prepare_road_replacement(
    map: &Map,
    id: RoadID,
    pattern: &LanePattern,
    remaining: &mut usize,
) -> Result<RoadReplacementProjection, ConnectionError> {
    const MAX_PREVIEW_ROADS: usize = 16;
    let road = map.get(id).ok_or(ConnectionError::InvalidEndpoint)?;
    let mut tile_tests = MAX_DECK_TILE_TESTS;
    let mut samples = 65_536;
    let mut work = CrossingWork::new(remaining, &mut tile_tests, &mut samples);
    let mut graph = LocalTopology::new(map, &mut work);
    let from = graph.load_junction(road.src)?;
    let to = graph.load_junction(road.dst)?;
    let local = graph.road_keys[&id];
    graph.roads[local].width = pattern.width();
    graph.patterns.insert(local, pattern.clone());
    graph.invalidate(from)?;
    graph.invalidate(to)?;
    graph.reserve_lot_clearance(local, 0)?;
    graph.validate_land()?;
    let (wet, junctions) = graph.validate()?;
    for id in &graph.changed_roads {
        graph
            .work
            .charge(graph.roads[*id].interfaced_points().len())?;
    }
    let material = common::hash_iter(graph.changed_roads.iter().map(|&local| {
        let road = &graph.roads[local];
        (
            graph.live_roads[&local],
            road.width.to_bits(),
            common::hash_iter(
                road.interfaced_points()
                    .iter()
                    .map(|p| (p.x.to_bits(), p.y.to_bits(), p.z.to_bits())),
            ),
        )
    }));
    let deck = graph.roads[local].interfaced_points();
    graph.work.charge(deck.len())?;
    let mut affected_roads = graph
        .changed_roads
        .iter()
        .map(|local| graph.live_roads[local])
        .collect::<Vec<_>>();
    affected_roads.sort_unstable();
    let affected_roads_omitted = affected_roads.len().saturating_sub(MAX_PREVIEW_ROADS);
    affected_roads.truncate(MAX_PREVIEW_ROADS);
    Ok(RoadReplacementProjection {
        preview: RoadPreview {
            deck: deck.clone(),
            junctions,
            crosses_water: wet.contains(&local),
        },
        affected_roads,
        affected_roads_omitted,
        material,
    })
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
    for triangle in land_deck_triangles(road) {
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

struct JunctionRoute {
    bounds: AABB,
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
        let bounds = points.bbox().flatten();
        Ok(Self {
            bounds,
            points,
            fits,
        })
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
        work.tile()?;
        if !self.bounds.intersects(&tile_bounds(tile)) {
            return Ok(());
        }
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
    let mut outline = Polygon::default();
    let ring = junction.roundabout_geometry();
    let mouth = |road: &Road| {
        let points = road.interfaced_points();
        let (point, dir) = if road.src == junction.id {
            (points.first(), points.first_dir()?.xy())
        } else {
            (points.last(), -points.last_dir()?.xy())
        };
        let normal = dir.perpendicular() * (road.width * 0.5);
        Some((point.xy() - normal, point.xy() + normal, dir))
    };
    for (&id, &next) in junction
        .roads
        .iter()
        .zip(junction.roads.iter().cycle().skip(1))
    {
        work.charge(1)?;
        let (road, next) = (&roads[id], &roads[next]);
        if let Some(points) = ring.and_then(|ring| ring.road_corner(junction.id, road, next, true))
        {
            work.charge(points.len())?;
            outline.extend(points.map(Vec3::xy));
        } else if let (Some((left, _, src)), Some((_, right, dst))) = (mouth(road), mouth(next)) {
            let spline = Turn::spline(left, right, -src, dst);
            for point in spline.smart_points(1.0, 0.0, 1.0) {
                work.charge(1)?;
                outline.push(point);
            }
        }
    }
    work.charge(outline.len())?;
    // Match Polygon::simplify in one pass over the original vertices.
    Ok(Polygon(
        outline
            .iter()
            .enumerate()
            .filter_map(|(i, &point)| {
                let prev = *outline.get_prev(i);
                let next = *outline.get_next(i);
                (!prev.approx_eq(point)
                    && !(point - prev).normalize().approx_eq((next - point).normalize()))
                .then_some(point)
            })
            .collect(),
    ))
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
    check_water: bool,
    work: &mut CrossingWork<'_>,
) -> Result<usize, ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    let src = graph.load_junction(src)?;
    let dst = graph.load_junction(dst)?;
    let originals = [first, second];
    let [first, second] = originals.map(|road| graph.road_keys[&road]);
    let joint = [graph.roads[first].src, graph.roads[first].dst]
        .into_iter()
        .find(|&id| id == graph.roads[second].src || id == graph.roads[second].dst)
        .ok_or(ConnectionError::InvalidEndpoint)?;
    graph.validate_removed_junction(joint)?;
    graph.remove_road(first);
    graph.remove_road(second);
    let merged = graph.add_road(src, dst, profile.clone(), pattern, elevated, 0)?;
    if check_water || map.land_ownership().is_tiled() {
        graph.invalidate(src)?;
        graph.invalidate(dst)?;
        graph.validate_land()?;
        if check_water {
            graph.validate()?;
        }
    }
    let profile_points = check_water.then(|| graph.roads[merged].interfaced_points().len());
    graph.reserve_reassigned_lots(originals, profile_points, &[])?;
    Ok(graph.frontage_work)
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
            prepare_topology(map, from, to, None, pattern, true, false, work)
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
            stage_connection(map, from, to, profile, pattern, elevated, true, false, work)
        }
    }
}

/// Preview only roads removed or reclipped by accepted demolition commands.
pub(crate) fn demolition_road_clips(
    map: &Map,
    edits: &[ProjectKind],
    work: &mut CrossingWork<'_>,
) -> Result<BTreeMap<RoadID, Option<PolyLine3>>, ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    graph.work.charge(edits.len())?;
    for &edit in edits {
        match edit {
            ProjectKind::Road(id) => {
                let road = map.get(id).ok_or(ConnectionError::InvalidEndpoint)?;
                let src = graph.load_junction(road.src)?;
                let dst = graph.load_junction(road.dst)?;
                let id = graph.road_keys[&id];
                if graph.roads.contains_key(id) {
                    graph.remove_road(id);
                    graph.invalidate(src)?;
                    graph.invalidate(dst)?;
                }
            }
            ProjectKind::Intersection(id) => {
                if map.get(id).is_none() {
                    return Err(ConnectionError::InvalidEndpoint);
                }
                let junction = graph.load_junction(id)?;
                for id in graph.junctions[junction].roads.clone() {
                    let other = graph.roads[id].other_end(junction).unwrap();
                    graph.ensure_arms(other)?;
                    graph.remove_road(id);
                    graph.invalidate(other)?;
                    // Junction removal also invalidates the surviving end after each arm.
                    graph.invalidate(other)?;
                }
            }
            _ => {}
        }
    }
    let mut clips = BTreeMap::new();
    for (&local, &live) in &graph.live_roads {
        match graph.roads.get(local) {
            Some(road) if graph.changed_roads.contains(&local) => {
                graph.work.charge(road.interfaced_points().len())?;
                clips.insert(live, Some(road.interfaced_points().clone()));
            }
            None => {
                clips.insert(live, None);
            }
            _ => {}
        }
    }
    Ok(clips)
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
    if graph.junctions[src].roads.is_empty() {
        graph.validate_removed_junction(src)?;
    }
    if graph.junctions[dst].roads.is_empty() {
        graph.validate_removed_junction(dst)?;
    }
    graph.validate_land()?;
    if map.environment().strict_water_siting() {
        graph.validate()?;
    }
    Ok(())
}

pub(crate) fn prepare_junction_removal(
    map: &Map,
    id: IntersectionID,
    work: &mut CrossingWork<'_>,
) -> Result<(), ConnectionError> {
    let mut graph = LocalTopology::new(map, work);
    let junction = graph.load_junction(id)?;
    graph.validate_removed_junction(junction)?;
    for id in graph.junctions[junction].roads.clone() {
        let src = graph.roads[id].src;
        let dst = graph.roads[id].dst;
        graph.ensure_arms(src)?;
        graph.ensure_arms(dst)?;
        graph.remove_road(id);
        let other = if src == junction { dst } else { src };
        graph.invalidate(other)?;
        graph.invalidate(other)?;
        if graph.junctions[other].roads.is_empty() {
            graph.validate_removed_junction(other)?;
        }
    }
    graph.changed_junctions.remove(&junction);
    graph.validate_land()?;
    if map.environment().strict_water_siting() {
        graph.validate()?;
    }
    Ok(())
}

pub(in crate::map) fn roundabout_changed_clips(
    map: &Map,
    id: IntersectionID,
) -> Result<Vec<RoadID>, ConnectionError> {
    let mut work = WaterTopologyWork::new();
    let mut geometry = work.geometry();
    let mut graph = LocalTopology::new(map, &mut geometry);
    let junction = graph.load_junction(id)?;
    graph.invalidate(junction)?;
    Ok(graph
        .changed_roads
        .iter()
        .filter_map(|road| {
            let original = graph.live_roads[road];
            (graph.roads[*road].interfaced_points().as_slice()
                != map.roads[original].interfaced_points().as_slice())
            .then_some(original)
        })
        .collect())
}

pub(in crate::map) fn check_roundabout_rail(
    map: &Map,
    junction: &Intersection,
    work: &mut CrossingWork<'_>,
    retain_road: impl Fn(RoadID) -> bool,
) -> Result<(), ConnectionError> {
    use crate::transportation::{junction_admission::segments_close, VehicleKind};
    use geom::Segment3;

    let Some(ring) = junction.roundabout_geometry() else {
        return Ok(());
    };
    let clearance = (VehicleKind::Bus.width() + LaneKind::Rail.width()) * 0.5 + 0.8;
    let mut bounds = AABB::centered(ring.center.xy(), Vec2::splat(ring.island_radius() * 2.0));
    for turn in junction.raw_turns() {
        work.charge(turn.points.len())?;
        for p in turn.points.iter() {
            bounds.ll = bounds.ll.min(p.xy());
            bounds.ur = bounds.ur.max(p.xy());
        }
    }
    let bounds = bounds.expand(clearance);
    map.spatial_map()
        .reserve_construction_query(bounds, work.remaining)
        .map_err(|_| ConnectionError::WorkLimit)?;
    let scale = |segment: Segment3, radius: f32| {
        let point = |p: Vec3| p.xy().z((p.z - ring.center.z) * radius / 3.0);
        Segment3::new(point(segment.src), point(segment.dst))
    };
    for hit in map
        .spatial_map()
        .construction_candidates(bounds, ProjectFilter::ROAD)
    {
        let ProjectKind::Road(id) = hit else { continue };
        if !retain_road(id) {
            continue;
        }
        let road = &map.roads[id];
        work.charge(road.n_lanes() + 1)?;
        for (lane, _) in road.lanes_iter().filter(|(_, kind)| kind.is_rail()) {
            for track in map.lanes[lane].points.segments() {
                work.charge(1)?;
                if ring.track_intersects_island(track, LaneKind::Rail.width() * 0.5) {
                    return Err(ConnectionError::RoundaboutRailConflict);
                }
                let track = scale(track, clearance);
                for turn in junction.raw_turns() {
                    work.charge(turn.points.len())?;
                    if turn.points.segments().any(|sweep| {
                        segments_close(track, scale(sweep, clearance), clearance * clearance)
                    }) {
                        return Err(ConnectionError::RoundaboutRailConflict);
                    }
                }
            }
        }
    }
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
    if next.is_roundabout() && next.has_rail_arm(&map.roads) {
        return Err(ConnectionError::RoundaboutRailConflict);
    }
    let mut graph = LocalTopology::new(map, work);
    let id = graph.load_junction(next.id)?;
    graph.junctions[id].turn_policy = next.turn_policy;
    graph.junctions[id].light_policy = next.light_policy;
    graph.invalidate(id)?;
    graph.validate_land()?;
    if map.environment().strict_water_siting() {
        graph.validate()?;
    }
    Ok(())
}

pub(crate) struct ExistingJunctionCrossing {
    outline: Option<(WaterFootprint, AABB, usize)>,
    elevated: bool,
    routes: Vec<JunctionRoute>,
}

impl ExistingJunctionCrossing {
    pub(crate) fn new(
        map: &Map,
        id: IntersectionID,
        work: &mut CrossingWork<'_>,
    ) -> Result<Self, ConnectionError> {
        let junction = &map.intersections[id];
        let outline = junction_outline(junction, &map.roads, work)?;
        let outline = WaterFootprint::new(&outline.0)
            .map(|shape| (shape, outline.bbox(), outline.len()));
        let ground = map
            .environment
            .base_height(junction.pos.xy())
            .unwrap_or(junction.pos.z)
            .max(0.0);
        let elevated = (junction.pos.z >= ground + 0.5
            || map.environment.is_water(junction.pos.xy()))
            && junction.roads.iter().any(|&id| map.roads[id].elevated_span());
        let routes = junction
            .turns()
            .map(|turn| JunctionRoute::new(turn, &map.lanes, work))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            outline,
            elevated,
            routes,
        })
    }

    pub(crate) fn validate_tile(
        &self,
        junction: &Intersection,
        tile: WaterTileCoord,
        water: f32,
        work: &mut CrossingWork<'_>,
    ) -> Result<(), WaterEditError> {
        let object = ProjectKind::Intersection(junction.id);
        work.tile().map_err(|_| WaterEditError::Limit)?;
        if let Some((shape, bounds, vertices)) = &self.outline {
            if bounds.intersects(&tile_bounds(tile)) {
                work.charge(*vertices).map_err(|_| WaterEditError::Limit)?;
                if shape.overlaps_tile(tile) {
                    if !self.elevated {
                        return Err(WaterEditError::TopologyConflict { object, tile });
                    }
                    if junction.pos.z < water + WATER_BRIDGE_CLEARANCE {
                        return Err(WaterEditError::InsufficientClearance { object, tile });
                    }
                }
            }
        }
        for route in &self.routes {
            route.validate_tile(tile, water, work).map_err(|reason| match reason {
                ConnectionError::WorkLimit => WaterEditError::Limit,
                ConnectionError::WetJunction(_) => WaterEditError::InsufficientClearance { object, tile },
                _ => WaterEditError::TopologyConflict { object, tile },
            })?;
        }
        Ok(())
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
            radius = radius.max(roundabout.radius + 4.0 + LaneKind::Walking.width());
        }
    }
    // Turn splines use at most 0.66 times the endpoint chord as a tangent.
    // Their control hull, the roundabout arc and the sidewalks fit in this bound.
    (radius > 0.0).then(|| AABB::centered(center, Vec2::splat(3.0 * radius)))
}

impl Map {
    /// Water-aware frontage rule for lot creation and reassignment.
    pub fn has_dry_frontage(&self, id: RoadID, lot: &geom::OBB) -> bool {
        self.get(id).is_some_and(|road| {
            Lot::allows_frontage(self, road) && Lot::has_dry_frontage(self.environment(), road, lot)
        })
    }
}

fn frontage_samples(shape: &geom::OBB) -> usize {
    (shape.corners[0].distance(shape.corners[1]) / (CELL_SIZE * 0.25))
        .ceil()
        .clamp(1.0, 256.0) as usize
}

/// Only lots on this road need the split projection and the subsequent
/// sample-by-sample frontage check.
pub(crate) fn reserve_reassigned_frontage(
    map: &Map,
    id: RoadID,
    points: usize,
    remaining: &mut usize,
) -> Result<(), WaterEditError> {
    let tile_work = frontage_attempt_work(map.roads[id].width, true);
    spend_water_work(remaining, map.road_lot_count(id))?;
    for id in map.road_lots(id) {
        let samples = frontage_samples(&map.lots[id].shape);
        spend_water_work(remaining, reassigned_lot_work(points, samples, tile_work))?;
    }
    Ok(())
}

fn reassigned_lot_work(points: usize, samples: usize, tile_work: usize) -> usize {
    points
        .saturating_mul(samples + 3)
        .saturating_add(tile_work.saturating_mul(samples.div_ceil(10)))
}

pub(crate) fn generated_frontage_work(length: f32, pattern: &LanePattern, wet: bool) -> usize {
    if pattern.is_walking_path() {
        return 0;
    }
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

    pub(crate) fn project(
        &self,
        points: &PolyLine3,
        xy: Vec2,
        station: f32,
    ) -> Option<(Vec3, usize)> {
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
                (a + (b - a) * segment.project_t(xy), i + 1)
            })
            .min_by(|(a, _), (b, _)| xy.distance2(a.xy()).total_cmp(&xy.distance2(b.xy())))
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

#[cfg(test)]
mod harden1_tests {
    use super::*;
    use crate::map::{LandOwnership, LandTileId, LanePatternBuilder, OwnedTileRect, TileExtent};
    use geom::{vec2, OBB};

    fn ground(x: f32, y: f32) -> MapProject {
        MapProject::ground(vec2(x, y).z0())
    }

    fn tiled(map: &mut Map) {
        map.install_land_ownership(
            LandOwnership::tiled(
                TileExtent {
                    width: 5,
                    height: 5,
                },
                OwnedTileRect {
                    origin: LandTileId { x: 1, y: 1 },
                    width: 3,
                    height: 3,
                },
                Default::default(),
            )
            .unwrap(),
        );
    }

    #[test]
    fn harden1_split_reassignment_reserves_and_batches_dense_stale_lots() {
        const LOTS: usize = 10_000;
        let mut environment = Environment::new(1, 1);
        environment.set_sea_level(Some(-10)).unwrap();
        assert!(!environment.strict_water_siting());
        let mut map = Map::empty();
        assert!(map.install_environment(environment));
        let pattern = LanePatternBuilder {
            rail: true,
            ..Default::default()
        }
        .build();
        let (_, parent) = map
            .make_connection(ground(70.0, 100.0), ground(370.0, 100.0), None, &pattern)
            .unwrap();
        let from = MapProject {
            kind: parent.into(),
            pos: ground(220.0, 100.0).pos,
        };
        let to = ground(220.0, 200.0);
        let cost = |map: &Map| {
            let mut work = WaterTopologyWork::new();
            let initial = work.remaining;
            map.prepare_connection(from, to, None, &pattern, Some(&mut work))
                .unwrap();
            initial - work.remaining
        };
        let empty_cost = cost(&map);
        let shape = OBB::new(vec2(400.0, 350.0), Vec2::X, 4.0, 4.0);
        for _ in 0..LOTS {
            Lot::insert(&mut map, parent, shape, 0.0);
        }
        let added_cost = cost(&map) - empty_cost;
        assert!(added_cost >= 8 * LOTS, "reserved {added_cost} additional work units");
        let mut work = WaterTopologyWork::new();
        work.remaining = empty_cost + 7 * LOTS;
        assert!(matches!(
            map.prepare_connection(from, to, None, &pattern, Some(&mut work)),
            Err(ConnectionError::WorkLimit),
        ));
        assert_eq!(map.lots.len(), LOTS);

        let before = map.spatial_map.removal_scan_entries();
        map.try_connect(from, to, None, &pattern).unwrap();
        let scanned = map.spatial_map.removal_scan_entries() - before;
        assert!(scanned <= 4 * LOTS + 100, "scanned {scanned} entries");
        assert!(map.lots.is_empty());
    }

    #[test]
    fn p7_spatial_density_charges_raw_split_projection_without_wet_tiles() {
        let mut env = Environment::new(1, 1);
        env.set_sea_level(Some(-10)).unwrap();
        assert!(!env.strict_water_siting());
        let mut map = Map::empty();
        assert!(map.install_environment(env));
        let pattern = LanePatternBuilder {
            rail: true,
            ..Default::default()
        }
        .build();
        let (_, parent) = map
            .make_connection(ground(70.0, 100.0), ground(370.0, 100.0), None, &pattern)
            .unwrap();
        map.roads[parent].points = PolyLine3::new(
            (0..=512)
                .map(|i| Vec3::new(70.0 + 300.0 * (i as f32 / 512.0), 100.0, ROAD_Z_OFFSET))
                .collect(),
        );
        use common::saveload::{Bincode, Encoder};
        let mut map: Map = Bincode::decode(&Bincode::encode(&map).unwrap()).unwrap();
        let from = MapProject {
            kind: parent.into(),
            pos: ground(220.0, 100.0).pos,
        };
        let to = ground(220.0, 200.0);
        let cost = |map: &Map| {
            let mut work = WaterTopologyWork::new();
            let initial = work.remaining;
            map.prepare_connection(from, to, None, &pattern, Some(&mut work))
                .unwrap();
            initial - work.remaining
        };
        let baseline = cost(&map);
        let raw_points = map.roads[parent].points().len();
        let shape = OBB::new(vec2(400.0, 350.0), Vec2::X, 4.0, 4.0);
        for _ in 0..200 {
            Lot::insert(&mut map, parent, shape, 0.0);
        }
        let required_projection = 200 * raw_points.saturating_sub(1);
        assert!(cost(&map) >= baseline + required_projection);
        let mut work = WaterTopologyWork::new();
        work.remaining = baseline + required_projection - 1;
        assert!(matches!(
            map.prepare_connection(from, to, None, &pattern, Some(&mut work)),
            Err(ConnectionError::WorkLimit)
        ));
        assert_eq!(map.lots.len(), 200);
    }

    #[test]
    fn harden1_first_merge_reserves_split_child_frontage() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::generate(5, 5, crate::MapPreset::Flat, 0, 0.0)));
        tiled(&mut map);
        let ground = |x: f32, y: f32| ground(x + 1024.0, y + 1024.0);
        let pattern = LanePatternBuilder::new().build();
        let parent = map
            .try_connect(ground(45.0, 150.0), ground(255.0, 150.0), None, &pattern)
            .unwrap()
            .road;
        let arm = map
            .try_connect(ground(150.0, 250.0), ground(150.0, 390.0), None, &pattern)
            .unwrap()
            .road;
        let from = MapProject { kind: parent.into(), pos: ground(150.0, 150.0).pos };
        let to = MapProject { kind: map.roads[arm].src.into(), pos: ground(150.0, 250.0).pos };
        let (_, _, prepared) = map.prepare_connection(from, to, None, &pattern, None).unwrap();
        let prepared = prepared.unwrap();
        assert_eq!(prepared.merges, Some([false, true]));
        assert!(prepared.merge_lot_allowances[1] >= 12, "{:?}", prepared.merge_lot_allowances);
        assert!(map.try_connect(from, to, None, &pattern).unwrap().dst.is_none());
    }

    #[test]
    fn harden1_second_merge_reserves_first_merge_frontage() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::generate(5, 5, crate::MapPreset::Flat, 0, 0.0)));
        tiled(&mut map);
        let ground = |x: f32, y: f32| ground(x + 1024.0, y + 1024.0);
        let pattern = LanePatternBuilder::new().build();
        let left = map
            .try_connect(ground(62.0, 250.0), ground(201.0, 250.0), None, &pattern)
            .unwrap()
            .road;
        let right = map
            .try_connect(ground(301.0, 250.0), ground(440.0, 250.0), None, &pattern)
            .unwrap()
            .road;
        let from = MapProject { kind: map.roads[left].dst.into(), pos: ground(201.0, 250.0).pos };
        let to = MapProject { kind: map.roads[right].src.into(), pos: ground(301.0, 250.0).pos };
        let (_, _, prepared) = map.prepare_connection(from, to, None, &pattern, None).unwrap();
        let prepared = prepared.unwrap();
        assert_eq!(prepared.merges, Some([true, true]));
        let [first, second] = prepared.merge_lot_allowances;
        assert!(second >= first + 10, "{:?}", prepared.merge_lot_allowances);
        let connected = map.try_connect(from, to, None, &pattern).unwrap();
        assert!(connected.src.is_none() && connected.dst.is_none());
    }
}
