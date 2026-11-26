#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![warn(clippy::iter_over_hash_type)]

use crate::init::{GSYSTEMS, INIT_FUNCS, SAVELOAD_FUNCS};
use crate::map::{
    BuildingKind, LotLayout, Map, WaterBasin, WaterBodyId, WaterEdit, WaterEditError,
    WaterEditOutcome, WaterEditPreview, WaterStroke,
};
use crate::map_dynamic::{transit, Itinerary, ItineraryLeader};
use crate::souls::add_souls_to_empty_buildings;
use crate::utils::resources::{Ref, RefMut, Resources};
use crate::utils::scheduler::RunnableSystem;
use crate::world_command::{CommandOutcomes, PrepareBudget, Rejection, WorldCommand};
use common::saveload::{
    self, AtomicWriteError, Bincode, CompressedBincode, Encoder, SlotEntry,
};
use common::FastMap;
use derive_more::{From, TryInto};
use economy::progression::ProgressionMode;
use economy::BudgetCategory;
use geom::{Polygon, Vec2, Vec3};
use prototypes::{prototype, ColorsPrototype, ColorsPrototypeID, GameTime, Tick};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::ptr::addr_of;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};
use utils::rand_provider::RandProvider;
use utils::replay::{replay_version, replay_version_error, ReplayJson};
use utils::scheduler::SeqSchedule;

#[macro_use]
extern crate common;

#[allow(unused_imports)]
#[macro_use]
extern crate inline_tweak;

#[macro_use]
extern crate egui_inspect;

#[macro_use]
extern crate log as extern_log;

pub mod diagnostics;
pub mod economy;
pub mod init;
pub mod map;
pub mod map_dynamic;
pub mod multiplayer;
mod rerun;
pub mod souls;
#[cfg(test)]
mod tests;
pub mod transportation;
pub mod utils;
mod world;
pub mod world_command;

pub use common::saveload::{
    list_slots_with_revisions, save_dir, SlotCamera, SlotId, SlotMeta, SlotPaths, SnapshotKind,
    SnapshotRevision,
};
pub use economy::population::PopulationScale;
pub use map::{AuthoredTownError, AuthoredTownOptions};
pub use world::*;

pub use utils::par_command_buffer::ParCommandBuffer;
pub use utils::replay::*;

pub fn colors() -> &'static ColorsPrototype {
    prototype::<ColorsPrototypeID>(ColorsPrototypeID::new("colors"))
}

#[derive(
    Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash, From, TryInto,
)]
pub enum SoulID {
    Human(HumanID),
    GoodsCompany(CompanyID),
    FreightStation(FreightStationID),
}

impl Display for SoulID {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SoulID::Human(id) => write!(f, "{:?}", id),
            SoulID::GoodsCompany(id) => write!(f, "{:?}", id),
            SoulID::FreightStation(id) => write!(f, "{:?}", id),
        }
    }
}

impl From<SoulID> for AnyEntity {
    fn from(value: SoulID) -> Self {
        match value {
            SoulID::Human(id) => AnyEntity::HumanID(id),
            SoulID::GoodsCompany(id) => AnyEntity::CompanyID(id),
            SoulID::FreightStation(id) => AnyEntity::FreightStationID(id),
        }
    }
}

impl TryFrom<AnyEntity> for SoulID {
    type Error = ();

    fn try_from(value: AnyEntity) -> Result<Self, Self::Error> {
        match value {
            AnyEntity::HumanID(id) => Ok(SoulID::Human(id)),
            AnyEntity::CompanyID(id) => Ok(SoulID::GoodsCompany(id)),
            AnyEntity::FreightStationID(id) => Ok(SoulID::FreightStation(id)),
            _ => Err(()),
        }
    }
}

debug_inspect_impl!(SoulID);

pub struct Simulation {
    pub(crate) world: World,
    resources: Resources,
}

const RNG_SEED: u64 = 123;
const INITIAL_TICK: u64 = 1;
const PRE_SCALE_VERSION: &str = "0.11.0";
const PRE_ORIGIN_VERSION: &str = "0.11.1-scale-p1";
const VERSION: &str = include_str!("../../VERSION");

fn compatible_save_version(version: &str) -> bool {
    matches!(version, VERSION | PRE_SCALE_VERSION | PRE_ORIGIN_VERSION)
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MapPreset {
    #[default]
    Procedural,
    Flat,
    Waters,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NewGameOptions {
    pub preset: MapPreset,
    pub seed: u64,
    pub terrain_chunks: u16,
    pub starting_money_bucks: u32,
    pub progression: ProgressionMode,
    pub lot_layout: LotLayout,
    pub land_mode: map::LandMode,
    pub persons_per_agent: u16,
}

impl NewGameOptions {
    pub const MIN_TERRAIN_CHUNKS: u16 = 16;
    pub const MAX_TERRAIN_CHUNKS: u16 = 80;
    pub const MIN_STARTING_MONEY_BUCKS: u32 = 10_000;
    pub const MAX_STARTING_MONEY_BUCKS: u32 = 10_000_000;

    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    fn validate(&self) -> Result<(), NewGameError> {
        PopulationScale::try_new(self.persons_per_agent)?;
        if !(Self::MIN_TERRAIN_CHUNKS..=Self::MAX_TERRAIN_CHUNKS).contains(&self.terrain_chunks) {
            return Err(NewGameError::TerrainChunks(self.terrain_chunks));
        }
        if !(Self::MIN_STARTING_MONEY_BUCKS..=Self::MAX_STARTING_MONEY_BUCKS)
            .contains(&self.starting_money_bucks)
        {
            return Err(NewGameError::StartingMoneyBucks(self.starting_money_bucks));
        }
        if self.land_mode == map::LandMode::Tiled {
            if self.preset != MapPreset::Flat {
                return Err(NewGameError::Land(map::LandSetupError::InvalidMode(
                    self.land_mode,
                )));
            }
            if self.terrain_chunks > map::MAX_NATIVE_TILED_SIDE {
                return Err(NewGameError::Land(map::LandSetupError::InvalidExtent(
                    map::TileExtent {
                        width: self.terrain_chunks,
                        height: self.terrain_chunks,
                    },
                )));
            }
            if self.progression == ProgressionMode::Guided && self.starting_money_bucks < 150_000 {
                return Err(NewGameError::Land(
                    map::LandSetupError::InvalidOpeningBudget(self.starting_money_bucks),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewGameError {
    TerrainChunks(u16),
    StartingMoneyBucks(u32),
    Bootstrap(Rejection),
    Land(map::LandSetupError),
    PersonsPerAgent(u16),
}

impl Display for NewGameError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::TerrainChunks(chunks) => write!(
                f,
                "Terrain size {chunks} is outside {}..={} chunks",
                NewGameOptions::MIN_TERRAIN_CHUNKS,
                NewGameOptions::MAX_TERRAIN_CHUNKS
            ),
            Self::StartingMoneyBucks(bucks) => write!(
                f,
                "Starting money {bucks} is outside {}..={} bucks",
                NewGameOptions::MIN_STARTING_MONEY_BUCKS,
                NewGameOptions::MAX_STARTING_MONEY_BUCKS
            ),
            Self::Land(reason) => write!(f, "Could not initialize tiled land ({reason:?})"),
            Self::Bootstrap(reason) => write!(f, "Could not initialize the map ({reason:?})"),
            Self::PersonsPerAgent(n) => {
                write!(
                    f,
                    "Invalid population ratio 1:{n}. Choose 1, 2, 4, 8 or 16."
                )
            }
        }
    }
}

impl std::error::Error for NewGameError {}

impl Default for NewGameOptions {
    fn default() -> Self {
        let legacy = SimulationOptions::LEGACY;
        Self {
            preset: legacy.preset,
            seed: legacy.seed,
            terrain_chunks: legacy.terrain_size,
            starting_money_bucks: legacy.starting_money_bucks,
            progression: legacy.progression,
            lot_layout: LotLayout::Varied,
            land_mode: map::LandMode::Unrestricted,
            persons_per_agent: legacy.persons_per_agent,
        }
    }
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SimulationOptions {
    pub terrain_size: u16,
    pub save_replay: bool,
    pub preset: MapPreset,
    pub seed: u64,
    pub starting_money_bucks: u32,
    pub progression: ProgressionMode,
    pub land_mode: map::LandMode,
    pub land_recipe: map::LandRecipe,
    pub persons_per_agent: u16,
}

impl SimulationOptions {
    const AUTHORED_TERRAIN_CHUNKS: std::ops::RangeInclusive<u16> =
        1..=NewGameOptions::MAX_TERRAIN_CHUNKS;

    pub const LEGACY: Self = Self {
        terrain_size: 50,
        save_replay: true,
        preset: MapPreset::Procedural,
        seed: 0,
        starting_money_bucks: 150_000,
        progression: ProgressionMode::FreeBuild,
        land_mode: map::LandMode::Unrestricted,
        land_recipe: map::LandRecipe::Unrestricted,
        persons_per_agent: PopulationScale::REALISTIC.persons_per_agent(),
    };

    fn new_game_options(self) -> NewGameOptions {
        NewGameOptions {
            preset: self.preset,
            seed: self.seed,
            terrain_chunks: self.terrain_size,
            starting_money_bucks: self.starting_money_bucks,
            progression: self.progression,
            lot_layout: self.recipe_lot_layout().unwrap_or_default(),
            land_mode: self.land_mode,
            persons_per_agent: self.persons_per_agent,
        }
    }

    fn recipe_lot_layout(self) -> Option<LotLayout> {
        match self.land_recipe {
            map::LandRecipe::Unrestricted => None,
            map::LandRecipe::NativeFlatV1 { lot_layout }
            | map::LandRecipe::AuthoredV1 { lot_layout, .. } => Some(lot_layout),
        }
    }

    fn valid_for_bootstrap(self) -> bool {
        if self.population_scale().is_err() {
            return false;
        }
        // Legacy scenarios and tests can start with a small or empty map.
        match (self.land_mode, self.land_recipe) {
            (map::LandMode::Tiled, map::LandRecipe::NativeFlatV1 { .. }) => {
                return self.new_game_options().is_valid();
            }
            (map::LandMode::Unrestricted, map::LandRecipe::Unrestricted) => {}
            _ => return false,
        }
        self.new_game_options().is_valid()
            || (self.terrain_size < NewGameOptions::MIN_TERRAIN_CHUNKS
                && self.preset == Self::LEGACY.preset
                && self.seed == Self::LEGACY.seed
                && self.starting_money_bucks == Self::LEGACY.starting_money_bucks)
    }

    fn population_scale(self) -> Result<PopulationScale, NewGameError> {
        PopulationScale::try_new(self.persons_per_agent)
    }

    fn valid_for_authored_bootstrap(self) -> bool {
        let minimum_money = if self.progression == ProgressionMode::Guided {
            150_000
        } else {
            NewGameOptions::MIN_STARTING_MONEY_BUCKS
        };
        !self.save_replay
            && self.preset == MapPreset::Flat
            && self.population_scale().is_ok()
            && Self::AUTHORED_TERRAIN_CHUNKS.contains(&self.terrain_size)
            && (minimum_money..=NewGameOptions::MAX_STARTING_MONEY_BUCKS)
                .contains(&self.starting_money_bucks)
    }
}

impl Default for SimulationOptions {
    fn default() -> Self {
        Self::LEGACY
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum MapConstructor {
    NewGame,
    Imported,
    AuthoredFlat,
    AuthoredTown,
    AuthoredTiled,
}

impl MapConstructor {
    fn is_authored(self) -> bool {
        matches!(self, Self::AuthoredFlat | Self::AuthoredTown | Self::AuthoredTiled)
    }
}

/// Immutable creation metadata paired with the terrain preset, size and seed in
/// SimulationOptions. An older unrestricted save can lack its original lot layout.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MapOrigin {
    constructor: MapConstructor,
    initial_lot_layout: Option<LotLayout>,
}

impl MapOrigin {
    fn new(constructor: MapConstructor, lot_layout: LotLayout) -> Self {
        Self {
            constructor,
            initial_lot_layout: Some(lot_layout),
        }
    }

    fn matches_options(self, options: SimulationOptions) -> bool {
        use map::{LandMode, LandRecipe};
        let unrestricted = (options.land_mode, options.land_recipe)
            == (LandMode::Unrestricted, LandRecipe::Unrestricted);
        let authored_free_build = || {
            unrestricted
                && options.progression == ProgressionMode::FreeBuild
                && options.valid_for_authored_bootstrap()
        };
        match self.constructor {
            MapConstructor::NewGame => {
                options.valid_for_bootstrap()
                    && (unrestricted || self.initial_lot_layout == options.recipe_lot_layout())
            }
            MapConstructor::Imported => {
                unrestricted
                    && !options.save_replay
                    && options.preset != MapPreset::Waters
                    && options.new_game_options().is_valid()
            }
            MapConstructor::AuthoredFlat => {
                authored_free_build()
                    && options.starting_money_bucks == SimulationOptions::LEGACY.starting_money_bucks
                    && self.initial_lot_layout == Some(LotLayout::default())
            }
            MapConstructor::AuthoredTown => {
                authored_free_build()
                    && options.terrain_size >= AuthoredTownOptions::MIN_TERRAIN_CHUNKS
                    && self.initial_lot_layout == Some(LotLayout::Uniform)
            }
            MapConstructor::AuthoredTiled => {
                matches!(
                    (options.land_mode, options.land_recipe),
                    (LandMode::Tiled, LandRecipe::AuthoredV1 { lot_layout, .. })
                        if self.initial_lot_layout == Some(lot_layout)
                ) && options.valid_for_authored_bootstrap()
            }
        }
    }

    fn validate_options(self, options: SimulationOptions) -> Result<PopulationScale, String> {
        let scale = options.population_scale().map_err(|error| error.to_string())?;
        if self.constructor.is_authored() && scale != PopulationScale::REALISTIC {
            return Err("Authored map constructors must use Realistic population".into());
        }
        if !self.matches_options(options) {
            return Err("Invalid options for the snapshot map constructor".into());
        }
        Ok(scale)
    }

    fn from_legacy(options: SimulationOptions, map: &Map) -> Result<Self, String> {
        // The importer already persisted a dedicated marker for its lane geometry.
        if map.has_imported_lane_reserves() {
            return Ok(Self {
                constructor: MapConstructor::Imported,
                initial_lot_layout: None,
            });
        }

        // Compare constructor contracts against the saved creation options. In
        // particular, neither a later layout change nor the ratio under validation
        // can resolve an overlap between constructors.
        let mut candidates = [
            Self {
                constructor: MapConstructor::NewGame,
                initial_lot_layout: options.recipe_lot_layout(),
            },
            Self::new(MapConstructor::AuthoredFlat, LotLayout::default()),
            Self::new(MapConstructor::AuthoredTown, LotLayout::Uniform),
            Self {
                constructor: MapConstructor::AuthoredTiled,
                initial_lot_layout: options.recipe_lot_layout(),
            },
        ]
        .into_iter()
        .filter(|origin| origin.matches_options(options));
        let origin = candidates
            .next()
            .ok_or("Invalid legacy snapshot map origin")?;
        if candidates.next().is_some() {
            return Err("Ambiguous legacy snapshot map origin".into());
        }
        Ok(origin)
    }
}

impl Simulation {
    /// The immutable population ratio selected when this city was created.
    pub fn population_scale(&self) -> PopulationScale {
        self.read::<SimulationOptions>()
            .population_scale()
            .expect("validated simulation options")
    }

    pub fn schedule() -> SeqSchedule {
        let mut schedule = SeqSchedule::default();
        unsafe {
            for s in &*addr_of!(GSYSTEMS) {
                let s = (s.s)();
                schedule.add_system(s);
            }
        }
        schedule
    }

    pub fn new(gen_terrain: bool) -> Simulation {
        if gen_terrain {
            return Self::new_game(&NewGameOptions::default()).expect("valid default new game");
        }
        Self::new_with_options(SimulationOptions {
            terrain_size: 0,
            ..SimulationOptions::LEGACY
        })
    }

    pub fn new_game(options: &NewGameOptions) -> Result<Self, NewGameError> {
        Self::new_game_with_recording(options, false)
    }

    pub fn new_game_with_replay(options: &NewGameOptions) -> Result<Self, NewGameError> {
        Self::new_game_with_recording(options, true)
    }

    fn new_game_with_recording(
        options: &NewGameOptions,
        save_replay: bool,
    ) -> Result<Self, NewGameError> {
        options.validate()?;
        Self::new_with_lot_layout(
            SimulationOptions {
                terrain_size: options.terrain_chunks,
                save_replay,
                preset: options.preset,
                seed: options.seed,
                starting_money_bucks: options.starting_money_bucks,
                progression: options.progression,
                land_mode: options.land_mode,
                land_recipe: match options.land_mode {
                    map::LandMode::Unrestricted => map::LandRecipe::Unrestricted,
                    map::LandMode::Tiled => map::LandRecipe::NativeFlatV1 {
                        lot_layout: options.lot_layout,
                    },
                },
                persons_per_agent: options.persons_per_agent,
            },
            options.lot_layout,
        )
        .map_err(|reason| match reason {
            Rejection::LandSetup(error) => NewGameError::Land(error),
            reason => NewGameError::Bootstrap(reason),
        })
    }

    /// Creates a blank, replay-disabled Flat world for an authored layout.
    pub fn new_authored_flat(terrain_chunks: u16, seed: u64) -> Result<Self, NewGameError> {
        if !SimulationOptions::AUTHORED_TERRAIN_CHUNKS.contains(&terrain_chunks) {
            return Err(NewGameError::TerrainChunks(terrain_chunks));
        }
        let mut sim = Self::with_initial_resources();
        WorldCommand::bootstrap(
            &mut sim,
            SimulationOptions {
                terrain_size: terrain_chunks,
                save_replay: false,
                preset: MapPreset::Flat,
                seed,
                ..SimulationOptions::LEGACY
            },
            MapOrigin::new(MapConstructor::AuthoredFlat, LotLayout::default()),
        )
        .map_err(NewGameError::Bootstrap)?;
        Ok(sim)
    }
    /// Creates a simulation and loader for a replay from the current version.
    ///
    /// # Panics
    ///
    /// Panics if the replay version or its bootstrap data is invalid.
    pub fn from_replay(replay: Replay) -> (Simulation, SimulationReplayLoader) {
        Self::try_from_replay(replay).unwrap_or_else(|error| panic!("{error}"))
    }

    pub fn try_from_replay(replay: Replay) -> io::Result<(Simulation, SimulationReplayLoader)> {
        if replay.version != replay_version() {
            return Err(replay_version_error(&replay.version));
        }
        replay.check_registry()?;
        let options = replay.bootstrap_options()?;
        let bootstrap_end = replay.bootstrap_end()?;
        let mut sim = Self::with_initial_resources();

        let lot_layout = match replay.commands(1..bootstrap_end).next() {
            Some(WorldCommand::SetLotLayout(layout)) => *layout,
            _ => options.recipe_lot_layout().unwrap_or_default(),
        };
        let origin = MapOrigin::new(MapConstructor::NewGame, lot_layout);
        WorldCommand::bootstrap(&mut sim, options, origin).map_err(|reason| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Replay Init: {reason:?}"),
            )
        })?;
        sim.apply_command_batch(
            replay.commands(1..bootstrap_end),
            false,
            BudgetCategory::StarterInfrastructure,
        );
        if options.preset == MapPreset::Waters {
            sim.check_waters_starters().map_err(|reason| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Replay starters: {reason:?}"),
                )
            })?;
        }
        if replay.has_timeline() {
            sim.record_constructor_end();
        }
        sim.write::<Replay>().registry = replay.registry;
        sim.world.reconcile_derived("replay bootstrap");
        let loader = SimulationReplayLoader::new(replay, Tick(sim.get_tick()), bootstrap_end);
        Ok((sim, loader))
    }

    pub fn new_with_options(opts: SimulationOptions) -> Simulation {
        Self::new_with_lot_layout(opts, LotLayout::Varied)
            .expect("invalid simulation bootstrap options")
    }

    fn with_initial_resources() -> Self {
        let mut sim = Simulation {
            world: Default::default(),
            resources: Default::default(),
        };

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }
        sim
    }

    fn new_with_lot_layout(
        opts: SimulationOptions,
        lot_layout: LotLayout,
    ) -> Result<Self, Rejection> {
        let mut sim = Self::with_initial_resources();
        let origin = MapOrigin::new(
            MapConstructor::NewGame,
            opts.recipe_lot_layout().unwrap_or(lot_layout),
        );
        WorldCommand::bootstrap(&mut sim, opts, origin)?;
        if opts.land_mode == map::LandMode::Tiled {
            sim.record_constructor_end();
            sim.world.reconcile_derived("new tiled world");
            return Ok(sim);
        }
        let mut start_commands: Vec<(u32, WorldCommand)> =
            common::saveload::JSON::decode(START_COMMANDS.as_bytes()).unwrap();
        {
            let map = sim.map();
            for (_, command) in &mut start_commands {
                if let WorldCommand::MapMakeMultipleConnections(projects, _) = command {
                    for project in projects.iter_mut().filter(|project| project.is_ground()) {
                        if let Some(height) = map.environment().height(project.pos.xy()) {
                            project.pos.z = height;
                        }
                    }
                }
            }
        }
        // The layout is recorded with the starting commands, so replays bootstrap with it.
        // The default needs no command, which keeps existing replays unchanged.
        let layout =
            (lot_layout != LotLayout::default()).then_some(WorldCommand::SetLotLayout(lot_layout));
        sim.apply_command_batch(
            layout
                .iter()
                .chain(start_commands.iter().map(|(_, cmd)| cmd)),
            false,
            BudgetCategory::StarterInfrastructure,
        );
        if opts.preset == MapPreset::Waters {
            sim.check_waters_starters()?;
        }
        sim.record_constructor_end();
        sim.world.reconcile_derived("new world");

        Ok(sim)
    }

    fn check_waters_starters(&self) -> Result<(), Rejection> {
        if let Some((_, reason)) = self.read::<CommandOutcomes>().last.first() {
            return Err(*reason);
        }
        let map = self.map();
        let external = map
            .external_train_stations
            .first()
            .and_then(|&id| map.get(id))
            .ok_or(Rejection::MutationFailed)?;
        let freight = map
            .buildings()
            .values()
            .find(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))
            .ok_or(Rejection::MutationFailed)?;
        let platform = crate::souls::freight_station::train_stop(&geom::Transform::new_dir(
            freight.obb.center().z(freight.height),
            freight.obb.axis()[1].z(0.0).normalize(),
        ));
        let market = external.obb.center().z(external.height);
        let tick = Tick(self.get_tick());
        if Itinerary::route(tick, platform, market, &map, crate::map::PathKind::Rail).is_none() {
            return Err(Rejection::MutationFailed);
        }
        Ok(())
    }

    fn record_constructor_end(&self) {
        self.write::<Replay>()
            .record_constructor_end(Tick(self.get_tick()));
    }

    /// Splits the world from its resources. Raw human storage edits through
    /// the returned world invalidate its work sets automatically.
    pub(crate) fn world_res(&mut self) -> (&mut World, &mut Resources) {
        (&mut self.world, &mut self.resources)
    }

    /// Raw resource access for unrestricted authoring and fixtures.
    pub fn unrestricted_world_res(&mut self) -> Option<(&mut World, &mut Resources)> {
        if self.map().land_ownership().is_tiled() {
            return None;
        }
        Some(self.world_res())
    }

    pub fn world(&self) -> &World {
        &self.world
    }
    pub fn world_mut_unchecked(&mut self) -> &mut World {
        self.world.invalidate_derived("unchecked world access");
        &mut self.world
    }

    pub fn is_equal(&self, other: &Self) -> bool {
        if self.resources.iter().count() != other.resources.iter().count() {
            return false;
        }

        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                let a = l.encode(self);
                let b = l.encode(other);

                if a != b {
                    std::fs::write(format!("{}_a.json", l.name), &*String::from_utf8_lossy(&a))
                        .unwrap();
                    std::fs::write(format!("{}_b.json", l.name), &*String::from_utf8_lossy(&b))
                        .unwrap();
                    return false;
                }
            }
        }

        true
    }

    pub fn tick<'a>(
        &mut self,
        game_schedule: &mut SeqSchedule,
        commands: impl IntoIterator<Item = &'a WorldCommand>,
    ) -> Duration {
        profiling::scope!("simulation::tick");
        let t = Instant::now();
        // It is very important that the first thing being done is applying commands
        // so that instant commands work on single player but the game is still deterministic
        {
            profiling::scope!("applying commands");
            self.apply_command_batch(commands, true, BudgetCategory::Construction);
        }

        {
            let mut time = self.write::<GameTime>();
            *time = GameTime::new(Tick(time.tick.0 + 1));
        }

        game_schedule.execute(self);

        self.write::<Replay>().record_step(Tick(self.get_tick()));

        t.elapsed()
    }

    /// Applies a command batch without advancing the simulation clock.
    pub fn apply_commands<'a>(&mut self, commands: impl IntoIterator<Item = &'a WorldCommand>) {
        self.apply_command_batch(commands, false, BudgetCategory::Construction);
    }

    pub fn edit_water(&mut self, edit: WaterEdit) -> Result<WaterEditOutcome, WaterEditError> {
        self.apply_commands([&WorldCommand::EditWater(edit)]);
        self.read::<CommandOutcomes>()
            .water_edit(0)
            .expect("EditWater always records an indexed outcome")
            .clone()
    }

    pub fn preview_water_edit(&self, edit: &WaterEdit) -> Result<WaterEditPreview, WaterEditError> {
        self.map()
            .preview_water_edit(edit, &self.read::<crate::map::ZoneGrid>())
    }

    pub fn carve_river(
        &mut self,
        points: &[Vec2],
        width_m: f32,
        depth_m: f32,
    ) -> Result<WaterBodyId, WaterEditError> {
        self.edit_water(WaterEdit::CarveRiver {
            polyline: WaterStroke::new(points)?,
            width_m,
            depth_m,
            level_dm: None,
        })
        .map(|outcome| outcome.body.expect("creating a river returns its ID"))
    }

    pub fn fill_lake(
        &mut self,
        basin: &Polygon,
        level_dm: i16,
        depth_m: f32,
    ) -> Result<WaterBodyId, WaterEditError> {
        self.edit_water(WaterEdit::FillLake {
            basin: WaterBasin::new(basin.as_slice())?,
            level_dm,
            depth_m,
        })
        .map(|outcome| outcome.body.expect("creating a lake returns its ID"))
    }

    fn apply_command_batch<'a>(
        &mut self,
        commands: impl IntoIterator<Item = &'a WorldCommand>,
        steps_systems: bool,
        construction: BudgetCategory,
    ) {
        let limit = self.command_batch_limit();
        let tiled = limit == world_command::MAX_TILED_COMMANDS;
        let mut commands = commands.into_iter();
        let Some(first) = commands.next() else {
            utils::scheduler::drain_commands(self);
            return;
        };
        let mut batch = Vec::new();
        let remaining = &mut commands;
        let commands = if let Some(second) = remaining.next() {
            batch.extend([first, second]);
            batch.extend(remaining.take(limit - 1));
            batch.as_slice()
        } else {
            std::slice::from_ref(&first)
        };
        if commands.len() > limit {
            self.reject_command_batch(commands, remaining, tiled);
            return;
        }
        if commands.len() > world_command::MAX_TILED_COMMANDS
            && commands.iter().any(|command| {
                matches!(
                    command,
                    WorldCommand::MapUpgradeRoad { .. } | WorldCommand::MapSetJunctionRules { .. }
                )
            })
        {
            self.reject_command_batch(commands, std::iter::empty(), tiled);
            return;
        }
        let wire_errors: Vec<_> = commands
            .iter()
            .map(|command| command.wire_error())
            .collect();
        let wire_commands;
        let admitted = if wire_errors.iter().all(Option::is_none) {
            commands
        } else {
            wire_commands = commands
                .iter()
                .copied()
                .zip(&wire_errors)
                .filter_map(|(command, error)| error.is_none().then_some(command))
                .collect::<Vec<_>>();
            &wire_commands
        };
        if admitted
            .iter()
            .map(|command| command.wire_descriptors())
            .fold(0usize, usize::saturating_add)
            > map::MAX_WATER_BATCH_WORK
        {
            self.reject_command_batch(commands, std::iter::empty(), tiled);
            return;
        }
        let mut water = PrepareBudget::new(admitted, self.map().environment());
        let topology_generation = self.map().walking_topology_generation;
        let recording_start = self.read::<Replay>().command_count();
        let parent = self.write::<CommandOutcomes>().begin_batch();
        let record = parent.is_none() && self.read::<Replay>().enabled;
        for (index, (command, wire_error)) in commands.iter().zip(wire_errors).enumerate() {
            let start = self.read::<CommandOutcomes>().command_start();
            if let Some(error) = wire_error {
                command.reject_wire(self, error);
            } else {
                // Preserve admitted positions, including preparation rejections and no-ops.
                if record {
                    let tick = self.read::<GameTime>().tick;
                    self.write::<Replay>().push(tick, (*command).clone());
                }
                match command.admitted(tiled, &water) {
                    Ok(()) => {
                        command.apply_in_batch(self, construction, &mut water);
                    }
                    Err(reason) => command.reject_unadmitted(self, reason),
                }
            }
            utils::scheduler::drain_commands(self);
            self.write::<CommandOutcomes>().record_command(index, start);
        }
        if self.map().walking_topology_generation != topology_generation {
            transit::on_topology_changed(self);
            economy::cargo::on_topology_changed(self);
        }
        if record {
            self.write::<Replay>().record_batch(
                recording_start,
                steps_systems,
                Tick(self.get_tick()),
            );
        }
        self.write::<CommandOutcomes>().finish_batch(parent);
    }

    fn reject_command_batch<'a>(
        &mut self,
        commands: &[&WorldCommand],
        excess: impl Iterator<Item = &'a WorldCommand>,
        tiled: bool,
    ) {
        let reason = if tiled {
            Rejection::LandDenied(map::LandDenial::WorkLimit)
        } else {
            Rejection::WaterWorkLimit
        };
        let parent = self.write::<CommandOutcomes>().begin_batch();
        for (index, command) in commands.iter().enumerate() {
            let start = self.read::<CommandOutcomes>().command_start();
            command.reject_unadmitted(self, reason);
            self.write::<CommandOutcomes>().record_command(index, start);
        }
        {
            let mut outcomes = self.write::<CommandOutcomes>();
            outcomes.reject_batch(reason);
            for (offset, command) in excess.enumerate() {
                if matches!(command, WorldCommand::BuyTile { .. }) {
                    outcomes.reject_excess_purchase(commands.len() + offset, reason);
                }
            }
        }
        utils::scheduler::drain_commands(self);
        self.write::<CommandOutcomes>().finish_batch(parent);
    }

    pub fn command_batch_limit(&self) -> usize {
        if self.map().land_ownership().is_tiled() {
            world_command::MAX_TILED_COMMANDS
        } else {
            world_command::MAX_WIRE_COMMANDS
        }
    }

    pub fn get_tick(&self) -> u64 {
        self.resources.read::<GameTime>().tick.0
    }

    pub fn hashes(&self) -> BTreeMap<String, u64> {
        let mut hashes = BTreeMap::new();
        let ser = common::saveload::Bincode::encode(&self.world).unwrap();
        hashes.insert("world".to_string(), common::hash_u64(&*ser));

        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                let v = l.encode(self);
                hashes.insert(l.name.to_string(), common::hash_u64(&*v));
            }
        }

        hashes
    }

    pub fn load_replay_from_disk(save_name: &str) -> Option<Replay> {
        let path = ReplayJson::filename(&format!("{save_name}_replay"));
        let result = (|| {
            let bytes = read_slot_member(&path)
                .map_err(SaveLoadError::Io)?
                .ok_or(SaveLoadError::NotFound)?;
            decode_snapshot_replay(
                &bytes,
                &CompressedBincode::filename(save_name),
                &Bincode::filename(save_name),
            )
        })();
        match result {
            Ok(replay) => Some(replay),
            Err(error) => {
                log::error!("Could not load replay {}: {error}", path.display());
                None
            }
        }
    }

    pub fn load_from_disk(save_name: &str) -> Result<Self, SaveLoadError> {
        let (file, kind, _) = open_snapshot(
            &CompressedBincode::filename(save_name),
            &Bincode::filename(save_name),
        )?;
        let sim = Self::load_snapshot(file, kind)?;
        log::info!("successfully loaded simulation {save_name}");
        Ok(sim)
    }

    pub fn load_from_slot(root: &Path, id: &SlotId) -> Result<Self, SaveLoadError> {
        let paths = saveload::slot_paths(root, id).map_err(SaveLoadError::Io)?;
        let (file, kind, _) = open_snapshot(&paths.zip, &paths.bc)?;
        Self::load_snapshot(file, kind)
    }

    fn load_snapshot(mut file: fs::File, kind: SnapshotKind) -> Result<Self, SaveLoadError> {
        // A readable old header is enough to reject the save. If the prefix is
        // unreadable, the normal load still distinguishes oversized and corrupt files.
        if let Ok(header) = read_snapshot_header(&mut file, kind) {
            header.check_load_compatibility()?;
        }
        file.rewind().map_err(SaveLoadError::Io)?;
        Self::decode_snapshot(&read_snapshot(file, kind)?)
    }

    /// Inspect the bounded prefix of compressed slot bytes.
    pub fn compressed_snapshot_header(bytes: &[u8]) -> Result<SnapshotHeader, SaveLoadError> {
        read_snapshot_header(bytes, SnapshotKind::Zip)
    }

    /// Load compressed slot bytes with the same registry and schema checks as a slot file.
    pub fn load_compressed_snapshot(bytes: &[u8], limit: usize) -> Result<Self, SaveLoadError> {
        let limit = limit.min(MAX_SNAPSHOT_BYTES);
        if bytes.len() > limit {
            return Err(SaveLoadError::SnapshotTooLarge { limit });
        }
        read_snapshot_header(bytes, SnapshotKind::Zip)?.check_load_compatibility()?;
        Self::decode_snapshot(&decompress_snapshot(bytes, limit)?)
    }

    pub fn validate_listed_snapshot(root: &Path, entry: &SlotEntry) -> Result<(), SaveLoadError> {
        open_listed_snapshot(root, entry).map(|_| ())
    }

    pub fn load_listed_snapshot(root: &Path, entry: &SlotEntry) -> Result<Self, SaveLoadError> {
        let (mut file, kind) = open_listed_snapshot(root, entry)?;
        file.rewind().map_err(SaveLoadError::Io)?;
        Self::decode_snapshot(&read_snapshot(file, kind)?)
    }

    pub fn load_replay_from_slot(root: &Path, id: &SlotId) -> Result<Replay, SaveLoadError> {
        let paths = saveload::slot_paths(root, id).map_err(SaveLoadError::Io)?;
        let bytes = read_slot_member(&paths.replay)
            .map_err(SaveLoadError::Io)?
            .ok_or(SaveLoadError::NotFound)?;
        decode_snapshot_replay(&bytes, &paths.zip, &paths.bc)
    }

    /// Reads a bounded prefix of the selected snapshot for slot discovery.
    pub fn read_slot_snapshot_header(
        path: &Path,
        kind: SnapshotKind,
    ) -> Result<SnapshotHeader, SaveLoadError> {
        if kind == SnapshotKind::Obstructed {
            return Err(SaveLoadError::Io(nonregular_slot_member(path)));
        }
        let file = open_slot_member(path)
            .map_err(SaveLoadError::Io)?
            .ok_or(SaveLoadError::NotFound)?;
        read_snapshot_header(file, kind)
    }

    fn decode_snapshot(bytes: &[u8]) -> Result<Self, SaveLoadError> {
        let (header, payload) = snapshot_header(bytes)?;
        header.check_load_compatibility()?;
        let version = header.version;
        let enveloped = header.revision.is_some();
        let data: SimulationDeser =
            Bincode::decode(payload).map_err(|error| SaveLoadError::Corrupt {
                version: Some(version.clone()),
                reason: error.to_string(),
            })?;
        if enveloped && data._header.version != version {
            return Err(SaveLoadError::Corrupt {
                version: Some(version),
                reason: "snapshot payload version does not match the slot header".into(),
            });
        }
        let sim = data.into_sim()?;
        if sim
            .resources
            .try_read::<Map>()
            .ok()
            .is_none_or(|map| map.environment().size().0 == 0)
        {
            return Err(SaveLoadError::Corrupt {
                version: Some(version),
                reason: "the map has no terrain".into(),
            });
        }
        Ok(sim)
    }

    /// Load the world, or back up a refused save before creating its replacement.
    /// The optional message describes the recovery for the caller to display.
    pub fn load_or_new(save_name: &str) -> Result<(Self, Option<String>), String> {
        let error = match Self::load_from_disk(save_name) {
            Ok(sim) => return Ok((sim, None)),
            Err(SaveLoadError::NotFound) => return Ok((Self::new(true), None)),
            Err(SaveLoadError::Io(error)) if error.kind() == io::ErrorKind::InvalidData => {
                let message = format!("Could not read save ({error}). Started a new world with the obstructed save left in place.");
                return Ok((Self::new(true), Some(message)));
            }
            Err(error @ SaveLoadError::Io(_)) => return Err(error.to_string()),
            Err(error) => error,
        };
        let version = match &error {
            SaveLoadError::Incompatible { version } => Some(version.as_str()),
            SaveLoadError::Corrupt { version, .. } => version.as_deref(),
            SaveLoadError::SnapshotTooLarge { .. } | SaveLoadError::PrototypeRegistry => None,
            _ => unreachable!(),
        };
        let backups = saveload::backup_save_files(save_name, version).map_err(|backup_error| {
            format!("{error}. Could not back up the save: {backup_error}")
        })?;
        let message = format!(
            "{error}. Moved {} save file(s) to .bak files in {} and started a new world.",
            backups.len(),
            saveload::save_dir().display()
        );
        log::error!("{message}");
        Ok((Self::new(true), Some(message)))
    }

    pub fn save_to_slot(
        &self,
        root: &Path,
        id: &SlotId,
        revision: SnapshotRevision,
    ) -> Result<SavePhase, SaveLoadError> {
        self.encode_slot(revision)?.publish(root, id)
    }

    /// Captures the current save boundary. The returned data can be encoded off the simulation.
    pub fn capture_slot(&self, revision: SnapshotRevision) -> Result<SlotCapture, SaveLoadError> {
        self.capture_slot_with_limit(revision, MAX_SNAPSHOT_BYTES)
    }

    pub fn capture_slot_with_limit(
        &self,
        revision: SnapshotRevision,
        limit: usize,
    ) -> Result<SlotCapture, SaveLoadError> {
        let header = RegistrySlotSnapshotHeader {
            slot: SlotSnapshotHeader {
                magic: REGISTRY_SLOT_MAGIC,
                version: VERSION.to_owned(),
                revision,
            },
            registry: prototypes::prototypes().fingerprint(),
        };
        // Serialize consecutive wire fields into owned fragments at this boundary.
        // All fragments draw from the same budget, including their map framing.
        let (prefix, resources) = (|| {
            let mut prefix = SlotBuffer::new(limit);
            Bincode::encode_writer(&(header, SaveHeader::current(), &self.world), &mut prefix)?;
            let resources = self.snapshot_resources(false, prefix.remaining())?;
            Ok((prefix.bytes, resources))
        })()
        .map_err(|error| slot_encode_error(error, limit))?;
        let replay = self.read::<Replay>().clone();
        let scale = self.population_scale();
        Ok(SlotCapture {
            prefix,
            resources,
            replay,
            revision,
            game_tick: self.get_tick(),
            population: scale.persons(self.world.humans.len()),
            persons_per_agent: scale.persons_per_agent(),
            money_cents: self.read::<economy::Government>().money.cents(),
        })
    }

    /// Synchronously captures and encodes a save.
    pub fn encode_slot(&self, revision: SnapshotRevision) -> Result<EncodedSlot, SaveLoadError> {
        self.capture_slot(revision)?.encode()
    }

    pub fn save_to_disk(&self, save_name: &str) {
        self.save_snapshot_and_replay(save_name);
    }

    /// Saves through the usual snapshot and replay paths and reports snapshot success.
    pub fn save_snapshot_and_replay(&self, save_name: &str) -> bool {
        if CompressedBincode::save(self, save_name).is_none() {
            return false;
        }
        let replay = self.read::<Replay>();
        let name = format!("{save_name}_replay");
        if replay.enabled {
            saveload::JSONPretty::save(&*replay, &name);
        } else if let Err(error) = remove_replay(&ReplayJson::filename(&name)) {
            log::error!("Could not remove replay {name}: {error}");
        }
        true
    }

    pub fn pos<E: WorldTransform>(&self, id: E) -> Option<Vec3> {
        self.world.pos(id)
    }

    pub fn pos_any(&self, id: AnyEntity) -> Option<Vec3> {
        self.world.pos_any(id)
    }

    pub fn get<E: EntityID>(&self, id: E) -> Option<&E::Entity> {
        self.world.get(id)
    }

    pub fn contains(&self, id: AnyEntity) -> bool {
        self.world.contains(id)
    }

    fn allows_resource_write<T: Any>(&self) -> bool {
        use std::any::TypeId;
        if TypeId::of::<T>() == TypeId::of::<SimulationOptions>()
            || TypeId::of::<T>() == TypeId::of::<MapOrigin>()
        {
            return false;
        }
        let protected = [
            TypeId::of::<Map>(),
            TypeId::of::<map::ZoneGrid>(),
            TypeId::of::<map::Environment>(),
        ];
        !protected.contains(&TypeId::of::<T>())
            || !self.resources.contains::<Map>()
            || self
                .resources
                .try_read::<Map>()
                .is_ok_and(|map| !map.land_ownership().is_tiled())
    }

    pub fn write_or_default<T: Any + Send + Sync + Default>(&mut self) -> RefMut<T> {
        assert!(
            self.allows_resource_write::<T>(),
            "resource cannot be updated directly"
        );
        self.resources.write_or_default::<T>()
    }

    pub fn try_write<T: Any + Send + Sync>(&self) -> Option<RefMut<T>> {
        if !self.allows_resource_write::<T>() {
            return None;
        }
        self.resources.try_write().ok()
    }

    pub fn write<T: Any + Send + Sync>(&self) -> RefMut<T> {
        assert!(
            self.allows_resource_write::<T>(),
            "resource cannot be updated directly"
        );
        self.resources.write()
    }

    pub fn read<T: Any + Send + Sync>(&self) -> Ref<T> {
        self.resources.read()
    }

    pub fn map(&self) -> Ref<'_, Map> {
        self.resources.read()
    }

    pub(crate) fn map_mut(&self) -> RefMut<'_, Map> {
        self.resources.write()
    }

    pub(crate) fn zone_grid_mut(&self) -> RefMut<'_, map::ZoneGrid> {
        self.resources.write()
    }

    pub fn insert<T: Any + Send + Sync>(&mut self, res: T) {
        assert!(
            self.allows_resource_write::<T>(),
            "resource cannot be updated directly"
        );
        self.resources.insert(res);
    }
}

impl Simulation {
    fn snapshot_resources(
        &self,
        encode_replay: bool,
        mut remaining: usize,
    ) -> io::Result<FastMap<String, Vec<u8>>> {
        let mut resources = FastMap::default();
        let registry = prototypes::try_prototypes();
        let registered = unsafe { &*addr_of!(SAVELOAD_FUNCS) };
        charge_slot_bytes(
            &mut remaining,
            Bincode::encoded_size(&(registered.len() + 1 + usize::from(registry.is_some())))?,
        )?;

        let mut capture = |name: &str, save: &dyn Fn(&mut SlotBuffer) -> io::Result<()>| {
            // Charge the name and the shortest length field before allocating.
            charge_slot_bytes(&mut remaining, Bincode::encoded_size(&(name, 0usize))?)?;
            let mut payload = SlotBuffer::new(slot_payload_limit(remaining)?);
            save(&mut payload)?;
            charge_slot_bytes(&mut remaining, payload.bytes.len())?;
            charge_slot_bytes(
                &mut remaining,
                Bincode::encoded_size(&payload.bytes.len())? - 1,
            )?;
            resources.insert(name.to_string(), payload.bytes);
            Ok::<_, io::Error>(())
        };
        for resource in registered {
            // Insert the placeholder at the original position to preserve the
            // legacy FxHashMap iteration order, including growth on insertion.
            capture(resource.name, &|output| {
                if encode_replay || resource.name != ReplayJson::RESOURCE_NAME {
                    (resource.save)(self, output)?;
                }
                Ok(())
            })?;
        }
        if let Some(registry) = registry {
            capture("prototype_registry", &|output| {
                Bincode::encode_writer(&registry.fingerprint(), output)
            })?;
        }
        capture("map_origin", &|output| {
            Bincode::encode_writer(&*self.read::<MapOrigin>(), output)
        })?;
        Ok(resources)
    }
}

impl Serialize for Simulation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        log::info!("serializing sim state");
        let t = Instant::now();
        let res = self
            .snapshot_resources(true, usize::MAX)
            .map_err(serde::ser::Error::custom)?;
        log::info!("took {}s to serialize resources", t.elapsed().as_secs_f32());

        let v = SimulationSer {
            header: SaveHeader::current(),
            world: &self.world,
            res,
        }
        .serialize(serializer);
        log::info!("took {}s to serialize in total", t.elapsed().as_secs_f32());
        v
    }
}

#[derive(Serialize, Deserialize)]
struct SaveHeader {
    version: String,
}

impl SaveHeader {
    fn current() -> Self {
        Self {
            version: VERSION.to_owned(),
        }
    }
}

// A legacy Bincode string length cannot start with 0xff. Keep the normal
// Simulation serialization inside the slot envelope for legacy and network callers.
const SLOT_SNAPSHOT_MAGIC: [u8; 12] = *b"\xffEGREGORIA\0\x01";
const REGISTRY_SLOT_MAGIC: [u8; 12] = *b"\xffEGREGORIA\0\x02";

#[derive(Serialize, Deserialize)]
struct SlotSnapshotHeader {
    magic: [u8; 12],
    version: String,
    revision: SnapshotRevision,
}

#[derive(Serialize, Deserialize)]
struct RegistrySlotSnapshotHeader {
    slot: SlotSnapshotHeader,
    registry: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SnapshotHeader {
    pub version: String,
    pub revision: Option<SnapshotRevision>,
    registry: Option<u64>,
}

impl SnapshotHeader {
    pub fn check_compatibility(&self) -> Result<(), SaveLoadError> {
        self.check_version()?;
        check_prototype_registry(self.registry)
    }

    fn check_load_compatibility(&self) -> Result<(), SaveLoadError> {
        if self.revision.is_none() {
            // Unenveloped debug snapshots carry the fingerprint in the payload.
            return self.check_version();
        }
        self.check_compatibility()
    }

    fn check_version(&self) -> Result<(), SaveLoadError> {
        if !compatible_save_version(&self.version) {
            return Err(SaveLoadError::Incompatible {
                version: self.version.clone(),
            });
        }
        Ok(())
    }
}

fn check_prototype_registry(fingerprint: Option<u64>) -> Result<(), SaveLoadError> {
    if !prototypes::try_prototypes()
        .is_some_and(|current| fingerprint == Some(current.fingerprint()))
    {
        return Err(SaveLoadError::PrototypeRegistry);
    }
    Ok(())
}

fn snapshot_header(mut bytes: &[u8]) -> Result<(SnapshotHeader, &[u8]), SaveLoadError> {
    let slot = if bytes.starts_with(&REGISTRY_SLOT_MAGIC) {
        let header: RegistrySlotSnapshotHeader =
            Bincode::decode_reader(&mut bytes).map_err(corrupt_header)?;
        Some((header.slot, Some(header.registry)))
    } else if bytes.starts_with(&SLOT_SNAPSHOT_MAGIC) {
        let header: SlotSnapshotHeader =
            Bincode::decode_reader(&mut bytes).map_err(corrupt_header)?;
        Some((header, None))
    } else {
        None
    };
    if let Some((slot, registry)) = slot {
        return Ok((
            SnapshotHeader {
                version: slot.version,
                revision: Some(slot.revision),
                registry,
            },
            bytes,
        ));
    }
    let SaveHeader { version } = Bincode::decode_prefix(bytes).map_err(corrupt_header)?;
    Ok((
        SnapshotHeader {
            version,
            revision: None,
            registry: None,
        },
        bytes,
    ))
}

fn deserialize_save_header<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SaveHeader, D::Error> {
    let header = SaveHeader::deserialize(deserializer)?;
    let version = &header.version;
    if !compatible_save_version(version) {
        return Err(serde::de::Error::custom(format!(
            "Save has version {version:?}, expected {VERSION:?}"
        )));
    }
    Ok(header)
}

#[derive(Debug)]
pub enum SaveLoadError {
    NotFound,
    SnapshotChanged,
    SnapshotTooLarge {
        limit: usize,
    },
    Incompatible {
        version: String,
    },
    IncompatibleReplay {
        version: String,
    },
    PrototypeRegistry,
    Corrupt {
        version: Option<String>,
        reason: String,
    },
    Io(io::Error),
    Encode(io::Error),
    SnapshotWrite(AtomicWriteError),
}

impl Display for SaveLoadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("Save not found"),
            Self::SnapshotChanged => f.write_str("Save changed since it was listed"),
            Self::SnapshotTooLarge { limit } => {
                write!(f, "Save exceeds the inflate size limit of {limit} bytes")
            }
            Self::Incompatible { version } => {
                write!(
                    f,
                    "Save version {:?} is incompatible with {:?}",
                    version.trim(),
                    VERSION.trim()
                )
            }
            Self::IncompatibleReplay { version } => {
                write!(f, "{}", replay_version_error(version))
            }
            Self::PrototypeRegistry => {
                f.write_str("Save or replay requires a different prototype registry")
            }
            Self::Corrupt { reason, .. } => write!(f, "Save is corrupt ({reason})"),
            Self::Io(error) => write!(f, "Could not read save ({error})"),
            Self::Encode(error) => write!(f, "Could not encode snapshot ({error})"),
            Self::SnapshotWrite(error) => write!(f, "Could not publish snapshot ({error})"),
        }
    }
}

impl std::error::Error for SaveLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::Encode(error) => Some(error),
            Self::SnapshotWrite(error) => Some(error),
            _ => None,
        }
    }
}

/// Returned after a durable snapshot replacement.
#[derive(Debug)]
pub enum SavePhase {
    Complete,
    SidecarsFailed {
        replay: Option<AtomicWriteError>,
        meta: Option<AtomicWriteError>,
    },
}

/// Owned state for a single revision of a city.
/// Limit Vec growth before writing, including when a serializer emits many small writes.
struct SlotBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl SlotBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn remaining(&self) -> usize {
        self.limit - self.bytes.len()
    }

    #[cold]
    fn grow(&mut self, size: usize) -> io::Result<()> {
        if size > self.remaining() {
            return Err(slot_limit_error());
        }
        let len = self.bytes.len();
        let capacity = self.bytes.capacity().saturating_mul(2).max(4096);
        let capacity = capacity.max(len + size).min(self.limit);
        self.bytes
            .try_reserve_exact(capacity - len)
            .map_err(io::Error::other)
    }
}

impl Write for SlotBuffer {
    #[inline]
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.write_all(input)?;
        Ok(input.len())
    }

    #[inline(always)]
    fn write_all(&mut self, input: &[u8]) -> io::Result<()> {
        if input.len() > self.bytes.capacity().min(self.limit) - self.bytes.len() {
            self.grow(input.len())?;
        }
        self.bytes.extend_from_slice(input);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn slot_limit_error() -> io::Error {
    io::Error::from(io::ErrorKind::FileTooLarge)
}

fn charge_slot_bytes(remaining: &mut usize, bytes: usize) -> io::Result<()> {
    *remaining = remaining.checked_sub(bytes).ok_or_else(slot_limit_error)?;
    Ok(())
}

/// Includes the variable-length Bincode length field, whose first byte is already charged.
fn slot_payload_limit(remaining: usize) -> io::Result<usize> {
    let mut payload = remaining;
    while Bincode::encoded_size(&payload)? - 1 > remaining - payload {
        payload -= 1;
    }
    Ok(payload)
}

fn slot_encode_error(error: io::Error, limit: usize) -> SaveLoadError {
    if error.kind() == io::ErrorKind::FileTooLarge {
        SaveLoadError::SnapshotTooLarge { limit }
    } else {
        SaveLoadError::Encode(error)
    }
}

pub struct SlotCapture {
    prefix: Vec<u8>,
    resources: FastMap<String, Vec<u8>>,
    replay: Replay,
    revision: SnapshotRevision,
    game_tick: u64,
    population: u64,
    persons_per_agent: u16,
    money_cents: i64,
}

impl SlotCapture {
    pub fn encode(self) -> Result<EncodedSlot, SaveLoadError> {
        self.encode_with_limit(MAX_SNAPSHOT_BYTES)
    }

    pub fn encode_with_limit(self, limit: usize) -> Result<EncodedSlot, SaveLoadError> {
        let Self {
            prefix: mut bytes,
            mut resources,
            replay,
            revision,
            game_tick,
            population,
            persons_per_agent,
            money_cents,
        } = self;
        let snapshot = (|| {
            // Reject the known contribution before writing any embedded JSON.
            let mut remaining = limit;
            charge_slot_bytes(&mut remaining, bytes.len())?;
            charge_slot_bytes(&mut remaining, Bincode::encoded_size(&resources.len())?)?;
            for (name, payload) in &resources {
                charge_slot_bytes(
                    &mut remaining,
                    Bincode::encoded_size(&(name, payload.len()))?,
                )?;
                charge_slot_bytes(&mut remaining, payload.len())?;
            }

            let mut embedded = SlotBuffer::new(slot_payload_limit(remaining)?);
            ReplayJson::encode_writer(&replay, &mut embedded)?;
            charge_slot_bytes(&mut remaining, embedded.bytes.len())?;
            charge_slot_bytes(
                &mut remaining,
                Bincode::encoded_size(&embedded.bytes.len())? - 1,
            )?;
            *resources
                .get_mut(ReplayJson::RESOURCE_NAME)
                .expect("the replay resource is registered") = embedded.bytes;

            // The final size is known. Preserve wire order and copy payloads directly.
            bytes
                .try_reserve_exact(limit - remaining - bytes.len())
                .map_err(io::Error::other)?;
            Bincode::encode_writer(&resources.len(), &mut bytes)?;
            for (name, payload) in resources {
                Bincode::encode_writer(&(name, payload.len()), &mut bytes)?;
                bytes.extend_from_slice(&payload);
            }
            Ok(CompressedBincode::compress(&bytes))
        })()
        .map_err(|error| slot_encode_error(error, limit))?;
        let replay = replay.enabled.then_some(replay);
        Ok(EncodedSlot {
            snapshot,
            replay,
            display_name: None,
            camera: None,
            revision,
            game_tick,
            population,
            persons_per_agent,
            money_cents,
        })
    }
}

pub struct EncodedSlot {
    snapshot: Vec<u8>,
    replay: Option<Replay>,
    display_name: Option<String>,
    camera: Option<SlotCamera>,
    revision: SnapshotRevision,
    game_tick: u64,
    population: u64,
    persons_per_agent: u16,
    money_cents: i64,
}

impl EncodedSlot {
    /// Returns the complete snapshot for an isolated in-memory restore.
    pub fn into_snapshot_bytes(self) -> Vec<u8> {
        self.snapshot
    }

    pub fn with_display_name(mut self, display_name: String) -> Self {
        self.display_name = Some(display_name);
        self
    }

    pub fn with_camera(mut self, camera: SlotCamera) -> Self {
        self.camera = Some(camera);
        self
    }

    /// Publishes a snapshot followed by its sidecars. The caller coordinates
    /// publication and deletion of the same slot.
    pub fn publish(self, root: &Path, id: &SlotId) -> Result<SavePhase, SaveLoadError> {
        self.publish_with_gate(root, id, || Some(())).unwrap()
    }

    /// Stream replay staging before acquiring the publication gate. The gate
    /// covers snapshot replacement and all sidecar changes.
    pub fn publish_with_gate<G>(
        self,
        root: &Path,
        id: &SlotId,
        acquire: impl FnOnce() -> Option<G>,
    ) -> Option<Result<SavePhase, SaveLoadError>> {
        let paths = match saveload::slot_paths(root, id) {
            Ok(paths) => paths,
            Err(error) => {
                return Some(Err(SaveLoadError::SnapshotWrite(
                    AtomicWriteError::PreCommit(error),
                )))
            }
        };
        let mut acquire = Some(acquire);
        let mut guard = None;
        let mut snapshot_result = None;
        let mut commit_snapshot = || {
            snapshot_result
                .get_or_insert_with(|| {
                    guard = acquire.take().unwrap()();
                    guard.as_ref()?;
                    Some(saveload::write_atomic(&paths.zip, &self.snapshot))
                })
                .as_ref()
                .is_some_and(Result::is_ok)
        };

        // The buffered writer has flushed the full pretty JSON before the
        // gate is taken. If staging fails, still try to publish the snapshot.
        let staged_replay = self.replay.as_ref().map(|replay| {
            saveload::write_atomic_with(&paths.replay, |output| {
                ReplayJson::write_sidecar(replay, self.revision, &mut *output)?;
                output.flush()?;
                if commit_snapshot() {
                    Ok(())
                } else {
                    Err(io::Error::other("Snapshot was not published"))
                }
            })
        });
        commit_snapshot();
        if let Err(error) = snapshot_result.unwrap()? {
            return Some(Err(SaveLoadError::SnapshotWrite(error)));
        }
        let replay = staged_replay
            .unwrap_or_else(|| remove_replay(&paths.replay).map_err(AtomicWriteError::PreCommit));
        let meta = SlotMeta {
            display_name: self.display_name.unwrap_or_else(|| id.to_string()),
            game_tick: self.game_tick,
            population: self.population,
            persons_per_agent: self.persons_per_agent,
            money_cents: self.money_cents,
            saved_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            revision: Some(self.revision.get()),
            camera: self.camera,
        };
        let meta = saveload::write_slot_meta(root, id, &meta);
        Some(Ok(match (replay, meta) {
            (Ok(()), Ok(())) => SavePhase::Complete,
            (replay, meta) => SavePhase::SidecarsFailed {
                replay: replay.err(),
                meta: meta.err(),
            },
        }))
    }
}

fn remove_replay(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn open_snapshot<'a>(
    zip: &'a Path,
    bc: &'a Path,
) -> Result<(fs::File, SnapshotKind, &'a Path), SaveLoadError> {
    if let Some(file) = open_slot_member(zip).map_err(SaveLoadError::Io)? {
        return Ok((file, SnapshotKind::Zip, zip));
    }
    let file = open_slot_member(bc)
        .map_err(SaveLoadError::Io)?
        .ok_or(SaveLoadError::NotFound)?;
    Ok((file, SnapshotKind::Bc, bc))
}

fn open_listed_snapshot(
    root: &Path,
    entry: &SlotEntry,
) -> Result<(fs::File, SnapshotKind), SaveLoadError> {
    let paths = saveload::slot_paths(root, &entry.id).map_err(SaveLoadError::Io)?;
    let (mut file, kind, path) =
        open_snapshot(&paths.zip, &paths.bc).map_err(|_| SaveLoadError::SnapshotChanged)?;
    if !entry
        .matches_snapshot_file(path, kind, &file)
        .map_err(SaveLoadError::Io)?
    {
        return Err(SaveLoadError::SnapshotChanged);
    }
    let header =
        read_snapshot_header(&mut file, kind).map_err(|_| SaveLoadError::SnapshotChanged)?;
    if !compatible_save_version(&header.version) || header.revision != entry.revision {
        return Err(SaveLoadError::SnapshotChanged);
    }
    header.check_compatibility()?;
    Ok((file, kind))
}

fn read_snapshot_header(
    file: impl Read,
    kind: SnapshotKind,
) -> Result<SnapshotHeader, SaveLoadError> {
    // The envelope contains magic, a version string and a u128 revision.
    let mut prefix = [0u8; 256];
    let len = if kind == SnapshotKind::Zip {
        CompressedBincode::read_prefix(file, &mut prefix).map_err(corrupt_header)?
    } else {
        let mut bytes = Vec::new();
        file.take(prefix.len() as u64)
            .read_to_end(&mut bytes)
            .map_err(SaveLoadError::Io)?;
        prefix[..bytes.len()].copy_from_slice(&bytes);
        bytes.len()
    };
    snapshot_header(&prefix[..len]).map(|(header, _)| header)
}

const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;

fn read_snapshot(mut file: fs::File, kind: SnapshotKind) -> Result<Vec<u8>, SaveLoadError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(SaveLoadError::Io)?;
    if kind == SnapshotKind::Bc {
        return Ok(bytes);
    }
    decompress_snapshot(&bytes, MAX_SNAPSHOT_BYTES)
}

fn decompress_snapshot(bytes: &[u8], limit: usize) -> Result<Vec<u8>, SaveLoadError> {
    CompressedBincode::decompress_limited(bytes, limit).map_err(|error| {
        if error.kind() == io::ErrorKind::FileTooLarge {
            SaveLoadError::SnapshotTooLarge { limit }
        } else {
            corrupt_header(error)
        }
    })
}

fn decode_snapshot_replay(bytes: &[u8], zip: &Path, bc: &Path) -> Result<Replay, SaveLoadError> {
    let header = match Simulation::read_slot_snapshot_header(zip, SnapshotKind::Zip) {
        Err(SaveLoadError::NotFound) => Simulation::read_slot_snapshot_header(bc, SnapshotKind::Bc),
        result => result,
    };
    // An unrevisioned debug replay can stand alone when its snapshot cannot be used.
    let revision = header
        .and_then(|header| {
            header.check_compatibility()?;
            Ok(header.revision)
        })
        .ok()
        .flatten();
    ReplayJson::decode_sidecar(bytes, revision)
}

fn corrupt_header(error: io::Error) -> SaveLoadError {
    SaveLoadError::Corrupt {
        version: None,
        reason: error.to_string(),
    }
}

fn nonregular_slot_member(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("slot path {} is not a regular file", path.display()),
    )
}

fn open_slot_member(path: &Path) -> io::Result<Option<fs::File>> {
    match saveload::SlotPaths::open_member(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_slot_member(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let Some(mut file) = open_slot_member(path)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

#[derive(Serialize)]
struct SimulationSer<'a> {
    header: SaveHeader,
    world: &'a World,
    res: FastMap<String, Vec<u8>>,
}

#[derive(Deserialize)]
struct SimulationDeser {
    // Bincode reads and validates the leading header before decoding World.
    #[serde(rename = "header", deserialize_with = "deserialize_save_header")]
    _header: SaveHeader,
    world: World,
    res: FastMap<String, Vec<u8>>,
}

impl<'de> Deserialize<'de> for Simulation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        SimulationDeser::deserialize(deserializer)?
            .into_sim()
            .map_err(serde::de::Error::custom)
    }
}

impl SimulationDeser {
    fn into_sim(self) -> Result<Simulation, SaveLoadError> {
        let t = Instant::now();
        let corrupt = |reason| SaveLoadError::Corrupt {
            version: Some(self._header.version.clone()),
            reason,
        };
        let registry = self
            .res
            .get("prototype_registry")
            .map(|bytes| Bincode::decode(bytes))
            .transpose()
            .map_err(|error| corrupt(error.to_string()))?;
        check_prototype_registry(registry)?;
        let mut sim = Simulation {
            // Resource initializers can rebuild derived indexes from the loaded world.
            world: self.world,
            resources: Resources::default(),
        };

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }

        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                let Some(data) = self.res.get(l.name) else {
                    if l.optional {
                        continue;
                    }
                    return Err(corrupt(format!("Missing resource {}", l.name)));
                };
                (l.load)(&mut sim, data)
                    .map_err(|error| corrupt(format!("Could not decode resource {}: {error}", l.name)))?;
            }
        }

        let options = *sim.read::<SimulationOptions>();
        let origin: MapOrigin = match self.res.get("map_origin") {
            Some(bytes) => Bincode::decode(bytes)
                .map_err(|error| corrupt(format!("Could not decode snapshot map origin: {error}")))?,
            None if self._header.version == VERSION => {
                return Err(corrupt("Missing snapshot map origin".into()));
            }
            None => MapOrigin::from_legacy(options, &sim.map()).map_err(&corrupt)?,
        };
        let scale = origin.validate_options(options).map_err(&corrupt)?;
        if self._header.version == PRE_SCALE_VERSION && scale != PopulationScale::REALISTIC {
            return Err(corrupt(
                "pre-SCALE snapshot must use Realistic population".into(),
            ));
        }
        if matches!(origin.constructor, MapConstructor::AuthoredFlat | MapConstructor::AuthoredTown)
            && sim.map().environment().size() != (options.terrain_size, options.terrain_size)
        {
            return Err(corrupt("Snapshot terrain extent does not match the map origin".into()));
        }
        sim.resources.insert(origin);
        sim.read::<Replay>()
            .validate_population_scale(scale)
            .map_err(|error| corrupt(error.to_string()))?;
        validate_loaded_prototypes(&sim).map_err(&corrupt)?;
        crate::map::validate_loaded_land(&sim).map_err(&corrupt)?;
        sim.map()
            .validate_junction_policy_land()
            .map_err(|reason| corrupt(format!("Invalid junction policy land effect: {reason:?}")))?;
        let elapsed = transit::elapsed_ticks(&sim.resources).0;
        sim.write::<crate::transportation::junction_admission::JunctionAdmission>()
            .validate_loaded(
                &sim.map(),
                sim.world(),
                &sim.read::<crate::transportation::TransportGrid>(),
                elapsed,
            )
            .map_err(|reason| corrupt(reason.to_owned()))?;
        if sim
            .map()
            .lane_scoring_pilot()
            .and_then(crate::map::LaneScoringPilot::sampled_at)
            .is_some_and(|sampled| sampled > elapsed)
        {
            return Err(corrupt("Lane scoring sample is ahead of the simulation".into()));
        }
        sim.map_mut()
            .rebuild_land_frontage()
            .map_err(|reason| corrupt(format!("Invalid land frontage: {reason:?}")))?;
        crate::souls::lifecycle::restore_company_departures(&mut sim);
        sim.world.reconcile_derived("after resource decode");

        log::info!(
            "took {}s to deserialize in total",
            t.elapsed().as_secs_f32()
        );

        Ok(sim)
    }
}

fn validate_loaded_prototypes(sim: &Simulation) -> Result<(), String> {
    use crate::souls::residence::Residences;
    for (id, building) in sim.map().buildings() {
        if !building_kind_prototype_exists(building.kind) {
            return Err(format!(
                "building {id:?} requires missing prototype {:?}",
                building.kind
            ));
        }
        if let Some(home) = building.development().and_then(|site| site.home) {
            require_prototype(home)?;
        }
    }
    for home in sim.read::<Residences>().by_building.values() {
        require_prototype(home.prototype)?;
    }
    for company in sim.world.companies.values() {
        require_prototype(company.comp.proto)?;
    }
    for station in sim.world.freight_stations.values() {
        require_prototype(station.f.proto)?;
    }
    for wagon in sim.world.wagons.values() {
        require_prototype(wagon.wagon.rolling_stock)?;
    }
    Ok(())
}

fn require_prototype<I>(id: I) -> Result<(), String>
where
    I: prototypes::PrototypeID,
    I::Prototype: prototypes::ConcretePrototype,
{
    prototypes::try_prototype(id)
        .map(|_| ())
        .ok_or_else(|| format!("missing prototype {id:?}"))
}

fn building_kind_prototype_exists(kind: BuildingKind) -> bool {
    match kind {
        BuildingKind::GoodsCompany(id) => prototypes::try_prototype(id).is_some(),
        BuildingKind::RailFreightStation(id) => prototypes::try_prototype(id).is_some(),
        _ => true,
    }
}

// The rail links share junctions by project index so they can be built before merging.
const START_COMMANDS: &str = r#"
[
  [0, {"MapMakeMultipleConnections": [
    [
      {"pos": [4343.2334, 6262.846, 0.0], "kind": "Ground"},
      {"pos": [4222.163, 6318.7007, 0.0], "kind": "Ground"},
      {"pos": [2024.0668, 147.33333, 0.0], "kind": {"Intersection": {"idx": 2, "version": 1}}},
      {"pos": [2831.1282, 2172.9182, 0.0], "kind": "Ground"},
      {"pos": [4098.609, 4876.7466, 0.0], "kind": "Ground"},
      {"pos": [4180.8335, 5765.297, 0.0], "kind": "Ground"},
      {"pos": [4312.9233, 5982.129, 0.0], "kind": "Ground"},
      {"pos": [4418.4004, 6150.2427, 0.0], "kind": "Ground"},
      {"pos": [4080.5332, 6242.0093, 0.0], "kind": "Ground"},
      {"pos": [4134.9575, 5988.7896, 0.0], "kind": "Ground"}
    ],
    [
      [0, 1, null, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": []}],
      [2, 3, 4972985476040057325, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": [["Rail", 9.0]]}],
      [3, 4, 5005986054842354977, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": [["Rail", 9.0]]}],
      [4, 5, 5008381821963615703, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": [["Rail", 9.0]]}],
      [5, 6, 5009042048632353377, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": []}],
      [6, 7, 5010742073997638941, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": []}],
      [7, 0, 5010861869225921041, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": []}],
      [1, 8, 5008055507530249597, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": []}],
      [8, 9, 5007338222221478024, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": []}],
      [9, 5, 5008608536108168175, {"lanes_forward": [["Rail", 9.0]], "lanes_backward": []}]
    ]
  ]}],
  [0, {"MapBuildSpecialBuilding": {"pos": {"corners": [5010057980197058898, 5010739497017993874, 5009490559183158337, 5008809038067256064]}, "kind": {"RailFreightStation": 9010703082962909221}, "gen": {"NoWalkway": {"door_pos": 0}}, "zone": null}}],
  [0, {"AddTrain": {"dist": 150.0, "n_wagons": 7, "lane": {"idx": 3, "version": 1}}}]
]
"#;

#[cfg(test)]
mod p4_fix_ac_tests {
    use super::*;

    fn save_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "egr-p4-fix-ac-{}-{:032x}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[cfg(unix)]
    #[test]
    fn p4_fix_final_legacy_snapshot_rejects_symlinks() {
        let root = save_root();
        let name = root.join("world");
        for extension in ["zip", "bc"] {
            let path = name.with_extension(extension);
            let target = root.join("target");
            fs::write(&target, b"bad snapshot").unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
            let error = Simulation::load_from_disk(name.to_str().unwrap())
                .err()
                .expect("legacy snapshot resolved a symlink");
            assert!(matches!(error, SaveLoadError::Io(_)), "{error}");
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn p4_fix_final_inflate_reports_size_limit() {
        let bytes = CompressedBincode::encode(&vec![0u8; 256 * 1024 * 1024]).unwrap();
        let root = save_root();
        let zip = root.join("world.zip");
        fs::write(&zip, bytes).unwrap();
        let result = Simulation::load_from_disk(root.join("world").to_str().unwrap());
        fs::remove_dir_all(root).unwrap();
        let error = result.err().expect("oversize snapshot was accepted");
        assert!(
            matches!(error, SaveLoadError::SnapshotTooLarge { .. }),
            "{error}"
        );
    }

    #[test]
    fn p4_fix_ac_discovery_reads_header_even_if_zlib_trailer_is_missing() {
        let root = save_root();
        let path = root.join("city.zip");
        let revision = SnapshotRevision::new(42);
        let header = SlotSnapshotHeader {
            magic: SLOT_SNAPSHOT_MAGIC,
            version: VERSION.to_owned(),
            revision,
        };
        let mut compressed = CompressedBincode::encode(&(header, vec![0u8; 1024 * 1024])).unwrap();
        compressed.truncate(compressed.len() - 4);
        fs::write(&path, compressed).unwrap();
        let header = Simulation::read_slot_snapshot_header(&path, SnapshotKind::Zip);
        fs::remove_dir_all(root).unwrap();
        assert_eq!(header.unwrap().revision, Some(revision));
    }

    #[test]
    fn p4_fix_ac_debug_replay_recovers_without_a_usable_snapshot() {
        let root = save_root();
        let slot = SlotId::legacy();
        let paths = saveload::slot_paths(&root, &slot).unwrap();
        drop(crate::tests::TestCtx::new());
        let mut replay = Replay::default();
        replay.registry = Some(prototypes::prototypes().fingerprint());
        fs::write(&paths.replay, ReplayJson::encode(&replay).unwrap()).unwrap();
        fs::write(&paths.zip, b"corrupt snapshot").unwrap();
        let recovered = Simulation::load_replay_from_slot(&root, &slot);
        assert!(recovered.is_ok(), "{:?}", recovered.err());
        let path_name = root.join(slot.as_str());
        assert!(Simulation::load_replay_from_disk(path_name.to_str().unwrap()).is_some());

        let revision = SnapshotRevision::new(42);
        let write_snapshot = |version: &str| {
            let header = RegistrySlotSnapshotHeader {
                slot: SlotSnapshotHeader {
                    magic: REGISTRY_SLOT_MAGIC,
                    version: version.to_owned(),
                    revision,
                },
                registry: prototypes::prototypes().fingerprint(),
            };
            fs::write(&paths.zip, CompressedBincode::encode(&header).unwrap()).unwrap();
        };
        write_snapshot("0.0.0");
        assert!(Simulation::load_replay_from_slot(&root, &slot).is_ok());
        fs::write(
            &paths.replay,
            ReplayJson::encode_sidecar(&replay, revision).unwrap(),
        )
        .unwrap();
        assert!(Simulation::load_replay_from_slot(&root, &slot).is_err());

        write_snapshot(VERSION);
        assert!(Simulation::load_replay_from_slot(&root, &slot).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
#[test]
fn af2_opening_balance_matches_starter_infrastructure_ledger() {
    use crate::economy::{Budget, BudgetCategory, Government};
    use prototypes::Money;

    drop(crate::tests::TestCtx::new());
    let options = NewGameOptions {
        preset: MapPreset::Flat,
        starting_money_bucks: 150_000,
        ..Default::default()
    };
    let city = Simulation::new_game(&options).unwrap();
    let budget = city.read::<Budget>();
    let treasury = city.read::<Government>().money;
    let spent = i128::from(Money::new_bucks(i64::from(options.starting_money_bucks)).inner())
        - i128::from(treasury.inner());
    assert!(spent > 0);
    assert_eq!(budget.total(BudgetCategory::Construction), 0);
    assert_eq!(
        budget.days()[&0].get(&BudgetCategory::StarterInfrastructure),
        Some(&-spent)
    );
}
