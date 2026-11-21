#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![warn(clippy::iter_over_hash_type)]

use crate::init::{GSYSTEMS, INIT_FUNCS, SAVELOAD_FUNCS};
use crate::map::{BuildingKind, Map};
use crate::map_dynamic::{Itinerary, ItineraryLeader};
use crate::souls::add_souls_to_empty_buildings;
use crate::utils::resources::{Ref, RefMut, Resources};
use crate::utils::scheduler::RunnableSystem;
use crate::world_command::WorldCommand::Init;
use crate::world_command::{CommandOutcomes, WorldCommand};
use common::saveload::Encoder;
use common::FastMap;
use derive_more::{From, TryInto};
use geom::Vec3;
use prototypes::{prototype, ColorsPrototype, ColorsPrototypeID, GameTime, Tick};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::hash::Hash;
use std::ptr::addr_of;
use std::time::{Duration, Instant};
use utils::rand_provider::RandProvider;
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
const VERSION: &str = include_str!("../../VERSION");

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct SimulationOptions {
    pub terrain_size: u16,
    pub save_replay: bool,
}

impl Default for SimulationOptions {
    fn default() -> Self {
        SimulationOptions {
            terrain_size: 50,
            save_replay: true,
        }
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
        Self::new_with_options(SimulationOptions {
            terrain_size: if gen_terrain { 50 } else { 0 },
            ..Default::default()
        })
    }

    pub fn from_replay(replay: Replay) -> (Simulation, SimulationReplayLoader) {
        let mut sim = Simulation {
            world: Default::default(),
            resources: Default::default(),
        };

        info!("Seed is {}", RNG_SEED);

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }

        (
            sim,
            SimulationReplayLoader {
                replay,
                pastt: Tick::default(),
                idx: 0,
                speed: 1,
                advance_n_ticks: 0,
            },
        )
    }

    pub fn new_with_options(opts: SimulationOptions) -> Simulation {
        let mut sim = Simulation {
            world: Default::default(),
            resources: Default::default(),
        };

        info!("Seed is {}", RNG_SEED);
        info!("{:?}", opts);

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }

        let start_commands: Vec<(u32, WorldCommand)> =
            common::saveload::JSON::decode(START_COMMANDS.as_bytes()).unwrap();
        let init = Init(Box::new(opts));
        sim.apply_commands(std::iter::once(&init).chain(start_commands.iter().map(|(_, cmd)| cmd)));

        sim
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
            self.apply_commands(commands);
        }

        {
            let mut time = self.write::<GameTime>();
            *time = GameTime::new(Tick(time.tick.0 + 1));
        }

        game_schedule.execute(self);

        self.resources.write::<Replay>().last_tick_recorded =
            self.resources.read::<GameTime>().tick;

        t.elapsed()
    }

    /// Applies a command batch without advancing the simulation clock.
    pub fn apply_commands<'a>(&mut self, commands: impl IntoIterator<Item = &'a WorldCommand>) {
        let mut commands = commands.into_iter().peekable();
        if commands.peek().is_none() {
            utils::scheduler::drain_commands(self);
            return;
        }
        self.write::<CommandOutcomes>().last.clear();
        for command in commands {
            command.apply_in_batch(self);
            utils::scheduler::drain_commands(self);
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
                let v = (l.save)(self);
                hashes.insert(l.name.to_string(), common::hash_u64(&*v));
            }
        }

        hashes
    }

    pub fn load_replay_from_disk(save_name: &str) -> Option<Replay> {
        let path = format!("{save_name}_replay");
        let replay: Replay = match common::saveload::JSON::load(&path) {
            Ok(replay) => replay,
            Err(error) => {
                log::error!("Could not load replay {path}: {error}");
                return None;
            }
        };
        if replay.version != VERSION {
            log::error!(
                "Replay {path} has version {:?}, expected {VERSION:?}",
                replay.version
            );
            return None;
        }
        Some(replay)
    }

    pub fn load_from_disk(save_name: &str) -> Option<Self> {
        let sim: Simulation = match common::saveload::CompressedBincode::load(save_name) {
            Ok(sim) => sim,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => {
                log::error!("Could not load simulation {save_name}: {error}");
                return None;
            }
        };
        if sim.resources.try_read::<Map>().ok()?.environment.size().0 == 0 {
            return None;
        }
        Some(sim)
    }

    pub fn save_to_disk(&self, save_name: &str) {
        common::saveload::CompressedBincode::save(&self, save_name);
        let rep = self.resources.read::<Replay>();
        if rep.enabled {
            common::saveload::JSONPretty::save(&*rep, &format!("{save_name}_replay"));
        }
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
            world: &self.world,
            version: VERSION.to_string(),
            res: m,
        }
        .serialize(serializer);
        log::info!("took {}s to serialize in total", t.elapsed().as_secs_f32());
        v
    }
}

#[derive(Serialize)]
struct SimulationSer<'a> {
    world: &'a World,
    version: String,
    res: FastMap<String, Vec<u8>>,
}

#[derive(Deserialize)]
struct SimulationDeser {
    world: World,
    version: String,
    res: FastMap<String, Vec<u8>>,
}

impl<'de> Deserialize<'de> for Simulation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        log::info!("deserializing sim state");
        let t = Instant::now();

        let simdeser = <SimulationDeser as Deserialize>::deserialize(deserializer)?;

        log::info!(
            "took {}s to deserialize base deser",
            t.elapsed().as_secs_f32()
        );

        if simdeser.version != VERSION {
            return Err(serde::de::Error::custom(format!(
                "Save has version {:?}, expected {VERSION:?}",
                simdeser.version
            )));
        }

        let mut sim = Self {
            world: World::default(),
            resources: Resources::default(),
        };

        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut sim);
            }
        }

        sim.world = simdeser.world;

        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                let data = simdeser.res.get(l.name).ok_or_else(|| {
                    serde::de::Error::custom(format!("Missing resource {}", l.name))
                })?;
                (l.load)(&mut sim, data).map_err(|error| {
                    serde::de::Error::custom(format!(
                        "Could not decode resource {}: {error}",
                        l.name
                    ))
                })?;
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
