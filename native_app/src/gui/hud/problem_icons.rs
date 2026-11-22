use geom::{vec2, Intersect, Vec2, AABB};
use goryak::{interact_box_radius, minrow, padxy, secondary_container};
use ordered_float::OrderedFloat;
use simulation::map::{BuildingID, BuildingKind, ProjectFilter};
use simulation::map_dynamic::{ElectricityFlow, UtilityFlow, UtilityStatus};
use simulation::souls::{
    events::Fires,
    residence::{self, Residences},
};
use simulation::Simulation;
use yakui::{opaque, reflow, Alignment, Color, Dim2, Pivot};

use super::toasts::{AdvisorFocus, ToastState};
use crate::gui::notifications::{AlertTarget, NotificationKind};
use crate::gui::overlays::view;
use crate::gui::textures::UiTextures;
use crate::uiworld::UiWorld;

const COMPACT_BADGE_DEPTH: f32 = 550.0;
const BLACKOUT_ICON_SIZE: f32 = 14.0;
const BADGE_TEXT_SIZE: f32 = 11.0;
const BADGE_ROW_SPACING: f32 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BadgeKind {
    Fire,
    Blackout,
    Disconnected,
    NoWater,
    NoSewage,
}

impl BadgeKind {
    fn appearance(self) -> (&'static str, &'static str, Color) {
        match self {
            Self::Fire => ("Fire", "F", Color::rgb(255, 153, 96)),
            Self::Blackout => ("", "", Color::WHITE),
            Self::Disconnected => ("Disconnected", "D", Color::rgb(210, 195, 255)),
            Self::NoWater => ("No water", "W", Color::rgb(135, 216, 255)),
            Self::NoSewage => ("No sewage", "S", Color::rgb(255, 212, 132)),
        }
    }

    /// The advisor notification that reports this problem.
    fn notification(self) -> NotificationKind {
        match self {
            Self::Fire => NotificationKind::Fire,
            Self::Blackout => NotificationKind::Blackout,
            Self::Disconnected | Self::NoWater => NotificationKind::NoWater,
            Self::NoSewage => NotificationKind::NoSewage,
        }
    }
}

struct Badge {
    point: Vec2,
    depth: f32,
    kind: BadgeKind,
    building: BuildingID,
}

impl Badge {
    fn advisor_focus(&self) -> AdvisorFocus {
        AdvisorFocus {
            kind: self.kind.notification(),
            daytime: None,
            target: Some(AlertTarget::Building(self.building)),
        }
    }
}

impl Badge {
    fn footprint(&self, count_digits: usize) -> AABB {
        // Reserve room for the count before clustering so the placed badge can grow in place.
        let glyph_width = 7.0;
        let width = if self.kind == BadgeKind::Blackout {
            BLACKOUT_ICON_SIZE + BADGE_ROW_SPACING + glyph_width * (1 + count_digits) as f32
        } else {
            let (label, short, _) = self.kind.appearance();
            let single = if self.depth < COMPACT_BADGE_DEPTH {
                label
            } else {
                short
            };
            glyph_width * single.len().max(short.len() + 2 + count_digits) as f32
        };
        let half_width = 5.0 + width * 0.5;
        AABB::new_ll_ur(vec2(-half_width, -19.0), vec2(half_width, 1.0))
    }
}

struct PlacedBadge {
    badge: Badge,
    point: Vec2,
    bounds: AABB,
    count: usize,
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
    let screen = AABB::new_ll_ur(Vec2::ZERO, vec2(camera.viewport_w, camera.viewport_h));
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
        let (point, depth) = camera.project(center.z(building.height + 27.0));
        if !depth.is_finite() || depth <= 0.0 || !point.is_finite() || !screen.contains(point) {
            continue;
        }

        if fires.by_building.contains_key(&id) {
            visible.push(Badge {
                point,
                depth,
                kind: BadgeKind::Fire,
                building: id,
            });
        }
        if map
            .electricity
            .net_id(id)
            .is_some_and(|net| power.blackout(net))
        {
            visible.push(Badge {
                point,
                depth,
                kind: BadgeKind::Blackout,
                building: id,
            });
        }

        if building.kind == BuildingKind::House && !residence::uses_water(sim, id) {
            continue;
        }
        let Some(status) = utilities.status(&map, &residences, id) else {
            continue;
        };
        let kind = match (status.water, status.sewage) {
            (UtilityStatus::Disconnected, _) => BadgeKind::Disconnected,
            (UtilityStatus::Shortage, _) => BadgeKind::NoWater,
            (_, UtilityStatus::Shortage) => BadgeKind::NoSewage,
            _ => continue,
        };
        visible.push(Badge {
            point,
            depth,
            kind,
            building: id,
        });
    }

    let no_power_img = uiworld.read::<UiTextures>().get("no_power");
    let mut clicked = None;
    for PlacedBadge {
        badge,
        point,
        count,
        ..
    } in place_badges(visible, screen)
    {
        reflow(
            Alignment::TOP_LEFT,
            Pivot::BOTTOM_CENTER,
            Dim2::pixels(point.x, point.y),
            || {
                let fill = secondary_container().with_alpha(0.93);
                let hover = secondary_container();
                let mut response = None;
                opaque(|| {
                    response = Some(interact_box_radius(fill, hover, hover, 5.0, || {
                        padxy(4.0, 1.0, || {
                            if badge.kind == BadgeKind::Blackout {
                                minrow(BADGE_ROW_SPACING, || {
                                    let size = yakui::Vec2::splat(BLACKOUT_ICON_SIZE);
                                    let mut image = yakui::widgets::Image::new(no_power_img, size);
                                    image.color = Color::WHITE.with_alpha(0.7);
                                    image.show();
                                    if count > 1 {
                                        yakui::widgets::Text::new(
                                            BADGE_TEXT_SIZE,
                                            format!("+{}", count - 1),
                                        )
                                        .show();
                                    }
                                });
                            } else {
                                let (label, short, color) = badge.kind.appearance();
                                let mut text = if count > 1 {
                                    yakui::widgets::Text::new(
                                        BADGE_TEXT_SIZE,
                                        format!("{} +{}", short, count - 1),
                                    )
                                } else {
                                    let label = if badge.depth < COMPACT_BADGE_DEPTH {
                                        label
                                    } else {
                                        short
                                    };
                                    yakui::widgets::Text::new(BADGE_TEXT_SIZE, label)
                                };
                                text.style.color = color;
                                text.show();
                            }
                        });
                    }));
                });
                if response.is_some_and(|response| response.clicked) {
                    clicked = Some(badge.advisor_focus());
                }
            },
        );
    }
    if let Some(focus) = clicked {
        uiworld.write::<ToastState>().open_advisor(focus);
    }
}

fn place_badges(mut visible: Vec<Badge>, screen: AABB) -> Vec<PlacedBadge> {
    visible.sort_by_key(|badge| (badge.kind != BadgeKind::Fire, OrderedFloat(badge.depth)));

    let count_digits = visible.len().max(1).ilog10() as usize + 1;
    let mut placed: Vec<PlacedBadge> = Vec::new();
    for badge in visible {
        let footprint = badge.footprint(count_digits);
        let center = screen.center() - footprint.center();
        let point = badge
            .point
            .max((screen.ll - footprint.ll).min(center))
            .min((screen.ur - footprint.ur).max(center));

        if badge.depth >= COMPACT_BADGE_DEPTH {
            let bounds = footprint.offset(point);
            if let Some(anchor) = placed.iter_mut().find(|anchor| {
                anchor.badge.kind == badge.kind
                    && anchor.badge.depth >= COMPACT_BADGE_DEPTH
                    && bounds.intersects(&anchor.bounds)
            }) {
                anchor.count += 1;
                continue;
            }
        }

        let free = [
            vec2(0.0, 0.0),
            vec2(0.0, -22.0),
            vec2(-28.0, -22.0),
            vec2(28.0, -22.0),
            vec2(0.0, -44.0),
            vec2(0.0, 24.0),
        ]
        .into_iter()
        .find_map(|offset| {
            let point = point + offset;
            let bounds = footprint.offset(point);
            (screen.contains(bounds.ll)
                && screen.contains(bounds.ur)
                && !placed
                    .iter()
                    .any(|anchor| bounds.intersects(&anchor.bounds)))
            .then_some((point, bounds))
        });

        let (point, bounds) = match free {
            Some(position) => position,
            None => {
                if let Some(anchor) = placed.iter_mut().min_by_key(|anchor| {
                    (
                        anchor.badge.kind != badge.kind,
                        anchor.badge.kind == BadgeKind::Fire,
                        OrderedFloat(point.distance2(anchor.point)),
                    )
                }) {
                    anchor.count += 1;
                    continue;
                }
                // Even a viewport smaller than the first badge gets a cluster anchor.
                (point, footprint.offset(point))
            }
        };
        placed.push(PlacedBadge {
            badge,
            point,
            bounds,
            count: 1,
        });
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_take_priority_and_collisions_cluster() {
        let point = vec2(80.0, 25.0);
        let screen = AABB::new_ll_ur(Vec2::ZERO, vec2(160.0, 60.0));
        let mut visible = vec![
            Badge {
                point,
                depth: 100.0,
                kind: BadgeKind::NoWater,
                building: BuildingID::default(),
            },
            Badge {
                point,
                depth: 100.0,
                kind: BadgeKind::Blackout,
                building: BuildingID::default(),
            },
        ];
        visible.extend((0..3).map(|_| Badge {
            point,
            depth: 200.0,
            kind: BadgeKind::Fire,
            building: BuildingID::default(),
        }));

        let placed = place_badges(visible, screen);
        assert_eq!(placed[0].point, point);
        assert_eq!(placed.len(), 2);
        assert!(placed
            .iter()
            .all(|anchor| anchor.badge.kind == BadgeKind::Fire));
        assert_eq!(placed.iter().map(|anchor| anchor.count).sum::<usize>(), 5);
        assert!(!placed[0].bounds.intersects(&placed[1].bounds));
    }

    #[test]
    fn compact_blackouts_cluster_and_leave_room_for_utility_badges() {
        let screen = AABB::new_ll_ur(Vec2::ZERO, vec2(160.0, 90.0));
        let placed = place_badges(
            vec![
                Badge {
                    point: vec2(80.0, 45.0),
                    depth: 1200.0,
                    kind: BadgeKind::Blackout,
                    building: BuildingID::default(),
                },
                Badge {
                    point: vec2(80.0, 45.0),
                    depth: 1200.0,
                    kind: BadgeKind::Blackout,
                    building: BuildingID::default(),
                },
                Badge {
                    point: vec2(110.0, 45.0),
                    depth: 1300.0,
                    kind: BadgeKind::NoWater,
                    building: BuildingID::default(),
                },
            ],
            screen,
        );

        assert_eq!(placed.len(), 2);
        assert_eq!(placed[0].badge.kind, BadgeKind::Blackout);
        assert_eq!(placed[0].count, 2);
        assert_eq!(placed[1].badge.kind, BadgeKind::NoWater);
        assert!(!placed[0].bounds.intersects(&placed[1].bounds));
    }
}
