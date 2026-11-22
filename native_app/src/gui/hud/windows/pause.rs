use goryak::{
    button_primary, button_secondary, error, minrow, on_secondary_container, textc, Window,
};
use simulation::Simulation;
use yakui::widgets::Pad;

use super::{self as session, Departure, DeparturePhase, SessionScreen, SessionState, SessionUi};
use crate::gui::windows::settings;
use crate::uiworld::{SaveLoadState, UiWorld};

pub(crate) fn render_modal(uiw: &UiWorld, sim: &Simulation) {
    let departure = uiw.read::<SessionState>().departure;
    if let Some(departure) = departure {
        render_departure(uiw, departure);
        return;
    }
    if session::render_loading(uiw) {
        return;
    }
    let (saving_as, settings_open) = {
        let ui = uiw.read::<SessionUi>();
        (ui.save_as.is_some(), ui.settings_open)
    };
    if saving_as {
        super::save_as::render(uiw);
        return;
    }
    if settings_open {
        settings::settings(uiw, sim, &mut uiw.write::<SessionUi>().settings_open);
        crate::gui::keybinds::keybind_modal(uiw, sim);
        return;
    }
    if !uiw.read::<SessionState>().paused {
        return;
    }

    let mut opened = true;
    Window {
        title: "Paused".into(),
        pad: Pad::all(16.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| {
        if button_primary("Resume").show().clicked {
            session::set_paused(uiw, false);
            return;
        }
        if session::can_save(uiw) {
            let io = uiw.read::<SaveLoadState>();
            if io.pending_save.is_some() || io.save_result.is_some() {
                textc(on_secondary_container(), "Saving…");
            } else {
                drop(io);
                if button_primary("Save").show().clicked {
                    session::request_save(uiw, None);
                }
                if button_primary("Save As").show().clicked {
                    session::open_save_as(uiw);
                }
            }
        }
        let singleplayer = crate::network::is_singleplayer(uiw);
        if singleplayer && button_primary("Load").show().clicked {
            session::request_departure(uiw, sim, Departure::Screen(SessionScreen::LoadGame));
        }
        if button_primary("Settings").show().clicked {
            uiw.write::<SessionUi>().settings_open = true;
        }
        if singleplayer && button_primary("Exit to menu").show().clicked {
            session::request_departure(uiw, sim, Departure::Screen(SessionScreen::MainMenu));
        }
        if button_secondary("Quit").show().clicked {
            session::request_departure(uiw, sim, Departure::Exit);
        }
    });
    if !opened {
        session::set_paused(uiw, false);
    }
}

pub(crate) fn render_departure(uiw: &UiWorld, departure: session::PendingDeparture) {
    if matches!(
        departure.phase,
        DeparturePhase::SaveRequested | DeparturePhase::Saving(_)
    ) {
        Window {
            title: "".into(),
            pad: Pad::all(24.0),
            radius: 10.0,
            opened: &mut true,
            child_spacing: 0.0,
        }
        .show(|| {
            textc(on_secondary_container(), "Saving…");
        });
        return;
    }

    let mut opened = true;
    let mut cancel = false;
    Window {
        title: "Save changes?".into(),
        pad: Pad::all(16.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| {
        if departure.phase == DeparturePhase::SaveFailed {
            textc(error(), "Could not save the city.");
        }
        let label = if matches!(departure.destination, Departure::Exit) {
            "Save and exit"
        } else {
            "Save"
        };
        minrow(8.0, || {
            if session::can_save(uiw) && button_primary(label).show().clicked {
                uiw.write::<SessionState>()
                    .departure
                    .as_mut()
                    .unwrap()
                    .phase = DeparturePhase::SaveRequested;
            }
            if button_secondary("Discard").show().clicked {
                session::finish_departure(uiw, departure.destination);
            }
            cancel = button_secondary("Cancel").show().clicked;
        });
    });
    if !opened || cancel {
        session::close_modal(uiw);
    }
}
