use geom::{vec2, Intersect, Vec2, AABB};
use goryak::{minrow, padxy, pass_pointer, round_rect, secondary_container};
use ordered_float::OrderedFloat;
use simulation::map::{BuildingID, ProjectFilter};
use simulation::map_dynamic::{ElectricityFlow, SiteUtilityReader, UtilityStatus};
use simulation::souls::events::Fires;
use simulation::Simulation;
use yakui::event::{EventInterest, EventResponse, WidgetEvent};
use yakui::input::MouseButton;
use yakui::widget::{EventContext, Widget};
use yakui::{reflow, Alignment, Color, Dim2, Pivot, TextureId};

use super::toasts::{AdvisorFocus, ToastState};
use crate::gui::notifications::{AlertTarget, NotificationKind};
use crate::gui::overlays::view;
use crate::gui::textures::UiTextures;
use crate::gui::Tool;
use crate::uiworld::UiWorld;

const COMPACT_BADGE_DEPTH: f32 = 550.0;
const BLACKOUT_ICON_SIZE: f32 = 14.0;
const BADGE_TEXT_SIZE: f32 = 11.0;
const BADGE_ROW_SPACING: f32 = 3.0;
const BADGE_RADIUS: f32 = 5.0;

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

    /// The advisor notification that reports this problem. No notification reports a
    /// disconnected building, so its badge has nothing to focus.
    fn notification(self) -> Option<NotificationKind> {
        match self {
            Self::Fire => Some(NotificationKind::Fire),
            Self::Blackout => Some(NotificationKind::Blackout),
            Self::NoWater => Some(NotificationKind::NoWater),
            Self::NoSewage => Some(NotificationKind::NoSewage),
            Self::Disconnected => None,
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
    fn advisor_focus(&self) -> Option<AdvisorFocus> {
        Some(AdvisorFocus {
            kind: self.kind.notification()?,
            daytime: None,
            target: Some(AlertTarget::Building(self.building)),
        })
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
    let mut utilities = SiteUtilityReader::new(sim);
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

        let Some(status) = utilities.status(id) else {
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
    // Other tools drag from a press on the world, so only the hand takes badge clicks.
    let hand = *uiworld.read::<Tool>() == Tool::Hand;
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
                let focus = badge.advisor_focus().filter(|_| hand);
                if show_badge(&badge, count, no_power_img, focus.is_some()) {
                    clicked = focus;
                }
            },
        );
    }
    if let Some(focus) = clicked {
        uiworld.write::<ToastState>().open_advisor(focus);
    }
}

/// Shows one badge. Returns whether a clickable badge was clicked.
/// Badges float over the world, so world tools act through them: cancel, placement and
/// bulldozing never see them as UI. Only the click target takes its left press.
fn show_badge(badge: &Badge, count: usize, no_power_img: TextureId, clickable: bool) -> bool {
    pass_pointer(|| show_badge_content(badge, count, no_power_img, clickable))
}

fn show_badge_content(
    badge: &Badge,
    count: usize,
    no_power_img: TextureId,
    clickable: bool,
) -> bool {
    let fill = secondary_container().with_alpha(0.93);
    let content = || {
        padxy(4.0, 1.0, || {
            if badge.kind == BadgeKind::Blackout {
                minrow(BADGE_ROW_SPACING, || {
                    let size = yakui::Vec2::splat(BLACKOUT_ICON_SIZE);
                    let mut image = yakui::widgets::Image::new(no_power_img, size);
                    image.color = Color::WHITE.with_alpha(0.7);
                    image.show();
                    if count > 1 {
                        yakui::widgets::Text::new(BADGE_TEXT_SIZE, format!("+{}", count - 1))
                            .show();
                    }
                });
            } else {
                let (label, short, color) = badge.kind.appearance();
                let mut text = if count > 1 {
                    yakui::widgets::Text::new(BADGE_TEXT_SIZE, format!("{} +{}", short, count - 1))
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
    };
    if !clickable {
        round_rect(BADGE_RADIUS, fill, content);
        return false;
    }

    let dom = yakui::context::dom();
    let hit = dom.begin_widget::<BadgeHitWidget>(());
    let color = if hit.hovering {
        secondary_container()
    } else {
        fill
    };
    round_rect(BADGE_RADIUS, color, content);
    dom.end_widget::<BadgeHitWidget>(hit.id);
    hit.clicked
}

#[derive(Clone, Copy, Debug, Default)]
struct BadgeHit {
    hovering: bool,
    clicked: bool,
}

/// The click target of a badge. Badges float over the world, so it takes a left press on
/// the badge itself and lets everything else through: hover, the wheel, other buttons and
/// releases still reach the world underneath.
#[derive(Debug, Default)]
struct BadgeHitWidget(BadgeHit);

impl Widget for BadgeHitWidget {
    type Props<'a> = ();
    type Response = BadgeHit;

    fn new() -> Self {
        Self::default()
    }

    fn update(&mut self, _: Self::Props<'_>) -> Self::Response {
        BadgeHit {
            clicked: std::mem::take(&mut self.0.clicked),
            ..self.0
        }
    }

    fn event_interest(&self) -> EventInterest {
        EventInterest::MOUSE_INSIDE
    }

    fn event(&mut self, _: EventContext<'_>, event: &WidgetEvent) -> EventResponse {
        match event {
            WidgetEvent::MouseEnter => self.0.hovering = true,
            WidgetEvent::MouseLeave => self.0.hovering = false,
            WidgetEvent::MouseButtonChanged {
                button: MouseButton::One,
                down: true,
                inside: true,
                ..
            } => {
                self.0.clicked = true;
                return EventResponse::Sink;
            }
            _ => {}
        }
        EventResponse::Bubble
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
    fn uxfix_badges_focus_only_the_notification_of_their_own_problem() {
        use NotificationKind as N;
        for (kind, notification) in [
            (BadgeKind::Fire, Some(N::Fire)),
            (BadgeKind::Blackout, Some(N::Blackout)),
            (BadgeKind::NoWater, Some(N::NoWater)),
            (BadgeKind::NoSewage, Some(N::NoSewage)),
            // No notification reports a disconnected building.
            (BadgeKind::Disconnected, None),
        ] {
            let badge = Badge {
                point: Vec2::ZERO,
                depth: 100.0,
                kind,
                building: BuildingID::default(),
            };
            let focus = badge.advisor_focus();
            assert_eq!(focus.map(|focus| focus.kind), notification, "{kind:?}");
            assert!(focus.is_none_or(|focus| focus.daytime.is_none()
                && focus.target == Some(AlertTarget::Building(badge.building))));
        }
    }

    /// Lays out one badge at the centre of a small viewport. Returns its rect and
    /// whether it reported a click.
    fn badge_frame(ui: &mut yakui::Yakui, kind: BadgeKind, clickable: bool) -> (yakui::Rect, bool) {
        let badge = Badge {
            point: vec2(100.0, 50.0),
            depth: 100.0,
            kind,
            building: BuildingID::default(),
        };
        ui.start();
        let mut clicked = false;
        let reflowed = reflow(
            Alignment::TOP_LEFT,
            Pivot::BOTTOM_CENTER,
            Dim2::pixels(100.0, 50.0),
            || clicked = show_badge(&badge, 1, TextureId::User(0), clickable),
        );
        goryak::render_tooltip();
        ui.finish();
        let badge = ui.dom().get(reflowed.id).unwrap().children[0];
        let rect = ui.layout_dom().get(badge).unwrap().rect;
        (rect, clicked)
    }

    #[test]
    fn uxfix_badges_take_only_a_left_click_on_themselves() {
        use yakui::event::Event;
        use yakui::input::MouseButton;

        let button = |button, down| Event::MouseButtonChanged { button, down };
        for (kind, clickable) in [
            (BadgeKind::Fire, true),
            (BadgeKind::Fire, false),
            (BadgeKind::NoWater, true),
        ] {
            let mut ui = yakui::Yakui::new();
            let viewport = yakui::Vec2::new(200.0, 100.0);
            ui.set_surface_size(viewport);
            ui.set_unscaled_viewport(yakui::Rect::from_pos_size(yakui::Vec2::ZERO, viewport));
            let (rect, _) = badge_frame(&mut ui, kind, clickable);
            assert!(rect.size().x > 0.0 && rect.size().x < 120.0, "{rect:?}");
            let centre = rect.pos() + rect.size() * 0.5;
            ui.handle_event(Event::CursorMoved(Some(centre)));
            badge_frame(&mut ui, kind, clickable);

            // Camera drags, zoom and right-click cancel reach the world.
            for button_event in [
                button(MouseButton::Two, true),
                button(MouseButton::Two, false),
                button(MouseButton::Three, true),
                button(MouseButton::Three, false),
            ] {
                let shown = format!("{button_event:?}");
                assert!(!ui.handle_event(button_event), "{kind:?} sank {shown}");
            }
            assert!(!ui.handle_event(Event::MouseScroll {
                delta: yakui::Vec2::new(0.0, 1.0)
            }));

            let sunk = ui.handle_event(button(MouseButton::One, true));
            assert_eq!(sunk, clickable, "{kind:?} left press");
            assert!(!ui.handle_event(button(MouseButton::One, false)));
            assert_eq!(badge_frame(&mut ui, kind, clickable).1, clickable);

            // Beside the badge nothing is taken.
            let beside = rect.pos() + yakui::Vec2::new(rect.size().x + 3.0, rect.size().y * 0.5);
            ui.handle_event(Event::CursorMoved(Some(beside)));
            badge_frame(&mut ui, kind, clickable);
            assert!(!ui.handle_event(button(MouseButton::One, true)));
            assert!(!ui.handle_event(button(MouseButton::One, false)));
        }
    }

    #[test]
    fn finalnative_badges_never_block_cancel_placement_or_bulldozing() {
        use crate::inputmap::{Bindings, InputAction, InputMap};
        use engine::{InputContext, MouseButton};

        let right_button = |input_map: &mut InputMap, held: bool, screen: yakui::Vec2| {
            let mut input = InputContext::default();
            if held {
                input.mouse.pressed.insert(MouseButton::Right);
            }
            input.mouse.screen = vec2(screen.x, screen.y);
            input_map.prepare_frame(&input, true, true);
        };
        for (kind, clickable) in [
            (BadgeKind::Fire, true),
            (BadgeKind::Fire, false),
            (BadgeKind::Blackout, true),
            (BadgeKind::Blackout, false),
            (BadgeKind::Disconnected, false),
        ] {
            let mut ui = yakui::Yakui::new();
            let viewport = yakui::Vec2::new(200.0, 100.0);
            ui.set_surface_size(viewport);
            ui.set_unscaled_viewport(yakui::Rect::from_pos_size(yakui::Vec2::ZERO, viewport));
            let (rect, _) = badge_frame(&mut ui, kind, clickable);
            let centre = rect.pos() + rect.size() * 0.5;
            ui.handle_event(yakui::event::Event::CursorMoved(Some(centre)));
            badge_frame(&mut ui, kind, clickable);
            ui.paint();

            // Placement and bulldozing test the same point before acting.
            assert!(
                !goryak::pointer_over_ui(centre.x, centre.y),
                "{kind:?} clickable={clickable} blocks the world"
            );

            let mut input_map = InputMap::default();
            input_map.build_input_tree(&mut Bindings::default());
            right_button(&mut input_map, true, centre);
            right_button(&mut input_map, false, centre);
            assert!(
                input_map.just_act.contains(&InputAction::Cancel),
                "{kind:?} clickable={clickable} swallowed a right click"
            );
        }
    }

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
