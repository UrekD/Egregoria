use crate::session::{self, WorldReplacement};
use crate::uiworld::{SaveLoadState, UiWorld};
use simulation::Simulation;

#[derive(Default)]
pub struct LoadState {
    pub(crate) has_save: bool,
    error: Option<String>,
}

pub(crate) fn replay_controls(ctx: &egui::Context, uiw: &UiWorld) {
    let mut io = uiw.write::<SaveLoadState>();
    let Some(replay) = &mut io.replay else {
        return;
    };
    let mut end = false;
    egui::TopBottomPanel::bottom("replay_controls").show(ctx, |ui| {
        ui.horizontal_wrapped(|ui| {
            let progress = if let Some((step, total)) = replay.step_progress() {
                format!("Replay {step} / {total} steps")
            } else {
                let tick = replay.pastt.0;
                let last_tick = replay.replay.last_tick_recorded.0;
                format!("Replay tick {tick} / {last_tick}")
            };
            ui.label(progress);
            for (label, speed) in [("Pause", 0), ("Play", 1), ("Fast", 100), ("Max", 10_000)] {
                if ui.selectable_label(replay.speed == speed, label).clicked() {
                    replay.speed = speed;
                }
            }
            ui.separator();
            for ticks in [1, 10, 100, 1000] {
                if ui.button(format!("+{ticks}")).clicked() {
                    replay.speed = 0;
                    replay.advance_n_ticks = ticks;
                }
            }
            ui.separator();
            end = ui.button("End replay").clicked();
        });
    });
    drop(io);
    if end {
        session::end_replay(uiw);
    }
}

pub(crate) fn debug_replay(ui: &mut egui::Ui, uiw: &UiWorld) {
    if uiw.read::<SaveLoadState>().replay.is_some() {
        return;
    }

    let mut state = uiw.write::<LoadState>();
    if ui
        .add_enabled(crate::network::is_singleplayer(uiw), egui::Button::new("Load replay"))
        .clicked()
    {
        let slot = uiw.read::<session::SessionState>().active_slot.clone();
        let stem = slot.as_ref().map_or("world", |slot| slot.as_str());
        let replay = Simulation::load_replay_from_disk(stem);
        state.has_save = replay.is_some();
        match replay {
            Some(replay) => match Simulation::try_from_replay(replay) {
                Ok((sim, loader)) => {
                    state.error = None;
                    session::request_world_replacement(uiw, WorldReplacement::ReplayDebug(sim, loader));
                }
                Err(error) => state.error = Some(error.to_string()),
            },
            None => state.error = Some("Could not load replay".into()),
        }
    }
    if let Some(error) = &state.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
}
