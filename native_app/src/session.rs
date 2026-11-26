use std::collections::{hash_map::RandomState, BTreeMap};
use std::hash::BuildHasher;
use std::io;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use common::saveload::{self, SlotCamera, SlotEntry, SlotId, SnapshotRevision};
use serde::Deserialize;
#[cfg(test)]
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::CommandOutcomes;
use simulation::{SaveLoadError, Simulation, SimulationReplayLoader};

#[cfg(test)]
use crate::game_loop::VERSION;
use crate::gui::{ExitState, GuiState};
use crate::inputmap::InputMap;
use crate::network::NetworkState;
#[cfg(test)]
use crate::uiworld::ReceivedCommands;
use crate::uiworld::{SaveLoadState, UiWorld};

#[path = "save_coordinator.rs"]
pub(crate) mod save_coordinator;

#[path = "gui/hud/windows/main_menu.rs"]
mod main_menu;
#[path = "gui/hud/windows/new_game.rs"]
mod new_game;
#[path = "gui/hud/windows/pause.rs"]
mod pause;
#[path = "gui/hud/windows/save_as.rs"]
mod save_as;

pub(crate) use main_menu::{main_menu_buttons, render_loading, render_session, render_status};
pub(crate) use pause::{render_departure, render_modal};
pub(crate) use save_as::open_save_as;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum SessionScreen {
    MainMenu,
    NewGame,
    LoadGame,
    #[default]
    Playing,
}

pub enum WorldReplacement {
    Snapshot(Simulation, SlotId),
    SnapshotWithCamera(Simulation, SlotId, SlotCamera),
    NewGame(Simulation, SlotId),
    ReplayDebug(Simulation, SimulationReplayLoader),
    #[cfg(feature = "multiplayer")]
    Network(Simulation),
}

impl WorldReplacement {
    pub(crate) fn from_slot(sim: Simulation, entry: &SlotEntry) -> Self {
        match slot_camera(entry) {
            Some(camera) => Self::SnapshotWithCamera(sim, entry.id.clone(), camera),
            None => Self::Snapshot(sim, entry.id.clone()),
        }
    }

    pub(crate) fn validate_native_size(&self) -> Result<(), String> {
        let sim = match self {
            Self::Snapshot(sim, _)
            | Self::SnapshotWithCamera(sim, ..)
            | Self::NewGame(sim, _)
            | Self::ReplayDebug(sim, _) => sim,
            #[cfg(feature = "multiplayer")]
            Self::Network(sim) => sim,
        };
        validate_native_world(sim)
    }
}

fn slot_camera(entry: &SlotEntry) -> Option<SlotCamera> {
    entry.meta.as_ref().and_then(|meta| meta.camera)
}

#[derive(Default)]
pub struct SessionState {
    pub active_slot: Option<SlotId>,
    recovery_slot: Option<SlotId>,
    dormant_snapshot: Option<DormantSnapshot>,
    pub generation: u64,
    pub(crate) interaction_blocked: bool,
    pub(crate) departure: Option<PendingDeparture>,
    pub(crate) city_name: String,
    pub(crate) paused: bool,
    pub(crate) load_back: SessionScreen,
    pub(crate) refresh_slots: bool,
    pub(crate) slots: DiscoveredSlots,
    pub(crate) failed_snapshots: FailedSnapshots,
    slot_scan: Option<Receiver<io::Result<DiscoveredSlots>>>,
    pub(crate) starting_up: bool,
    pub(crate) world_job_id: u64,
    pub(crate) replay_requested: bool,
    pub(crate) slot_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotCompatibility {
    Compatible,
    Incompatible,
    IncompatibleRegistry,
    Unreadable,
}

impl SlotCompatibility {
    pub(crate) fn unavailable_reason(self) -> Option<&'static str> {
        match self {
            Self::Compatible => None,
            Self::Incompatible => Some("incompatible version"),
            Self::IncompatibleRegistry => Some("incompatible prototype registry"),
            Self::Unreadable => Some("unreadable snapshot"),
        }
    }
}

pub(crate) type DiscoveredSlots = Vec<(SlotEntry, SlotCompatibility)>;

#[derive(Default)]
pub(crate) struct FailedSnapshots(BTreeMap<SlotId, SlotEntry>);

enum DormantSnapshot {
    Loaded(SlotEntry),
    Published(SnapshotRevision),
}

impl FailedSnapshots {
    pub(crate) fn record(&mut self, entry: &SlotEntry) {
        self.0.insert(entry.id.clone(), entry.clone());
    }

    pub(crate) fn apply(&mut self, slots: &mut DiscoveredSlots) {
        let mut previous = std::mem::take(&mut self.0);
        for (entry, status) in slots {
            if let Some(failed) = previous.remove(&entry.id) {
                if failed.same_snapshot_as(entry) {
                    *status = SlotCompatibility::Unreadable;
                    self.0.insert(entry.id.clone(), failed);
                }
            }
        }
    }
}

impl SessionState {
    pub(crate) fn snapshot_slot(&self) -> Option<&SlotId> {
        self.active_slot.as_ref().or(self.recovery_slot.as_ref())
    }

    pub(crate) fn set_snapshot_slot(&mut self, slot: Option<SlotId>) {
        self.dormant_snapshot = None;
        if slot.as_ref().is_some_and(SlotId::is_autosave) {
            self.active_slot = None;
            self.recovery_slot = slot;
        } else {
            self.active_slot = slot;
            self.recovery_slot = None;
        }
    }

    pub(crate) fn remember_loaded_snapshot(&mut self, entry: SlotEntry) {
        self.dormant_snapshot = Some(DormantSnapshot::Loaded(entry));
    }

    pub(crate) fn remember_published_snapshot(&mut self, revision: SnapshotRevision) {
        self.dormant_snapshot = Some(DormantSnapshot::Published(revision));
    }

    pub(crate) fn scanning_slots(&self) -> bool {
        self.starting_up || self.refresh_slots || self.slot_scan.is_some()
    }

    pub(crate) fn mark_unreadable(&mut self, entry: &SlotEntry) {
        self.failed_snapshots.record(entry);
        for (candidate, status) in &mut self.slots {
            if candidate.same_snapshot_as(entry) {
                *status = SlotCompatibility::Unreadable;
            }
        }
        self.refresh_slots = true;
    }

    pub(crate) fn update_slot_catalog(&mut self, result: io::Result<DiscoveredSlots>) {
        match result {
            Ok(mut slots) => {
                self.failed_snapshots.apply(&mut slots);
                self.slots = slots;
                self.slot_error = None;
            }
            Err(error) => {
                self.slots.clear();
                self.slot_error = Some(format!("Could not list saves. {error}"));
            }
        }
    }

    pub(crate) fn poll_slot_scan(&mut self) {
        if self.starting_up {
            return;
        }
        if let Some(scan) = &self.slot_scan {
            let result = match scan.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => Err(io::Error::other("The save scan stopped.")),
            };
            self.slot_scan = None;
            if !self.refresh_slots {
                self.update_slot_catalog(result);
            }
        }
        if std::mem::take(&mut self.refresh_slots) {
            let (sender, result) = mpsc::channel();
            self.slot_scan = Some(result);
            std::thread::spawn(move || {
                let _ = sender.send(discover_slots(saveload::save_dir()));
            });
        }
    }
}

pub(crate) fn discover_slots(root: &Path) -> io::Result<DiscoveredSlots> {
    let mut compatibility = BTreeMap::new();
    let slots = saveload::list_slots_with_revisions(root, |path, kind| {
        let (status, revision) = match Simulation::read_slot_snapshot_header(path, kind) {
            Ok(header) => {
                let status = match header.check_compatibility() {
                    Ok(()) => SlotCompatibility::Compatible,
                    Err(SaveLoadError::PrototypeRegistry) => {
                        SlotCompatibility::IncompatibleRegistry
                    }
                    Err(_) => SlotCompatibility::Incompatible,
                };
                (status, header.revision)
            }
            Err(_) => (SlotCompatibility::Unreadable, None),
        };
        compatibility.insert(path.to_owned(), status);
        revision
    })?;
    Ok(slots
        .into_iter()
        .map(|slot| {
            let status = compatibility
                .remove(slot.snapshot_path())
                .unwrap_or(SlotCompatibility::Unreadable);
            (slot, status)
        })
        .collect())
}

fn continue_slot<T>(slots: &[(T, SlotCompatibility)]) -> Option<&T> {
    slots
        .iter()
        .find_map(|(slot, status)| (*status == SlotCompatibility::Compatible).then_some(slot))
}

pub(crate) fn continue_entry(uiw: &UiWorld) -> Option<SlotEntry> {
    if !crate::network::is_singleplayer(uiw) {
        return None;
    }
    continue_slot(&uiw.read::<SessionState>().slots).cloned()
}

fn can_resume_dormant(uiw: &UiWorld, sim: Option<&Simulation>, entry: &SlotEntry) -> bool {
    let session = uiw.read::<SessionState>();
    session.snapshot_slot() == Some(&entry.id)
        && match &session.dormant_snapshot {
            Some(DormantSnapshot::Loaded(loaded)) => loaded.same_snapshot_as(entry),
            Some(DormantSnapshot::Published(revision)) => entry.revision == Some(*revision),
            None => false,
        }
        && sim.is_some_and(|sim| {
            uiw.read::<SaveLoadState>().saved_progress == Some(CityProgress::capture(sim))
        })
}

pub(crate) fn continue_city(uiw: &UiWorld, sim: Option<&Simulation>, entry: SlotEntry) {
    let resume_dormant = can_resume_dormant(uiw, sim, &entry);
    start_snapshot_job(uiw, entry, resume_dormant);
}

pub(crate) fn restore_dormant_camera(uiw: &UiWorld, sim: &Simulation, entry: &SlotEntry) {
    if let Some(camera) = slot_camera(entry) {
        reset_world_camera(uiw, sim, Some(camera));
    }
}

pub(crate) fn display_name(entry: &SlotEntry) -> &str {
    entry.meta.as_ref().map_or_else(
        || {
            if entry.id.as_str() == "world" {
                "World (legacy)"
            } else {
                entry.id.as_str()
            }
        },
        |meta| meta.display_name.as_str(),
    )
}

fn city_slot(name: &str, existing: &[SlotId]) -> SlotId {
    SlotId::from_display_name(name, existing)
}

fn known_slot_ids(uiw: &UiWorld) -> Vec<SlotId> {
    let state = uiw.read::<SessionState>();
    state
        .slots
        .iter()
        .map(|(slot, _)| slot.id.clone())
        .chain(state.active_slot.clone())
        .collect()
}

fn choose_city_slot(root: &Path, name: &str, mut existing: Vec<SlotId>) -> io::Result<SlotId> {
    loop {
        let slot = city_slot(name, &existing);
        match saveload::slot_paths(root, &slot) {
            Ok(paths) => {
                let mut occupied = false;
                for path in [paths.zip, paths.bc, paths.meta, paths.replay] {
                    match path.symlink_metadata() {
                        Ok(_) => occupied = true,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                }
                if !occupied {
                    return Ok(slot);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
            Err(error) => return Err(error),
        }
        existing.push(slot);
    }
}

pub(crate) fn random_seed() -> u64 {
    RandomState::new().hash_one(0u8).max(1)
}

pub(crate) fn snapshot_revision() -> SnapshotRevision {
    let entropy = RandomState::new();
    SnapshotRevision::new(
        (u128::from(entropy.hash_one(0u8)) << 64) | u128::from(entropy.hash_one(1u8)),
    )
}

#[derive(Default)]
pub(crate) struct SessionUi {
    settings_open: bool,
    new_game: new_game::NewGameForm,
    save_as: Option<save_as::SaveAsForm>,
    pub(crate) delete: Option<SlotId>,
}

pub(crate) struct PreparedWorld {
    pub replacement: WorldReplacement,
    pub name: String,
}

pub(crate) enum WorldJobKind {
    NewGame,
    Snapshot(SlotEntry),
    Replay,
}

impl WorldJobKind {
    fn label(&self) -> &'static str {
        match self {
            Self::NewGame => "Generating…",
            Self::Snapshot(_) => "Loading…",
            Self::Replay => "Loading replay…",
        }
    }
}

pub(crate) struct WorldJob {
    pub id: u64,
    pub kind: WorldJobKind,
    pub presented: bool,
    pub result: Receiver<Result<WorldJobOutcome, WorldJobError>>,
}

pub(crate) enum WorldJobOutcome {
    Replace(PreparedWorld),
    ResumeDormant,
}

#[derive(Debug)]
pub(crate) enum WorldJobError {
    SnapshotChanged,
    Failed(String),
}

impl From<SaveLoadError> for WorldJobError {
    fn from(error: SaveLoadError) -> Self {
        match error {
            SaveLoadError::SnapshotChanged => Self::SnapshotChanged,
            _ => Self::Failed(error.to_string()),
        }
    }
}

fn start_world_job(
    uiw: &UiWorld,
    kind: WorldJobKind,
    prepare: impl FnOnce() -> Result<PreparedWorld, String> + Send + 'static,
) {
    spawn_world_job(uiw, kind, move || {
        prepare()
            .map(WorldJobOutcome::Replace)
            .map_err(WorldJobError::Failed)
    });
}

fn spawn_world_job(
    uiw: &UiWorld,
    kind: WorldJobKind,
    prepare: impl FnOnce() -> Result<WorldJobOutcome, WorldJobError> + Send + 'static,
) {
    let mut io = uiw.write::<SaveLoadState>();
    if !crate::network::is_singleplayer(uiw) {
        return;
    }
    if io.world_job.is_some() {
        drop(io);
        notify(uiw, "Finishing the previous city job…");
        return;
    }
    let mut session = uiw.write::<SessionState>();
    session.world_job_id += 1;
    let (sender, result) = mpsc::channel();
    io.world_job = Some(WorldJob {
        id: session.world_job_id,
        kind,
        presented: false,
        result,
    });
    io.world_error = None;
    std::thread::spawn(move || {
        let _ = sender.send(prepare());
    });
}

pub(crate) fn load_slot(uiw: &UiWorld, entry: SlotEntry) {
    start_snapshot_job(uiw, entry, false);
}

fn start_snapshot_job(uiw: &UiWorld, entry: SlotEntry, resume_dormant: bool) {
    spawn_world_job(uiw, WorldJobKind::Snapshot(entry.clone()), move || {
        let root = saveload::save_dir();
        if resume_dormant {
            Simulation::validate_listed_snapshot(root, &entry)?;
            return Ok(WorldJobOutcome::ResumeDormant);
        }
        let name = display_name(&entry).to_owned();
        let simulation = Simulation::load_listed_snapshot(root, &entry)?;
        Ok(WorldJobOutcome::Replace(PreparedWorld {
            replacement: WorldReplacement::from_slot(simulation, &entry),
            name,
        }))
    });
}

pub(crate) fn world_job_active(uiw: &UiWorld) -> bool {
    let id = uiw.read::<SessionState>().world_job_id;
    uiw.read::<SaveLoadState>()
        .world_job
        .as_ref()
        .is_some_and(|job| job.id == id)
}

pub(crate) fn cancel_generation(uiw: &UiWorld) -> bool {
    let mut session = uiw.write::<SessionState>();
    let mut io = uiw.write::<SaveLoadState>();
    let cancellable = io.world_job.as_ref().is_some_and(|job| {
        job.id == session.world_job_id && matches!(job.kind, WorldJobKind::NewGame)
    });
    if cancellable {
        io.world_job = None;
        session.world_job_id += 1;
        drop(io);
        drop(session);
        reset_interaction(uiw);
    }
    cancellable
}

pub(crate) fn modal(uiw: &UiWorld) -> bool {
    #[cfg(feature = "multiplayer")]
    if uiw.read::<SaveLoadState>().network_busy {
        return true;
    }
    let session = uiw.read::<SessionState>();
    session.paused
        || uiw.read::<SaveLoadState>().departure_intent.is_some()
        || session.departure.is_some()
        || session.replay_requested
        || uiw.read::<SessionUi>().save_as.is_some()
        || world_job_active(uiw)
}

pub(crate) fn reset_interaction(uiw: &UiWorld) {
    uiw.write::<InputMap>().reset_world_input();
    uiw.write::<NetworkState>().reset_timestep();
}

pub(crate) fn set_paused(uiw: &UiWorld, paused: bool) {
    let was_paused = std::mem::replace(&mut uiw.write::<SessionState>().paused, paused);
    if was_paused && !paused {
        uiw.write::<GuiState>().last_save = Instant::now();
    }
    reset_interaction(uiw);
}

/// Close the current interaction before falling back to the pause menu.
pub(crate) fn close_modal(uiw: &UiWorld) {
    #[cfg(feature = "multiplayer")]
    if uiw.read::<SaveLoadState>().network_busy {
        uiw.write::<SaveLoadState>().network_cancel = true;
        return;
    }
    if uiw
        .write::<SaveLoadState>()
        .departure_intent
        .take()
        .is_some()
    {
        reset_interaction(uiw);
        return;
    }
    let mut session = uiw.write::<SessionState>();
    if let Some(departure) = session.departure {
        if matches!(
            departure.phase,
            DeparturePhase::Confirm | DeparturePhase::SaveFailed
        ) {
            session.departure = None;
            drop(session);
            reset_interaction(uiw);
        }
        return;
    }
    drop(session);
    if world_job_active(uiw) {
        cancel_generation(uiw);
        return;
    }
    let closed_session_window = {
        let mut ui = uiw.write::<SessionUi>();
        ui.save_as.take().is_some()
            || ui.delete.take().is_some()
            || std::mem::take(&mut ui.settings_open)
    };
    if closed_session_window {
        reset_interaction(uiw);
    } else if playing(uiw) && !uiw.read::<SaveLoadState>().is_replaying() {
        if uiw.read::<SessionState>().paused {
            set_paused(uiw, false);
        } else if close_world_interaction(uiw) {
            uiw.write::<InputMap>().require_fresh_pointer_press();
        } else {
            set_paused(uiw, true);
        }
    } else if uiw
        .write::<SaveLoadState>()
        .startup_message
        .take()
        .is_some()
    {
        reset_interaction(uiw);
    } else if matches!(
        *uiw.read::<SessionScreen>(),
        SessionScreen::LoadGame | SessionScreen::NewGame
    ) {
        back(uiw);
    }
}

fn close_world_interaction(uiw: &UiWorld) -> bool {
    if crate::gui::cancel_interaction(uiw) || crate::gui::chat::close(uiw) {
        return true;
    }

    let mut gui = uiw.write::<GuiState>();
    gui.windows.close_open_window()
        || std::mem::take(&mut gui.debug_window)
        || uiw
            .write::<SaveLoadState>()
            .startup_message
            .take()
            .is_some()
}

pub(crate) fn back(uiw: &UiWorld) {
    let (destination, paused) = if *uiw.read::<SessionScreen>() == SessionScreen::LoadGame {
        let state = uiw.read::<SessionState>();
        (state.load_back, state.paused)
    } else {
        (SessionScreen::MainMenu, false)
    };
    set_screen(uiw, destination);
    if destination == SessionScreen::Playing {
        set_paused(uiw, paused);
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SaveKind {
    Manual,
    SaveAs,
    Autosave,
    Departure,
}

#[derive(Clone)]
pub(crate) struct SaveRequest {
    pub slot: SlotId,
    pub name: String,
    pub kind: SaveKind,
}

pub(crate) struct SaveInFlight {
    pub id: u64,
    pub request: SaveRequest,
    pub revision: SnapshotRevision,
    pub result: Receiver<Result<(CityProgress, simulation::SavePhase), String>>,
}

pub(crate) fn request_save(uiw: &UiWorld, target: Option<(SlotId, String)>) {
    if !can_save(uiw) {
        return;
    }
    let mut io = uiw.write::<SaveLoadState>();
    if io.saving() {
        return;
    }
    let request = match target {
        Some((slot, _)) if slot.is_autosave() => return,
        Some((slot, name)) => SaveRequest {
            slot,
            name,
            kind: SaveKind::SaveAs,
        },
        None => {
            let state = uiw.read::<SessionState>();
            let Some(slot) = state.active_slot.clone() else {
                drop(state);
                drop(io);
                open_save_as(uiw);
                return;
            };
            SaveRequest {
                slot,
                name: state.city_name.clone(),
                kind: SaveKind::Manual,
            }
        }
    };
    io.pending_save = Some(request);
}

pub(crate) fn request_named_save(uiw: &UiWorld, name: String) {
    if !can_save(uiw) {
        return;
    }
    let mut io = uiw.write::<SaveLoadState>();
    if io.saving() {
        return;
    }
    let known = known_slot_ids(uiw);
    let (sender, receiver) = mpsc::channel();
    io.named_save = Some(receiver);
    std::thread::spawn(move || {
        let result = choose_city_slot(saveload::save_dir(), &name, known).map(|slot| SaveRequest {
            slot,
            name,
            kind: SaveKind::SaveAs,
        });
        let _ = sender.send(result);
    });
}

pub(crate) fn poll_named_save(uiw: &UiWorld) {
    let mut io = uiw.write::<SaveLoadState>();
    let Some(receiver) = &io.named_save else {
        return;
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => {
            Err(io::Error::other("The save request stopped unexpectedly."))
        }
    };
    io.named_save = None;
    match result {
        Ok(request) => io.pending_save = Some(request),
        Err(error) => {
            drop(io);
            notify(uiw, format!("Could not choose a save slot. {error}"));
        }
    }
}

pub(crate) fn departure_save_plan(
    uiw: &UiWorld,
) -> impl FnOnce(&Path) -> io::Result<SaveRequest> + Send + 'static {
    let (slot, name) = {
        let state = uiw.read::<SessionState>();
        let name = if state.active_slot.is_some() {
            state.city_name.clone()
        } else {
            state.city_name.trim_end_matches(" (autosave)").to_owned()
        };
        (state.active_slot.clone(), name)
    };
    let known = known_slot_ids(uiw);
    move |root| {
        let slot = match slot {
            Some(slot) => slot,
            None => choose_city_slot(root, &name, known)?,
        };
        Ok(SaveRequest {
            slot,
            name,
            kind: SaveKind::Departure,
        })
    }
}

#[cfg(test)]
fn departure_save_request(uiw: &UiWorld, root: &Path) -> io::Result<SaveRequest> {
    departure_save_plan(uiw)(root)
}

pub(crate) fn notify(uiw: &UiWorld, message: impl Into<String>) {
    uiw.write::<SaveLoadState>().notice =
        Some((Instant::now() + Duration::from_secs(7), message.into()));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CityProgress {
    tick: u64,
    batch_seq: u64,
}

impl CityProgress {
    pub(crate) fn capture(sim: &Simulation) -> Self {
        Self {
            tick: sim.get_tick(),
            batch_seq: sim.read::<CommandOutcomes>().batch_seq(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Departure {
    Screen(SessionScreen),
    ReplayDebug,
    #[cfg(feature = "multiplayer")]
    Network(crate::network::NetworkTarget),
    Exit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeparturePhase {
    Confirm,
    SaveRequested,
    Saving(u64),
    SaveFailed,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingDeparture {
    pub destination: Departure,
    pub phase: DeparturePhase,
}

pub type WorldResetHook = fn(&UiWorld, u64);

#[derive(Default)]
pub struct WorldResetHooks(Vec<WorldResetHook>);

impl WorldResetHooks {
    pub fn register(&mut self, reset: WorldResetHook) {
        self.0.push(reset);
    }
}

pub fn playing(uiw: &UiWorld) -> bool {
    *uiw.read::<SessionScreen>() == SessionScreen::Playing
}

pub(crate) fn view_input(uiw: &UiWorld) -> bool {
    playing(uiw) && !uiw.read::<SessionState>().interaction_blocked
}

pub fn world_interaction(uiw: &UiWorld) -> bool {
    view_input(uiw) && !modal(uiw) && !uiw.read::<SaveLoadState>().is_replaying()
}

pub fn can_save(uiw: &UiWorld) -> bool {
    playing(uiw)
        && !uiw.read::<SessionState>().interaction_blocked
        && uiw.read::<SessionState>().snapshot_slot().is_some()
        && !uiw.read::<SaveLoadState>().is_replaying()
}

pub(crate) fn request_departure(uiw: &UiWorld, sim: &Simulation, destination: Departure) {
    if matches!(destination, Departure::ReplayDebug) && sim.read::<simulation::Replay>().migrated_city() {
        notify(uiw, "Replay is unavailable for this migrated city.");
        return;
    }

    if !playing(uiw) || uiw.read::<SessionState>().departure.is_some() {
        return;
    }
    if !matches!(destination, Departure::Exit) && !crate::network::is_singleplayer(uiw) {
        return;
    }
    if uiw.read::<SaveLoadState>().threaded {
        uiw.write::<SaveLoadState>().departure_intent = Some(destination);
    } else {
        decide_departure(uiw, CityProgress::capture(sim), destination);
    }
}

pub(crate) fn decide_departure(uiw: &UiWorld, progress: CityProgress, destination: Departure) {
    let io = uiw.read::<SaveLoadState>();
    let dirty =
        !io.is_replaying() && (io.saved_progress != Some(progress) || !uiw.commands().is_empty());
    drop(io);
    if dirty {
        reset_interaction(uiw);
        uiw.write::<SessionState>().departure = Some(PendingDeparture {
            destination,
            phase: DeparturePhase::Confirm,
        });
    } else {
        finish_departure(uiw, destination);
    }
}

pub(crate) fn finish_departure(uiw: &UiWorld, destination: Departure) {
    match destination {
        Departure::Screen(screen) => {
            set_screen(uiw, screen);
        }
        Departure::ReplayDebug => {
            let mut state = uiw.write::<SessionState>();
            state.departure = None;
            let slot = state
                .snapshot_slot()
                .cloned()
                .unwrap_or_else(SlotId::legacy);
            drop(state);
            reset_interaction(uiw);
            start_world_job(uiw, WorldJobKind::Replay, move || {
                let replay = Simulation::load_replay_from_slot(saveload::save_dir(), &slot)
                    .map_err(|error| error.to_string())?;
                let (sim, loader) =
                    Simulation::try_from_replay(replay).map_err(|error| error.to_string())?;
                loader
                    .validate_batches(crate::sim_thread::fits_replay_batch)
                    .map_err(|error| error.to_string())?;
                Ok(PreparedWorld {
                    replacement: WorldReplacement::ReplayDebug(sim, loader),
                    name: String::new(),
                })
            });
        }
        #[cfg(feature = "multiplayer")]
        Departure::Network(target) => {
            uiw.write::<SessionState>().departure = None;
            let mut io = uiw.write::<SaveLoadState>();
            io.network_busy = true;
            io.network_target = Some(target);
        }
        Departure::Exit => uiw.write::<SaveLoadState>().exit_requested = true,
    }
}

pub(crate) fn end_replay(uiw: &UiWorld) {
    let mut io = uiw.write::<SaveLoadState>();
    io.replay = None;
    io.replay_ui = None;
    drop(io);
    uiw.write::<SessionState>().interaction_blocked = true;
    uiw.write::<InputMap>().reset_world_input();
}

pub fn set_screen(uiw: &UiWorld, screen: SessionScreen) -> bool {
    if screen != SessionScreen::Playing && !crate::network::is_singleplayer(uiw) {
        return false;
    }
    let mut current = uiw.write::<SessionScreen>();
    if *current != screen {
        let previous = std::mem::replace(&mut *current, screen);
        let mut session = uiw.write::<SessionState>();
        session.interaction_blocked = true;
        session.departure = None;
        session.world_job_id += 1;
        if screen == SessionScreen::LoadGame {
            session.load_back = previous;
        } else {
            session.paused = false;
        }
        session.refresh_slots |= screen != SessionScreen::Playing;
        let mut io = uiw.write::<SaveLoadState>();
        io.please_save = false;
        io.world_job = None;
        io.world_error = None;
        drop(io);
        let mut ui = uiw.write::<SessionUi>();
        ui.settings_open = false;
        ui.delete = None;
        ui.save_as = None;
        if screen == SessionScreen::NewGame {
            ui.new_game = new_game::NewGameForm::fresh();
        }
        if screen == SessionScreen::Playing {
            uiw.write::<GuiState>().last_save = Instant::now();
        }
        reset_interaction(uiw);
        *uiw.write::<ExitState>() = ExitState::NoExit;
    }
    true
}

pub fn request_world_replacement(uiw: &UiWorld, replacement: WorldReplacement) -> bool {
    if !crate::network::is_singleplayer(uiw) {
        return false;
    }
    uiw.write::<SaveLoadState>().pending = Some(replacement);
    true
}

pub(crate) fn validate_native_world(sim: &Simulation) -> Result<(), String> {
    let (width, height) = sim.map().environment().size();
    let limit = simulation::map::MAX_NATIVE_LAND_SIDE;
    if width > limit || height > limit {
        return Err(format!(
            "This city is {width} × {height} terrain chunks. The native renderer supports up to {limit} chunks per axis."
        ));
    }
    Ok(())
}

#[cfg(test)]
pub fn apply_world_replacement(
    uiw: &UiWorld,
    sim: &mut Simulation,
    schedule: &mut SeqSchedule,
    replacement: WorldReplacement,
    reset_runtime: impl FnOnce(&Simulation),
) {
    let generation = uiw.read::<SessionState>().generation + 1;
    let coordinator = uiw.read::<save_coordinator::SaveCoordinator>().clone();
    coordinator.swap(generation, || {});
    let next = install_world(uiw, replacement, generation);
    *sim = next;
    *schedule = Simulation::schedule();
    reset_runtime(sim);
}

pub(crate) fn install_world(
    uiw: &UiWorld,
    replacement: WorldReplacement,
    generation: u64,
) -> Simulation {
    let (next, mut slot, replay, is_saved, camera) = match replacement {
        WorldReplacement::Snapshot(sim, slot) => (sim, Some(slot), None, true, None),
        WorldReplacement::SnapshotWithCamera(sim, slot, camera) => {
            (sim, Some(slot), None, true, Some(camera))
        }
        WorldReplacement::NewGame(sim, slot) => (sim, Some(slot), None, false, None),
        WorldReplacement::ReplayDebug(sim, replay) => (sim, None, Some(replay), false, None),
        #[cfg(feature = "multiplayer")]
        WorldReplacement::Network(sim) => (sim, None, None, false, None),
    };

    let city_name = slot.as_ref().map(ToString::to_string).unwrap_or_default();
    let migrated = next.read::<simulation::Replay>().migrated_from_legacy_slot();
    if migrated {
        slot = None;
    }
    {
        let mut session = uiw.write::<SessionState>();
        session.generation = generation;
        session.city_name = city_name;
        session.set_snapshot_slot(slot);
        session.replay_requested = false;
        session.interaction_blocked = true;
        session.departure = None;
        session.paused = false;
        let mut io = uiw.write::<SaveLoadState>();
        #[cfg(test)]
        io.saving_status.store(false, Ordering::SeqCst);
        let rotation = io.autosave_index;
        *io = SaveLoadState {
            autosave_index: rotation,
            replay,
            saved_progress: is_saved.then(|| CityProgress::capture(&next)),
            ..SaveLoadState::default()
        };
    }
    *uiw.write::<SessionScreen>() = SessionScreen::Playing;
    *uiw.write::<SessionUi>() = SessionUi::default();
    uiw.write::<NetworkState>().reset_timestep();

    uiw.write::<GuiState>().last_save = Instant::now();
    uiw.write::<InputMap>().reset_world_input();
    reset_world_camera(uiw, &next, camera);

    let hooks = uiw.read::<WorldResetHooks>().0.clone();
    for reset in hooks {
        reset(uiw, generation);
    }
    if migrated {
        notify(uiw, "City migrated. Save to a new slot. Replay is unavailable for this city.");
    }
    next
}

fn reset_world_camera(uiw: &UiWorld, sim: &Simulation, camera: Option<SlotCamera>) {
    uiw.camera_mut().reset_for_world(&sim.map(), camera);
    *uiw.write::<geom::Camera>() = uiw.camera().camera;
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod p4_fix_esc_tests {
    use super::*;
    use crate::gui::bulldozer::BulldozerState;
    use crate::gui::follow::FollowEntity;
    use crate::gui::roadbuild::{BuildState, RoadBuildResource};
    use crate::gui::terraforming::TerraformingResource;
    use crate::gui::zoning::ZoningState;
    use crate::gui::{InspectedBuilding, InspectedEntity, Tool};
    use crate::inputmap::{Bindings, InputAction};
    use engine::{InputContext, MouseButton};
    use geom::Vec3;
    use simulation::map::MapProject;
    use simulation::AnyEntity;

    fn playing_ui() -> UiWorld {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        UiWorld::with_defaults()
    }

    #[test]
    fn p4_fix_esc_idle_pauses_and_paused_resumes() {
        let uiw = playing_ui();
        close_modal(&uiw);
        assert!(uiw.read::<SessionState>().paused);
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
    }

    #[test]
    fn p4_fix_esc_road_placement_then_tool_then_pause() {
        let uiw = playing_ui();
        *uiw.write::<Tool>() = Tool::RoadbuildStraight;
        uiw.write::<RoadBuildResource>().build_state =
            BuildState::Start(MapProject::ground(Vec3::ZERO));

        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(matches!(
            uiw.read::<RoadBuildResource>().build_state,
            BuildState::Hover
        ));
        assert!(*uiw.read::<Tool>() == Tool::RoadbuildStraight);
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(*uiw.read::<Tool>() == Tool::Hand);
        close_modal(&uiw);
        assert!(uiw.read::<SessionState>().paused);
    }

    #[test]
    fn p4_fix_esc_cancels_active_tool() {
        let uiw = playing_ui();
        *uiw.write::<Tool>() = Tool::SpecialBuilding;
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(*uiw.read::<Tool>() == Tool::Hand);
    }

    #[test]
    fn p4_fix_esc_cancels_zoning_drag() {
        let uiw = playing_ui();
        *uiw.write::<Tool>() = Tool::Zoning;
        uiw.write::<ZoningState>().start_test_gesture();
        let mut input = InputContext::default();
        input.mouse.pressed.insert(MouseButton::Left);
        input.keyboard.pressed.insert(engine::Key::c("E"));
        {
            let mut actions = uiw.write::<InputMap>();
            actions.build_input_tree(&mut Bindings::default());
            actions.prepare_frame(&input, true, true);
            assert!(actions.just_act.contains(&InputAction::OpenEconomyMenu));
        }
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(uiw.read::<ZoningState>().gesture_is_clear());
        assert!(*uiw.read::<Tool>() == Tool::Hand);

        let mut actions = uiw.write::<InputMap>();
        actions.prepare_frame(&input, true, true);
        assert!(!actions.act.contains(&InputAction::Select));
        assert!(!actions.just_act.contains(&InputAction::OpenEconomyMenu));
        input.mouse.pressed.clear();
        actions.observe_pointer_buttons(false);
        actions.prepare_frame(&input, true, true);
        input.mouse.pressed.insert(MouseButton::Left);
        actions.prepare_frame(&input, true, true);
        assert!(actions.just_act.contains(&InputAction::Select));
    }

    #[test]
    fn p4_fix_esc_cancels_terraforming_drag() {
        let uiw = playing_ui();
        *uiw.write::<Tool>() = Tool::Terraforming;
        uiw.write::<TerraformingResource>().start_test_gesture();
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(uiw.read::<TerraformingResource>().gesture_is_clear());
        assert!(*uiw.read::<Tool>() == Tool::Hand);
    }

    #[test]
    fn p4_fix_esc_cancels_bulldozer_drag() {
        let uiw = playing_ui();
        *uiw.write::<Tool>() = Tool::Bulldozer;
        uiw.write::<BulldozerState>().start_test_gesture();
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(uiw.read::<BulldozerState>().gesture_is_clear());
        assert!(*uiw.read::<Tool>() == Tool::Hand);
    }

    #[test]
    fn p4_fix_esc_clears_selection() {
        let uiw = playing_ui();
        uiw.write::<InspectedEntity>().e = Some(AnyEntity::VehicleID(Default::default()));
        uiw.write::<InspectedBuilding>().e = Some(Default::default());
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(uiw.read::<InspectedEntity>().e.is_none());
        assert!(uiw.read::<InspectedBuilding>().e.is_none());
    }

    #[test]
    fn p4_fix_esc_stops_following() {
        let uiw = playing_ui();
        uiw.write::<FollowEntity>().0 = Some(AnyEntity::VehicleID(Default::default()));
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(uiw.read::<FollowEntity>().0.is_none());
    }

    #[test]
    fn p4_fix_esc_dismisses_open_window_before_pause() {
        let uiw = playing_ui();
        let window_states = || {
            let mut gui = uiw.write::<GuiState>();
            gui.windows
                .window_states()
                .filter_map(|(name, open)| open.then_some(name))
                .collect::<Vec<_>>()
        };
        for (name, open) in uiw.write::<GuiState>().windows.window_states() {
            *open = matches!(name, "Economy" | "Settings");
        }
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert_eq!(window_states(), ["Economy"]);
        close_modal(&uiw);
        assert!(!uiw.read::<SessionState>().paused);
        assert!(window_states().is_empty());
        close_modal(&uiw);
        assert!(uiw.read::<SessionState>().paused);
    }

    #[test]
    fn p4_fix_esc_restarts_autosave_interval_when_play_resumes() {
        let uiw = playing_ui();
        let old = Instant::now() - Duration::from_secs(3600);

        set_paused(&uiw, true);
        uiw.write::<GuiState>().last_save = old;
        close_modal(&uiw);
        assert!(uiw.read::<GuiState>().last_save > old);

        set_screen(&uiw, SessionScreen::MainMenu);
        uiw.write::<GuiState>().last_save = old;
        set_screen(&uiw, SessionScreen::Playing);
        assert!(uiw.read::<GuiState>().last_save > old);
    }
}

#[cfg(test)]
#[test]
fn p4_departure_detects_paused_edits_since_the_snapshot() {
    use simulation::world_command::WorldCommand;
    use simulation::SimulationOptions;

    crate::init::init();
    let mut sim = Simulation::new_with_options(SimulationOptions {
        terrain_size: 0,
        save_replay: false,
        ..SimulationOptions::LEGACY
    });
    let uiw = UiWorld::with_defaults();
    uiw.write::<SessionState>().active_slot = Some(SlotId::legacy());
    let saved_progress = CityProgress::capture(&sim);
    uiw.write::<SaveLoadState>().saved_progress = Some(saved_progress);
    let destination = Departure::Screen(SessionScreen::LoadGame);

    sim.apply_commands([&WorldCommand::SetTaxRate {
        zone: simulation::map::ZoneKind::Residential,
        rate_permille: 125,
    }]);
    assert_eq!(sim.get_tick(), saved_progress.tick);
    request_departure(&uiw, &sim, destination);
    assert!(playing(&uiw));
    assert!(view_input(&uiw));
    assert!(!world_interaction(&uiw));
    assert!(matches!(
        uiw.read::<SessionState>().departure,
        Some(PendingDeparture {
            phase: DeparturePhase::Confirm,
            ..
        })
    ));

    uiw.write::<SessionState>().departure = None;
    uiw.write::<SaveLoadState>().saved_progress = Some(CityProgress::capture(&sim));
    request_departure(&uiw, &sim, destination);
    assert_eq!(*uiw.read::<SessionScreen>(), SessionScreen::LoadGame);
}

#[cfg(test)]
mod p4c_tests {
    use super::save_coordinator::SaveCoordinator;
    use super::*;

    #[test]
    fn p4c_delayed_publish_cannot_rename_after_swap() {
        let coordinator = SaveCoordinator::default();
        let permit = coordinator.request(0, &SlotId::legacy()).unwrap();
        let root = std::env::temp_dir().join(format!("p4c-delayed-{:016x}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let staging = root.join("world.zip.tmp");
        let snapshot = root.join("world.zip");
        std::fs::write(&staging, b"old world").unwrap();
        let (ready, wait) = std::sync::mpsc::channel();
        let (resume, delayed) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let (staging, snapshot) = (&staging, &snapshot);
            let coordinator = &coordinator;
            let worker = scope.spawn(move || {
                ready.send(()).unwrap();
                delayed.recv().unwrap();
                coordinator.publish(&permit, || std::fs::rename(staging, snapshot).unwrap())
            });
            wait.recv().unwrap();
            coordinator.swap(1, || {});
            resume.send(()).unwrap();
            assert_eq!(worker.join().unwrap(), None);
        });
        assert!(!snapshot.exists());
        assert_eq!(std::fs::read(&staging).unwrap(), b"old world");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn p4c_continue_skips_incompatible_slots_in_discovery_order() {
        let slots = [
            (
                SlotId::new("newest").unwrap(),
                SlotCompatibility::Incompatible,
            ),
            (
                SlotId::new("recent").unwrap(),
                SlotCompatibility::Compatible,
            ),
            (SlotId::legacy(), SlotCompatibility::Compatible),
        ];
        assert_eq!(continue_slot(&slots), Some(&slots[1].0));
        assert_eq!(continue_slot(&slots[..1]), None);
    }

    #[test]
    fn p4c_city_names_derive_unique_non_autosave_ids() {
        let loaded = [SlotId::new("old_town").unwrap(), SlotId::autosave(0)];
        assert_eq!(city_slot(" Old / Town ", &loaded).as_str(), "old_town_2");
        assert_eq!(city_slot("AUTOSAVE_0", &loaded).as_str(), "city_autosave_0");
        assert_eq!(city_slot("東京", &loaded).as_str(), "city");
    }
}

#[cfg(test)]
mod p4_fix_ac_tests {
    use super::*;
    use crate::rendering::OrbitCamera;
    use geom::{Camera, Vec3};

    fn ui_with_camera() -> UiWorld {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let mut uiw = UiWorld::with_defaults();
        let camera = Camera::new(Vec3::ZERO, 1280.0, 720.0);
        uiw.insert(camera);
        uiw.insert(OrbitCamera::from_camera(camera));
        uiw
    }

    #[test]
    fn tiles_fix_rejects_oversize_native_world_during_preflight() {
        use simulation::map::{Environment, Map};

        drop(ui_with_camera());
        for (width, height) in [(51, 1), (1, 51)] {
            let oversized = Simulation::new(false);
            let mut map = Map::empty();
            assert!(map.install_environment(Environment::new(width, height)));
            *oversized.write::<Map>() = map;
            let replacement = WorldReplacement::Snapshot(oversized, SlotId::legacy());
            let result = replacement.validate_native_size();
            assert!(result.unwrap_err().contains("50 chunks per axis"));
        }
    }

    #[test]
    fn af2_loading_a_slot_restores_its_camera_pose() {
        let uiw = ui_with_camera();
        let root = std::env::temp_dir().join(format!("af2-slot-camera-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let mut sim = Simulation::new_with_options(simulation::SimulationOptions {
            terrain_size: 8,
            save_replay: false,
            ..simulation::SimulationOptions::LEGACY
        });
        let poses = [
            SlotCamera {
                position: [3000.0, 3100.0, 0.0],
                yaw: -0.25,
                pitch: 0.75,
                distance: 450.0,
            },
            SlotCamera {
                position: [3300.0, 3050.0, 0.0],
                yaw: 0.3,
                pitch: 0.6,
                distance: 650.0,
            },
        ];
        for (index, pose) in poses.into_iter().enumerate() {
            sim.encode_slot(SnapshotRevision::new(index as u128 + 1))
                .unwrap()
                .with_camera(pose)
                .publish(&root, &SlotId::autosave(index as u8))
                .unwrap();
        }

        let catalog = discover_slots(&root).unwrap();
        for (index, pose) in poses.into_iter().enumerate().rev() {
            let slot = SlotId::autosave(index as u8);
            let (entry, _) = catalog.iter().find(|(entry, _)| entry.id == slot).unwrap();
            let loaded = Simulation::load_listed_snapshot(&root, entry).unwrap();
            apply_world_replacement(
                &uiw,
                &mut sim,
                &mut SeqSchedule::default(),
                WorldReplacement::from_slot(loaded, entry),
                |_| {},
            );
            let camera = uiw.camera();
            assert_eq!(camera.slot_camera(), pose);
            assert_eq!(camera.targetpos, Vec3::from(pose.position));
            assert_eq!(camera.targetdist, pose.distance);
            assert_eq!(
                (camera.camera.viewport_w, camera.camera.viewport_h),
                (1280.0, 720.0)
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn p4_fix_ac_dirty_install_frame_requires_close_confirmation() {
        let uiw = ui_with_camera();
        let mut sim = Simulation::new(false);
        apply_world_replacement(
            &uiw,
            &mut sim,
            &mut SeqSchedule::default(),
            WorldReplacement::NewGame(Simulation::new(false), SlotId::legacy()),
            |_| {},
        );
        assert!(uiw.read::<SessionState>().interaction_blocked);

        request_departure(&uiw, &sim, Departure::Exit);

        assert!(!uiw.read::<SaveLoadState>().exit_requested);
        assert!(matches!(
            uiw.read::<SessionState>().departure,
            Some(PendingDeparture {
                phase: DeparturePhase::Confirm,
                ..
            })
        ));
    }

    #[test]
    fn p4_fix_ac_autosave_recovery_prompts_for_a_manual_slot() {
        let uiw = ui_with_camera();
        let mut sim = Simulation::new(false);
        let recovery = SlotId::autosave(0);
        apply_world_replacement(
            &uiw,
            &mut sim,
            &mut SeqSchedule::default(),
            WorldReplacement::Snapshot(Simulation::new(false), recovery.clone()),
            |_| {},
        );
        let mut state = uiw.write::<SessionState>();
        assert!(state.active_slot.is_none());
        assert_eq!(state.snapshot_slot(), Some(&recovery));
        state.interaction_blocked = false;
        drop(state);
        assert_eq!(
            uiw.read::<SaveLoadState>().saved_progress,
            Some(CityProgress::capture(&sim))
        );

        request_save(&uiw, Some((recovery, "Reserved".into())));
        assert!(uiw.read::<SaveLoadState>().pending_save.is_none());
        request_save(&uiw, None);
        assert!(uiw.read::<SessionUi>().save_as.is_some());
        assert!(uiw.read::<SaveLoadState>().pending_save.is_none());
    }

    #[test]
    fn p4_fix_ac_departure_save_survives_autosave_rotation_in_another_city() {
        let uiw = ui_with_camera();
        let root = std::env::temp_dir().join(format!("p4-fix-ac-rotation-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();

        {
            let mut state = uiw.write::<SessionState>();
            state.set_snapshot_slot(Some(SlotId::autosave(0)));
            state.city_name = "Harbor (autosave)".into();
        }
        // The on-disk name can already exist while the catalog is being refreshed.
        std::fs::write(root.join("harbor.bc"), b"previous saved city").unwrap();
        let request = departure_save_request(&uiw, &root).unwrap();
        assert_eq!(request.slot.as_str(), "harbor_2");
        assert_eq!(request.name, "Harbor");

        let sim = Simulation::new(false);
        sim.save_to_slot(&root, &request.slot, SnapshotRevision::new(1))
            .unwrap();
        let manual_path = root.join("harbor_2.zip");
        let manual_bytes = std::fs::read(&manual_path).unwrap();

        for index in 0..3 {
            Simulation::new(false)
                .save_to_slot(
                    &root,
                    &SlotId::autosave(index),
                    SnapshotRevision::new(2 + u128::from(index)),
                )
                .unwrap();
        }
        assert_eq!(std::fs::read(manual_path).unwrap(), manual_bytes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn p4_fix_ac_debug_replay_asks_before_leaving_a_dirty_city() {
        let uiw = ui_with_camera();
        let sim = Simulation::new(false);
        uiw.write::<SessionState>().active_slot = Some(SlotId::legacy());

        request_departure(&uiw, &sim, Departure::ReplayDebug);

        assert!(matches!(
            uiw.read::<SessionState>().departure,
            Some(PendingDeparture {
                destination: Departure::ReplayDebug,
                phase: DeparturePhase::Confirm,
            })
        ));
        assert!(!world_job_active(&uiw));
        close_modal(&uiw);
        assert!(uiw.read::<SessionState>().departure.is_none());
        assert!(!world_job_active(&uiw));
    }

    #[test]
    fn p4_fix_ac_cancel_generation_invalidates_even_a_ready_result() {
        let uiw = ui_with_camera();
        let (sender, result) = mpsc::channel();
        uiw.write::<SessionState>().world_job_id = 1;
        uiw.write::<SaveLoadState>().world_job = Some(WorldJob {
            id: 1,
            kind: WorldJobKind::NewGame,
            presented: true,
            result,
        });
        sender
            .send(Ok(WorldJobOutcome::Replace(PreparedWorld {
                replacement: WorldReplacement::NewGame(Simulation::new(false), SlotId::legacy()),
                name: "Cancelled".into(),
            })))
            .unwrap();
        assert!(world_job_active(&uiw));

        close_modal(&uiw);

        assert!(!world_job_active(&uiw));
        assert_ne!(uiw.read::<SessionState>().world_job_id, 1);
        assert!(uiw.read::<SaveLoadState>().pending.is_none());
    }

    #[test]
    fn p4_fix_final_cancel_allows_retry_before_the_old_worker_finishes() {
        let uiw = ui_with_camera();
        let (sender, result) = mpsc::channel();
        uiw.write::<SaveLoadState>().world_job = Some(WorldJob {
            id: 0,
            kind: WorldJobKind::NewGame,
            presented: true,
            result,
        });
        assert!(cancel_generation(&uiw));
        start_world_job(&uiw, WorldJobKind::NewGame, || Err("new worker".into()));

        assert!(
            world_job_active(&uiw),
            "the previous receiver blocked the retry"
        );
        assert!(sender
            .send(Err(WorldJobError::Failed("old worker".into())))
            .is_err());
    }

    #[test]
    fn p4_fix_final_continue_checks_the_dormant_snapshot_revision() {
        let uiw = ui_with_camera();
        let sim = Simulation::new(false);
        let root = std::env::temp_dir().join(format!("p4-fix-final-continue-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let slot = SlotId::new("city").unwrap();
        sim.save_to_slot(&root, &slot, SnapshotRevision::new(1))
            .unwrap();
        let listed = discover_slots(&root).unwrap().pop().unwrap().0;
        uiw.write::<SessionState>()
            .set_snapshot_slot(Some(slot.clone()));
        uiw.write::<SessionState>()
            .remember_loaded_snapshot(listed.clone());
        uiw.write::<SaveLoadState>().saved_progress = Some(CityProgress::capture(&sim));
        assert!(can_resume_dormant(&uiw, Some(&sim), &listed));
        assert!(Simulation::validate_listed_snapshot(&root, &listed).is_ok());

        sim.save_to_slot(&root, &slot, SnapshotRevision::new(2))
            .unwrap();
        let replacement = discover_slots(&root).unwrap().pop().unwrap().0;
        assert!(matches!(
            Simulation::validate_listed_snapshot(&root, &listed),
            Err(SaveLoadError::SnapshotChanged)
        ));
        std::fs::remove_dir_all(root).unwrap();
        assert!(!can_resume_dormant(&uiw, Some(&sim), &replacement));
    }

    #[test]
    fn p4_fix_final_open_distinguishes_a_replacement_from_a_listed_snapshot_failure() {
        let root = std::env::temp_dir().join(format!("p4-fix-final-open-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let slot = SlotId::new("city").unwrap();
        Simulation::new(false)
            .save_to_slot(&root, &slot, SnapshotRevision::new(1))
            .unwrap();
        let listed = discover_slots(&root).unwrap().pop().unwrap().0;
        let replacement = root.join("replacement.tmp");
        std::fs::write(&replacement, b"bad snapshot").unwrap();
        std::fs::File::open(&replacement)
            .unwrap()
            .set_modified(listed.modified)
            .unwrap();
        std::fs::rename(replacement, &listed.paths.zip).unwrap();
        let error = Simulation::load_listed_snapshot(&root, &listed)
            .err()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(matches!(error, SaveLoadError::SnapshotChanged), "{error}");
    }

    #[test]
    fn p4_fix_ac_catalog_remembers_a_failed_snapshot_until_it_changes() {
        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let root = std::env::temp_dir().join(format!("p4-fix-ac-catalog-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let slot = SlotId::new("city").unwrap();
        // The normal writer supplies a compatible header, but a world with no
        // terrain fails when the selected snapshot is decoded.
        Simulation::new(false)
            .save_to_slot(&root, &slot, SnapshotRevision::new(1))
            .unwrap();
        let mut state = SessionState::default();
        let catalog = discover_slots(&root).unwrap();
        let entry = continue_slot(&catalog).unwrap().clone();
        assert!(matches!(
            Simulation::load_listed_snapshot(&root, &entry),
            Err(SaveLoadError::Corrupt { .. })
        ));
        state.update_slot_catalog(Ok(catalog));
        state.mark_unreadable(&entry);

        state.update_slot_catalog(discover_slots(&root));
        assert!(continue_slot(&state.slots).is_none());

        std::fs::File::open(entry.snapshot_path())
            .unwrap()
            .set_modified(entry.modified + Duration::from_secs(1))
            .unwrap();
        state.update_slot_catalog(discover_slots(&root));
        assert!(continue_slot(&state.slots).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn p6_save_cut_marks_a_same_version_registry_mismatch() {
        use common::saveload::{Bincode, Encoder};

        let root = std::env::temp_dir().join(format!("p6-registry-cut-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let id = SlotId::new("staged").unwrap();
        let paths = saveload::slot_paths(&root, &id).unwrap();
        let other_registry = prototypes::try_prototypes()
            .map_or(0, |registry| registry.fingerprint().wrapping_add(1));
        let revision = SnapshotRevision::new(42);
        let header = (*b"\xffEGREGORIA\0\x02", VERSION, revision, other_registry);
        std::fs::write(paths.bc, Bincode::encode(&header).unwrap()).unwrap();

        let catalog = discover_slots(&root).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].0.revision, Some(revision));
        assert_eq!(catalog[0].1, SlotCompatibility::IncompatibleRegistry);
        assert_eq!(
            catalog[0].1.unavailable_reason(),
            Some("incompatible prototype registry")
        );
        assert!(continue_slot(&catalog).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn p6_save_cut_marks_wave_b_slots_incompatible() {
        let root = std::env::temp_dir().join(format!("p6-save-cut-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let id = SlotId::new("wave-b-010").unwrap();
        let paths = saveload::slot_paths(&root, &id).unwrap();
        // A legacy Bincode header with no world and a replay with an unknown command
        // show whether the version is checked before either payload is decoded.
        std::fs::write(&paths.bc, b"\x060.10.0").unwrap();
        std::fs::write(
            &paths.replay,
            br#"{"commands":[[1,"UnknownCommand"]],"version":"0.10.0"}"#,
        )
        .unwrap();

        let catalog = discover_slots(&root).unwrap();
        let [(entry, compatibility)] = catalog.as_slice() else {
            panic!("expected one slot, got {}", catalog.len());
        };
        assert_eq!(entry.id, id);
        assert!(matches!(compatibility, SlotCompatibility::Incompatible));
        assert!(continue_slot(&catalog).is_none());
        assert!(matches!(
            Simulation::validate_listed_snapshot(&root, entry),
            Err(SaveLoadError::SnapshotChanged)
        ));
        assert!(matches!(
            Simulation::load_listed_snapshot(&root, entry),
            Err(SaveLoadError::SnapshotChanged)
        ));
        assert!(matches!(
            Simulation::load_from_slot(&root, &id),
            Err(SaveLoadError::Incompatible { version }) if version == "0.10.0"
        ));
        assert!(matches!(
            Simulation::load_replay_from_slot(&root, &id),
            Err(SaveLoadError::Incompatible { version }) if version == "0.10.0"
        ));
        std::fs::remove_dir_all(root).unwrap();
    }
}
