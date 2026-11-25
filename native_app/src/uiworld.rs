use crate::gui::TimeAlways;
use crate::init::{INIT_FUNCS, SAVELOAD_FUNCS};
use crate::session::{CityProgress, SaveInFlight, SaveRequest, WorldJob, WorldReplacement};
pub use crate::sim_thread::TilePurchaseReceipt;
use common::saveload::Encoder;
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
    pub(crate) authoring_blocked: bool,
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

    pub(crate) fn can_author(&self) -> bool {
        !self.read::<SaveLoadState>().authoring_blocked
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

    pub(crate) fn save_lot_brush_async(&self) -> std::sync::mpsc::Receiver<()> {
        use crate::gui::lotbrush::LotBrushResource;
        let brush = self.read::<LotBrushResource>();
        let brush = LotBrushResource {
            kind: brush.kind,
            radius: brush.radius,
        };
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // Settings and bindings are persisted by their editing controls.
            // A late city save must not publish a second snapshot of either.
            common::saveload::JSONPretty::save_silent(&brush, "lot_brush");
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
    commands: WorldCommands,
    placements: Vec<PlacementResult>,
    pub tile_purchases: Vec<TilePurchaseReceipt>,
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
        self.commands.merge(&batch.accepted_commands());
        self.tile_purchases.extend_from_slice(&batch.tile_purchases);
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

#[cfg(test)]
#[test]
fn scale_t1_fix_city_save_keeps_newer_settings_and_bindings() {
    use crate::gui::windows::settings::Settings;
    use crate::inputmap::{Bindings, InputAction};
    use common::saveload::{self, JSONPretty};
    use std::{fs, path::PathBuf, process::Command, time::Duration};

    const SAVE_ROOT: &str = "EGR_SCALE_T1_PREFERENCES_TEST";
    const TEST_NAME: &str = "uiworld::scale_t1_fix_city_save_keeps_newer_settings_and_bindings";
    if let Some(root) = std::env::var_os(SAVE_ROOT) {
        saveload::set_save_dir(PathBuf::from(root)).unwrap();
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let uiw = UiWorld::with_defaults();

        // Disk has newer edits than a city-save snapshot would capture.
        let newer_settings = Settings {
            camera_fov: 71.0,
            ..Settings::default()
        };
        newer_settings.save();
        let mut newer_bindings = Bindings::default();
        newer_bindings.0.remove(&InputAction::OpenDebugMenu);
        JSONPretty::save_atomic(&newer_bindings, "bindings").unwrap();

        uiw.save_lot_brush_async()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(
            JSONPretty::load::<Settings>("settings").unwrap().camera_fov,
            71.0
        );
        assert!(!JSONPretty::load::<Bindings>("bindings")
            .unwrap()
            .0
            .contains_key(&InputAction::OpenDebugMenu));
        assert!(saveload::save_dir().join("lot_brush.json").is_file());
        return;
    }

    let root = std::env::temp_dir().join(format!(
        "egr-scale-t1-preferences-{}-{}",
        std::process::id(),
        crate::session::snapshot_revision().get()
    ));
    fs::create_dir(&root).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST_NAME])
        .env(SAVE_ROOT, &root)
        .status();
    let clean = fs::remove_dir_all(root);
    assert!(result.unwrap().success());
    clean.unwrap();
}
