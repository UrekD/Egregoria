use crate::gui::TimeAlways;
use crate::init::{INIT_FUNCS, SAVELOAD_FUNCS};
use crate::session::{CityProgress, WorldReplacement};
use simulation::utils::resources::{RefMutSingle, RefSingle, ResourcesSingleThread};
use simulation::world_command::{WorldCommand, WorldCommands};
use simulation::SimulationReplayLoader;
use std::any::Any;
use std::ptr::addr_of;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

#[derive(Default)]
pub struct UiWorld {
    resources: ResourcesSingleThread,
}

#[derive(Default)]
pub struct SaveLoadState {
    pub startup_message: Option<String>,
    pub pending: Option<WorldReplacement>,
    pub replay: Option<SimulationReplayLoader>,
    pub please_save: bool,
    pub saving_status: Arc<AtomicBool>,
    pub(crate) saved_progress: Option<CityProgress>,
    pub(crate) save_result: Option<Receiver<Option<CityProgress>>>,
}

#[allow(dead_code)]
impl UiWorld {
    pub fn init() -> UiWorld {
        let mut w = Self::with_defaults();
        w.load_from_disk();
        w
    }

    pub(crate) fn with_defaults() -> UiWorld {
        let mut w = UiWorld::default();
        unsafe {
            for s in &*addr_of!(INIT_FUNCS) {
                (s.f)(&mut w);
            }
        }
        w
    }

    pub fn commands(&self) -> RefMutSingle<WorldCommands> {
        self.write::<WorldCommands>()
    }

    pub fn received_commands(&self) -> RefSingle<ReceivedCommands> {
        self.read::<ReceivedCommands>()
    }

    pub fn try_write<T: Any>(&self) -> Option<RefMutSingle<T>> {
        self.resources.try_write().ok()
    }

    pub fn write<T: Any>(&self) -> RefMutSingle<T> {
        self.resources.write()
    }

    pub fn read<T: Any>(&self) -> RefSingle<T> {
        self.resources.read()
    }

    pub fn insert<T: Any>(&mut self, res: T) {
        self.resources.insert(res);
    }

    pub fn time_always(&self) -> f32 {
        self.read::<TimeAlways>().0
    }

    pub fn camera(&self) -> RefSingle<crate::rendering::OrbitCamera> {
        self.read::<crate::rendering::OrbitCamera>()
    }

    pub fn camera_mut(&self) -> RefMutSingle<crate::rendering::OrbitCamera> {
        self.write::<crate::rendering::OrbitCamera>()
    }

    pub fn check_present<T: Any>(&mut self, res: fn() -> T) {
        self.resources.write_or_insert_with(res);
    }

    fn load_from_disk(&mut self) {
        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                (l.load)(self);
            }
        }
    }

    pub fn save_to_disk(&self) {
        unsafe {
            for l in &*addr_of!(SAVELOAD_FUNCS) {
                (l.save)(self);
            }
        }
    }
}

#[derive(Default)]
pub struct ReceivedCommands(WorldCommands);

impl ReceivedCommands {
    #[allow(dead_code)]
    pub fn new(commands: WorldCommands) -> Self {
        Self(commands)
    }
    pub fn iter(&self) -> impl Iterator<Item = &WorldCommand> {
        self.0.iter()
    }
}
