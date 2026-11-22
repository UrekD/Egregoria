use goryak::{
    blur_bg, button_primary, button_secondary, error, on_primary_container, on_secondary_container,
    outline, padxy, secondary_container, textc, tooltip, Window,
};
use simulation::Simulation;
use yakui::widgets::Pad;
use yakui::{reflow, Alignment, Dim2, Pivot};

use super::{self as session, SessionScreen, SessionState, SessionUi, WorldJobKind};
use crate::gui::windows::{load, settings};
use crate::uiworld::{SaveLoadState, UiWorld};

pub(crate) fn main_menu_buttons(uiw: &UiWorld, sim: Option<&Simulation>, settings_open: &mut bool) {
    if let Some(entry) = session::continue_entry(uiw) {
        let label = format!("Continue {}", session::display_name(&entry));
        if tooltip(label, || button_primary("Continue").show()).clicked {
            session::continue_city(uiw, sim, entry);
            return;
        }
    }
    if crate::network::is_singleplayer(uiw) {
        if button_primary("New Game").show().clicked {
            session::set_screen(uiw, SessionScreen::NewGame);
            return;
        }
        if button_primary("Load").show().clicked {
            session::set_screen(uiw, SessionScreen::LoadGame);
            return;
        }
    }
    if button_primary("Settings").show().clicked {
        *settings_open = true;
    }
    if button_secondary("Quit").show().clicked {
        session::finish_departure(uiw, session::Departure::Exit);
    }
}

pub(crate) fn render_session(uiw: &UiWorld, sim: &Simulation) {
    if render_loading(uiw) {
        return;
    }

    let screen = *uiw.read::<SessionScreen>();
    if uiw.read::<SessionState>().starting_up {
        Window {
            title: "".into(),
            pad: Pad::all(24.0),
            radius: 10.0,
            opened: &mut true,
            child_spacing: 0.0,
        }
        .show(|| {
            textc(on_secondary_container(), "Scanning…");
        });
        return;
    }
    if screen == SessionScreen::MainMenu && render_startup_message(uiw) {
        return;
    }
    let settings_open = uiw.read::<SessionUi>().settings_open;
    if settings_open {
        settings::settings(uiw, sim, &mut uiw.write::<SessionUi>().settings_open);
        crate::gui::keybinds::keybind_modal(uiw, sim);
        return;
    }
    if screen == SessionScreen::LoadGame && load::render_delete(uiw) {
        return;
    }

    let mut opened = true;
    let title = match screen {
        SessionScreen::MainMenu => "",
        SessionScreen::NewGame => "New Game",
        SessionScreen::LoadGame => "Load",
        SessionScreen::Playing => return,
    };

    Window {
        title: title.into(),
        pad: Pad::all(16.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| {
        match screen {
            SessionScreen::MainMenu => {
                textc(on_primary_container(), "Egregoria");
                yakui::divider(outline(), 6.0, 1.0);
                let mut settings_open = false;
                main_menu_buttons(uiw, Some(sim), &mut settings_open);
                if settings_open {
                    uiw.write::<SessionUi>().settings_open = true;
                }
                if uiw.read::<SessionState>().scanning_slots() {
                    textc(on_secondary_container(), "Scanning…");
                }
            }
            SessionScreen::NewGame => super::new_game::render(uiw),
            SessionScreen::LoadGame => load::render_slots(uiw),
            SessionScreen::Playing => unreachable!(),
        }
        let io = uiw.read::<SaveLoadState>();
        if let Some(message) = &io.world_error {
            textc(error(), message.clone());
        }
    });

    if !opened && screen != SessionScreen::MainMenu {
        session::back(uiw);
    }
}

fn render_startup_message(uiw: &UiWorld) -> bool {
    let Some(message) = uiw.read::<SaveLoadState>().startup_message.clone() else {
        return false;
    };
    let mut opened = true;
    let mut dismissed = false;
    Window {
        title: "Save could not be loaded".into(),
        pad: Pad::all(16.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| {
        textc(error(), message);
        dismissed = button_primary("Dismiss").show().clicked;
    });
    if dismissed || !opened {
        uiw.write::<SaveLoadState>().startup_message = None;
    }
    true
}

pub(crate) fn render_loading(uiw: &UiWorld) -> bool {
    if !session::world_job_active(uiw) {
        return false;
    }
    let (label, cancellable) = {
        let mut io = uiw.write::<SaveLoadState>();
        let job = io.world_job.as_mut().unwrap();
        job.presented = true;
        (job.kind.label(), matches!(job.kind, WorldJobKind::NewGame))
    };
    let mut opened = true;
    let mut cancel = false;
    Window {
        title: "".into(),
        pad: Pad::all(24.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| {
        textc(on_secondary_container(), label);
        if cancellable {
            cancel = button_secondary("Cancel").show().clicked;
        }
    });
    if cancellable && (!opened || cancel) {
        session::cancel_generation(uiw);
    }
    true
}

pub(crate) fn render_status(uiw: &UiWorld) {
    let io = uiw.read::<SaveLoadState>();
    let message = if io.save_result.is_some() {
        Some("Saving…")
    } else {
        io.notice.as_ref().and_then(|(until, message)| {
            (*until > std::time::Instant::now()).then_some(message.as_str())
        })
    };
    if let Some(message) = message {
        let message = message.to_owned();
        reflow(
            Alignment::TOP_CENTER,
            Pivot::TOP_CENTER,
            Dim2::pixels(0.0, 55.0),
            || {
                blur_bg(secondary_container(), 8.0, || {
                    padxy(14.0, 8.0, || {
                        textc(on_secondary_container(), message);
                    });
                });
            },
        );
    }
}
