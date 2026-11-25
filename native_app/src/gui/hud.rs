use std::time::{Duration, Instant};

use goryak::{button_primary, error, image_button, minrow, on_secondary_container, textc, Window};
use prototypes::ItemID;
use yakui::{reflow, Alignment, Color, Dim2, Pivot, Vec2};

use simulation::Simulation;

use crate::gui::hud::menu::{menu_bar, render_departure};
use crate::gui::hud::time_controls::time_controls;
use crate::gui::hud::toolbox::new_toolbox;
use crate::gui::inspect::new_inspector;
use crate::gui::textures::UiTextures;
use crate::gui::windows::settings::Settings;
use crate::gui::GuiState;
use crate::session::{self, SessionState};
use crate::uiworld::{SaveLoadState, UiWorld};

pub mod chat;
mod demand_bars;
pub mod keybinds;
mod menu;
mod problem_icons;
mod time_controls;
mod toasts;
pub mod toolbox;
pub mod windows;

pub(crate) use demand_bars::top_driver as top_demand_driver;
pub(crate) use toasts::ToastState;

/// Root GUI entrypoint
pub fn render_newgui(uiworld: &UiWorld, sim: &Simulation) {
    profiling::scope!("hud::render");
    if uiworld.read::<SessionState>().interaction_blocked {
        return;
    }
    if !session::playing(uiworld) {
        let mut gui = uiworld.write::<GuiState>();
        let show_unfinished_ui = gui.show_unfinished_ui;
        gui.windows.render_session(uiworld, sim, show_unfinished_ui);
        drop(gui);
        keybinds::keybind_modal(uiworld, sim);
        startup_save_message(uiworld);
        return;
    }
    if !session::world_interaction(uiworld) {
        render_departure(uiworld);
        return;
    }
    if session::can_save(uiworld) {
        auto_save(uiworld);
    }

    let hidden = uiworld.read::<GuiState>().hidden;
    uiworld
        .write::<ToastState>()
        .observe(sim, !hidden, Instant::now());
    if hidden {
        return;
    }

    yakui::column(|| {
        yakui::column(|| problem_icons::render(uiworld, sim));
        new_toolbox(uiworld, sim);
        menu_bar(uiworld, sim);
        if !session::world_interaction(uiworld) {
            render_departure(uiworld);
            return;
        }
        demand_bars::render(uiworld, sim);
        chat::chat(uiworld, sim);
        new_inspector(uiworld, sim);
        {
            let mut gui = uiworld.write::<GuiState>();
            let show_unfinished_ui = gui.show_unfinished_ui;
            gui.windows.render(uiworld, sim, show_unfinished_ui);
        }
        time_controls(uiworld, sim);
        yakui::column(|| toasts::render(sim, uiworld));
        keybinds::keybind_modal(uiworld, sim);
        startup_save_message(uiworld);
    });
    //goryak::debug_layout();
}

fn startup_save_message(uiworld: &UiWorld) {
    let mut state = uiworld.write::<SaveLoadState>();
    let Some(message) = state.startup_message.as_ref() else {
        return;
    };
    let mut opened = true;
    let mut dismissed = false;
    Window {
        title: "Save could not be loaded".into(),
        pad: yakui::widgets::Pad::all(10.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 10.0,
    }
    .show(|| {
        textc(error(), message.clone());
        dismissed = button_primary("Dismiss").show().clicked;
    });
    if dismissed || !opened {
        state.startup_message = None;
    }
}

fn auto_save(uiworld: &UiWorld) {
    let every: Option<Duration> = uiworld.read::<Settings>().auto_save_every.into();
    let mut gui = uiworld.write::<GuiState>();
    if let Some(every) = every {
        if gui.last_save.elapsed() > every {
            uiworld.write::<SaveLoadState>().please_save = true;
            gui.last_save = Instant::now();
        }
    }
}

pub fn item_icon_yakui(uiworld: &UiWorld, id: ItemID, multiplier: i32) {
    let item = id.prototype();
    minrow(5.0, || {
        if let Some(id) = uiworld
            .read::<UiTextures>()
            .try_get(&format!("icon/{}", item.name))
        {
            if image_button(
                id,
                Vec2::new(32.0, 32.0),
                Color::WHITE,
                Color::WHITE,
                Color::WHITE,
                "",
            )
            .hovering
            {
                reflow(Alignment::CENTER, Pivot::TOP_LEFT, Dim2::ZERO, || {
                    textc(
                        on_secondary_container(),
                        format!("{} x{}", item.name, multiplier),
                    );
                });
            }
        } else {
            textc(on_secondary_container(), format!("- {} ", &item.label));
        }
        textc(on_secondary_container(), format!("x{multiplier}"))
    });
}
