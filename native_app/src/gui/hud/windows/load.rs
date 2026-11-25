use crate::session;
use crate::uiworld::{SaveLoadState, UiWorld};

#[derive(Default)]
pub struct LoadState {
    #[cfg(test)]
    pub(crate) has_save: bool,
}

pub(crate) fn replay_controls(ctx: &egui::Context, uiw: &UiWorld) {
    let mut io = uiw.write::<SaveLoadState>();
    if let Some(replay) = &mut io.replay_ui {
        use crate::sim_thread::ReplayControl;
        egui::TopBottomPanel::bottom("replay_controls").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                let progress = replay.progress;
                if let Some((step, total)) = progress.steps {
                    ui.label(format!("Replay {step} / {total} steps"));
                } else {
                    ui.label(format!(
                        "Replay tick {} / {}",
                        progress.tick, progress.last_tick
                    ));
                }
                for (label, speed) in [("Pause", 0), ("Play", 1), ("Fast", 100), ("Max", 10_000)] {
                    if ui.selectable_label(replay.speed == speed, label).clicked() {
                        replay.speed = speed;
                        replay.control = Some(ReplayControl::Speed(speed));
                    }
                }
                ui.separator();
                for ticks in [1, 10, 100, 1000] {
                    if ui.button(format!("+{ticks}")).clicked() && replay.control.is_none() {
                        replay.speed = 0;
                        replay.control = Some(ReplayControl::Step(ticks));
                    }
                }
                ui.separator();
                if ui.button("End replay").clicked() {
                    replay.control = Some(ReplayControl::End);
                }
            });
        });
        return;
    }
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
    if uiw.read::<SaveLoadState>().is_replaying() {
        return;
    }

    if ui
        .add_enabled(
            crate::network::is_singleplayer(uiw),
            egui::Button::new("Load replay"),
        )
        .clicked()
    {
        uiw.write::<session::SessionState>().replay_requested = true;
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
    if state.scanning_slots() {
        textc(on_secondary_container(), "Scanning…");
    } else if let Some(message) = &state.slot_error {
        textc(error(), message.clone());
    } else if state.slots.is_empty() {
        textc(on_secondary_container(), "No saved cities.");
    } else {
        VertScrollSize::Fixed(310.0).show(|| {
            goryak::mincolumn(10.0, || {
                for (entry, compatibility) in &state.slots {
                    let name = session::display_name(entry);
                    goryak::mincolumn(3.0, || {
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
                        let mut identity = format!("Slot {}", entry.id);
                        if let Some(revision) = entry.revision {
                            let short = (revision.get() >> 96) as u32;
                            identity.push_str(&format!("  ·  Rev {short:08x}"));
                        }
                        textc(on_secondary_container(), identity);
                        let saved_at = entry
                            .meta
                            .as_ref()
                            .map_or(entry.modified_unix, |meta| meta.saved_at_unix);
                        textc(
                            on_secondary_container(),
                            format!("Saved {}", saved_time(saved_at)),
                        );
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
                    });
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

fn saved_time(seconds: u64) -> String {
    fn is_leap(year: u64) -> bool {
        year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
    }

    const SECONDS_PER_DAY: u64 = 86_400;
    const DAYS_PER_400_YEARS: u64 = 146_097;
    let days = seconds / SECONDS_PER_DAY;
    let mut year = 1970 + days / DAYS_PER_400_YEARS * 400;
    let mut day = days % DAYS_PER_400_YEARS;
    loop {
        let days_in_year = 365 + u64::from(is_leap(year));
        if day < days_in_year {
            break;
        }
        day -= days_in_year;
        year += 1;
    }
    let mut month = 1;
    for days_in_month in [
        31,
        28 + u64::from(is_leap(year)),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ] {
        if day < days_in_month {
            break;
        }
        day -= days_in_month;
        month += 1;
    }
    let hour = seconds / 3600 % 24;
    let minute = seconds / 60 % 60;
    let second = seconds % 60;
    format!(
        "{year:04}-{month:02}-{:02} {hour:02}:{minute:02}:{second:02} UTC",
        day + 1
    )
}

#[cfg(test)]
#[test]
fn af1_saved_time_formats_utc_date_and_time() {
    assert_eq!(saved_time(0), "1970-01-01 00:00:00 UTC");
    assert_eq!(saved_time(951_827_696), "2000-02-29 12:34:56 UTC");
    assert_eq!(saved_time(4_107_542_400), "2100-03-01 00:00:00 UTC");
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
