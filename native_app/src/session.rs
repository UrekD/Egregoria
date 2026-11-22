use std::collections::{hash_map::RandomState, BTreeMap};
use std::hash::BuildHasher;
use std::io;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use common::saveload::{self, SlotEntry, SlotId, SnapshotRevision};
use serde::Deserialize;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::CommandOutcomes;
use simulation::{Simulation, SimulationReplayLoader};

use crate::game_loop::VERSION;
use crate::gui::{ExitState, GuiState};
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

pub(crate) use main_menu::{main_menu_buttons, render_session, render_status};
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
    pub generation: u64,
    pub(crate) interaction_blocked: bool,
    pub(crate) departure: Option<PendingDeparture>,
    pub(crate) city_name: String,
    pub(crate) paused: bool,
    pub(crate) load_back: SessionScreen,
    pub(crate) refresh_slots: bool,
    pub(crate) slots: Vec<(SlotEntry, SlotCompatibility)>,
    pub(crate) slot_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotCompatibility {
    Compatible,
    Incompatible,
    Unreadable,
}

pub(crate) fn discover_slots(root: &Path) -> io::Result<Vec<(SlotEntry, SlotCompatibility)>> {
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

fn new_city_slot(uiw: &UiWorld, name: &str) -> io::Result<SlotId> {
    let state = uiw.read::<SessionState>();
    let mut existing = state
        .slots
        .iter()
        .map(|(slot, _)| slot.id.clone())
        .chain(state.active_slot.clone())
        .collect::<Vec<_>>();
    loop {
        let slot = city_slot(name, &existing);
        match saveload::slot_paths(saveload::save_dir(), &slot) {
            Ok(_) => return Ok(slot),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => existing.push(slot),
            Err(error) => return Err(error),
        }
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

pub(crate) struct WorldJob {
    pub label: &'static str,
    pub presented: bool,
    pub result: Receiver<Result<PreparedWorld, String>>,
}

fn start_world_job(
    uiw: &UiWorld,
    label: &'static str,
    prepare: impl FnOnce() -> Result<PreparedWorld, String> + Send + 'static,
) {
    let mut io = uiw.write::<SaveLoadState>();
    if io.world_job.is_some() || !crate::network::is_singleplayer(uiw) {
        return;
    }
    let (sender, result) = mpsc::channel();
    io.world_job = Some(WorldJob {
        label,
        presented: false,
        result,
    });
    io.world_error = None;
    std::thread::spawn(move || {
        let _ = sender.send(prepare());
    });
}

pub(crate) fn load_slot(uiw: &UiWorld, entry: SlotEntry) {
    start_world_job(uiw, "Loading…", move || {
        let name = display_name(&entry).to_owned();
        let simulation = Simulation::load_from_slot(saveload::save_dir(), &entry.id)
            .map_err(|error| error.to_string())?;
        Ok(PreparedWorld {
            replacement: WorldReplacement::Snapshot(simulation, entry.id),
            name,
        })
    });
}

pub(crate) fn modal(uiw: &UiWorld) -> bool {
    let session = uiw.read::<SessionState>();
    session.paused
        || session.departure.is_some()
        || uiw.read::<SessionUi>().save_as.is_some()
        || uiw.read::<SaveLoadState>().world_job.is_some()
}

pub(crate) fn reset_interaction(uiw: &UiWorld) {
    uiw.write::<InputMap>().reset_world_input();
    uiw.write::<NetworkState>().reset_timestep();
}

pub(crate) fn set_paused(uiw: &UiWorld, paused: bool) {
    uiw.write::<SessionState>().paused = paused;
    reset_interaction(uiw);
}

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
    if uiw.read::<SaveLoadState>().world_job.is_some() {
        return;
    }
    let mut ui = uiw.write::<SessionUi>();
    if ui.save_as.take().is_some()
        || ui.delete.take().is_some()
        || std::mem::take(&mut ui.settings_open)
    {
        drop(ui);
        reset_interaction(uiw);
    } else if playing(uiw) && uiw.read::<SaveLoadState>().replay.is_none() {
        drop(ui);
        let paused = uiw.read::<SessionState>().paused;
        set_paused(uiw, !paused);
    } else if matches!(
        *uiw.read::<SessionScreen>(),
        SessionScreen::LoadGame | SessionScreen::NewGame
    ) {
        drop(ui);
        back(uiw);
    }
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
        Some((slot, name)) => SaveRequest {
            slot,
            name,
            kind: SaveKind::SaveAs,
        },
        None => {
            let state = uiw.read::<SessionState>();
            SaveRequest {
                slot: state.active_slot.clone().unwrap(),
                name: state.city_name.clone(),
                kind: SaveKind::Manual,
            }
        }
    };
    io.pending_save = Some(request);
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
        && uiw.read::<SessionState>().active_slot.is_some()
        && uiw.read::<SaveLoadState>().replay.is_none()
}

pub(crate) fn request_departure(uiw: &UiWorld, sim: &Simulation, destination: Departure) {
    if !playing(uiw) || uiw.read::<SessionState>().departure.is_some() {
        return;
    }
    if matches!(destination, Departure::Screen(_)) && !crate::network::is_singleplayer(uiw) {
        return;
    }
    if can_save(uiw)
        && uiw.read::<SaveLoadState>().saved_progress != Some(CityProgress::capture(sim))
    {
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
        if screen == SessionScreen::LoadGame {
            session.load_back = previous;
        } else {
            session.paused = false;
        }
        session.refresh_slots = screen != SessionScreen::Playing;
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
        session.active_slot = slot;
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
