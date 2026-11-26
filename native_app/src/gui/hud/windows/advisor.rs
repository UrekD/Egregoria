use crate::gui::follow::FollowEntity;
use crate::gui::hud::toasts::{publish, severity_color, AdvisorFocus, ToastState};
use crate::gui::inspect::focus_building;
use crate::gui::notifications::{AlertTarget, Notification, NotificationCenter, NotificationKind};
use crate::gui::overlays::OverlayMode;
use crate::gui::Tool;
use crate::uiworld::UiWorld;
use geom::Vec3;
use goryak::{
    mincolumn, minrow, on_secondary_container, outline, padxy, primary, primary_link, round_rect,
    textc, VertScrollSize, Window,
};
use prototypes::DayTime;
use simulation::economy::progression::progression_status;
use simulation::map::{BuildingID, Map};
use simulation::map_dynamic::Transit;
use simulation::Simulation;
use yakui::widgets::Pad;
use yakui::{constrained, divider, Constraints, Vec2};

pub(super) const AVAILABLE: bool = true;

pub(super) fn render(uiw: &UiWorld, sim: &Simulation, opened: &mut bool) {
    if uiw.write::<ToastState>().take_advisor_request() {
        *opened = true;
    }
    let mut navigated = false;
    Window {
        title: "Advisor".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 6.0,
    }
    .show(|| {
        let status = progression_status(sim);
        if let Some((milestone, needed, unlocks)) = status.next {
            textc(
                on_secondary_container(),
                format!(
                    "Next goal · Milestone {milestone} ({} / {needed} citizens)",
                    status.population
                ),
            );
            for unlock in unlocks {
                textc(on_secondary_container(), format!("Unlocks {unlock}"));
            }
        }

        publish(uiw);
        let center = uiw.read::<NotificationCenter>();
        let alerts = uiw.read::<ToastState>();
        if center.iter().len() == 0 {
            textc(on_secondary_container(), "No notifications yet");
            return;
        }
        let map = sim.map();
        let transit = sim.read::<Transit>();
        constrained(
            Constraints {
                min: Vec2::new(320.0, 0.0),
                max: Vec2::new(390.0, f32::INFINITY),
            },
            || {
                VertScrollSize::Fixed(335.0).show(|| {
                    let entries: Vec<_> = center
                        .iter()
                        .rev()
                        .map(|notice| {
                            let (text, target) = alerts.advisor_details(notice);
                            (notice, text, target)
                        })
                        .collect();
                    let focused = alerts.advisor_focus().and_then(|focus| {
                        focused_entry(
                            entries
                                .iter()
                                .map(|(notice, _, target)| (notice.kind, notice.daytime, *target)),
                            focus,
                        )
                    });
                    // The focused entry is pinned first so it is visible without scrolling.
                    let order = focused
                        .into_iter()
                        .chain((0..entries.len()).filter(|&index| Some(index) != focused));
                    mincolumn(8.0, || {
                        for (position, index) in order.enumerate() {
                            if position != 0 {
                                divider(outline().with_alpha(0.4), 2.0, 1.0);
                            }
                            let (notice, text, target) = entries[index];
                            let mut show = || {
                                navigated |= entry(uiw, &map, &transit, notice, text, target);
                            };
                            if Some(index) == focused {
                                round_rect(6.0, primary().with_alpha(0.15), || {
                                    padxy(6.0, 4.0, show);
                                });
                            } else {
                                show();
                            }
                        }
                    });
                });
            },
        );
    });
    if navigated {
        *opened = false;
    }
    if !*opened {
        uiw.write::<ToastState>().clear_advisor_focus();
    }
}

/// Shows one notification. Returns whether it moved the camera to its target.
fn entry(
    uiw: &UiWorld,
    map: &Map,
    transit: &Transit,
    notice: &Notification,
    text: &str,
    target: AlertTarget,
) -> bool {
    let mut navigated = false;
    mincolumn(3.0, || {
        let time = notice.daytime;
        minrow(12.0, || {
            textc(severity_color(notice.severity), notice.kind.label());
            textc(
                on_secondary_container(),
                format!("Day {} {:02}:{:02}", time.day, time.hour, time.minute),
            );
        });
        textc(on_secondary_container(), text.to_owned());
        match resolve_target(map, transit, target) {
            TargetLink::Building(id, door) => {
                if primary_link("Go to building") {
                    if *uiw.read::<Tool>() == Tool::SpecialBuilding {
                        *uiw.write::<Tool>() = Tool::Hand;
                    }
                    focus_building(uiw, id, door);
                    navigated = true;
                }
            }
            TargetLink::Line(stop) => {
                if primary_link("Go to line") {
                    uiw.write::<FollowEntity>().0 = None;
                    uiw.camera_mut().targetpos = stop;
                    *uiw.write::<OverlayMode>() = OverlayMode::Transit;
                    navigated = true;
                }
            }
            TargetLink::Removed => textc(on_secondary_container(), "(removed)"),
            TargetLink::NoLocation => {}
        }
    });
    navigated
}

/// Index of the entry `focus` refers to, among entries listed newest first: the newest
/// matching notification, preferring one about the focused target.
fn focused_entry(
    entries: impl IntoIterator<Item = (NotificationKind, DayTime, AlertTarget)>,
    focus: AdvisorFocus,
) -> Option<usize> {
    let mut first = None;
    for (index, (kind, daytime, target)) in entries.into_iter().enumerate() {
        if kind != focus.kind || focus.daytime.is_some_and(|focused| focused != daytime) {
            continue;
        }
        if focus.target == Some(target) {
            return Some(index);
        }
        first.get_or_insert(index);
    }
    first
}

enum TargetLink {
    Building(BuildingID, Vec3),
    Line(Vec3),
    Removed,
    NoLocation,
}

fn resolve_target(map: &Map, transit: &Transit, target: AlertTarget) -> TargetLink {
    match target {
        AlertTarget::Building(id) => map.get(id).map_or(TargetLink::Removed, |building| {
            TargetLink::Building(id, building.door_pos)
        }),
        AlertTarget::Line(id) => {
            let Some(line) = transit.lines.get(id) else {
                return TargetLink::Removed;
            };
            line.stops
                .iter()
                .find_map(|id| transit.stops.get(*id))
                .map_or(TargetLink::NoLocation, |stop| TargetLink::Line(stop.pos))
        }
        AlertTarget::CityWide => TargetLink::NoLocation,
    }
}

#[cfg(test)]
#[test]
fn ux2_advisor_focuses_the_clicked_notification() {
    use slotmapd::KeyData;
    use NotificationKind::*;

    let day = |hour| DayTime {
        day: 1,
        hour,
        minute: 0,
        second: 0,
    };
    let house = |n| AlertTarget::Building(BuildingID::from(KeyData::from_ffi(n)));
    // Newest first, as the advisor lists them.
    let entries = [
        (NoWater, day(9), AlertTarget::CityWide),
        (Fire, day(8), house(1)),
        (Blackout, day(7), house(2)),
        (Fire, day(6), house(3)),
        (Fire, day(5), house(1)),
    ];
    let focus = |kind, daytime, target| AdvisorFocus {
        kind,
        daytime,
        target,
    };
    // A toast names its notification.
    assert_eq!(
        focused_entry(entries, focus(Fire, Some(day(5)), Some(house(1)))),
        Some(4)
    );
    // The live target may have changed since the toast was shown.
    assert_eq!(
        focused_entry(entries, focus(Fire, Some(day(6)), Some(house(2)))),
        Some(3)
    );
    // A problem icon names its building, or falls back to the newest of its kind.
    assert_eq!(
        focused_entry(entries, focus(Fire, None, Some(house(3)))),
        Some(3)
    );
    assert_eq!(
        focused_entry(entries, focus(Fire, None, Some(house(9)))),
        Some(1)
    );
    assert_eq!(focused_entry(entries, focus(NoSewage, None, None)), None);
}

#[cfg(test)]
#[test]
fn ux2_clicks_queue_the_advisor_until_it_opens() {
    let mut state = ToastState::default();
    assert!(!state.take_advisor_request());
    let focus = AdvisorFocus {
        kind: NotificationKind::Fire,
        daytime: None,
        target: None,
    };
    state.open_advisor(focus);
    assert!(state.take_advisor_request());
    assert!(!state.take_advisor_request(), "opened twice");
    assert_eq!(state.advisor_focus(), Some(focus));
    state.clear_advisor_focus();
    assert_eq!(state.advisor_focus(), None);
}

#[cfg(test)]
#[test]
fn p4g_advisor_resolves_removed_targets_safely() {
    use scenarios::ScenarioCtx;
    use simulation::SimulationOptions;

    let mut test = ScenarioCtx::new(SimulationOptions {
        terrain_size: 1,
        save_replay: false,
        ..SimulationOptions::LEGACY
    });
    scenarios::builders::grid_city(&mut test, geom::vec2(150.0, 150.0), 2, 2, 160.0);
    let house = scenarios::builders::fill_houses(&mut test, 1)[0];
    let target = AlertTarget::Building(house);
    let door = test.sim.map().get(house).unwrap().door_pos;
    let resolve =
        |sim: &Simulation, target| resolve_target(&sim.map(), &sim.read::<Transit>(), target);
    assert!(
        matches!(resolve(&test.sim, target), TargetLink::Building(id, pos) if id == house && pos == door)
    );
    let quote = simulation::souls::demolition::demolition_quote(&test.sim, house).unwrap().quote;
    test.sim.apply_commands([&simulation::world_command::WorldCommand::MapStartDemolition { site: house, quote }]);
    test.run_ticks(u64::from(quote.minutes) * prototypes::TICKS_PER_MINUTE);
    assert!(matches!(resolve(&test.sim, target), TargetLink::Removed));
    assert!(matches!(
        resolve(&test.sim, AlertTarget::Line(Default::default())),
        TargetLink::Removed
    ));
}
