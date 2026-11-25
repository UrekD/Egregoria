use crate::gui::TimeAlways;
use crate::init::{INIT_FUNCS, SAVELOAD_FUNCS};
use crate::session::{CityProgress, SaveInFlight, SaveRequest, WorldJob, WorldReplacement};
use simulation::utils::resources::{RefMutSingle, RefSingle, ResourcesSingleThread};
use simulation::world_command::{Rejection, WorldCommand, WorldCommands};
use simulation::SimulationReplayLoader;
use std::any::Any;
use std::ptr::addr_of;
use std::time::Instant;

#[derive(Default)]
pub struct UiWorld {
    resources: ResourcesSingleThread,
}

#[derive(Default)]
pub struct SaveLoadState {
    pub startup_message: Option<String>,
    pub pending: Option<WorldReplacement>,
    pub replay: Option<SimulationReplayLoader>,
    pub(crate) replay_ui: Option<ReplayUi>,
    pub(crate) threaded: bool,
    pub(crate) departure_intent: Option<crate::session::Departure>,
    #[cfg(feature = "multiplayer")]
    pub(crate) network_target: Option<crate::network::NetworkTarget>,
    #[cfg(feature = "multiplayer")]
    pub(crate) network_busy: bool,
    #[cfg(feature = "multiplayer")]
    pub(crate) network_cancel: bool,
    pub(crate) achieved_warp: Option<(u32, f64)>,
    pub please_save: bool,
    #[cfg(test)]
    pub saving_status: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub(crate) saved_progress: Option<CityProgress>,
    pub(crate) save_result: Option<SaveInFlight>,
    pub(crate) pending_save: Option<SaveRequest>,
    pub(crate) named_save: Option<std::sync::mpsc::Receiver<std::io::Result<SaveRequest>>>,
    pub(crate) autosave_index: u8,
    pub(crate) world_job: Option<WorldJob>,
    pub(crate) world_error: Option<String>,
    pub(crate) delete_slot: Option<common::saveload::SlotId>,
    pub(crate) exit_requested: bool,
    pub(crate) notice: Option<(Instant, String)>,
}

pub(crate) struct ReplayUi {
    pub progress: crate::sim_thread::ReplayProgress,
    pub speed: usize,
    pub control: Option<crate::sim_thread::ReplayControl>,
}

impl SaveLoadState {
    pub(crate) fn saving(&self) -> bool {
        self.named_save.is_some() || self.pending_save.is_some() || self.save_result.is_some()
    }

    pub(crate) fn is_replaying(&self) -> bool {
        self.replay_ui.is_some() || self.replay.is_some()
    }
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

    pub fn try_read<T: Any>(&self) -> Option<RefSingle<T>> {
        self.resources.try_read().ok()
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

    pub(crate) fn save_to_disk_async(&self) -> std::sync::mpsc::Receiver<()> {
        let settings = *self.read::<crate::gui::windows::settings::Settings>();
        let bindings = self.read::<crate::inputmap::Bindings>().0.clone();
        let (kind, radius) = {
            let brush = self.read::<crate::gui::lotbrush::LotBrushResource>();
            (brush.kind, brush.radius)
        };
        #[cfg(feature = "multiplayer")]
        let network_info =
            (*self.read::<crate::gui::windows::network::NetworkConnectionInfo>()).clone();
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut uiw = UiWorld::with_defaults();
            uiw.insert(settings);
            #[cfg(feature = "multiplayer")]
            uiw.insert(network_info);
            uiw.insert(crate::inputmap::Bindings(bindings));
            uiw.insert(crate::gui::lotbrush::LotBrushResource { kind, radius });
            uiw.save_to_disk();
            let _ = done.send(());
        });
        result
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
pub struct ReceivedCommands {
    batches: usize,
    commands: WorldCommands,
    placements: Vec<PlacementResult>,
    pub(crate) rejections: Vec<(u64, Vec<(usize, &'static str, Rejection)>)>,
}

struct PlacementResult {
    input: WorldCommand,
    index: usize,
    outcome: Result<(), Option<Rejection>>,
}

impl ReceivedCommands {
    #[allow(dead_code)]
    pub fn new(commands: WorldCommands) -> Self {
        Self {
            commands,
            ..Self::default()
        }
    }

    pub(crate) fn receive(&mut self, batch: &crate::sim_thread::AppliedBatch) {
        self.batches += 1;
        self.commands.merge(&batch.accepted_commands());
        self.placements
            .extend(
                batch
                    .commands
                    .as_ref()
                    .iter()
                    .enumerate()
                    .filter_map(|(index, input)| {
                        if !matches!(input, WorldCommand::MapBuildSpecialBuilding { .. }) {
                            return None;
                        }
                        let outcome = if batch.accepted_indices.contains(&index) {
                            Ok(())
                        } else {
                            Err(batch
                                .rejections
                                .iter()
                                .find(|(i, _, _)| *i == index)
                                .map(|&(_, _, reason)| reason))
                        };
                        Some(PlacementResult {
                            input: input.clone(),
                            index,
                            outcome,
                        })
                    }),
            );
        self.rejections
            .push((batch.outcome_sequence, batch.rejections.clone()));
    }

    pub(crate) fn is_full(&self) -> bool {
        self.batches >= 8
    }

    pub(crate) fn placement(
        &self,
        index: usize,
        matches: impl Fn(&WorldCommand) -> bool,
    ) -> Option<Result<(), Option<Rejection>>> {
        self.placements
            .iter()
            .find(|result| result.index == index && matches(&result.input))
            .map(|result| result.outcome)
    }

    pub fn iter(&self) -> impl Iterator<Item = &WorldCommand> {
        self.commands.iter()
    }
}
