use std::collections::{BTreeMap, BTreeSet};

use geom::{Intersect, Vec3, OBB};
use slotmapd::Key;

use super::{
    candidate_tiles, spend_water_work, Building, LandDenial, LandTileId, Lot, LotLayout, Map,
    RoadID, UpdateType, WaterFootprint, FRONTAGE_HEIGHT_TOLERANCE_M, MAX_WATER_BATCH_WORK,
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

    pub(super) fn generate_land_frontage(&mut self, road: RoadID) {
        self.retire_land_frontage(road);
        if self.index_land_frontage(road).is_err() {
            return;
        }
        let mut sites = BTreeMap::new();
        for tile in self.land_frontage.roads.get(&road).into_iter().flatten() {
            for (&key, &site) in self.land_frontage.sites[tile]
                .range(SiteKey(road, 0, 0)..=SiteKey(road, u8::MAX, u16::MAX))
            {
                sites.insert(key, site);
            }
        }
        let mut remaining = MAX_WATER_BATCH_WORK;
        if let Ok(prepared) = self.prepare_land_frontage(sites, None, &mut remaining) {
            self.commit_land_frontage(prepared);
        }
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
