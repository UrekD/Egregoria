use yakui::widgets::{List, Pad};
use yakui::{
    constrained, opaque, reflow, row, spacer, Alignment, Color, Constraints, CrossAxisAlignment,
    Dim2, MainAxisAlignment, MainAxisSize, Pivot, Vec2,
};

use goryak::{
    blur_bg, button_primary, button_secondary, constrained_viewport, icon_button, monospace,
    on_secondary_container, padx, padxy, secondary_container, tooltip,
};
use prototypes::GameTime;
use simulation::Simulation;

use crate::gui::windows::settings::Settings;
use crate::gui::GuiState;
use crate::inputmap::{Bindings, InputAction, InputMap};
use crate::network::NetworkState;
use crate::uiworld::UiWorld;

const SPEEDS: [(u32, &str, Option<&str>, InputAction); 6] = [
    (0, "Pause", Some("pause"), InputAction::PausePlay),
    (1, "1×", Some("play"), InputAction::Speed1),
    (3, "3×", Some("forward"), InputAction::Speed3),
    (8, "8×", None, InputAction::Speed8),
    (20, "20×", None, InputAction::Speed20),
    (50, "50×", None, InputAction::Speed50),
];

fn set_warp(warp: &mut u32, depause_warp: &mut u32, requested: u32) {
    if requested == 0 {
        if *warp == 0 {
            *warp = *depause_warp;
        } else {
            *depause_warp = *warp;
            *warp = 0;
        }
    } else {
        *warp = requested;
    }
}

impl GuiState {
    pub(crate) fn apply_time_shortcut(uiworld: &UiWorld) {
        let input = uiworld.read::<InputMap>();
        if let Some((requested, _, _, _)) = SPEEDS
            .iter()
            .find(|(_, _, _, action)| input.just_act.contains(action))
        {
            set_warp(
                &mut uiworld.write::<Settings>().time_warp,
                &mut uiworld.write::<GuiState>().depause_warp,
                *requested,
            );
        }
    }
}

pub fn time_controls(uiworld: &UiWorld, sim: &Simulation) {
    profiling::scope!("hud::time_controls");
    let time = sim.read::<GameTime>().daytime;
    let warp = &mut uiworld.write::<Settings>().time_warp;
    let mut gui = uiworld.write::<GuiState>();
    let depause_warp = &mut gui.depause_warp;
    let network = uiworld.read::<NetworkState>();
    let actual_warp = match &*network {
        NetworkState::Singleplayer(step) => step.achieved_warp(*warp),
        #[cfg(feature = "multiplayer")]
        NetworkState::Client(_) | NetworkState::Server(_) => None,
    };
    let bindings = uiworld.read::<Bindings>();

    let paused = *warp == 0;
    // Keep the control widgets in place when the pause outline changes.
    yakui::canvas(move |ctx| {
        if paused {
            let w = ctx.layout.viewport().size().length() * 0.002;
            yakui::shapes::outline(
                ctx.paint,
                ctx.layout.viewport(),
                w,
                Color::rgba(255, 0, 0, 128),
            );
        }
    });

    let time_text = || {
        padx(5.0, || {
            row(|| {
                monospace(on_secondary_container(), format!("Day {}", time.day));
                spacer(1);
                monospace(
                    on_secondary_container(),
                    format!("{:02}:{:02}:{:02}", time.hour, time.minute, time.second),
                );
            });
        });
        let mut l = List::row();
        l.main_axis_alignment = MainAxisAlignment::SpaceBetween;
        l.show(|| {
            for (speed, label, icon, action) in &SPEEDS {
                let selected = *warp == *speed;
                let text = icon.unwrap_or(label);
                let mut b = if selected {
                    button_primary(text)
                } else {
                    button_secondary(text)
                };
                if icon.is_some() {
                    b = icon_button(b);
                }
                b.padding = Pad::balanced(7.0, 3.0);

                let mut label = label.to_string();
                if let Some(binding) = bindings
                    .0
                    .get(action)
                    .filter(|binding| !binding.0.is_empty())
                {
                    label.push_str(&format!(" ({binding})"));
                }
                if let Some(actual) =
                    actual_warp.filter(|actual| selected && *actual < f64::from(*speed) * 0.9)
                {
                    label.push_str(&format!(" · ≈{actual:.1}× actual"));
                }
                if tooltip(label, || b.show()).clicked {
                    set_warp(warp, depause_warp, *speed);
                }
            }
        });
    };

    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(-10.0, 10.0),
        || {
            constrained_viewport(|| {
                let mut l = List::row();
                l.main_axis_alignment = MainAxisAlignment::End;
                l.show(|| {
                    opaque(|| {
                        blur_bg(secondary_container().with_alpha(0.5), 10.0, || {
                            padxy(10.0, 5.0, || {
                                constrained(
                                    Constraints::loose(Vec2::new(220.0, f32::INFINITY)),
                                    || {
                                        let mut l = List::column();
                                        l.cross_axis_alignment = CrossAxisAlignment::Stretch;
                                        l.main_axis_size = MainAxisSize::Min;
                                        l.item_spacing = 5.0;
                                        l.show(time_text);
                                    },
                                );
                            });
                        });
                    });
                });
            });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qf1_pause_toggle_restores_previous_speed() {
        let mut warp = 20;
        let mut depause_warp = 1;
        set_warp(&mut warp, &mut depause_warp, 0);
        assert_eq!(warp, 0);
        set_warp(&mut warp, &mut depause_warp, 0);
        assert_eq!(warp, 20);
    }

    #[test]
    fn qf1_speed_shortcuts_apply_with_the_hud_hidden() {
        let mut uiw = UiWorld::default();
        uiw.insert(GuiState {
            hidden: true,
            ..GuiState::default()
        });
        uiw.insert(Settings::default());
        uiw.insert(InputMap::default());

        uiw.write::<InputMap>()
            .just_act
            .insert(InputAction::Speed50);
        GuiState::apply_time_shortcut(&uiw);
        assert_eq!(uiw.read::<Settings>().time_warp, 50);

        uiw.write::<InputMap>().just_act.clear();
        uiw.write::<InputMap>()
            .just_act
            .insert(InputAction::PausePlay);
        GuiState::apply_time_shortcut(&uiw);
        assert_eq!(uiw.read::<Settings>().time_warp, 0);
        GuiState::apply_time_shortcut(&uiw);
        assert_eq!(uiw.read::<Settings>().time_warp, 50);
    }
}
