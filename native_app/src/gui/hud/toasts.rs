use std::collections::VecDeque;
use std::time::{Duration, Instant};

use goryak::{
    blur_bg, constrained_viewport, error, interact_box_radius, mincolumn, on_secondary_container,
    padxy, primary, secondary_container, textc,
};
use prototypes::DayTime;
use simulation::Simulation;
use yakui::widgets::List;
use yakui::{opaque, reflow, Alignment, Color, Dim2, MainAxisAlignment, Pivot};

use crate::gui::notifications::{
    AlertTarget, Notification, NotificationCenter, NotificationKind, NotificationSampler, Severity,
};
use crate::uiworld::UiWorld;

const LIFETIME: Duration = Duration::from_secs(5);
const MAX_TOASTS: usize = 3;

/// The advisor entry a toast or problem icon refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdvisorFocus {
    pub kind: NotificationKind,
    /// Narrows the focus to one notification, when known.
    pub daytime: Option<DayTime>,
    /// Preferred among several matching notifications.
    pub target: Option<AlertTarget>,
}

#[derive(Default)]
pub(crate) struct ToastState {
    sampler: NotificationSampler,
    observed_at: Option<Instant>,
    notices: VecDeque<(Notification, Instant)>,
    advisor_request: Option<AdvisorFocus>,
    advisor_focus: Option<AdvisorFocus>,
}

impl ToastState {
    /// Open the advisor window on its next frame, focused on `focus`.
    pub(crate) fn open_advisor(&mut self, focus: AdvisorFocus) {
        self.advisor_request = Some(focus);
    }

    /// Moves a pending request into the focus. Returns whether the advisor should open.
    pub(super) fn take_advisor_request(&mut self) -> bool {
        let Some(focus) = self.advisor_request.take() else {
            return false;
        };
        self.advisor_focus = Some(focus);
        true
    }

    pub(super) fn advisor_focus(&self) -> Option<AdvisorFocus> {
        self.advisor_focus
    }

    pub(super) fn clear_advisor_focus(&mut self) {
        self.advisor_focus = None;
    }

    pub(super) fn advisor_details<'a>(
        &'a self,
        notice: &'a Notification,
    ) -> (&'a str, AlertTarget) {
        self.sampler.details(notice)
    }

    pub(crate) fn observe_applied(
        &mut self,
        sim: &Simulation,
        sequence: u64,
        rejections: &[(usize, &'static str, simulation::world_command::Rejection)],
    ) {
        self.sampler.observe_applied(sim, sequence, rejections);
    }

    pub(super) fn observe(&mut self, sim: &Simulation, visible: bool, now: Instant) {
        self.sampler.observe(sim);
        self.observed_at = visible.then_some(now);
        if visible {
            self.notices
                .retain(|(_, at)| now.duration_since(*at) < LIFETIME);
        } else {
            self.notices.clear();
        }
    }

    #[cfg(test)]
    pub(crate) fn track_test_world(&mut self, sim: &Simulation) {
        self.observe(sim, true, Instant::now());
    }

    #[cfg(test)]
    pub(crate) fn world_state_is_clear(&self) -> bool {
        self.sampler.is_clear()
            && self.observed_at.is_none()
            && self.notices.is_empty()
            && self.advisor_request.is_none()
            && self.advisor_focus.is_none()
    }
}

pub(in crate::gui) fn severity_color(severity: Severity) -> Color {
    match severity {
        Severity::Info => on_secondary_container(),
        Severity::Warning => primary(),
        Severity::Error => error(),
    }
}

pub(super) fn publish(uiworld: &UiWorld) {
    let mut state = uiworld.write::<ToastState>();
    let ToastState {
        sampler,
        observed_at,
        notices,
        ..
    } = &mut *state;
    let Some(now) = *observed_at else { return };
    let pending = sampler.drain();
    let toast_count = pending.len().min(MAX_TOASTS);
    let mut center = uiworld.write::<NotificationCenter>();
    for notice in pending {
        center.push(notice);
    }
    for notice in center.iter().rev().take(toast_count).rev() {
        if notices.len() == MAX_TOASTS {
            notices.pop_front();
        }
        let (text, target) = sampler.details(notice);
        let mut toast = notice.clone();
        toast.text = text.to_owned();
        toast.target = target;
        notices.push_back((toast, now));
    }
}

pub(super) fn render(_sim: &Simulation, uiworld: &UiWorld) {
    publish(uiworld);
    let state = uiworld.read::<ToastState>();
    let notices = &state.notices;
    if notices.is_empty() {
        return;
    }
    let mut clicked = None;

    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(-14.0, 80.0),
        || {
            constrained_viewport(|| {
                let mut row = List::row();
                row.main_axis_alignment = MainAxisAlignment::End;
                row.show(|| {
                    opaque(|| {
                        blur_bg(secondary_container().with_alpha(0.96), 8.0, || {
                            padxy(8.0, 4.0, || {
                                mincolumn(0.0, || {
                                    for (notice, _) in notices.iter().rev() {
                                        if toast(notice) {
                                            clicked = Some(AdvisorFocus {
                                                kind: notice.kind,
                                                daytime: Some(notice.daytime),
                                                target: Some(notice.target),
                                            });
                                        }
                                    }
                                });
                            });
                        });
                    });
                });
            });
        },
    );
    drop(state);
    if let Some(focus) = clicked {
        uiworld.write::<ToastState>().open_advisor(focus);
    }
}

/// A toast opens its entry in the advisor when clicked.
fn toast(notice: &Notification) -> bool {
    let hover = on_secondary_container().with_alpha(0.08);
    interact_box_radius(Color::CLEAR, hover, hover, 6.0, || {
        padxy(4.0, 4.0, || {
            mincolumn(2.0, || {
                textc(severity_color(notice.severity), notice.kind.label());
                textc(on_secondary_container(), notice.text.clone());
            });
        });
    })
    .clicked
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec2;
    use prototypes::{Tick, TICKS_PER_HOUR};
    use scenarios::{
        builders::{fill_houses, grid_city},
        ScenarioCtx,
    };
    use simulation::souls::events::{FireEvent, FireEventKind, Fires};

    use crate::gui::notifications::NotificationKind;
    use simulation::world_command::WorldCommand;
    use simulation::SimulationOptions;

    #[test]
    fn p2_ui_wire_demolition_after_fire_is_not_a_collapse() {
        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        let mut events = NotificationSampler::default();
        test.sim.write::<Fires>().by_building.insert(house, Tick(0));
        events.observe(&test.sim);
        test.sim.write::<Fires>().by_building.remove(&house);
        test.sim.write::<prototypes::GameTime>().tick = Tick(3 * TICKS_PER_HOUR + 1);
        events.observe(&test.sim);
        test.submit(&[WorldCommand::MapRemoveBuilding(house)]);
        test.run_ticks(1);
        assert!(test.sim.map().get(house).is_none());
        events.observe(&test.sim);
        assert!(!events
            .drain()
            .any(|item| item.kind == NotificationKind::FireCollapse));
    }

    #[test]
    fn p2_ui_wire_consumes_fire_events_once_even_in_the_same_tick() {
        let mut test = ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut test, vec2(150.0, 150.0), 2, 2, 160.0);
        let house = fill_houses(&mut test, 1)[0];
        let mut events = NotificationSampler::default();
        let record = |seq, kind| {
            test.sim.write::<Fires>().events.push_back(FireEvent {
                seq,
                tick: Tick(0),
                building: house,
                kind,
            });
        };
        record(1, FireEventKind::Ignited);
        events.observe(&test.sim);
        assert!(!events.drain().any(|item| matches!(
            item.kind,
            NotificationKind::Fire | NotificationKind::FireCollapse
        )));
        record(2, FireEventKind::Extinguished);
        record(3, FireEventKind::Ignited);
        record(4, FireEventKind::Collapsed);
        events.observe(&test.sim);
        assert_eq!(
            events.drain().map(|item| item.kind).collect::<Vec<_>>(),
            [NotificationKind::Fire, NotificationKind::FireCollapse]
        );
        events.observe(&test.sim);
        assert_eq!(events.drain().count(), 0);
    }
}
