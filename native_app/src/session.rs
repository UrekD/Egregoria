use std::sync::atomic::Ordering;
use std::time::Instant;

use common::saveload::SlotId;
use serde::Deserialize;
use simulation::utils::scheduler::SeqSchedule;
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
}

#[derive(Default)]
pub struct NotificationCenter;

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

pub fn world_interaction(uiw: &UiWorld) -> bool {
    playing(uiw)
        && !uiw.read::<SessionState>().interaction_blocked
        && uiw.read::<SaveLoadState>().replay.is_none()
}

pub fn can_save(uiw: &UiWorld) -> bool {
    playing(uiw)
        && !uiw.read::<SessionState>().interaction_blocked
        && uiw.read::<SessionState>().active_slot.is_some()
}

pub fn set_screen(uiw: &UiWorld, screen: SessionScreen) -> bool {
    if screen != SessionScreen::Playing && !crate::network::is_singleplayer(uiw) {
        return false;
    }
    let mut current = uiw.write::<SessionScreen>();
    if *current != screen {
        *current = screen;
        uiw.write::<SessionState>().interaction_blocked = true;
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
        session.generation
    };

    // The save worker checks this flag under the read lock before using the world.
    let old_io = std::mem::take(&mut *uiw.write::<SaveLoadState>());
    old_io.saving_status.store(false, Ordering::SeqCst);
    uiw.write::<SaveLoadState>().replay = replay;
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
