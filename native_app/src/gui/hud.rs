use std::time::{Duration, Instant};

use goryak::{button_primary, error, image_button, minrow, on_secondary_container, textc, Window};
use prototypes::ItemID;
use yakui::{reflow, Alignment, Color, Dim2, Pivot, Vec2};

use simulation::Simulation;

use crate::gui::hud::menu::{menu_bar_view, render_departure};
use crate::gui::hud::time_controls::time_controls_at;
use crate::gui::inspect::new_inspector;
use crate::gui::textures::UiTextures;
use crate::gui::windows::settings::Settings;
use crate::gui::{GuiState, Tool};
use crate::rendering::world_snapshot::HudSnapshot;
use crate::session;
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

pub(crate) use demand_bars::DemandSnapshot;
pub(crate) use problem_icons::ProblemMarker;
pub(crate) use toolbox::ToolboxSnapshot;

pub(crate) fn capture_problem_markers(
    sim: &Simulation,
    camera: &geom::Camera,
    generation: u64,
    map_epoch: u64,
) -> Vec<ProblemMarker> {
    problem_icons::capture(sim, camera, generation, map_epoch)
}

/// Panels that resolve live detail or author commands are built at a
/// simulation boundary. The ordinary HUD consumes owned display inputs.
pub(crate) fn needs_world_widgets(uiw: &UiWorld, sim: &Simulation) -> bool {
    needs_live_world(uiw) || (session::world_interaction(uiw) && chat::needs_world(uiw, sim))
}

pub(crate) fn needs_live_world(uiw: &UiWorld) -> bool {
    if !session::playing(uiw) {
        return true;
    }
    if session::modal(uiw) {
        return false;
    }
    if uiw.read::<SaveLoadState>().is_replaying() {
        return true;
    }
    let mut gui = uiw.write::<GuiState>();
    if gui.hidden {
        return false;
    }
    let open_window = gui.windows.window_states().any(|(_, open)| *open);
    let input = uiw.read::<crate::inputmap::InputMap>();
    let panel_shortcut = [
        crate::inputmap::InputAction::OpenEconomyMenu,
        crate::inputmap::InputAction::OpenDebugMenu,
        crate::inputmap::InputAction::OpenChat,
    ]
    .iter()
    .any(|action| input.just_act.contains(action));
    drop(input);
    open_window
        || gui.debug_window
        || *uiw.read::<Tool>() != Tool::Hand
        || uiw
            .read::<crate::debug_gui::debug_window::DebugState>()
            .debug_inspector
        || uiw.read::<super::InspectedEntity>().e.is_some()
        || uiw.read::<super::InspectedBuilding>().e.is_some()
        || uiw.read::<super::InspectedRoad>().0.is_some()
        || uiw.read::<super::ErrorTooltip>().msg.is_some()
        || uiw.read::<ToastState>().advisor_requested()
        || panel_shortcut
}

pub(crate) fn observe_hud(uiw: &UiWorld, sim: &Simulation) {
    if session::world_interaction(uiw) {
        let visible = !uiw.read::<GuiState>().hidden;
        if visible {
            toolbox::update_transit_tools(uiw, sim);
        }
        uiw.write::<ToastState>()
            .observe(sim, visible, Instant::now());
    }
}

pub(crate) fn render_newgui(
    uiworld: &UiWorld,
    sim: Option<&Simulation>,
    hud: &HudSnapshot,
    badges_interactive: bool,
    yakui_width: f32,
) {
    profiling::scope!("hud::render");
    if !session::world_interaction(uiworld) {
        render_departure(uiworld);
        return;
    }
    if session::can_save(uiworld) {
        auto_save(uiworld);
    }
    if uiworld.read::<GuiState>().hidden {
        return;
    }

    yakui::column(|| {
        yakui::column(|| problem_icons::render(uiworld, &hud.markers, badges_interactive));
        toolbox::render(uiworld, sim, hud.toolbox, yakui_width);
        menu_bar_view(uiworld, sim, hud.treasury);
        if !session::world_interaction(uiworld) {
            render_departure(uiworld);
            return;
        }
        demand_bars::render(&hud.demand);
        if let Some(sim) = sim {
            chat::chat(uiworld, sim);
            new_inspector(uiworld, sim);
            let mut gui = uiworld.write::<GuiState>();
            let show_unfinished_ui = gui.show_unfinished_ui;
            gui.windows.render(uiworld, sim, show_unfinished_ui);
        }
        time_controls_at(uiworld, hud.time.daytime);
        yakui::column(|| toasts::render(uiworld));
        keybinds::keybind_modal(uiworld);
        startup_save_message(uiworld);
    });
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
