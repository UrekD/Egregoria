use yakui::widgets::{List, Pad};
use yakui::{
    constrained, opaque, reflow, row, spacer, Alignment, Color, Constraints, CrossAxisAlignment,
    Dim2, MainAxisAlignment, MainAxisSize, Pivot, Vec2,
};

use common::timestep::MAX_WARP;
use goryak::{
    blur_bg, button_primary, button_secondary, constrained_viewport, icon_button, monospace,
    on_secondary_container, padx, padxy, secondary_container, tooltip,
};
use prototypes::GameTime;
use simulation::Simulation;

use crate::gui::windows::settings::Settings;
use crate::gui::GuiState;
use crate::inputmap::{Bindings, InputAction, InputCombinations, InputMap};
use crate::network::NetworkState;
use crate::uiworld::UiWorld;

/// Padding around the clock and the speed row.
const PANEL_PAD: f32 = 10.0;
/// Fits the seven speed buttons, MAX included, with a gap between each.
const PANEL_WIDTH: f32 = 260.0;

const SPEEDS: [(u32, &str, Option<&str>, InputAction); 7] = [
    (0, "Pause", Some("pause"), InputAction::PausePlay),
    (1, "1×", Some("play"), InputAction::Speed1),
    (3, "3×", Some("forward"), InputAction::Speed3),
    (8, "8×", None, InputAction::Speed8),
    (20, "20×", None, InputAction::Speed20),
    (50, "50×", None, InputAction::Speed50),
    (MAX_WARP, "MAX", None, InputAction::SpeedMax),
];

fn speed_tooltip(
    label: &str,
    binding: Option<&InputCombinations>,
    speed: u32,
    selected: bool,
    actual: Option<f64>,
) -> String {
    let mut text = label.to_string();
    if let Some(binding) = binding.filter(|binding| !binding.0.is_empty()) {
        text.push_str(&format!(" ({binding})"));
    }
    if let Some(actual) = shortfall(speed, actual).filter(|_| selected) {
        text.push_str(&format!(" · ≈{actual:.1}× actual"));
    }
    text
}

/// The achieved speed when it is worth reporting for `warp`.
fn shortfall(warp: u32, actual: Option<f64>) -> Option<f64> {
    // MAX has no target speed, so it always reports what it reaches.
    actual.filter(|actual| warp == MAX_WARP || *actual < f64::from(warp) * 0.9)
}

/// The achieved speed shown next to the clock.
fn achieved_readout(warp: u32, actual: Option<f64>) -> Option<String> {
    shortfall(warp, actual)
        .filter(|_| warp != 0)
        .map(|actual| format!("≈{actual:.0}×"))
}

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
    let readout = achieved_readout(*warp, actual_warp);

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
                if let Some(readout) = &readout {
                    monospace(on_secondary_container(), readout.clone());
                    spacer(1);
                }
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

                let label =
                    speed_tooltip(label, bindings.0.get(action), *speed, selected, actual_warp);
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
                            padxy(PANEL_PAD, 5.0, || {
                                constrained(
                                    Constraints::loose(Vec2::new(PANEL_WIDTH, f32::INFINITY)),
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
    fn ux2_ladder_ends_with_max_on_its_own_shortcut() {
        let speeds: Vec<_> = SPEEDS.iter().map(|(speed, ..)| *speed).collect();
        assert_eq!(speeds, [0, 1, 3, 8, 20, 50, MAX_WARP]);
        assert!(matches!(
            SPEEDS.last(),
            Some((_, "MAX", None, InputAction::SpeedMax))
        ));

        let mut uiw = UiWorld::default();
        uiw.insert(GuiState::default());
        uiw.insert(Settings::default());
        uiw.insert(InputMap::default());
        for action in [InputAction::SpeedMax, InputAction::PausePlay] {
            uiw.write::<InputMap>().just_act.clear();
            uiw.write::<InputMap>().just_act.insert(action);
            GuiState::apply_time_shortcut(&uiw);
        }
        assert_eq!(uiw.read::<Settings>().time_warp, 0);
        GuiState::apply_time_shortcut(&uiw);
        assert_eq!(uiw.read::<Settings>().time_warp, MAX_WARP);
    }

    #[test]
    fn ux2_max_tooltip_always_reports_the_achieved_speed() {
        let binding = Bindings::default();
        let binding = binding.0.get(&InputAction::SpeedMax);
        assert_eq!(
            speed_tooltip("MAX", binding, MAX_WARP, true, Some(312.04)),
            "MAX (6) · ≈312.0× actual"
        );
        assert_eq!(
            speed_tooltip("MAX", binding, MAX_WARP, false, Some(312.0)),
            "MAX (6)"
        );
        assert_eq!(
            speed_tooltip("MAX", binding, MAX_WARP, true, None),
            "MAX (6)"
        );
        assert_eq!(speed_tooltip("8×", None, 8, true, Some(7.9)), "8×");
        assert_eq!(
            speed_tooltip("8×", None, 8, true, Some(4.0)),
            "8× · ≈4.0× actual"
        );
    }

    #[test]
    fn ux2a_clock_shows_the_achieved_speed_when_it_falls_short() {
        assert_eq!(
            achieved_readout(MAX_WARP, Some(312.04)).as_deref(),
            Some("≈312×")
        );
        assert_eq!(achieved_readout(50, Some(31.6)).as_deref(), Some("≈32×"));
        assert_eq!(achieved_readout(50, Some(48.0)), None);
        assert_eq!(achieved_readout(MAX_WARP, None), None);
        assert_eq!(achieved_readout(0, Some(0.0)), None);
    }

    #[test]
    fn uxfix_speed_row_with_max_keeps_its_padding_and_gaps() {
        use simulation::Simulation;
        use yakui::font::{Font, FontSettings, Fonts};
        use yakui::widgets::{ButtonWidget, OpaqueWidget};
        use yakui::{Rect, WidgetId, Yakui};

        fn rects<T: yakui::widget::Widget>(ui: &Yakui, id: WidgetId, out: &mut Vec<Rect>) {
            let node = ui.dom().get(id).unwrap();
            if node.widget.is::<T>() {
                out.push(ui.layout_dom().get(id).unwrap().rect);
            }
            for &child in &node.children {
                rects::<T>(ui, child, out);
            }
        }

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new(false);
        let uiworld = UiWorld::with_defaults();
        for (width, height) in [(1024.0, 768.0), (1280.0, 720.0)] {
            let mut ui = Yakui::new();
            let fonts = ui.dom().get_global_or_init(Fonts::default);
            for (name, bytes) in [
                (
                    "icons",
                    include_bytes!("../../../../assets/font_awesome_solid_900.otf").as_slice(),
                ),
                (
                    "monospace",
                    include_bytes!("../../../../assets/SpaceMono-Regular.ttf").as_slice(),
                ),
            ] {
                fonts.add(
                    Font::from_bytes(bytes, FontSettings::default()).unwrap(),
                    Some(name),
                );
            }
            let viewport = Vec2::new(width, height);
            ui.set_surface_size(viewport);
            ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));
            ui.start();
            time_controls(&uiworld, &sim);
            ui.finish();

            let mut panels = Vec::new();
            rects::<OpaqueWidget>(&ui, ui.dom().root(), &mut panels);
            let [panel] = panels.as_slice() else {
                panic!("expected one clock panel, got {panels:?}");
            };
            assert!(panel.max().x <= width);
            let mut buttons = Vec::new();
            rects::<ButtonWidget>(&ui, ui.dom().root(), &mut buttons);
            assert_eq!(buttons.len(), SPEEDS.len());
            buttons.sort_by(|a, b| a.pos().x.total_cmp(&b.pos().x));
            for pair in buttons.windows(2) {
                assert!(
                    pair[1].pos().x - pair[0].max().x >= 2.0,
                    "speed buttons touch at {width}: {pair:?}"
                );
            }
            let left = buttons[0].pos().x - panel.pos().x;
            let right = panel.max().x - buttons[SPEEDS.len() - 1].max().x;
            assert!(
                right >= PANEL_PAD - 0.5 && (left - right).abs() < 1.0,
                "MAX crowds the panel edge at {width}: {left} left, {right} right"
            );
        }
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
