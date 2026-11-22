use std::sync::atomic::Ordering;
use std::time::Instant;

use common::saveload::SlotId;
use serde::Deserialize;
use simulation::utils::scheduler::SeqSchedule;
use simulation::world_command::CommandOutcomes;
use simulation::{Simulation, SimulationReplayLoader};

use crate::gui::{ExitState, GuiState};
use crate::inputmap::InputMap;
use crate::network::NetworkState;
use crate::uiworld::{ReceivedCommands, SaveLoadState, UiWorld};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum SessionScreen {
    MainMenu,
    NewGame,
    LoadGame,
    #[default]
    Playing,
}

#[allow(dead_code)] // Snapshot and new-game producers are supplied by the session UI.
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
    Saving,
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
    view_input(uiw)
        && uiw.read::<SessionState>().departure.is_none()
        && uiw.read::<SaveLoadState>().replay.is_none()
}

pub fn can_save(uiw: &UiWorld) -> bool {
    playing(uiw)
        && !uiw.read::<SessionState>().interaction_blocked
        && uiw.read::<SessionState>().active_slot.is_some()
}

pub(crate) fn request_departure(uiw: &UiWorld, sim: &Simulation, destination: Departure) {
    if !world_interaction(uiw) {
        return;
    }
    if matches!(destination, Departure::Screen(_)) && !crate::network::is_singleplayer(uiw) {
        return;
    }
    if can_save(uiw)
        && uiw.read::<SaveLoadState>().saved_progress != Some(CityProgress::capture(sim))
    {
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
        Departure::Exit => std::process::exit(0),
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
        *current = screen;
        let mut session = uiw.write::<SessionState>();
        session.interaction_blocked = true;
        session.departure = None;
        uiw.write::<InputMap>().reset_world_input();
        *uiw.commands() = Default::default();
        *uiw.write::<ReceivedCommands>() = Default::default();
        uiw.write::<SaveLoadState>().please_save = false;
        *uiw.write::<ExitState>() = ExitState::NoExit;
        uiw.write::<NetworkState>().reset_timestep();
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
    let (next, slot, replay) = match replacement {
        WorldReplacement::Snapshot(sim, slot) | WorldReplacement::NewGame(sim, slot) => {
            (sim, Some(slot), None)
        }
        WorldReplacement::ReplayDebug(sim, replay) => (sim, None, Some(replay)),
        #[cfg(feature = "multiplayer")]
        WorldReplacement::Network(sim) => (sim, None, None),
    };

    *sim = next;
    *schedule = Simulation::schedule();
    uiw.write::<NetworkState>().reset_timestep();

    *uiw.write::<SessionScreen>() = SessionScreen::Playing;
    let generation = {
        let mut session = uiw.write::<SessionState>();
        session.generation += 1;
        session.active_slot = slot;
        session.interaction_blocked = true;
        session.departure = None;
        session.generation
    };

    // The save worker checks this flag under the read lock before using the world.
    let old_io = std::mem::take(&mut *uiw.write::<SaveLoadState>());
    old_io.saving_status.store(false, Ordering::SeqCst);
    let mut io = uiw.write::<SaveLoadState>();
    io.replay = replay;
    io.saved_progress = Some(CityProgress::capture(sim));
    drop(io);
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
