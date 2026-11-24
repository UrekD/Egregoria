use yakui::widgets::List;
use yakui::{column, CrossAxisAlignment, MainAxisAlignment, Vec2};

use goryak::{
    button_primary, button_secondary, fixed_spacer, mincolumn, minrow, on_secondary_container,
    padxy, primary_image_button, textc,
};
use simulation::map::TerraformKind;

use crate::gui::hud::toolbox::{select_triangle, updown_value};
use crate::gui::terraforming::{TerraformingResource, WaterMode, WaterTools};
use crate::gui::textures::UiTextures;
use crate::uiworld::UiWorld;

pub fn terraform_properties(uiw: &UiWorld) {
    let state = &mut *uiw.write::<TerraformingResource>();

    mincolumn(5.0, || {
        minrow(8.0, || {
            if choice("Terrain", !state.water_active()) {
                state.set_water_active(false);
            }
            if choice("Water", state.water_active()) {
                state.set_water_active(true);
            }
        });
        if state.water_active() {
            water_properties(&mut state.water);
        } else {
            terrain_properties(uiw, state);
        }
    });
}

fn terrain_properties(uiw: &UiWorld, state: &mut TerraformingResource) {
    padxy(0.0, 10.0, || {
        let mut l = List::row();
        l.main_axis_alignment = MainAxisAlignment::Center;
        l.cross_axis_alignment = CrossAxisAlignment::Center;
        l.item_spacing = 10.0;
        l.show(|| {
            let texs = uiw.read::<UiTextures>();

            let terraform_choices = &[
                (
                    TerraformKind::Elevation,
                    "Elevation (up/down)",
                    "terraforming_raise_lower",
                ),
                (TerraformKind::Smooth, "Smooth", "terraforming_smooth"),
                (TerraformKind::Level, "Level", "terraforming_level"),
                (TerraformKind::Slope, "Slope", "terraforming_slope"),
                (TerraformKind::Erode, "Erode", "terraforming_erode"),
            ];

            for (kind, label, icon) in terraform_choices {
                column(|| {
                    let enabled = state.kind == *kind;
                    if primary_image_button(texs.get(icon), Vec2::new(64.0, 64.0), enabled, *label)
                        .clicked
                    {
                        if state.kind != *kind {
                            state.cancel_gesture();
                            state.kind = *kind;
                        }
                    }

                    if enabled {
                        select_triangle(uiw);
                    }
                });
            }

            fixed_spacer((30.0, 0.0));

            let radius_choices = &[
                (200.0, "200m", "terraforming_radius_small"),
                (400.0, "400m", "terraforming_radius_medium"),
                (700.0, "700m", "terraforming_radius_large"),
            ];

            for (radius, label, icon) in radius_choices {
                column(|| {
                    let enabled = state.radius == *radius;
                    if primary_image_button(texs.get(icon), Vec2::new(64.0, 64.0), enabled, *label)
                        .clicked
                    {
                        state.radius = *radius;
                    }

                    if enabled {
                        select_triangle(uiw);
                    }
                });
            }

            let step = if state.radius < 150.0 {
                10.0
            } else if state.radius < 500.0 {
                50.0
            } else {
                100.0
            };

            updown_value(&mut state.radius, step, "m");

            fixed_spacer((30.0, 0.0));

            let amount_choices = &[
                (100.0, "Small", "terraforming_speed_low"),
                (300.0, "Medium", "terraforming_speed_medium"),
                (500.0, "Large", "terraforming_speed_large"),
            ];

            for (amount, label, icon) in amount_choices {
                column(|| {
                    let enabled = state.amount == *amount;
                    if primary_image_button(texs.get(icon), Vec2::new(64.0, 64.0), enabled, *label)
                        .clicked
                    {
                        state.amount = *amount;
                    }

                    if enabled {
                        select_triangle(uiw);
                    }
                });
            }

            updown_value(&mut state.amount, 100.0, "");
        });
    });
}

fn water_properties(state: &mut WaterTools) {
    use WaterMode::*;
    minrow(8.0, || {
        for (mode, label) in [
            (River, "Paint river"),
            (Lake, "Paint lake"),
            (Remove, "Remove water"),
            (Sea, "Sea level"),
        ] {
            if choice(label, state.mode == mode) {
                state.set_mode(mode);
            }
        }
    });
    match state.mode {
        River | Lake => {
            minrow(8.0, || {
                if state.mode == River {
                    textc(on_secondary_container(), "Width");
                    updown_value(&mut state.width_m, 16.0, " m");
                    state.width_m = state.width_m.clamp(32.0, 512.0);
                } else {
                    textc(on_secondary_container(), "Surface");
                    water_level(&mut state.lake_level_dm);
                }
                textc(on_secondary_container(), "Depth");
                updown_value(&mut state.depth_m, 1.0, " m");
                state.depth_m = state.depth_m.clamp(1.0, 512.0);
            });
            textc(
                on_secondary_container(),
                if state.mode == River {
                    "Drag from upstream to downstream."
                } else {
                    "Drag around the basin and release to close it."
                },
            );
        }
        Remove => {
            textc(
                on_secondary_container(),
                "Click a visible river or lake to drain it.",
            );
        }
        Sea => {
            minrow(8.0, || {
                textc(on_secondary_container(), "Sea surface");
                water_level(&mut state.sea_level_dm);
                if button_primary("Set sea level").show().clicked {
                    state.sea_action = Some(Some(state.sea_level_dm));
                }
                if state.current_sea_dm.is_some() && button_secondary("Clear sea").show().clicked {
                    state.sea_action = Some(None);
                }
            });
            let current = state.current_sea_dm.map_or_else(
                || "Sea is off".to_owned(),
                |level| format!("Current sea surface {:.1} m", f32::from(level) * 0.1),
            );
            textc(on_secondary_container(), current);
        }
    }
    if let Some(status) = &state.status {
        textc(on_secondary_container(), status.clone());
    }
}

fn water_level(level_dm: &mut i16) {
    if button_secondary("−1 m").show().clicked {
        *level_dm = level_dm.saturating_sub(10);
    }
    textc(
        on_secondary_container(),
        format!("{:.1} m", f32::from(*level_dm) / 10.0),
    );
    if button_secondary("+1 m").show().clicked {
        *level_dm = level_dm.saturating_add(10);
    }
}

fn choice(label: &str, selected: bool) -> bool {
    if selected {
        button_primary(label).show().clicked
    } else {
        button_secondary(label).show().clicked
    }
}
