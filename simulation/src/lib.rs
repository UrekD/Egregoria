#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![warn(clippy::iter_over_hash_type)]

use crate::init::{GSYSTEMS, INIT_FUNCS, SAVELOAD_FUNCS};
use crate::map::{BuildingKind, Map};
use crate::map_dynamic::{transit, Itinerary, ItineraryLeader};
use crate::souls::add_souls_to_empty_buildings;
use crate::utils::resources::{Ref, RefMut, Resources};
use crate::utils::scheduler::RunnableSystem;
use crate::world_command::{CommandOutcomes, WorldCommand};
use common::saveload::{
    self, AtomicWriteError, Bincode, CompressedBincode, Encoder, SlotEntry, SlotMeta,
    SnapshotKind,
};
use common::FastMap;
use derive_more::{From, TryInto};
use economy::BudgetCategory;
use geom::Vec3;
use prototypes::{prototype, ColorsPrototype, ColorsPrototypeID, GameTime, Tick};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::io::{Read, Seek};
use std::path::Path;
use std::ptr::addr_of;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{fs, io};
use utils::rand_provider::RandProvider;
use utils::replay::ReplayJson;
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

pub use common::saveload::{list_slots_with_revisions, save_dir, SlotCamera, SlotId, SnapshotRevision};
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
const VERSION: &str = include_str!("../../VERSION");

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MapPreset {
    #[default]
    Procedural,
    Flat,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct NewGameOptions {
    pub preset: MapPreset,
    pub seed: u64,
    pub terrain_chunks: u16,
    pub starting_money_bucks: u32,
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
        if !(Self::MIN_TERRAIN_CHUNKS..=Self::MAX_TERRAIN_CHUNKS).contains(&self.terrain_chunks) {
            return Err(NewGameError::TerrainChunks(self.terrain_chunks));
        }
        if !(Self::MIN_STARTING_MONEY_BUCKS..=Self::MAX_STARTING_MONEY_BUCKS)
            .contains(&self.starting_money_bucks)
        {
            return Err(NewGameError::StartingMoneyBucks(self.starting_money_bucks));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewGameError {
    TerrainChunks(u16),
    StartingMoneyBucks(u32),
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
}

impl SimulationOptions {
    pub const LEGACY: Self = Self {
        terrain_size: 50,
        save_replay: true,
        preset: MapPreset::Procedural,
        seed: 0,
        starting_money_bucks: 150_000,
    };

    fn new_game_options(self) -> NewGameOptions {
        NewGameOptions {
            preset: self.preset,
            seed: self.seed,
            terrain_chunks: self.terrain_size,
            starting_money_bucks: self.starting_money_bucks,
        }
    }

    fn valid_for_bootstrap(self) -> bool {
        // Legacy scenarios and tests can start with a small or empty map.
        self.new_game_options().is_valid()
            || (self.terrain_size < NewGameOptions::MIN_TERRAIN_CHUNKS
                && self.preset == Self::LEGACY.preset
                && self.seed == Self::LEGACY.seed
                && self.starting_money_bucks == Self::LEGACY.starting_money_bucks)
    }
}

impl Default for SimulationOptions {
    fn default() -> Self {
        Self::LEGACY
    }
}

impl Simulation {
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
        Ok(Self::new_with_options(SimulationOptions {
            terrain_size: options.terrain_chunks,
            save_replay,
            preset: options.preset,
            seed: options.seed,
            starting_money_bucks: options.starting_money_bucks,
        }))
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
        if replay.version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Replay has version {:?}, expected {VERSION:?}", replay.version),
            ));
        }
        let options = replay.bootstrap_options()?;
        let bootstrap_end = replay.bootstrap_end()?;
        let mut sim = Simulation {
            world: Default::default(),
            resources: Default::default(),
        };

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }

        WorldCommand::bootstrap(&mut sim, options).map_err(|reason| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Replay Init: {reason:?}"))
        })?;
        sim.apply_command_batch(
            replay.commands(1..bootstrap_end),
            false,
            BudgetCategory::StarterInfrastructure,
        );
        if replay.has_timeline() {
            sim.record_constructor_end();
        }
        let loader = SimulationReplayLoader::new(replay, Tick(sim.get_tick()), bootstrap_end);
        Ok((sim, loader))
    }

    pub fn new_with_options(opts: SimulationOptions) -> Simulation {
        let mut sim = Simulation {
            world: Default::default(),
            resources: Default::default(),
        };

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }

        let mut start_commands: Vec<(u32, WorldCommand)> =
            common::saveload::JSON::decode(START_COMMANDS.as_bytes()).unwrap();
        WorldCommand::bootstrap(&mut sim, opts).expect("invalid simulation bootstrap options");
        {
            let map = sim.map();
            for (_, command) in &mut start_commands {
                if let WorldCommand::MapMakeMultipleConnections(projects, _) = command {
                    for project in projects.iter_mut().filter(|project| project.is_ground()) {
                        if let Some(height) = map.environment.height(project.pos.xy()) {
                            project.pos.z = height;
                        }
                    }
                }
            }
        }
        sim.apply_command_batch(
            start_commands.iter().map(|(_, cmd)| cmd),
            false,
            BudgetCategory::StarterInfrastructure,
        );
        sim.record_constructor_end();

        sim
    }

    fn record_constructor_end(&self) {
        self.write::<Replay>()
            .record_constructor_end(Tick(self.get_tick()));
    }

    pub fn world_res(&mut self) -> (&mut World, &mut Resources) {
        (&mut self.world, &mut self.resources)
    }

    pub fn world(&self) -> &World {
        &self.world
    }
    pub fn world_mut_unchecked(&mut self) -> &mut World {
        &mut self.world
    }

    pub fn is_equal(&self, other: &Self) -> bool {
        if self.resources.iter().count() != other.resources.iter().count() {
            return false;
        }

        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                let a = (l.save)(self);
                let b = (l.save)(other);

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

    fn apply_command_batch<'a>(
        &mut self,
        commands: impl IntoIterator<Item = &'a WorldCommand>,
        steps_systems: bool,
        construction: BudgetCategory,
    ) {
        let mut commands = commands.into_iter().peekable();
        if commands.peek().is_none() {
            utils::scheduler::drain_commands(self);
            return;
        }
        let topology_generation = self.map().walking_topology_generation;
        let recording_start = self.read::<Replay>().command_count();
        self.write::<CommandOutcomes>().begin_batch();
        for (index, command) in commands.enumerate() {
            let before = self.read::<CommandOutcomes>().last.len();
            command.apply_in_batch(self, construction);
            utils::scheduler::drain_commands(self);
            let mut outcomes = self.write::<CommandOutcomes>();
            if outcomes.last.len() == before {
                outcomes.accepted.push(index);
            }
        }
        if self.map().walking_topology_generation != topology_generation {
            transit::on_topology_changed(self);
        }
        self.write::<Replay>()
            .record_batch(recording_start, steps_systems, Tick(self.get_tick()));
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
                let v = (l.save)(self);
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
        let sim = Self::decode_snapshot(&read_snapshot(file, kind)?)?;
        log::info!("successfully loaded simulation {save_name}");
        Ok(sim)
    }

    pub fn load_from_slot(root: &Path, id: &SlotId) -> Result<Self, SaveLoadError> {
        let paths = saveload::slot_paths(root, id).map_err(SaveLoadError::Io)?;
        let (file, kind, _) = open_snapshot(&paths.zip, &paths.bc)?;
        Self::decode_snapshot(&read_snapshot(file, kind)?)
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
        header.check_version()?;
        let version = header.version;
        let data: SimulationDeser =
            Bincode::decode(payload).map_err(|error| SaveLoadError::Corrupt {
                version: Some(version.clone()),
                reason: error.to_string(),
            })?;
        let sim = data.into_sim().map_err(|reason| SaveLoadError::Corrupt {
            version: Some(version.clone()),
            reason,
        })?;
        if sim
            .resources
            .try_read::<Map>()
            .ok()
            .is_none_or(|map| map.environment.size().0 == 0)
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
            SaveLoadError::SnapshotTooLarge { .. } => None,
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

    /// Encodes a save under the caller's simulation lock so publication can happen after unlocking.
    pub fn encode_slot(&self, revision: SnapshotRevision) -> Result<EncodedSlot, SaveLoadError> {
        let header = SlotSnapshotHeader {
            magic: SLOT_SNAPSHOT_MAGIC,
            version: VERSION.to_owned(),
            revision,
        };
        let snapshot = CompressedBincode::encode(&(header, self)).map_err(SaveLoadError::Encode)?;
        let replay = self.read::<Replay>();
        let replay = replay
            .enabled
            .then(|| ReplayJson::encode_sidecar(&replay, revision))
            .transpose()
            .map_err(AtomicWriteError::PreCommit);
        Ok(EncodedSlot {
            snapshot,
            replay,
            display_name: None,
            camera: None,
            revision,
            game_tick: self.get_tick(),
            population: self.world.humans.len().try_into().unwrap_or(u32::MAX),
            money_cents: self.read::<economy::Government>().money.cents(),
        })
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

    pub fn write_or_default<T: Any + Send + Sync + Default>(&mut self) -> RefMut<T> {
        self.resources.write_or_default::<T>()
    }

    pub fn try_write<T: Any + Send + Sync>(&self) -> Option<RefMut<T>> {
        self.resources.try_write().ok()
    }

    pub fn write<T: Any + Send + Sync>(&self) -> RefMut<T> {
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

    pub fn insert<T: Any + Send + Sync>(&mut self, res: T) {
        self.resources.insert(res);
    }
}

impl Serialize for Simulation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        log::info!("serializing sim state");
        let t = Instant::now();
        let mut m: FastMap<String, Vec<u8>> = FastMap::default();

        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                let v: Vec<u8> = (l.save)(self);
                m.insert(l.name.to_string(), v);
            }
        }

        log::info!("took {}s to serialize resources", t.elapsed().as_secs_f32());

        let v = SimulationSer {
            header: SaveHeader {
                version: VERSION.to_owned(),
            },
            world: &self.world,
            res: m,
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

// A legacy Bincode string length cannot start with 0xff. Keep the normal
// Simulation serialization inside the slot envelope for legacy and network callers.
const SLOT_SNAPSHOT_MAGIC: [u8; 12] = *b"\xffEGREGORIA\0\x01";

#[derive(Serialize, Deserialize)]
struct SlotSnapshotHeader {
    magic: [u8; 12],
    version: String,
    revision: SnapshotRevision,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SnapshotHeader {
    pub version: String,
    pub revision: Option<SnapshotRevision>,
}

impl SnapshotHeader {
    fn check_version(&self) -> Result<(), SaveLoadError> {
        if self.version != VERSION {
            return Err(SaveLoadError::Incompatible {
                version: self.version.clone(),
            });
        }
        Ok(())
    }
}

fn snapshot_header(mut bytes: &[u8]) -> Result<(SnapshotHeader, &[u8]), SaveLoadError> {
    if bytes.starts_with(&SLOT_SNAPSHOT_MAGIC) {
        let header: SlotSnapshotHeader =
            Bincode::decode_reader(&mut bytes).map_err(corrupt_header)?;
        return Ok((
            SnapshotHeader {
                version: header.version,
                revision: Some(header.revision),
            },
            bytes,
        ));
    }
    let SaveHeader { version } = Bincode::decode_prefix(bytes).map_err(corrupt_header)?;
    Ok((
        SnapshotHeader {
            version,
            revision: None,
        },
        bytes,
    ))
}

fn deserialize_save_header<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<SaveHeader, D::Error> {
    let header = SaveHeader::deserialize(deserializer)?;
    let version = &header.version;
    if version != VERSION {
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

pub struct EncodedSlot {
    snapshot: Vec<u8>,
    replay: Result<Option<Vec<u8>>, AtomicWriteError>,
    display_name: Option<String>,
    camera: Option<SlotCamera>,
    revision: SnapshotRevision,
    game_tick: u64,
    population: u32,
    money_cents: i64,
}

impl EncodedSlot {
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
        let paths = saveload::slot_paths(root, id)
            .map_err(|error| SaveLoadError::SnapshotWrite(AtomicWriteError::PreCommit(error)))?;
        saveload::write_atomic(&paths.zip, &self.snapshot).map_err(SaveLoadError::SnapshotWrite)?;
        let replay = self.replay.and_then(|bytes| match bytes {
            Some(bytes) => saveload::write_atomic(&paths.replay, &bytes),
            None => remove_replay(&paths.replay).map_err(AtomicWriteError::PreCommit),
        });
        let meta = SlotMeta {
            display_name: self.display_name.unwrap_or_else(|| id.to_string()),
            game_tick: self.game_tick,
            population: self.population,
            money_cents: self.money_cents,
            saved_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            revision: Some(self.revision.get()),
            camera: self.camera,
        };
        let meta = saveload::write_slot_meta(root, id, &meta);
        match (replay, meta) {
            (Ok(()), Ok(())) => Ok(SavePhase::Complete),
            (replay, meta) => Ok(SavePhase::SidecarsFailed {
                replay: replay.err(),
                meta: meta.err(),
            }),
        }
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
    if header.version != VERSION || header.revision != entry.revision {
        return Err(SaveLoadError::SnapshotChanged);
    }
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

fn read_snapshot(mut file: fs::File, kind: SnapshotKind) -> Result<Vec<u8>, SaveLoadError> {
    const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(SaveLoadError::Io)?;
    if kind == SnapshotKind::Bc {
        return Ok(bytes);
    }
    CompressedBincode::decompress_limited(&bytes, MAX_SNAPSHOT_BYTES).map_err(|error| {
        if error.kind() == io::ErrorKind::FileTooLarge {
            SaveLoadError::SnapshotTooLarge {
                limit: MAX_SNAPSHOT_BYTES,
            }
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
            header.check_version()?;
            Ok(header.revision)
        })
        .ok()
        .flatten();
    ReplayJson::decode_sidecar(bytes, revision).map_err(replay_corrupt)
}

fn corrupt_header(error: io::Error) -> SaveLoadError {
    SaveLoadError::Corrupt {
        version: None,
        reason: error.to_string(),
    }
}

fn replay_corrupt(error: io::Error) -> SaveLoadError {
    SaveLoadError::Corrupt {
        version: None,
        reason: format!("replay sidecar: {error}"),
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
    fn into_sim(self) -> Result<Simulation, String> {
        let t = Instant::now();
        let mut sim = Simulation {
            world: World::default(),
            resources: Resources::default(),
        };

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }

        sim.world = self.world;

        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                let data = self
                    .res
                    .get(l.name)
                    .ok_or_else(|| format!("Missing resource {}", l.name))?;
                (l.load)(&mut sim, data)
                    .map_err(|error| format!("Could not decode resource {}: {error}", l.name))?;
            }
        }

        log::info!(
            "took {}s to deserialize in total",
            t.elapsed().as_secs_f32()
        );

        Ok(sim)
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
        let replay = Replay::default();
        fs::write(&paths.replay, ReplayJson::encode(&replay).unwrap()).unwrap();
        fs::write(&paths.zip, b"corrupt snapshot").unwrap();
        let recovered = Simulation::load_replay_from_slot(&root, &slot);
        assert!(recovered.is_ok(), "{:?}", recovered.err());
        let path_name = root.join(slot.as_str());
        assert!(Simulation::load_replay_from_disk(path_name.to_str().unwrap()).is_some());

        let revision = SnapshotRevision::new(42);
        let write_snapshot = |version: &str| {
            let header = SlotSnapshotHeader {
                magic: SLOT_SNAPSHOT_MAGIC,
                version: version.to_owned(),
                revision,
            };
            fs::write(&paths.zip, CompressedBincode::encode(&header).unwrap()).unwrap();
        };
        write_snapshot("0.0.0");
        assert!(Simulation::load_replay_from_slot(&root, &slot).is_ok());
        fs::write(&paths.replay, ReplayJson::encode_sidecar(&replay, revision).unwrap()).unwrap();
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
