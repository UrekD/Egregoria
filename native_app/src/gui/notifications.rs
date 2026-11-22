use std::collections::VecDeque;

use prototypes::Tick;
use simulation::map::BuildingID;
use simulation::map_dynamic::LineID;

const CAPACITY: usize = 200;

#[allow(dead_code)] // Alert producers and readers land with the Advisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NotificationKind {
    CommandRejected,
    NegativeBudget,
    BudgetDeficit,
    Blackout,
    NoWater,
    NoSewage,
    Fire,
    SuspendedLine,
    HighUnemployment,
    StalledDemand,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Warning,
    Error,
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AlertTarget {
    Building(BuildingID),
    Line(LineID),
    CityWide,
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct Notification {
    pub kind: NotificationKind,
    pub severity: Severity,
    pub tick: Tick,
    pub target: AlertTarget,
    pub text: String,
}

#[derive(Default)]
pub(crate) struct NotificationCenter {
    items: VecDeque<Notification>,
}

impl NotificationCenter {
    #[allow(dead_code)]
    pub(crate) fn push(&mut self, notification: Notification) {
        if self.items.len() == CAPACITY {
            self.items.pop_front();
        }
        self.items.push_back(notification);
    }

    #[allow(dead_code)]
    pub(crate) fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = &Notification> + ExactSizeIterator {
        self.items.iter()
    }

    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
#[test]
fn p4_notification_history_evicts_oldest_and_clears() {
    let mut center = NotificationCenter::default();
    for tick in 0..=CAPACITY as u64 {
        center.push(Notification {
            kind: NotificationKind::Blackout,
            severity: Severity::Warning,
            tick: Tick(tick),
            target: AlertTarget::CityWide,
            text: "Blackout".into(),
        });
    }
    let ticks: Vec<_> = center.iter().map(|item| item.tick.0).collect();
    assert_eq!(ticks, (1..=CAPACITY as u64).collect::<Vec<_>>());
    center.clear();
    assert_eq!(center.iter().len(), 0);
}
