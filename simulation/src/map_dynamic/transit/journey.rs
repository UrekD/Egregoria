use std::collections::BTreeMap;

use prototypes::{Money, Tick};
use serde::{Deserialize, Serialize};

use crate::economy::transit_stats;
use crate::utils::resources::Resources;
use crate::world::HumanID;

use super::{LineID, Router, Transit};

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub struct LineJourneyStats {
    pub transfers: u64,
    pub boardings: u64,
    pub wait_ticks: u64,
    pub max_wait_ticks: u64,
    pub alightings: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct JourneyStats {
    lines: BTreeMap<LineID, LineJourneyStats>,
    fares: BTreeMap<HumanID, FareWindow>,
}

#[derive(Serialize, Deserialize)]
struct FareWindow {
    first_board: Tick,
    highest_paid: Money,
}

impl FareWindow {
    fn charge(&mut self, now: Tick, fare: Money) -> Money {
        if now.0.saturating_sub(self.first_board.0) >= Transit::FARE_WINDOW_TICKS {
            self.first_board = now;
            self.highest_paid = Money::ZERO;
        }
        let delta = (fare - self.highest_paid).max(Money::ZERO);
        self.highest_paid = self.highest_paid.max(fare);
        delta
    }
}

impl JourneyStats {
    pub fn line(&self, line: LineID) -> LineJourneyStats {
        self.lines.get(&line).copied().unwrap_or_default()
    }

    pub(super) fn remove_line(&mut self, line: LineID) {
        self.lines.remove(&line);
    }

    pub(super) fn on_alight(&mut self, line: LineID) {
        self.lines.entry(line).or_default().alightings += 1;
    }

    pub(crate) fn forget(&mut self, human: HumanID) {
        self.fares.remove(&human);
    }

    pub(super) fn expire(&mut self, now: Tick) {
        self.fares.retain(|_, window| {
            now.0.saturating_sub(window.first_board.0) < Transit::FARE_WINDOW_TICKS
        });
    }

    pub(crate) fn credit(&self, human: HumanID, now: Tick) -> Option<(Tick, Money)> {
        self.fares.get(&human).and_then(|window| {
            (now.0.saturating_sub(window.first_board.0) < Transit::FARE_WINDOW_TICKS)
                .then_some((window.first_board, window.highest_paid))
        })
    }
}

pub(super) fn record_board(
    resources: &Resources,
    human: HumanID,
    router: &Router,
    line: LineID,
    fare: Money,
    now: Tick,
    since: Tick,
) {
    let wait = now.0.saturating_sub(since.0);
    let mut journeys = resources.write::<JourneyStats>();
    let counters = journeys.lines.entry(line).or_default();
    counters.boardings += 1;
    counters.wait_ticks += wait;
    counters.max_wait_ticks = counters.max_wait_ticks.max(wait);
    counters.transfers += u64::from(router.has_ridden_transit());
    let delta = journeys
        .fares
        .entry(human)
        .or_insert(FareWindow {
            first_board: now,
            highest_paid: Money::ZERO,
        })
        .charge(now, fare);
    transit_stats::on_board_with_fare(resources, line, delta, wait);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p8_journey_fare_window_includes_zero_and_expires_at_9000() {
        let mut fare = FareWindow {
            first_board: Tick(50),
            highest_paid: Money::ZERO,
        };
        assert_eq!(fare.charge(Tick(50), Money::ZERO), Money::ZERO);
        assert_eq!(
            fare.charge(Tick(8_049), Money::new_bucks(2)),
            Money::new_bucks(2)
        );
        assert_eq!(
            fare.charge(Tick(9_049), Money::new_bucks(3)),
            Money::new_bucks(1)
        );
        assert_eq!(
            fare.charge(Tick(9_050), Money::new_bucks(3)),
            Money::new_bucks(3)
        );
        assert_eq!(fare.charge(Tick(9_051), Money::new_bucks(2)), Money::ZERO);
    }
}
