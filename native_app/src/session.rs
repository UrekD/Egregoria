use std::collections::{hash_map::RandomState, BTreeMap};
use std::hash::BuildHasher;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use common::saveload::{self, SlotEntry, SlotId, SnapshotKind, SnapshotRevision};
use serde::Deserialize;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::CommandOutcomes;
use simulation::{Simulation, SimulationReplayLoader};

use crate::game_loop::VERSION;
use crate::gui::bulldozer::BulldozerState;
use crate::gui::follow::FollowEntity;
use crate::gui::roadbuild::RoadBuildResource;
use crate::gui::terraforming::TerraformingResource;
use crate::gui::zoning::ZoningState;
use crate::gui::{
    ExitState, GuiState, InspectedBuilding, InspectedEntity, PotentialCommands, Tool,
};
use crate::inputmap::InputMap;
use crate::network::NetworkState;
use crate::uiworld::{ReceivedCommands, SaveLoadState, UiWorld};

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
    NewGame(Simulation, SlotId),
    ReplayDebug(Simulation, SimulationReplayLoader),
    #[cfg(feature = "multiplayer")]
    Network(Simulation),
}

#[derive(Default)]
pub struct SessionState {
    pub active_slot: Option<SlotId>,
    recovery_slot: Option<SlotId>,
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
    Unreadable,
}

pub(crate) type DiscoveredSlots = Vec<(SlotEntry, SlotCompatibility)>;

#[derive(Default)]
pub(crate) struct FailedSnapshots(BTreeMap<SlotId, SnapshotIdentity>);

#[derive(PartialEq, Eq)]
struct SnapshotIdentity {
    path: PathBuf,
    modified: SystemTime,
    revision: Option<SnapshotRevision>,
}

impl From<&SlotEntry> for SnapshotIdentity {
    fn from(entry: &SlotEntry) -> Self {
        let path = match entry.snapshot {
            SnapshotKind::Bc => &entry.paths.bc,
            _ => &entry.paths.zip,
        };
        Self {
            path: path.clone(),
            modified: entry.modified,
            revision: entry.revision,
        }
    }
}

impl FailedSnapshots {
    pub(crate) fn record(&mut self, entry: &SlotEntry) {
        self.0.insert(entry.id.clone(), entry.into());
    }

    pub(crate) fn apply(&mut self, slots: &mut DiscoveredSlots) {
        let mut previous = std::mem::take(&mut self.0);
        for (entry, status) in slots {
            if let Some(failed) = previous.remove(&entry.id) {
                if failed == SnapshotIdentity::from(&*entry) {
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
        if slot.as_ref().is_some_and(SlotId::is_autosave) {
            self.active_slot = None;
            self.recovery_slot = slot;
        } else {
            self.active_slot = slot;
            self.recovery_slot = None;
        }
    }

    pub(crate) fn scanning_slots(&self) -> bool {
        self.starting_up || self.refresh_slots || self.slot_scan.is_some()
    }

    pub(crate) fn mark_unreadable(&mut self, entry: &SlotEntry) {
        self.failed_snapshots.record(entry);
        let failed = SnapshotIdentity::from(entry);
        for (candidate, status) in &mut self.slots {
            if candidate.id == entry.id && SnapshotIdentity::from(&*candidate) == failed {
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
            Ok(header) if header.version == VERSION => {
                (SlotCompatibility::Compatible, header.revision)
            }
            Ok(header) => (SlotCompatibility::Incompatible, header.revision),
            Err(_) => (SlotCompatibility::Unreadable, None),
        };
        compatibility.insert(path.to_owned(), status);
        revision
    })?;
    Ok(slots
        .into_iter()
        .map(|slot| {
            let path = match slot.snapshot {
                saveload::SnapshotKind::Bc => &slot.paths.bc,
                _ => &slot.paths.zip,
            };
            let status = compatibility
                .remove(path)
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

pub(crate) fn new_city_slot(uiw: &UiWorld, name: &str) -> io::Result<SlotId> {
    choose_city_slot(saveload::save_dir(), name, known_slot_ids(uiw))
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
    pub result: Receiver<Result<PreparedWorld, String>>,
}

fn start_world_job(
    uiw: &UiWorld,
    kind: WorldJobKind,
    prepare: impl FnOnce() -> Result<PreparedWorld, String> + Send + 'static,
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
    start_world_job(uiw, WorldJobKind::Snapshot(entry.clone()), move || {
        let name = display_name(&entry).to_owned();
        let simulation = Simulation::load_from_slot(saveload::save_dir(), &entry.id)
            .map_err(|error| error.to_string())?;
        Ok(PreparedWorld {
            replacement: WorldReplacement::Snapshot(simulation, entry.id),
            name,
        })
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
    let cancellable = uiw
        .read::<SaveLoadState>()
        .world_job
        .as_ref()
        .is_some_and(|job| {
            job.id == session.world_job_id && matches!(job.kind, WorldJobKind::NewGame)
        });
    if cancellable {
        session.world_job_id += 1;
        drop(session);
        reset_interaction(uiw);
    }
    cancellable
}

pub(crate) fn modal(uiw: &UiWorld) -> bool {
    let session = uiw.read::<SessionState>();
    session.paused
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
    } else if playing(uiw) && uiw.read::<SaveLoadState>().replay.is_none() {
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
    let mut tool = uiw.write::<Tool>();
    if *tool != Tool::Hand {
        let keep_road_tool =
            tool.is_roadbuild() && uiw.write::<RoadBuildResource>().cancel_placement();
        if !keep_road_tool {
            match *tool {
                Tool::Bulldozer => uiw.write::<BulldozerState>().reset_gesture(),
                Tool::Terraforming => uiw.write::<TerraformingResource>().cancel_gesture(),
                Tool::Zoning => uiw.write::<ZoningState>().cancel_gesture(),
                _ => {}
            }
            *tool = Tool::Hand;
        }
        uiw.write::<PotentialCommands>().0.clear();
        return true;
    }
    drop(tool);

    let entity = uiw.write::<InspectedEntity>().e.take().is_some();
    let building = uiw.write::<InspectedBuilding>().e.take().is_some();
    let following = uiw.write::<FollowEntity>().0.take().is_some();
    if entity || building || following || crate::gui::chat::close(uiw) {
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
    pub result: Receiver<Result<(CityProgress, simulation::SavePhase), String>>,
}

pub(crate) fn request_save(uiw: &UiWorld, target: Option<(SlotId, String)>) {
    if !can_save(uiw) {
        return;
    }
    let mut io = uiw.write::<SaveLoadState>();
    if io.pending_save.is_some() || io.save_result.is_some() {
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

pub(crate) fn departure_save_request(uiw: &UiWorld, root: &Path) -> io::Result<SaveRequest> {
    let state = uiw.read::<SessionState>();
    let (slot, name) = if let Some(slot) = &state.active_slot {
        (slot.clone(), state.city_name.clone())
    } else {
        let name = state.city_name.trim_end_matches(" (autosave)").to_owned();
        drop(state);
        let slot = choose_city_slot(root, &name, known_slot_ids(uiw))?;
        (slot, name)
    };
    Ok(SaveRequest {
        slot,
        name,
        kind: SaveKind::Departure,
    })
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
    view_input(uiw) && !modal(uiw) && uiw.read::<SaveLoadState>().replay.is_none()
}

pub fn can_save(uiw: &UiWorld) -> bool {
    playing(uiw)
        && !uiw.read::<SessionState>().interaction_blocked
        && uiw.read::<SessionState>().snapshot_slot().is_some()
        && uiw.read::<SaveLoadState>().replay.is_none()
}

pub(crate) fn request_departure(uiw: &UiWorld, sim: &Simulation, destination: Departure) {
    if !playing(uiw) || uiw.read::<SessionState>().departure.is_some() {
        return;
    }
    if !matches!(destination, Departure::Exit) && !crate::network::is_singleplayer(uiw) {
        return;
    }
    let io = uiw.read::<SaveLoadState>();
    let dirty = io.replay.is_none() && io.saved_progress != Some(CityProgress::capture(sim));
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
                Ok(PreparedWorld {
                    replacement: WorldReplacement::ReplayDebug(sim, loader),
                    name: String::new(),
                })
            });
        }
        Departure::Exit => uiw.write::<SaveLoadState>().exit_requested = true,
    }
}

pub(crate) fn end_replay(uiw: &UiWorld) {
    uiw.write::<SaveLoadState>().replay = None;
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
        *uiw.commands() = Default::default();
        *uiw.write::<ReceivedCommands>() = Default::default();
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

/// Run under the simulation write lock, before any new-world interaction for this frame.
pub fn apply_world_replacement(
    uiw: &UiWorld,
    sim: &mut Simulation,
    schedule: &mut SeqSchedule,
    replacement: WorldReplacement,
    reset_runtime: impl FnOnce(&Simulation),
) {
    let (next, slot, replay, is_saved) = match replacement {
        WorldReplacement::Snapshot(sim, slot) => (sim, Some(slot), None, true),
        WorldReplacement::NewGame(sim, slot) => (sim, Some(slot), None, false),
        WorldReplacement::ReplayDebug(sim, replay) => (sim, None, Some(replay), false),
        #[cfg(feature = "multiplayer")]
        WorldReplacement::Network(sim) => (sim, None, None, false),
    };

    let generation = uiw.read::<SessionState>().generation + 1;
    let coordinator = uiw.read::<save_coordinator::SaveCoordinator>().clone();
    coordinator.swap(generation, || {
        *sim = next;
        let mut session = uiw.write::<SessionState>();
        session.generation = generation;
        session.city_name = slot.as_ref().map(ToString::to_string).unwrap_or_default();
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
            saved_progress: is_saved.then(|| CityProgress::capture(sim)),
            ..SaveLoadState::default()
        };
    });
    *schedule = Simulation::schedule();
    *uiw.write::<SessionScreen>() = SessionScreen::Playing;
    *uiw.write::<SessionUi>() = SessionUi::default();
    uiw.write::<NetworkState>().reset_timestep();

    uiw.write::<GuiState>().last_save = Instant::now();
    uiw.write::<InputMap>().reset_world_input();
    uiw.camera_mut().reset_for_world(&sim.map());
    *uiw.write::<geom::Camera>() = uiw.camera().camera;

    let hooks = uiw.read::<WorldResetHooks>().0.clone();
    for reset in hooks {
        reset(uiw, generation);
    }
    reset_runtime(sim);
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
            .send(Ok(PreparedWorld {
                replacement: WorldReplacement::NewGame(Simulation::new(false), SlotId::legacy()),
                name: "Cancelled".into(),
            }))
            .unwrap();
        assert!(world_job_active(&uiw));

        close_modal(&uiw);

        assert!(!world_job_active(&uiw));
        assert_ne!(uiw.read::<SessionState>().world_job_id, 1);
        assert!(uiw.read::<SaveLoadState>().pending.is_none());
    }

    #[test]
    fn p4_fix_ac_catalog_remembers_a_failed_snapshot_until_it_changes() {
        let root = std::env::temp_dir().join(format!("p4-fix-ac-catalog-{}", random_seed()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("city.bc");
        std::fs::write(
            &path,
            [VERSION.len() as u8]
                .into_iter()
                .chain(VERSION.bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let mut state = SessionState::default();
        let catalog = discover_slots(&root).unwrap();
        let entry = continue_slot(&catalog).unwrap().clone();
        assert!(Simulation::load_from_slot(&root, &entry.id).is_err());
        state.update_slot_catalog(Ok(catalog));
        state.mark_unreadable(&entry);

        state.update_slot_catalog(discover_slots(&root));
        assert!(continue_slot(&state.slots).is_none());

        std::fs::File::open(path)
            .unwrap()
            .set_modified(entry.modified + Duration::from_secs(1))
            .unwrap();
        state.update_slot_catalog(discover_slots(&root));
        assert!(continue_slot(&state.slots).is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
