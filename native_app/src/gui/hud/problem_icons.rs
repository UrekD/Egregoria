use geom::{vec2, Intersect, Vec2, AABB};
use goryak::{padxy, round_rect, secondary_container};
use ordered_float::OrderedFloat;
use simulation::map::{BuildingKind, ProjectFilter};
use simulation::map_dynamic::{ElectricityFlow, UtilityFlow, UtilityStatus};
use simulation::souls::{
    events::Fires,
    residence::{self, Residences},
};
use simulation::Simulation;
use yakui::{reflow, Alignment, Color, Dim2, Pivot};

use crate::gui::overlays::view;
use crate::uiworld::UiWorld;

const COMPACT_BADGE_DEPTH: f32 = 550.0;

struct Badge {
    point: Vec2,
    depth: f32,
    label: &'static str,
    short: &'static str,
    color: Color,
    count: usize,
}

impl Badge {
    fn bounds(&self, point: Vec2) -> AABB {
        // Reserve room for a count even before another warning joins the badge.
        let half_width = if self.depth < COMPACT_BADGE_DEPTH {
            5.0 + self.label.len() as f32 * 3.5
        } else {
            23.0
        };
        AABB::new_ll_ur(
            point - vec2(half_width, 17.0),
            point + vec2(half_width, 1.0),
        )
    }
}

pub(super) fn render(uiworld: &UiWorld, sim: &Simulation) {
    let map = sim.map();
    let utilities = sim.read::<UtilityFlow>();
    let residences = sim.read::<Residences>();
    let fires = sim.read::<Fires>();
    let power = sim.read::<ElectricityFlow>();
    let camera = uiworld.camera().camera;
    let Some(bounds) = view::bounds(&camera, &map) else {
        return;
    };
    let mut visible = Vec::new();
    let mut power_icons = Vec::new();

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
        if map
            .electricity
            .net_id(id)
            .is_some_and(|net| power.blackout(net))
        {
            // The main HUD draws blackout icons at this height and scale.
            let (point, depth) = camera.project(center.z(building.height + 20.0 + bob));
            if depth.is_finite() && depth > 0.0 && point.is_finite() {
                let extent = Vec2::splat(5_000.0 / depth + 2.0);
                power_icons.push(AABB::new_ll_ur(point - extent, point + extent));
            }
        }
        let (point, depth) = camera.project(center.z(building.height + 27.0));
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
            if building.kind == BuildingKind::House && !residence::uses_water(sim, id) {
                continue;
            }
            let Some(status) = utilities.status(&map, &residences, id) else {
                continue;
            };
            match (status.water, status.sewage) {
                (UtilityStatus::Disconnected, _) => {
                    ("Disconnected", "D", Color::rgb(210, 195, 255))
                }
                (UtilityStatus::Shortage, _) => ("No water", "W", Color::rgb(135, 216, 255)),
                (_, UtilityStatus::Shortage) => ("No sewage", "S", Color::rgb(255, 212, 132)),
                _ => continue,
            }
        };
        visible.push(Badge {
            point,
            depth,
            label,
            short,
            color,
            count: 1,
        });
    }

    // Keep fires and the nearest utility issues when the view is crowded.
    const MAX_BADGES: usize = 256;
    let priority = |badge: &Badge| (badge.short != "F", OrderedFloat(badge.depth));
    if visible.len() > MAX_BADGES {
        visible.select_nth_unstable_by_key(MAX_BADGES, priority);
        visible.truncate(MAX_BADGES);
    }
    visible.sort_unstable_by_key(priority);

    let screen = AABB::new_ll_ur(Vec2::ZERO, vec2(camera.viewport_w, camera.viewport_h));
    let mut placed: Vec<(Badge, Vec2, AABB)> = Vec::new();
    for badge in visible {
        let nearby = placed.iter_mut().find(|(anchor, _, _)| {
            badge.depth >= COMPACT_BADGE_DEPTH
                && anchor.depth >= COMPACT_BADGE_DEPTH
                && badge
                    .bounds(badge.point)
                    .intersects(&anchor.bounds(anchor.point))
        });
        if let Some((anchor, _, _)) = nearby {
            anchor.count += 1;
            continue;
        }
        for offset in [
            vec2(0.0, 0.0),
            vec2(0.0, -18.0),
            vec2(-28.0, -18.0),
            vec2(28.0, -18.0),
            vec2(0.0, -36.0),
            vec2(0.0, 24.0),
        ] {
            let point = badge.point + offset;
            let bounds = badge.bounds(point);
            if screen.contains(bounds.ll)
                && screen.contains(bounds.ur)
                && !placed.iter().any(|(_, _, other)| bounds.intersects(other))
                && !power_icons.iter().any(|icon| bounds.intersects(icon))
            {
                placed.push((badge, point, bounds));
                break;
            }
        }
    }
    for (badge, point, _) in placed {
        reflow(
            Alignment::TOP_LEFT,
            Pivot::BOTTOM_CENTER,
            Dim2::pixels(point.x, point.y),
            || {
                round_rect(5.0, secondary_container().with_alpha(0.93), || {
                    padxy(4.0, 1.0, || {
                        let label = if badge.depth < COMPACT_BADGE_DEPTH {
                            badge.label
                        } else {
                            badge.short
                        };
                        let mut text = yakui::widgets::Text::new(11.0, label);
                        if badge.count > 1 {
                            text.text = format!("{} +{}", badge.short, badge.count - 1).into();
                        }
                        text.style.color = badge.color;
                        text.show();
                    });
                });
            },
        );
    }
}
