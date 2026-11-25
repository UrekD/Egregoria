//! Offline bootstrap and rail market placement for authored town layouts.

use super::{
    Building, BuildingID, BuildingKind, LaneID, LaneKind, LotLayout, PathKind, Pathfinder,
    ProjectFilter, ProjectKind, TerrainChunkID,
};
use crate::economy::progression::ProgressionMode;
use crate::map_dynamic::{BuildingInfos, Dispatcher, Itinerary};
use crate::world_command::WorldCommand;
use crate::{MapPreset, NewGameError, NewGameOptions, Replay, Simulation, SimulationOptions};
use geom::{Intersect, Polygon, Vec2, Vec3, AABB, OBB};
use prototypes::{BuildingGen, Tick};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AuthoredTownOptions {
    pub preset: MapPreset,
    pub seed: u64,
    pub terrain_chunks: u16,
    /// Authored start area in metres. Ownership itself is managed by the land system.
    pub owned_rect: AABB,
    pub starting_money_bucks: u32,
    pub progression: ProgressionMode,
    pub lot_layout: LotLayout,
    pub save_replay: bool,
}

impl Default for AuthoredTownOptions {
    fn default() -> Self {
        Self {
            preset: MapPreset::Flat,
            seed: 20261122,
            terrain_chunks: 6,
            owned_rect: AABB::new_ll_ur(Vec2::ZERO, Vec2::splat(6.0 * TerrainChunkID::SIZE_F32)),
            starting_money_bucks: 500_000,
            progression: ProgressionMode::FreeBuild,
            lot_layout: LotLayout::Uniform,
            save_replay: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthoredTownError {
    NewGame(NewGameError),
    TerrainChunks(u16),
    OwnedRect,
    Preset,
    Progression,
    LotLayout,
    Replay,
}

impl fmt::Display for AuthoredTownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewGame(error) => error.fmt(f),
            Self::TerrainChunks(chunks) => write!(
                f,
                "authored terrain size {chunks} is outside 6..={} chunks",
                NewGameOptions::MAX_TERRAIN_CHUNKS,
            ),
            Self::OwnedRect => f.write_str(
                "owned area must fill a 6–7 chunk compact map or leave at least one terrain chunk on every side",
            ),
            Self::Preset => f.write_str("authored towns require Flat terrain"),
            Self::Progression => f.write_str("authored towns require Free Build"),
            Self::LotLayout => f.write_str("authored towns require Uniform lots"),
            Self::Replay => f.write_str("authored towns require replay recording to be disabled"),
        }
    }
}

impl std::error::Error for AuthoredTownError {}

impl AuthoredTownOptions {
    fn validate(self) -> Result<(), AuthoredTownError> {
        let chunk = TerrainChunkID::SIZE_F32;
        if !(6..=NewGameOptions::MAX_TERRAIN_CHUNKS).contains(&self.terrain_chunks) {
            return Err(AuthoredTownError::TerrainChunks(self.terrain_chunks));
        }
        let edge = Vec2::splat(f32::from(self.terrain_chunks) * chunk);
        let owned = self.owned_rect;
        let compact = self.terrain_chunks <= 7 && owned == AABB::new_ll_ur(Vec2::ZERO, edge);
        let banded = owned.ll.x >= chunk
            && owned.ll.y >= chunk
            && owned.ur.x <= edge.x - chunk
            && owned.ur.y <= edge.y - chunk
            && owned.w() >= 6.0 * chunk
            && owned.h() >= 6.0 * chunk;
        if !owned.ll.is_finite() || !owned.ur.is_finite() || !(compact || banded) {
            return Err(AuthoredTownError::OwnedRect);
        }
        if !(NewGameOptions::MIN_STARTING_MONEY_BUCKS..=NewGameOptions::MAX_STARTING_MONEY_BUCKS)
            .contains(&self.starting_money_bucks)
        {
            return Err(AuthoredTownError::NewGame(
                NewGameError::StartingMoneyBucks(self.starting_money_bucks),
            ));
        }
        if self.preset != MapPreset::Flat {
            return Err(AuthoredTownError::Preset);
        }
        if self.progression != ProgressionMode::FreeBuild {
            return Err(AuthoredTownError::Progression);
        }
        if self.lot_layout != LotLayout::Uniform {
            return Err(AuthoredTownError::LotLayout);
        }
        if self.save_replay {
            return Err(AuthoredTownError::Replay);
        }
        Ok(())
    }
}

/// A private commissioning run publishes a replay-disabled tiled snapshot only
/// after the terrain, sites and typed gateway IDs pass the land finalizer.
#[derive(Clone, Copy, Debug)]
pub struct AuthoredTilesSpec {
    pub recipe_id: u64,
    pub source_crop: AABB,
    /// Destination of the source crop's lower-left corner and metres per source unit.
    pub town_origin: Vec2,
    pub town_scale: f32,
    pub terrain_extent: super::TileExtent,
    pub owned_rect: super::OwnedTileRect,
    pub seed: u64,
    pub lot_layout: LotLayout,
    pub starting_money_bucks: u32,
    pub progression: ProgressionMode,
}

impl Simulation {
    pub fn build_authored_tiled<T>(
        spec: AuthoredTilesSpec,
        assemble: impl FnOnce(
            &mut Simulation,
        ) -> Result<
            (T, std::collections::BTreeSet<super::ProtectedRoot>),
            super::LandSetupError,
        >,
    ) -> Result<(Simulation, T), super::LandSetupError> {
        use super::{LandMode, LandOwnership, LandRecipe, LandSetupError};
        let extent = spec.terrain_extent;
        if extent.area().is_none() {
            return Err(LandSetupError::InvalidExtent(extent));
        }
        if !spec.owned_rect.valid(extent) {
            return Err(LandSetupError::InvalidOwnedRect(spec.owned_rect));
        }
        if spec.recipe_id == 0
            || !spec.source_crop.ll.is_finite()
            || !spec.source_crop.ur.is_finite()
            || spec.source_crop.w() <= 0.0
            || spec.source_crop.h() <= 0.0
            || !spec.town_origin.is_finite()
            || !spec.town_scale.is_finite()
            || spec.town_scale <= 0.0
        {
            return Err(LandSetupError::InvalidMode(LandMode::Tiled));
        }
        let minimum = if spec.progression == ProgressionMode::Guided {
            150_000
        } else {
            10_000
        };
        if !(minimum..=NewGameOptions::MAX_STARTING_MONEY_BUCKS)
            .contains(&spec.starting_money_bucks)
        {
            return Err(LandSetupError::InvalidOpeningBudget(
                spec.starting_money_bucks,
            ));
        }
        let options = SimulationOptions {
            terrain_size: extent.width,
            preset: MapPreset::Flat,
            seed: spec.seed,
            starting_money_bucks: spec.starting_money_bucks,
            progression: spec.progression,
            save_replay: false,
            land_mode: LandMode::Tiled,
            land_recipe: LandRecipe::AuthoredV1 {
                recipe_id: spec.recipe_id,
                extent,
                owned_rect: spec.owned_rect,
                lot_layout: spec.lot_layout,
            },
        };
        let mut sim = Self::with_initial_resources();
        WorldCommand::bootstrap_authored(&mut sim, options, spec.lot_layout)
            .map_err(|_| LandSetupError::InvalidMode(LandMode::Tiled))?;
        let (result, reserves) = assemble(&mut sim)?;
        if sim.get_tick() != crate::INITIAL_TICK || sim.read::<Replay>().enabled {
            return Err(LandSetupError::InvalidMode(LandMode::Tiled));
        }
        {
            let mut map = sim.map_mut();
            map.install_land_ownership(LandOwnership::tiled(extent, spec.owned_rect, reserves)?);
            // Transit and property placement during assembly initially use trusted
            // inputs. Vacant legacy lots are regenerated at fixed frontage stations.
            let roads: Vec<_> = map.roads().keys().collect();
            map.clear_authored_vacant_lots();
            let mut remaining = super::MAX_WATER_BATCH_WORK;
            for road in roads {
                map.generate_land_frontage(road, &mut remaining)
                    .map_err(|_| LandSetupError::NoBuildableFrontage(spec.owned_rect.origin))?;
            }
            map.rebuild_land_frontage()
                .map_err(|_| LandSetupError::NoBuildableFrontage(spec.owned_rect.origin))?;
            if map.lots().is_empty() {
                return Err(LandSetupError::NoBuildableFrontage(spec.owned_rect.origin));
            }
            map.update();
        }
        super::validate_loaded_land(&sim)
            .map_err(|_| LandSetupError::InvalidOwnedRect(spec.owned_rect))?;
        sim.world.reconcile_derived("authored tiled world");
        Ok((sim, result))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RailMarketSite {
    pub footprint: OBB,
    pub service_lane: LaneID,
    /// Distance along the intended spawn lane to the locomotive.
    pub service_offset: f32,
    /// Actual freight platform stop, as returned by `freight_station::train_stop`.
    pub platform_stop: Vec3,
    /// Clear track required for the full consist, including operating margin.
    pub consist_length_m: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailMarketError {
    Replay,
    Duplicate,
    Footprint,
    Wet,
    Obstructed,
    ServiceTrack,
    PlatformTrack,
    ConsistClearance,
    Route,
    Placement,
}

impl fmt::Display for RailMarketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Replay => "rail market placement requires replay recording to be disabled",
            Self::Duplicate => "an external rail market already exists",
            Self::Footprint => "rail market footprint is invalid or outside terrain",
            Self::Wet => "rail market footprint overlaps water",
            Self::Obstructed => "rail market footprint overlaps another corridor or building",
            Self::ServiceTrack => "market centre does not resolve to the intended rail spawn",
            Self::PlatformTrack => "the freight stop does not resolve to nearby platform track",
            Self::ConsistClearance => "insufficient clear track for the freight consist",
            Self::Route => {
                "freight dispatch requires a usable rail route from the market to the platform and back"
            }
            Self::Placement => "rail market building could not be placed",
        };
        f.write_str(message)
    }
}

impl std::error::Error for RailMarketError {}

impl Simulation {
    /// Boot an authored map at the initial tick with empty infrastructure.
    pub fn new_authored_town(options: AuthoredTownOptions) -> Result<Self, AuthoredTownError> {
        options.validate()?;
        let mut sim = Self::with_initial_resources();
        WorldCommand::bootstrap_authored(
            &mut sim,
            SimulationOptions {
                terrain_size: options.terrain_chunks,
                save_replay: options.save_replay,
                preset: options.preset,
                seed: options.seed,
                starting_money_bucks: options.starting_money_bucks,
                progression: options.progression,
                land_mode: crate::map::LandMode::Unrestricted,
                land_recipe: crate::map::LandRecipe::Unrestricted,
            },
            options.lot_layout,
        )
        .map_err(|reason| AuthoredTownError::NewGame(NewGameError::Bootstrap(reason)))?;
        Ok(sim)
    }

    /// Place the external trade marker on the intended corridor after validating
    /// the rail endpoints used by freight dispatch.
    pub fn place_authored_rail_market(
        &mut self,
        site: RailMarketSite,
    ) -> Result<BuildingID, RailMarketError> {
        if self.read::<Replay>().enabled || self.map().land_ownership().is_tiled() {
            return Err(RailMarketError::Replay);
        }
        let RailMarketSite {
            footprint,
            service_lane,
            service_offset,
            platform_stop,
            consist_length_m,
        } = site;
        let mut map = self.map_mut();
        if !map.external_train_stations.is_empty() {
            return Err(RailMarketError::Duplicate);
        }
        let gen = BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        };
        let terrain = map.environment();
        let bounds = terrain.bounds();
        if !Building::valid_footprint(&footprint, gen)
            || footprint
                .corners
                .iter()
                .any(|&corner| !bounds.contains(corner))
        {
            return Err(RailMarketError::Footprint);
        }
        let area = Polygon::from(footprint.corners.as_slice());
        if terrain.intersects_water(&area) {
            return Err(RailMarketError::Wet);
        }

        let lane = map
            .get(service_lane)
            .filter(|lane| lane.kind == LaneKind::Rail)
            .ok_or(RailMarketError::ServiceTrack)?;
        if !(0.0..=lane.points.length()).contains(&service_offset) {
            return Err(RailMarketError::ServiceTrack);
        }
        let spawn = lane.points.point_along(service_offset);
        let centre = footprint.center().z0();
        if !footprint.intersects(&spawn.xy())
            || PathKind::Rail.nearest_lane(&map, centre) != Some(service_lane)
            || lane.points.project(centre).distance2(spawn) > 1.0
            || PathKind::Rail.nearest_lane(&map, spawn) != Some(service_lane)
        {
            return Err(RailMarketError::ServiceTrack);
        }
        let road = lane.parent;
        let rail_only = map
            .get(road)
            .is_some_and(|road| road.lanes_iter().all(|(_, kind)| kind == LaneKind::Rail));
        if !rail_only
            || map.building_overlaps(footprint)
            || map
                .spatial_map()
                .query(&area, ProjectFilter::ROAD | ProjectFilter::INTER)
                .any(|object| object != ProjectKind::Road(road))
        {
            return Err(RailMarketError::Obstructed);
        }

        if !platform_stop.is_finite() {
            return Err(RailMarketError::PlatformTrack);
        }
        let platform = map
            .nearest_lane(platform_stop, LaneKind::Rail, Some(50.0))
            .and_then(|id| map.get(id))
            .ok_or(RailMarketError::PlatformTrack)?;
        let (stop, segment) = platform.points.project_segment(platform_stop);
        if segment == 0
            || stop.distance2(platform_stop) > 50.0 * 50.0
            || PathKind::Rail.nearest_lane(&map, platform_stop) != Some(platform.id)
            || PathKind::Rail.nearest_lane(&map, stop) != Some(platform.id)
        {
            return Err(RailMarketError::PlatformTrack);
        }
        // project_segment returns the index of the point after the projection.
        let platform_offset = platform.points[0..segment]
            .windows(2)
            .map(|pair| pair[0].distance(pair[1]))
            .sum::<f32>()
            + platform.points[segment - 1].distance(stop);
        if !consist_length_m.is_finite()
            || consist_length_m <= 0.0
            || service_offset.min(lane.points.length() - service_offset) < consist_length_m
            || platform_offset.min(platform.points.length() - platform_offset) < consist_length_m
            || (platform.id == service_lane
                && (platform_offset - service_offset).abs() < consist_length_m)
        {
            return Err(RailMarketError::ConsistClearance);
        }
        // A route can loop back through the end turns, but the dispatcher
        // rejects a train already past the stop on the target lane.
        let dispatch_start = lane.points.length_at_proj(lane.points.project(spawn));
        let dispatch_target = platform.points.length_at_proj(stop);
        if !Dispatcher::position_is_eligible(
            service_lane,
            dispatch_start,
            platform.id,
            dispatch_target,
        ) {
            return Err(RailMarketError::Route);
        }

        let tick = Tick(self.get_tick());
        // The dispatcher uses the actual stop and raw market centre. Cargo also
        // tests reachability through the platform projection.
        for (from, to) in [(spawn, platform_stop), (stop, centre), (centre, stop)] {
            if Itinerary::route(tick, from, to, &map, PathKind::Rail).is_none() {
                return Err(RailMarketError::Route);
            }
        }
        let id = map
            .build_special_building(&footprint, BuildingKind::ExternalTrading, gen, None, None)
            .ok_or(RailMarketError::Placement)?;
        drop(map);
        self.write::<BuildingInfos>().insert(id);
        Ok(id)
    }
}
