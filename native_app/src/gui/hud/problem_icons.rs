use goryak::{padxy, round_rect, secondary_container};
use ordered_float::OrderedFloat;
use simulation::map::ProjectFilter;
use simulation::map_dynamic::{UtilityFlow, UtilityStatus};
use simulation::souls::{events::Fires, residence::Residences};
use simulation::Simulation;
use yakui::{reflow, Alignment, Color, Dim2, Pivot};

use crate::gui::overlays::view;
use crate::uiworld::UiWorld;

pub(super) fn render(uiworld: &UiWorld, sim: &Simulation) {
    let map = sim.map();
    let utilities = sim.read::<UtilityFlow>();
    let residences = sim.read::<Residences>();
    let fires = sim.read::<Fires>();
    let camera = uiworld.camera().camera;
    let Some(bounds) = view::bounds(&camera, &map) else {
        return;
    };
    let mut visible = Vec::new();

    for id in map
        .spatial_map()
        .query(bounds, ProjectFilter::BUILDING)
        .filter_map(|project| project.as_building())
    {
        let Some(building) = map.get(id) else {
            continue;
        };
        let center = building.obb.center();
        let bob = (uiworld.time_always() + center.mag() * 0.05).cos();
        let (point, depth) = camera.project(center.z(building.height + 27.0 + bob));
        if !depth.is_finite()
            || depth <= 0.0
            || !point.is_finite()
            || !(0.0..camera.viewport_w).contains(&point.x)
            || !(0.0..camera.viewport_h).contains(&point.y)
        {
            continue;
        }

        let (label, short, color) = if fires.by_building.contains_key(&id) {
            ("Fire", "F", Color::rgb(255, 153, 96))
        } else {
            let Some(status) = utilities.status(&map, &residences, id) else {
                continue;
            };
            match (status.water, status.sewage) {
                (UtilityStatus::Disconnected, _) => {
                    ("Disconnected", "D", Color::rgb(210, 195, 255))
                }
                (UtilityStatus::Unknown, _) => {
                    ("Awaiting utilities", "U", Color::rgb(210, 195, 255))
                }
                (UtilityStatus::Shortage, _) => ("No water", "W", Color::rgb(135, 216, 255)),
                (_, UtilityStatus::Shortage) => ("No sewage", "S", Color::rgb(255, 212, 132)),
                _ => continue,
            }
        };
        visible.push((point, depth, label, short, color));
    }

    // Keep fires and the nearest utility issues when the view is crowded.
    const MAX_BADGES: usize = 256;
    if visible.len() > MAX_BADGES {
        visible.select_nth_unstable_by_key(MAX_BADGES, |&(_, depth, _, short, _)| {
            (short != "F", OrderedFloat(depth))
        });
        visible.truncate(MAX_BADGES);
    }
    visible.sort_unstable_by_key(|&(_, depth, ..)| OrderedFloat(-depth));
    for (point, depth, label, short, color) in visible {
        reflow(
            Alignment::TOP_LEFT,
            Pivot::BOTTOM_CENTER,
            Dim2::pixels(point.x, point.y),
            || {
                round_rect(5.0, secondary_container().with_alpha(0.93), || {
                    padxy(4.0, 1.0, || {
                        let mut text = yakui::widgets::Text::new(
                            11.0,
                            if depth < 550.0 { label } else { short },
                        );
                        text.style.color = color;
                        text.show();
                    });
                });
            },
        );
    }
}
