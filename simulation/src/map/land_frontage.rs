use std::collections::{BTreeMap, BTreeSet};

use geom::{Intersect, Vec2, Vec3, AABB, OBB};
use slotmapd::Key;

use super::{
    candidate_tiles, spend_water_work, Building, LandDenial, LandTileId, Lot, LotLayout, Map,
    ProjectFilter, ProjectKind, RoadID, UpdateType, WaterFootprint, FRONTAGE_HEIGHT_TOLERANCE_M,
};
use crate::world_command::Rejection;

const PITCH: f32 = 42.0;
const MAX_TILE_SITES: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SiteKey(RoadID, u8, u16);

#[derive(Clone, Copy)]
struct Site {
    shape: OBB,
    shoulder: Vec3,
}

#[derive(Clone, Default)]
pub(super) struct LandFrontage {
    sites: BTreeMap<LandTileId, BTreeMap<SiteKey, Site>>,
    roads: BTreeMap<RoadID, BTreeSet<LandTileId>>,
}

pub(crate) struct PreparedFrontage(Vec<(RoadID, OBB, f32)>);

impl Map {
    pub(crate) fn rebuild_land_frontage(&mut self) -> Result<(), Rejection> {
        self.land_frontage = LandFrontage::default();
        if self.land_ownership().is_tiled() {
            let roads: BTreeSet<_> = self.roads.keys().collect();
            for road in roads {
                self.index_land_frontage(road)?;
            }
        }
        Ok(())
    }

    pub(super) fn retire_land_frontage(&mut self, road: RoadID) {
        if let Some(tiles) = self.land_frontage.roads.remove(&road) {
            for tile in tiles {
                let sites = self.land_frontage.sites.get_mut(&tile).unwrap();
                sites.retain(|key, _| key.0 != road);
                if sites.is_empty() {
                    self.land_frontage.sites.remove(&tile);
                }
            }
        }
    }

    pub(crate) fn check_land_frontage_capacity<'a>(
        &self,
        staged_roads: impl IntoIterator<Item = &'a super::Road>,
        remaining: &mut usize,
    ) -> Result<(), super::ConnectionError> {
        let Some(extent) = self.land_ownership().extent() else {
            return Ok(());
        };
        let mut additions = BTreeMap::<LandTileId, usize>::new();
        for road in staged_roads {
            if road.elevated_span() {
                continue;
            }
            let mut stations = road.points().points_dirs_manual();
            spend_water_work(remaining, road.points().len())?;
            let candidate_count = (road.length() / PITCH) as usize * 2;
            let per_candidate = road.points().len().saturating_mul(12) + candidate_count + 1;
            spend_water_work(remaining, candidate_count.saturating_mul(per_candidate))?;
            for station in 1..=(road.length() / PITCH) as u16 {
                let Some((point, dir)) = stations.next(f32::from(station) * PITCH) else {
                    break;
                };
                let offset = dir.perp_up() * (road.width * 0.5 + 1.0 + super::MAX_LOT_SIZE * 0.5);
                for side in [-1.0, 1.0] {
                    spend_water_work(remaining, 1)?;
                    let shape = OBB::new(
                        (point + side * offset).xy(),
                        dir.perp_up().xy(),
                        super::MAX_LOT_SIZE,
                        super::MAX_LOT_SIZE,
                    );
                    let mut tiles: BTreeSet<_> = candidate_tiles(extent, &shape.corners).collect();
                    if let Some(tile) = self.land_ownership().tile_at(point.xy()) {
                        tiles.insert(tile);
                    }
                    for tile in tiles {
                        let pending = additions.entry(tile).or_default();
                        *pending += 1;
                        if *pending + self.land_frontage.sites.get(&tile).map_or(0, BTreeMap::len)
                            > MAX_TILE_SITES
                        {
                            return Err(LandDenial::WorkLimit.into());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn index_land_frontage(&mut self, id: RoadID) -> Result<(), Rejection> {
        let (Some(extent), Some(road)) = (self.land_ownership().extent(), self.get(id)) else {
            return Ok(());
        };
        if !Lot::allows_frontage(self, road) || road.elevated_span() {
            return Ok(());
        }
        let points = road.points();
        let road_width = road.width;
        let length = points.length();
        let mut sites = Vec::new();
        let sidewalks = road.sidewalks(road.src);
        for (side, sidewalk) in [(0, sidewalks.outgoing), (1, sidewalks.incoming)] {
            if sidewalk.is_none() {
                continue;
            }
            let mut positions = points.points_dirs_manual();
            for station in 1..=(length / PITCH) as u16 {
                let distance = f32::from(station) * PITCH;
                let Some((point, dir)) = positions.next(distance) else {
                    break;
                };
                let size = match self.lot_layout() {
                    LotLayout::Uniform => 30.0,
                    LotLayout::Varied => {
                        let hash = common::hash_u64(&(id.data().as_ffi(), side, station));
                        [20.0, 30.0, 40.0][hash as usize % 3]
                    }
                };
                let axis = if side == 0 {
                    dir.perp_up()
                } else {
                    -dir.perp_up()
                };
                let shoulder = point + axis * (road_width * 0.5 + 1.0);
                let shape = OBB::new(
                    shoulder.xy() + axis.xy() * size * 0.5,
                    axis.xy(),
                    size,
                    size,
                );
                let Some(footprint) = WaterFootprint::new(&shape.corners) else {
                    continue;
                };
                let mut tiles: BTreeSet<_> = candidate_tiles(extent, &shape.corners)
                    .filter(|&tile| {
                        footprint
                            .overlaps_square(super::tile_bounds(tile).ll, super::LAND_TILE_SIZE)
                    })
                    .collect();
                // The access span can cross a seam independently of the lot.
                if let Some(tile) = self.land_ownership().tile_at(point.xy()) {
                    tiles.insert(tile);
                }
                sites.extend(
                    tiles
                        .into_iter()
                        .map(|tile| (tile, SiteKey(id, side, station), Site { shape, shoulder })),
                );
            }
        }
        let mut counts = BTreeMap::new();
        for (tile, _, _) in &sites {
            *counts.entry(*tile).or_insert(0) += 1;
        }
        if counts.iter().any(|(tile, count)| {
            self.land_frontage.sites.get(tile).map_or(0, BTreeMap::len) + count > MAX_TILE_SITES
        }) {
            return Err(LandDenial::WorkLimit.into());
        }
        for (tile, key, site) in sites {
            self.land_frontage
                .sites
                .entry(tile)
                .or_default()
                .insert(key, site);
            self.land_frontage.roads.entry(id).or_default().insert(tile);
        }
        Ok(())
    }

    pub(super) fn generate_land_frontage(
        &mut self,
        road: RoadID,
        remaining: &mut usize,
    ) -> Result<(), Rejection> {
        self.retire_land_frontage(road);
        self.index_land_frontage(road)?;
        let mut sites = BTreeMap::new();
        for tile in self.land_frontage.roads.get(&road).into_iter().flatten() {
            for (&key, &site) in self.land_frontage.sites[tile]
                .range(SiteKey(road, 0, 0)..=SiteKey(road, u8::MAX, u16::MAX))
            {
                sites.insert(key, site);
            }
        }
        let prepared = self.prepare_land_frontage(sites, None, remaining)?;
        self.commit_land_frontage(prepared);
        Ok(())
    }

    pub(super) fn indexed_land_sites(
        &self,
        tiles: impl IntoIterator<Item = LandTileId>,
        remaining: &mut usize,
    ) -> Result<impl Iterator<Item = (RoadID, OBB, Vec3)>, super::WaterEditError> {
        let mut selected = BTreeMap::new();
        for tile in tiles {
            if let Some(bucket) = self.land_frontage.sites.get(&tile) {
                spend_water_work(remaining, bucket.len())?;
                selected.extend(bucket.iter().map(|(&key, &site)| (key, site)));
            }
        }
        Ok(selected
            .into_iter()
            .map(|(key, site)| (key.0, site.shape, site.shoulder)))
    }

    /// The access edge of each newly prepared frontage site, plus nearby mutable
    /// street ends facing the tile across a dry, owned-or-purchased corridor.
    pub fn preview_land_approaches(&self, tile: LandTileId) -> Result<Vec<Vec2>, Rejection> {
        let land = self.land_ownership();
        let Some(bounds) = land.tile_bounds(tile).filter(|_| !land.owns(tile)) else {
            return Ok(Vec::new());
        };
        let mut remaining = super::MAX_WATER_BATCH_WORK;
        let PreparedFrontage(sites) = self.prepare_purchase_frontage(tile, &mut remaining)?;
        let mut markers: Vec<_> = sites
            .into_iter()
            .map(|(_, shape, _)| (shape.corners[0] + shape.corners[1]) * 0.5)
            .collect();

        const STREET_END_REACH: f32 = 128.0;
        let mut sockets = BTreeSet::new();
        for object in self
            .spatial_map
            .construction_candidates(bounds.expand(STREET_END_REACH), ProjectFilter::INTER)
        {
            spend_water_work(&mut remaining, 1).map_err(|_| LandDenial::WorkLimit)?;
            if let ProjectKind::Intersection(id) = object {
                sockets.insert(id);
            }
        }
        for id in sockets {
            let Some(junction) = self.get(id) else {
                continue;
            };
            let [road] = junction.roads.as_slice() else {
                continue;
            };
            let Some(road) = self.get(*road) else {
                continue;
            };
            let start = junction.pos.xy();
            if !land.tile_at(start).is_some_and(|start| land.owns(start))
                || self.mutable_object(id.into()).is_err()
                || !Lot::allows_frontage(self, road)
                || road.elevated_span()
            {
                continue;
            }
            let axis = -road.dir_from(id);
            let Some(distance) = street_end_entry(bounds, start, axis, STREET_END_REACH) else {
                continue;
            };
            let end = start + axis * distance;
            if land.tile_at(end) != Some(tile) {
                continue;
            }
            let corridor = OBB::new((start + end) * 0.5, axis, distance, road.width);
            if self
                .authorize_land_footprint(&corridor.corners, Some(tile))
                .is_ok()
                && Building::dry_footprint(self.environment(), &corridor, None)
            {
                markers.push(start);
            }
        }
        Ok(markers)
    }

    pub(crate) fn prepare_purchase_frontage(
        &self,
        tile: LandTileId,
        remaining: &mut usize,
    ) -> Result<PreparedFrontage, Rejection> {
        let sites = self
            .land_frontage
            .sites
            .get(&tile)
            .into_iter()
            .flatten()
            .map(|(&key, &site)| (key, site));
        self.prepare_land_frontage(sites, Some(tile), remaining)
    }

    fn prepare_land_frontage(
        &self,
        sites: impl IntoIterator<Item = (SiteKey, Site)>,
        purchased: Option<LandTileId>,
        remaining: &mut usize,
    ) -> Result<PreparedFrontage, Rejection> {
        let mut prepared: Vec<(RoadID, OBB, f32)> = Vec::new();
        for (SiteKey(id, _, _), site) in sites {
            let road = &self.roads[id];
            spend_water_work(
                remaining,
                road.points().len().saturating_mul(12) + prepared.len() + 1,
            )
            .map_err(|_| LandDenial::WorkLimit)?;
            if self.authorize_frontage(id, &site.shape, purchased).is_err() {
                continue;
            }
            if purchased.is_some() && self.authorize_frontage(id, &site.shape, None).is_ok() {
                continue;
            }
            let Some(height) = self
                .environment()
                .height(site.shoulder.xy())
                .filter(|height| (height - site.shoulder.z).abs() <= FRONTAGE_HEIGHT_TOLERANCE_M)
            else {
                continue;
            };
            if !Building::dry_footprint(self.environment(), &site.shape, None)
                || !Lot::has_dry_frontage(self.environment(), road, &site.shape)
                || Lot::footprint_obstructed(self, site.shape, true)
                || prepared
                    .iter()
                    .any(|(_, shape, _)| site.shape.intersects(shape))
            {
                continue;
            }
            prepared.push((id, site.shape, height));
        }
        Ok(PreparedFrontage(prepared))
    }

    pub(crate) fn commit_land_frontage(&mut self, prepared: PreparedFrontage) {
        for (road, shape, height) in prepared.0 {
            let id = Lot::insert(self, road, shape, height);
            self.subscribers.dispatch(UpdateType::Road, &self.lots[id]);
        }
    }
}

fn street_end_entry(bounds: AABB, start: Vec2, axis: Vec2, reach: f32) -> Option<f32> {
    let mut entry = 0.0_f64;
    let mut exit = f64::from(reach);
    for (start, direction, min, max) in [
        (start.x, axis.x, bounds.ll.x, bounds.ur.x),
        (start.y, axis.y, bounds.ll.y, bounds.ur.y),
    ] {
        if direction == 0.0 {
            if start < min || start >= max {
                return None;
            }
            continue;
        }
        let distance_to = |edge| (f64::from(edge) - f64::from(start)) / f64::from(direction);
        let a = distance_to(min);
        let b = distance_to(max);
        entry = entry.max(a.min(b));
        exit = exit.min(a.max(b));
    }
    if entry >= exit {
        return None;
    }
    let mut first = entry as f32;
    if f64::from(first) < entry {
        first = first.next_up();
    }
    let mut last = exit as f32;
    if f64::from(last) > exit {
        last = last.next_down();
    }
    if first > last {
        return None;
    }

    // For shallow rays, a distance inside the clipped interval can still
    // round the endpoint onto an excluded upper edge. The entering tests are
    // monotone, so search the representable distances and check the exit edges
    // at the first point that passes.
    let entered = |distance: f32| {
        let end = start + axis * distance;
        (axis.x <= 0.0 || end.x >= bounds.ll.x)
            && (axis.x >= 0.0 || end.x < bounds.ur.x)
            && (axis.y <= 0.0 || end.y >= bounds.ll.y)
            && (axis.y >= 0.0 || end.y < bounds.ur.y)
    };
    if !entered(first) {
        let mut low = first.to_bits();
        let mut high = last.to_bits();
        while low < high {
            let mid = low + (high - low) / 2;
            if entered(f32::from_bits(mid)) {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        first = f32::from_bits(low);
    }
    let end = start + axis * first;
    (end.x >= bounds.ll.x && end.x < bounds.ur.x && end.y >= bounds.ll.y && end.y < bounds.ur.y)
        .then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{
        Environment, LandOwnership, LanePatternBuilder, MapProject, OwnedTileRect, TileExtent,
    };
    use geom::vec2;

    #[test]
    fn tiles_harden1_street_end_approaches_a_tile_corner() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(10, 11)));
        let end = vec2(4020.0, 4562.0);
        map.make_connection(
            MapProject::ground(vec2(3966.0, 4490.0).z0()),
            MapProject::ground(end.z0()),
            None,
            &LanePatternBuilder::new().build(),
        )
        .unwrap();
        map.install_land_ownership(
            LandOwnership::tiled(
                TileExtent {
                    width: 10,
                    height: 11,
                },
                OwnedTileRect {
                    origin: LandTileId { x: 7, y: 8 },
                    width: 1,
                    height: 1,
                },
                Default::default(),
            )
            .unwrap(),
        );
        let approaches = map
            .preview_land_approaches(LandTileId { x: 7, y: 9 })
            .unwrap();
        assert!(approaches.contains(&end), "approaches: {approaches:?}");
    }

    #[test]
    fn tiles_harden1_street_end_crosses_shallow_upper_x_edge() {
        let mut map = Map::empty();
        assert!(map.install_environment(Environment::new(32, 32)));
        let end = vec2(15361.0, 768.0);
        map.make_connection(
            MapProject::ground(vec2(15364.0, 668.0).z0()),
            MapProject::ground(end.z0()),
            None,
            &LanePatternBuilder::new().build(),
        )
        .unwrap();
        map.install_land_ownership(
            LandOwnership::tiled(
                TileExtent {
                    width: 32,
                    height: 32,
                },
                OwnedTileRect {
                    origin: LandTileId { x: 30, y: 1 },
                    width: 1,
                    height: 1,
                },
                Default::default(),
            )
            .unwrap(),
        );
        let approaches = map
            .preview_land_approaches(LandTileId { x: 29, y: 1 })
            .unwrap();
        assert!(approaches.contains(&end), "approaches: {approaches:?}");
    }

    #[test]
    fn tiles_harden1_street_end_crosses_shallow_upper_y_edge() {
        let bounds = super::super::tile_bounds(LandTileId { x: 1, y: 63 });
        let start = vec2(850.0, 32776.0);
        let axis = vec2(0.9967905, -0.08005474);
        let distance = street_end_entry(bounds, start, axis, 128.0).unwrap();
        let point = |distance| start + axis * distance;
        assert!(
            point(distance).y < bounds.ur.y,
            "endpoint: {:?}",
            point(distance)
        );
        assert!(point(distance.next_down()).y >= bounds.ur.y);
    }
}
