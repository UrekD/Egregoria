use crate::map::terrain::CELL_SIZE;
use crate::map::{
    spend_water_work, Building, Environment, FrontageStations, LotOrigin, Map, ProjectFilter,
    ProjectKind, Road, RoadID, UpdateType,
    WaterEditError, WaterFootprint, WaterTileCoord, MAX_WATER_BATCH_WORK,
};
use geom::{Intersect, Polygon, Segment, Shape, Vec2, Vec3, AABB, OBB};
use ordered_float::OrderedFloat;
use prototypes::BuildingGen;
use serde::{Deserialize, Serialize};
use slotmapd::new_key_type;
use std::collections::BTreeSet;

new_key_type! {
    pub struct LotID;
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum LotKind {
    Unassigned,
    Residential,
}

/// How new lots are sized along roads. Built lots keep the shape they were built on.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LotLayout {
    /// A deterministic mix of 20, 30 and 40 m lots.
    #[default]
    Varied,
    /// Every lot is `UNIFORM_LOT_SIZE` square, so lots line up along both sides of a road.
    Uniform,
}

/// The middle of the varied mix and the smallest lot every growable fits on (900 m²).
pub const UNIFORM_LOT_SIZE: f32 = 30.0;
pub(crate) const MAX_LOT_SIZE: f32 = 40.0;
pub(crate) const FRONTAGE_HEIGHT_TOLERANCE_M: f32 = 1.0;
const FRONTAGE_DISTANCE_EPSILON_M: f32 = 0.01;
pub(crate) const MIN_LOT_SIZE: f32 = 20.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lot {
    pub id: LotID,
    pub parent: RoadID,
    pub kind: LotKind,
    pub shape: OBB,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowableLotError {
    MissingLot,
    MissingRoad,
    UnsupportedDensity,
    MissingPrototype,
    TooSmall,
    Obstructed,
    Wet,
    InfeasibleGrade,
    MixedUseDisabled,
    LandDenied(crate::map::LandDenial),
}

impl Lot {
    pub(crate) fn check_growable_footprint(
        &self,
        map: &Map,
        generator: BuildingGen,
    ) -> Result<(), GrowableLotError> {
        let road = map.get(self.parent).ok_or(GrowableLotError::MissingRoad)?;
        map.authorize_frontage(self.parent, &self.shape, None)
            .map_err(GrowableLotError::LandDenied)?;
        if !Self::allows_frontage(map, road) {
            return Err(GrowableLotError::MissingRoad);
        }
        if !Building::valid_footprint(&self.shape, generator) {
            return Err(GrowableLotError::Obstructed);
        }
        if !Building::dry_footprint(map.environment(), &self.shape, None)
            || !Self::has_dry_frontage(map.environment(), road, &self.shape)
        {
            return Err(GrowableLotError::Wet);
        }
        if Self::footprint_obstructed(map, self.shape, false) {
            return Err(GrowableLotError::Obstructed);
        }
        Ok(())
    }

    pub(crate) fn footprint_obstructed(map: &Map, shape: OBB, include_lots: bool) -> bool {
        let mut filter = ProjectFilter::INTER | ProjectFilter::BUILDING;
        if include_lots {
            filter = filter | ProjectFilter::LOT;
        }
        map.spatial_map.query(shape, filter).next().is_some()
            || map
                .spatial_map
                .query(Polygon::from(shape.corners.as_slice()), ProjectFilter::ROAD)
                .next()
                .is_some()
    }

    pub(crate) fn try_restore(map: &mut Map, origin: LotOrigin) -> Option<LotID> {
        const FRONTAGE_RADIUS: f32 = 15.0;

        let shape = origin.shape;
        if !Building::dry_footprint(map.environment(), &shape, None)
            || Self::footprint_obstructed(map, shape, true)
        {
            return None;
        }

        let front = (shape.corners[0] + shape.corners[1]) * 0.5;
        let (road, projected, _) = map
            .spatial_map
            .query_around(front, FRONTAGE_RADIUS, ProjectFilter::ROAD)
            .filter_map(|object| {
                let ProjectKind::Road(id) = object else {
                    return None;
                };
                let road = map.roads.get(id)?;
                if !Self::allows_frontage(map, road)
                    || map.authorize_frontage(id, &shape, None).is_err()
                    || !Self::has_dry_frontage(map.environment(), road, &shape)
                {
                    return None;
                }
                let projected = road.points.project_2d(front);
                let dist = projected.xy().distance2(front);
                (dist <= FRONTAGE_RADIUS.powi(2)).then_some((id, projected, OrderedFloat(dist)))
            })
            .min_by_key(|&(id, _, distance)| (distance, id))?;

        let depth = shape.corners[3] - shape.corners[0];
        let id = Self::try_make(
            map,
            road,
            front.z(projected.z),
            depth.normalize(),
            depth.mag(),
        )?;
        map.subscribers.dispatch(UpdateType::Road, &map.lots[id]);
        Some(id)
    }

    pub fn try_make(
        map: &mut Map,
        parent: RoadID,
        at: Vec3,
        axis: Vec2,
        size: f32,
    ) -> Option<LotID> {
        if !at.is_finite()
            || !axis.is_finite()
            || axis == Vec2::ZERO
            || !size.is_finite()
            || size <= 0.0
        {
            return None;
        }
        let shape = OBB::new(at.xy() + axis * size * 0.5, axis, size, size);
        Self::make_if_clear(map, parent, at, shape)
    }

    fn make_if_clear(map: &mut Map, parent: RoadID, at: Vec3, shape: OBB) -> Option<LotID> {
        map.authorize_frontage(parent, &shape, None).ok()?;
        let height = map.environment.height(at.xy())?;
        if (height - at.z).abs() > FRONTAGE_HEIGHT_TOLERANCE_M {
            return None;
        }
        let road = map.roads.get(parent)?;
        if !Self::allows_frontage(map, road)
            || !Building::dry_footprint(&map.environment, &shape, None)
            || Self::footprint_obstructed(map, shape, true)
            || !Self::has_dry_frontage(map.environment(), road, &shape)
        {
            return None;
        }

        Some(Self::insert(map, parent, shape, height))
    }

    pub(crate) fn insert(map: &mut Map, parent: RoadID, shape: OBB, height: f32) -> LotID {
        let id = map.lots.insert_with_key(move |id| Lot {
            id,
            parent,
            kind: LotKind::Unassigned,
            shape,
            height,
        });
        map.spatial_map.insert(&map.lots[id]);
        map.track_road_lot(parent, id);
        id
    }

    pub(crate) fn allows_frontage(map: &Map, road: &Road) -> bool {
        !map.no_frontage.contains(&road.id)
            && !map.land_ownership().is_protected(road.id.into())
            && road.has_sidewalks()
    }

    /// Check the lot, its adjoining deck and ground-level access.
    pub(crate) fn has_dry_frontage(env: &Environment, road: &Road, shape: &OBB) -> bool {
        let mut remaining = MAX_WATER_BATCH_WORK;
        !env.strict_water_siting()
            || Self::check_dry_frontage(
                road,
                shape,
                |front, remaining| {
                    let points = road.interfaced_points();
                    spend_water_work(remaining, points.n_points())?;
                    Ok(Some(points.project_segment_2d(front)))
                },
                |point| env.height(point),
                |tile| env.is_water(tile.center()),
                &mut remaining,
            )
            .unwrap_or(false)
    }

    pub(crate) fn check_dry_frontage(
        road: &Road,
        shape: &OBB,
        project: impl FnOnce(Vec2, &mut usize) -> Result<Option<(Vec3, usize)>, WaterEditError>,
        height: impl Fn(Vec2) -> Option<f32>,
        mut is_wet: impl FnMut(WaterTileCoord) -> bool,
        remaining: &mut usize,
    ) -> Result<bool, WaterEditError> {
        let Some(footprint) = WaterFootprint::new(&shape.corners) else {
            return Ok(false);
        };
        for tile in footprint_tiles(shape.bbox()) {
            spend_water_work(remaining, 1)?;
            if footprint.overlaps_tile(tile) && is_wet(tile) {
                return Ok(false);
            }
        }

        let [a, b, _, _] = shape.corners;
        let front = (a + b) * 0.5;
        let points = road.interfaced_points();
        let Some((projected, end)) = project(front, remaining)? else {
            return Ok(false);
        };
        if end == 0 || projected.xy().distance(front) > road.width * 0.5 + 2.0 {
            return Ok(false);
        }
        let segment = Segment {
            src: points[end - 1].xy(),
            dst: points[end].xy(),
        };
        let sidewalks = road.sidewalks(road.src);
        let sidewalk =
            if (front - projected.xy()).dot((segment.dst - segment.src).perpendicular()) > 0.0 {
                sidewalks.outgoing
            } else {
                sidewalks.incoming
            };
        if sidewalk.is_none() {
            return Ok(false);
        }

        let mut usable = |index: usize, start: f32, end: f32| -> Result<bool, WaterEditError> {
            spend_water_work(remaining, 1)?;
            if start == end {
                return Ok(true);
            }
            for triangle in deck_triangles(road, index, start, end) {
                for tile in footprint_tiles(triangle.bounds) {
                    spend_water_work(remaining, 1)?;
                    if triangle.minimum_z_in(tile).is_some() && is_wet(tile) {
                        return Ok(false);
                    }
                }
            }
            let from = points[index];
            let delta = points[index + 1] - from;
            let length = delta.xy().mag() * (end - start);
            let steps = (length / (CELL_SIZE * 0.5)).ceil().max(1.0) as usize;
            for step in 0..=steps {
                spend_water_work(remaining, 1)?;
                let pos = from + delta * (start + (end - start) * (step as f32 / steps as f32));
                if !height(pos.xy())
                    .is_some_and(|ground| (ground - pos.z).abs() <= FRONTAGE_HEIGHT_TOLERANCE_M)
                {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let projection = segment.project_t(front);
        for backwards in [true, false] {
            let mut left = a.distance(b) * 0.5;
            let mut index = end - 1;
            let mut t = projection;
            while left > 0.0 {
                let length = points[index].xy().distance(points[index + 1].xy());
                if length > 0.0 {
                    let edge = if backwards {
                        (t - left / length).max(0.0)
                    } else {
                        (t + left / length).min(1.0)
                    };
                    if !usable(index, edge.min(t), edge.max(t))? {
                        return Ok(false);
                    }
                    let covered = (edge - t).abs() * length;
                    if left <= covered || (edge > 0.0 && edge < 1.0) {
                        break;
                    }
                    left -= covered;
                }
                if backwards && index > 0 {
                    index -= 1;
                    t = 1.0;
                } else if !backwards && index + 2 < points.n_points() {
                    index += 1;
                    t = 0.0;
                } else if left <= FRONTAGE_DISTANCE_EPSILON_M {
                    // Reversing a deck during a merge can round a projection
                    // slightly differently at the same junction.
                    break;
                } else {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    pub fn generate_along_road(map: &mut Map, road: RoadID) {
        Self::generate_along_final_road(map, road, None);
    }

    pub(crate) fn generate_along_final_road(
        map: &mut Map,
        road: RoadID,
        mut reserved: Option<usize>,
    ) {
        if map.land_ownership().is_tiled() {
            // Tiled topology provisions frontage through the prepared reducer.
            return;
        }
        let Some(r) = map.roads.get(road) else {
            log::error!("trying to generate along invalid road");
            return;
        };
        if !Self::allows_frontage(map, r) {
            return;
        }
        fn gen_side(
            map: &mut Map,
            road: RoadID,
            side: f32,
            reserved: &mut Option<usize>,
        ) -> Result<(), WaterEditError> {
            let r = &map.roads[road];

            let w = r.width * 0.5;
            let mut ri = 0.0;
            let r1 = r.points.first().x + r.points.last().x;
            let r2 = r.points.last().y + r.points.first().y;
            let r3 = side * r.length();
            let layout = map.lot_layout;
            let mut picksize = || match layout {
                LotLayout::Uniform => UNIFORM_LOT_SIZE,
                LotLayout::Varied => {
                    ri += 1.0;
                    match (common::rand::rand4(r1, r2, r3, ri) * 3.0) as usize {
                        0 => MIN_LOT_SIZE,
                        1 => UNIFORM_LOT_SIZE,
                        _ => MAX_LOT_SIZE,
                    }
                }
            };

            let strict = map.environment.strict_water_siting();
            let points = if strict {
                r.interfaced_points()
            } else {
                r.points()
            }
            .clone();
            if let Some(work) = reserved.as_mut() {
                spend_water_work(work, points.len())?;
            }
            let stations = strict.then(|| FrontageStations::new(&points));
            let mut along = points.points_dirs_manual();
            let mut size = picksize();
            let mut d = size * 0.5;

            while let Some((pos, dir)) = along.next(d) {
                let axis = side * dir.perp_up();
                let at = pos + axis * (w + 1.0);
                let shape = OBB::new(at.xy() + axis.xy() * size * 0.5, axis.xy(), size, size);
                let lot = if let Some(stations) = &stations {
                    let mut allowance = MAX_WATER_BATCH_WORK;
                    let metered = reserved.is_some();
                    let work = reserved.as_mut().unwrap_or(&mut allowance);
                    spend_water_work(work, 1)?;
                    let env = map.environment();
                    let height = env
                        .height(at.xy())
                        .filter(|height| (height - at.z).abs() <= FRONTAGE_HEIGHT_TOLERANCE_M);
                    if let Some(height) = height {
                        let dry = Lot::check_dry_frontage(
                            &map.roads[road],
                            &shape,
                            |point, work| {
                                spend_water_work(work, 3)?;
                                Ok(stations.project(&points, point, d))
                            },
                            |point| env.height(point),
                            |tile| env.is_water(tile.center()),
                            work,
                        )?;
                        let clear = dry
                            && if metered {
                                !Lot::frontage_obstructed(map, shape, work)?
                            } else {
                                !Lot::footprint_obstructed(map, shape, true)
                            };
                        clear.then(|| Lot::insert(map, road, shape, height))
                    } else {
                        None
                    }
                } else {
                    Lot::make_if_clear(map, road, at, shape)
                };
                if let Some(id) = lot {
                    map.subscribers.dispatch(UpdateType::Road, &map.lots[id]);

                    d += size * 0.5 + 2.0;
                    size = picksize();
                    d += size * 0.5;
                } else {
                    d += 2.0;
                }
            }
            Ok(())
        }

        let r = &map.roads[road];
        let pair = r.sidewalks(r.src);
        // Lot allocation is optional. Stop at the reserved limit even when
        // dense overlapping shapes make the obstruction checks expensive.
        if pair.outgoing.is_some() && gen_side(map, road, 1.0, &mut reserved).is_err() {
            return;
        }
        if pair.incoming.is_some() {
            let _ = gen_side(map, road, -1.0, &mut reserved);
        }
    }

    fn frontage_obstructed(map: &Map, shape: OBB, work: &mut usize) -> Result<bool, WaterEditError> {
        let polygon = Polygon::from(shape.corners.as_slice());
        for object in map
            .spatial_map
            .construction_candidates(shape.bbox(), ProjectFilter::ALL)
        {
            spend_water_work(work, 1)?;
            let blocked = match object {
                ProjectKind::Road(id) => {
                    let road = &map.roads[id];
                    spend_water_work(work, road.points().len().saturating_mul(6))?;
                    polygon.intersects(&road.boldline())
                }
                ProjectKind::Intersection(id) => shape.intersects(&map.intersections[id].bcircle()),
                ProjectKind::Building(id) => {
                    let building = &map.buildings[id];
                    if let Some(zone) = &building.zone {
                        spend_water_work(work, zone.poly.len().saturating_mul(5))?;
                        shape.intersects(&zone.poly) || shape.intersects(&building.obb)
                    } else {
                        shape.intersects(&building.obb)
                    }
                }
                ProjectKind::Lot(id) => shape.intersects(&map.lots[id].shape),
                ProjectKind::Ground => false,
            };
            if blocked {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn remove_intersecting_lots(map: &mut Map, road: RoadID) {
        let r = unwrap_retlog!(map.roads.get(road), "{:?} does not exist", road);
        let road_shape = r.boldline();
        let mut to_remove: BTreeSet<_> = map
            .spatial_map
            .query(road_shape.bbox(), ProjectFilter::LOT)
            .filter_map(|object| {
                let id = object.to_lot()?;
                let shape = &map.lots[id].shape;
                road_shape
                    .intersects(&Polygon::from(shape.corners.as_slice()))
                    .then_some(object)
            })
            .collect();

        // Connecting a road can widen its junctions before the lane update. Use
        // their new bounds both to clear old lots and to place the new ones.
        for id in [r.src, r.dst] {
            let junction = unwrap_cont!(map.intersections.get_mut(id));
            junction.update_interface_radius(&mut map.roads);
            map.spatial_map.update(junction);
            to_remove.extend(
                map.spatial_map
                    .query(junction.bcircle(), ProjectFilter::LOT),
            );
        }

        for lot in to_remove {
            if let ProjectKind::Lot(lot) = lot {
                map.remove_lot_record(lot);
                map.spatial_map.remove(lot);
            }
        }
    }
}

/// Mirror the deck tessellator's capped XY miter. Endpoint offsets use the same
/// horizontal component of its 3D tangent as the mesh.
pub(crate) fn deck_miter(before: Option<Vec3>, pos: Vec3, after: Option<Vec3>) -> Vec2 {
    if let (Some(before), Some(after)) = (before, after) {
        let incoming = (pos - before).xy().try_normalize();
        let outgoing = (pos - after).xy().try_normalize();
        if let (Some(incoming), Some(outgoing)) = (incoming, outgoing) {
            let cross = incoming.perp_dot(outgoing);
            let normal = -incoming.perpendicular();
            let direction = if cross.abs() < 0.01 {
                normal
            } else {
                (incoming + outgoing)
                    .try_normalize()
                    .map_or(normal, |axis| axis * cross.signum())
            };
            return direction / incoming.perp_dot(direction).max(0.1);
        }
    }
    let tangent = after
        .map(|end| end - pos)
        .or_else(|| before.map(|start| pos - start));
    -tangent
        .and_then(Vec3::try_normalize)
        .unwrap_or_default()
        .xy()
        .perpendicular()
}

#[derive(Clone, Copy)]
pub(crate) struct DeckTriangle {
    pub(crate) bounds: AABB,
    pub(crate) vertices: [Vec3; 3],
}

impl DeckTriangle {
    pub(crate) fn new(vertices: [Vec3; 3]) -> Self {
        let [a, b, c] = vertices.map(Vec3::xy);
        Self {
            bounds: AABB::new_ll_ur(a.min(b).min(c), a.max(b).max(c)),
            vertices,
        }
    }

    pub(crate) fn minimum_z_in(self, tile: WaterTileCoord) -> Option<f64> {
        let bounds = AABB::new_ll_size(tile.corner(), Vec2::splat(CELL_SIZE));
        if !bounds.intersects(&self.bounds) {
            return None;
        }
        let mut clipped = [[0.0; 3]; 8];
        for (vertex, pos) in clipped.iter_mut().zip(self.vertices) {
            *vertex = [pos.x.into(), pos.y.into(), pos.z.into()];
        }
        let [a, b, c, ..] = clipped;
        if triangle_area(a, b, c) == 0.0 {
            return None;
        }
        let mut len = 3;
        // A triangle clipped to a rectangle has at most seven vertices.
        for (axis, edge, sign) in [
            (0, bounds.ll.x, 1.0),
            (0, bounds.ur.x, -1.0),
            (1, bounds.ll.y, 1.0),
            (1, bounds.ur.y, -1.0),
        ] {
            let mut output = [[0.0; 3]; 8];
            let mut count = 0;
            let mut before = *clipped[..len].last()?;
            for &after in &clipped[..len] {
                let a = (before[axis] - f64::from(edge)) * sign;
                let b = (after[axis] - f64::from(edge)) * sign;
                if (a < 0.0 && b > 0.0) || (a > 0.0 && b < 0.0) {
                    let t = a / (a - b);
                    output[count] = std::array::from_fn(|i| before[i] + (after[i] - before[i]) * t);
                    output[count][axis] = f64::from(edge);
                    count += 1;
                }
                if b >= 0.0 {
                    output[count] = after;
                    count += 1;
                }
                before = after;
            }
            clipped = output;
            len = count;
        }
        if len < 3 {
            return None;
        }
        let twice_area: f64 = clipped[1..len]
            .windows(2)
            .map(|pair| triangle_area(clipped[0], pair[0], pair[1]))
            .sum();
        if twice_area == 0.0 {
            return None;
        }
        clipped[..len]
            .iter()
            .map(|vertex| vertex[2])
            .reduce(f64::min)
    }
}

fn triangle_area([x, y, _]: [f64; 3], [bx, by, _]: [f64; 3], [cx, cy, _]: [f64; 3]) -> f64 {
    (bx - x) * (cy - y) - (by - y) * (cx - x)
}

/// Cross-section of the final deck. Fractions clip along the segment while
/// keeping its rendered miter at each original vertex.
pub(crate) fn deck_triangles(
    road: &Road,
    segment: usize,
    start: f32,
    end: f32,
) -> [DeckTriangle; 2] {
    let points = road.interfaced_points().as_slice();
    let edge = |i: usize| {
        let pos = points[i];
        let before = i.checked_sub(1).map(|i| points[i]);
        let after = points.get(i + 1).copied();
        let offset = (deck_miter(before, pos, after) * (road.width * 0.5)).z0();
        [pos + offset, pos - offset]
    };
    let [a, b] = edge(segment);
    let [c, d] = edge(segment + 1);
    let interpolate = |a: Vec3, b: Vec3, t: f32| match t {
        0.0 => a,
        1.0 => b,
        _ => a + (b - a) * t,
    };
    let [left, right] = [interpolate(a, c, start), interpolate(b, d, start)];
    let [next_left, next_right] = [interpolate(a, c, end), interpolate(b, d, end)];
    [
        DeckTriangle::new([next_left, right, left]),
        DeckTriangle::new([right, next_left, next_right]),
    ]
}

pub(crate) fn footprint_tiles(bounds: AABB) -> impl Iterator<Item = WaterTileCoord> {
    let lo = (bounds.ll / CELL_SIZE).floor();
    let hi = (bounds.ur / CELL_SIZE).ceil();
    (lo.y as i16..hi.y as i16)
        .flat_map(move |y| (lo.x as i16..hi.x as i16).map(move |x| WaterTileCoord(x, y)))
}

#[cfg(test)]
mod water_tests {
    use super::*;
    use crate::map::{Environment, LanePatternBuilder, MapProject};
    use geom::vec2;

    #[test]
    fn w2b_zs_prepared_growable_rechecks_dry_frontage() {
        use crate::map::BuildingKind;

        drop(crate::tests::TestCtx::new_fast());
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        let (_, road) = map
            .make_connection(
                MapProject::ground(vec2(48.0, 109.0).z0()),
                MapProject::ground(vec2(448.0, 109.0).z0()),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let curb_y = 109.0 + map.roads[road].width * 0.5 + 1.0;
        let shape = OBB::new(vec2(164.0, curb_y + 10.0), Vec2::Y, 20.0, 20.0);
        for id in map.lots.keys().collect::<Vec<_>>() {
            map.remove_lot_record(id);
            map.spatial_map.remove(id);
        }
        let lot = Lot::insert(&mut map, road, shape, 0.0);
        let prepared = map
            .prepare_checked_growable(&map.lots[lot], BuildingKind::House, BuildingGen::House, None)
            .unwrap();

        // Change only the frontage so commit must check the live lot before using its plan.
        map.environment_mut_for_test()
            .fill_lake(&Polygon::centered_rect(vec2(152.0, 104.0), 8.0, 8.0), 0, 1.0)
            .unwrap();
        assert!(Building::dry_footprint(map.environment(), &shape, None));
        assert!(!Lot::has_dry_frontage(map.environment(), &map.roads[road], &shape));
        assert_eq!(
            map.commit_prepared_growable(prepared),
            Err(GrowableLotError::Wet),
        );
        assert!(map.lots.contains_key(lot));
        assert!(map.buildings.is_empty());
        assert!(map.place_growable(lot, BuildingKind::House).is_none());
    }

    #[test]
    fn w2b_water_lot_rejects_wet_footprint_and_wet_frontage() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        let (_, road) = map
            .make_connection(
                MapProject::ground(vec2(48.0, 109.0).z0()),
                MapProject::ground(vec2(448.0, 109.0).z0()),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let curb_y = 109.0 + map.roads[road].width * 0.5 + 1.0;
        let lots = map.lots.keys().collect::<Vec<_>>();
        for id in lots {
            map.remove_lot_record(id);
            map.spatial_map.remove(id);
        }
        let env = map.environment_mut_for_test();
        for center in [vec2(152.0, 104.0), vec2(248.0, 136.0)] {
            env.fill_lake(&Polygon::centered_rect(center, 8.0, 8.0), 0, 1.0)
                .unwrap();
        }

        let at = |x| vec2(x, curb_y).z(0.3);
        assert!(Lot::try_make(&mut map, road, at(248.0), Vec2::Y, 20.0).is_none());
        let frontage_only = OBB::new(at(164.0).xy() + Vec2::Y * 10.0, Vec2::Y, 20.0, 20.0);
        assert!(!map
            .environment()
            .intersects_water(&Polygon(frontage_only.corners.to_vec())));
        assert!(Lot::try_make(&mut map, road, at(164.0), Vec2::Y, 20.0).is_none());
        assert!(Lot::try_make(&mut map, road, at(336.0), Vec2::Y, 20.0).is_some());
    }

    #[test]
    fn w2b_water_lot_rejects_a_wet_deck_miter() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(1, 1)));
        let (_, road) = map
            .make_connection(
                MapProject::ground(vec2(48.0, 132.0).z(8.0)),
                MapProject::ground(vec2(124.0, 232.0).z(8.0)),
                None,
                &LanePatternBuilder::new().build(),
            )
            .unwrap();
        let env = map.environment_mut_for_test();
        env.fill_lake(
            &Polygon::centered_rect(vec2(136.0, 120.0), 8.0, 8.0),
            0,
            1.0,
        )
        .unwrap();
        let mut road = map.roads[road].clone();
        assert!(road.elevated_span());
        // Isolate the ground-level approach of a curved crossing. Only its outer
        // miter enters the wet tile southeast of the bend.
        road.points = geom::PolyLine3::new(vec![
            vec2(48.0, 132.0).z(0.3),
            vec2(124.0, 132.0).z(0.3),
            vec2(124.0, 232.0).z(0.3),
        ]);
        road.interfaced_points = road.points.clone();
        let lot = |x| OBB::new(vec2(x, 143.0 + road.width * 0.5), Vec2::Y, 20.0, 20.0);
        assert!(Lot::has_dry_frontage(map.environment(), &road, &lot(90.0)));
        assert!(Building::dry_footprint(
            map.environment(),
            &lot(119.0),
            None
        ));
        assert!(!Lot::has_dry_frontage(
            map.environment(),
            &road,
            &lot(119.0)
        ));
    }
}
