pub mod budget;
pub mod citystats;
pub mod economy;
pub mod load;
pub mod settings;
pub(crate) mod transit_lines;

use crate::inputmap::{InputAction, InputMap};
use crate::session::{self, SessionScreen};
use crate::uiworld::UiWorld;
use goryak::{button_primary, Window};
use yakui::widgets::Pad;
use simulation::Simulation;

#[cfg(feature = "multiplayer")]
pub mod network;

#[derive(Default)]
pub struct GUIWindows {
    economy_open: bool,
    budget_open: bool,
    citystats_open: bool,
    settings_open: bool,
    main_menu_open: bool,
    new_game_open: bool,
    load_open: bool,
    save_as_open: bool,
    advisor_open: bool,
    transit_lines_open: bool,
    #[cfg(feature = "multiplayer")]
    network_open: bool,
}

impl GUIWindows {
    pub fn menu(&mut self, uiw: &UiWorld) {
        if button_primary("Economy").show().clicked {
            self.economy_open ^= true;
        }

        if button_primary("Budget").show().clicked {
            self.budget_open ^= true;
        }

        if button_primary("City Stats").show().clicked {
            self.citystats_open ^= true;
        }

        if button_primary("Transit").show().clicked {
            self.transit_lines_open ^= true;
        }

        if button_primary("Settings").show().clicked {
            self.settings_open ^= true;
        }

        if crate::network::is_singleplayer(uiw) && button_primary("Load").show().clicked {
            session::set_screen(uiw, SessionScreen::LoadGame);
        }

        #[cfg(feature = "multiplayer")]
        if button_primary("Network").show().clicked {
            self.network_open ^= true;
        }
    }

    pub fn render(&mut self, uiworld: &UiWorld, sim: &Simulation) {
        profiling::scope!("windows::render");
        if uiworld
            .write::<InputMap>()
            .just_act
            .contains(&InputAction::OpenEconomyMenu)
        {
            self.economy_open ^= true;
        }

        economy::economy(uiworld, sim, &mut self.economy_open);
        budget::budget(uiworld, sim, &mut self.budget_open);
        citystats::citystats(uiworld, sim, &mut self.citystats_open);
        transit_lines::transit_lines(uiworld, &mut self.transit_lines_open);
        settings::settings(uiworld, sim, &mut self.settings_open);
        stub_window("Save As", &mut self.save_as_open);
        stub_window("Advisor", &mut self.advisor_open);

        #[cfg(feature = "multiplayer")]
        network::network(uiworld, sim, &mut self.network_open);
    }

    pub fn additional_menu(&mut self, savable: bool) {
        if savable && button_primary("Save As").show().clicked {
            self.save_as_open ^= true;
        }
        if button_primary("Advisor").show().clicked {
            self.advisor_open ^= true;
        }
    }

    pub fn render_session(&mut self, uiw: &UiWorld, sim: &Simulation) {
        let screen = *uiw.read::<SessionScreen>();
        let (title, opened) = match screen {
            SessionScreen::MainMenu => ("Egregoria", &mut self.main_menu_open),
            SessionScreen::NewGame => ("New Game", &mut self.new_game_open),
            SessionScreen::LoadGame => ("Load", &mut self.load_open),
            SessionScreen::Playing => return,
        };
        *opened = true;
        Window {
            title: title.into(),
            pad: Pad::all(14.0),
            radius: 10.0,
            opened,
            child_spacing: 8.0,
        }
        .show(|| {
            if screen == SessionScreen::MainMenu {
                super::menu::main_menu(uiw, &mut self.settings_open);
            } else if button_primary("Back").show().clicked {
                session::set_screen(uiw, SessionScreen::MainMenu);
            }
        });
        if !*opened && screen != SessionScreen::MainMenu {
            session::set_screen(uiw, SessionScreen::MainMenu);
        }
        settings::settings(uiw, sim, &mut self.settings_open);
    }

    pub(crate) fn window_states(&mut self) -> impl Iterator<Item = (&'static str, &mut bool)> {
        [
            ("Economy", &mut self.economy_open),
            ("Budget", &mut self.budget_open),
            ("City Stats", &mut self.citystats_open),
            ("Settings", &mut self.settings_open),
            ("Save As", &mut self.save_as_open),
            ("Advisor", &mut self.advisor_open),
            ("Transit", &mut self.transit_lines_open),
            #[cfg(feature = "multiplayer")]
            ("Network", &mut self.network_open),
        ]
        .into_iter()
    }
}

fn stub_window(title: &'static str, opened: &mut bool) {
    Window {
        title: title.into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 8.0,
    }
    .show(|| {});
}
