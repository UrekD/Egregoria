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
        .add_enabled(
            crate::network::is_singleplayer(uiw),
            egui::Button::new("Load replay"),
        )
        .clicked()
    {
        use common::saveload::{self, SlotId};

        let slot = uiw
            .read::<session::SessionState>()
            .active_slot
            .clone()
            .unwrap_or_else(SlotId::legacy);
        let root = saveload::save_dir();
        let standalone_legacy = slot == SlotId::legacy()
            && saveload::slot_paths(root, &slot).is_ok_and(|paths| {
                [paths.zip, paths.bc].into_iter().all(|path| {
                    std::fs::symlink_metadata(path)
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                })
            });
        let replay = if standalone_legacy {
            Simulation::load_replay_from_disk("world").ok_or_else(|| "Could not load replay".into())
        } else {
            Simulation::load_replay_from_slot(root, &slot).map_err(|error| error.to_string())
        };
        state.has_save = replay.is_ok();
        match replay {
            Ok(replay) => match Simulation::try_from_replay(replay) {
                Ok((sim, loader)) => {
                    state.error = None;
                    session::request_world_replacement(
                        uiw,
                        WorldReplacement::ReplayDebug(sim, loader),
                    );
                }
                Err(error) => state.error = Some(error.to_string()),
            },
            Err(error) => state.error = Some(error),
        }
    }
    if let Some(error) = &state.error {
        ui.colored_label(egui::Color32::LIGHT_RED, error);
    }
}

pub(crate) fn render_slots(uiw: &UiWorld) {
    use crate::session::{SessionState, SessionUi, SlotCompatibility};
    use goryak::{
        button_primary, button_secondary, error, minrow, on_secondary_container, textc,
        VertScrollSize,
    };

    let state = uiw.read::<SessionState>();
    let mut load = None;
    let mut delete = None;
    if let Some(message) = &state.slot_error {
        textc(error(), message.clone());
    } else if state.slots.is_empty() {
        textc(on_secondary_container(), "No saved cities.");
    } else {
        VertScrollSize::Fixed(310.0).show(|| {
            goryak::mincolumn(10.0, || {
                for (entry, compatibility) in &state.slots {
                    let name = session::display_name(entry);
                    minrow(12.0, || {
                        if *compatibility == SlotCompatibility::Compatible {
                            if button_primary(name).show().clicked {
                                load = Some(entry.clone());
                            }
                        } else {
                            let reason = if *compatibility == SlotCompatibility::Incompatible {
                                "incompatible version"
                            } else {
                                "unreadable snapshot"
                            };
                            textc(
                                on_secondary_container().adjust(0.6),
                                format!("{name} ({reason})"),
                            );
                        }
                        if button_secondary("Delete").show().clicked {
                            delete = Some(entry.id.clone());
                        }
                    });
                    if let Some(meta) = &entry.meta {
                        textc(
                            on_secondary_container(),
                            format!(
                                "Tick {}  ·  Population {}  ·  {}",
                                meta.game_tick,
                                meta.population,
                                prototypes::Money::new_cents(meta.money_cents)
                            ),
                        );
                    }
                }
            });
        });
    }
    drop(state);
    if let Some(entry) = load {
        session::load_slot(uiw, entry);
    } else if let Some(id) = delete {
        uiw.write::<SessionUi>().delete = Some(id);
    } else if button_secondary("Back").show().clicked {
        session::back(uiw);
    }
}

pub(crate) fn render_delete(uiw: &UiWorld) -> bool {
    use crate::session::{SessionState, SessionUi};
    use goryak::{button_primary, button_secondary, minrow, on_secondary_container, textc, Window};
    use yakui::widgets::Pad;

    let Some(id) = uiw.read::<SessionUi>().delete.clone() else {
        return false;
    };
    let name = uiw
        .read::<SessionState>()
        .slots
        .iter()
        .find_map(|(entry, _)| (entry.id == id).then(|| session::display_name(entry).to_owned()))
        .unwrap_or_else(|| id.to_string());
    let mut opened = true;
    let mut confirmed = false;
    let mut cancelled = false;
    Window {
        title: "Delete save?".into(),
        pad: Pad::all(16.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| {
        textc(
            on_secondary_container(),
            format!("Delete {name} from disk?"),
        );
        minrow(8.0, || {
            confirmed = button_primary("Delete").show().clicked;
            cancelled = button_secondary("Cancel").show().clicked;
        });
    });
    if confirmed {
        uiw.write::<SaveLoadState>().delete_slot = Some(id);
    }
    if !opened || confirmed || cancelled {
        uiw.write::<SessionUi>().delete = None;
    }
    true
}
