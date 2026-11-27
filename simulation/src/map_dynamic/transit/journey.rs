use std::collections::BTreeMap;

use prototypes::{Money, Tick};
use serde::{Deserialize, Serialize};

use crate::economy::transit_stats;
use crate::utils::resources::Resources;
use crate::world::HumanID;

use super::service::ServiceVehicle;
use super::{LineID, Router, StopID, Transit};
use crate::map_dynamic::RoutingStep;
use crate::map_dynamic::router::PendingSearch;

const MAX_PENDING_PLANS: usize = 4096;

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
    #[serde(deserialize_with = "super::rail::bounded_map::<_, _, _, 16384>")]
    lines: BTreeMap<LineID, LineJourneyStats>,
    #[serde(deserialize_with = "super::rail::bounded_map::<_, _, _, 160000>")]
    fares: BTreeMap<HumanID, FareWindow>,
    #[serde(deserialize_with = "super::rail::bounded_map::<_, _, _, 16384>")]
    patterns: BTreeMap<LineID, u64>,
    #[serde(deserialize_with = "super::rail::bounded_map::<_, _, _, 160000>")]
    plans: BTreeMap<HumanID, JourneyPlan>,
    #[serde(deserialize_with = "super::rail::bounded_map::<_, _, _, MAX_PENDING_PLANS>")]
    pending: BTreeMap<HumanID, PendingSearch>,
    topology_revision: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PatternCall {
    line: LineID,
    pattern: u64,
    ordinal: u8,
    stop: StopID,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct ServiceOccurrence {
    pub vehicle: ServiceVehicle,
    pub sequence: u64,
    pub ordinal: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct BoardingKey {
    trip: Tick,
    leg: u8,
    arrival: ServiceOccurrence,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct PlannedRide {
    board: PatternCall,
    alight: PatternCall,
    calls: u8,
}

#[derive(Serialize, Deserialize)]
struct JourneyPlan {
    trip: Tick,
    rides: [Option<PlannedRide>; 2],
    leg: u8,
    boarding: Option<BoardingKey>,
}

impl JourneyPlan {
    fn ride(&self) -> Option<PlannedRide> {
        self.rides.get(usize::from(self.leg)).copied().flatten()
    }
}

#[derive(Serialize, Deserialize)]
struct FareWindow {
    first_board: Tick,
    highest_paid: Money,
    last_board: Option<BoardingKey>,
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
        self.patterns.remove(&line);
    }

    pub(super) fn on_alight(&mut self, line: LineID) {
        let counters = self.lines.entry(line).or_default();
        counters.alightings = counters.alightings.saturating_add(1);
    }

    pub(crate) fn forget(&mut self, human: HumanID) {
        self.fares.remove(&human);
        self.plans.remove(&human);
        self.pending.remove(&human);
    }

    pub(super) fn topology_changed(&mut self) {
        self.topology_revision += 1;
    }

    pub(crate) fn take_search(
        &mut self, human: HumanID, now: Tick, transit_revision: u64, origin: geom::Vec3,
        destination: geom::Vec3,
    ) -> (PendingSearch, bool) {
        let revision = (self.topology_revision, transit_revision);
        let old = self.pending.remove(&human);
        let resuming = old.is_some();
        let search = old.filter(|pending| {
            pending.revision == revision && pending.origin == origin && pending.destination == destination
        }).unwrap_or_else(|| PendingSearch::new(now, revision, origin, destination));
        (search, resuming)
    }

    pub(crate) fn defer_search(&mut self, human: HumanID, search: PendingSearch) -> bool {
        if self.pending.len() == MAX_PENDING_PLANS {
            return false;
        }
        self.pending.insert(human, search);
        true
    }

    pub(crate) fn forget_search(&mut self, human: HumanID) {
        self.pending.remove(&human);
    }

    pub(crate) fn discard_plan(&mut self, human: HumanID) {
        self.plans.remove(&human);
    }

    pub(super) fn pattern_changed(&mut self, line: LineID) {
        let revision = self.patterns.entry(line).or_default();
        *revision = revision.saturating_add(1);
    }

    pub(crate) fn install_plan(
        &mut self, human: HumanID, trip: Tick, steps: &[RoutingStep], transit: &Transit, ordinals: [Option<(u8, u8)>; 2],
    ) {
        let mut rides = steps.windows(2).filter_map(|pair| {
            let [RoutingStep::WaitAtStop { line, stop, .. }, RoutingStep::RideTransit { alight, .. }] = pair
            else {
                return None;
            };
            let line = transit.lines.get(*line)?;
            let pattern = self.patterns.get(&line.id).copied().unwrap_or_default();
            let call = |stop: StopID| {
                PatternCall { line: line.id, pattern, ordinal: 0, stop }
            };
            Some(PlannedRide { board: call(*stop), alight: call(*alight), calls: line.stops.len() as u8 })
        });
        let rides = ordinals.map(|ordinals| {
            let (board, alight) = ordinals?;
            let mut ride = rides.next()?;
            ride.board.ordinal = board;
            ride.alight.ordinal = alight;
            Some(ride)
        });
        if rides[0].is_some() {
            self.plans.insert(human, JourneyPlan { trip, rides, leg: 0, boarding: None });
        } else {
            self.plans.remove(&human);
        }
    }

    pub(super) fn valid_wait(
        &self, human: HumanID, line: LineID, stop: StopID, calls: &[StopID],
    ) -> bool {
        let Some(plan) = self.plans.get(&human) else {
            // Supported older snapshots and stock bus test routes have no companion plan.
            return true;
        };
        plan.boarding.is_none() && plan.ride().is_some_and(|ride| {
            ride.board.line == line && ride.board.stop == stop
                && ride.board.pattern == self.patterns.get(&line).copied().unwrap_or_default()
                && calls.get(usize::from(ride.board.ordinal)) == Some(&stop)
                && calls.get(usize::from(ride.alight.ordinal)) == Some(&ride.alight.stop)
        })
    }

    pub(super) fn boarding_ordinal(&self, human: HumanID) -> Option<u8> {
        Some(self.plans.get(&human)?.ride()?.board.ordinal)
    }

    pub(super) fn can_alight(
        &self, human: HumanID, call: ServiceOccurrence, line: LineID, stop: StopID,
    ) -> bool {
        let Some(plan) = self.plans.get(&human) else {
            return true;
        };
        let (Some(ride), Some(board)) = (plan.ride(), plan.boarding) else {
            return false;
        };
        if ride.alight.line != line || ride.alight.stop != stop || ride.alight.ordinal != call.ordinal
            || board.arrival.vehicle != call.vehicle
            || ride.alight.pattern != self.patterns.get(&line).copied().unwrap_or_default()
        {
            return false;
        }
        match call.vehicle {
            ServiceVehicle::Rail(_) => {
                let offset = (u64::from(ride.alight.ordinal) + u64::from(ride.calls)
                    - u64::from(ride.board.ordinal)) % u64::from(ride.calls);
                board.arrival.sequence.checked_add(offset) == Some(call.sequence)
            }
            ServiceVehicle::Bus(_) => board.arrival.sequence < call.sequence,
        }
    }

    pub(super) fn finish_ride(&mut self, human: HumanID, continued: bool) {
        if let Some(plan) = self.plans.get_mut(&human) {
            plan.boarding = None;
            plan.leg += 1;
            if !continued || plan.ride().is_none() {
                self.plans.remove(&human);
            }
        }
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
    occurrence: ServiceOccurrence,
) {
    let wait = now.0.saturating_sub(since.0);
    let mut journeys = resources.write::<JourneyStats>();
    let key = BoardingKey {
        trip: router.trip_start.unwrap_or(now),
        leg: journeys.plans.get(&human).map_or(0, |plan| plan.leg),
        arrival: occurrence,
    };
    if let Some(plan) = journeys.plans.get_mut(&human) {
        plan.boarding = Some(key);
    }
    if journeys.fares.get(&human).is_some_and(|fare| fare.last_board == Some(key)) {
        return;
    }
    let counters = journeys.lines.entry(line).or_default();
    counters.boardings = counters.boardings.saturating_add(1);
    counters.wait_ticks = counters.wait_ticks.saturating_add(wait);
    counters.max_wait_ticks = counters.max_wait_ticks.max(wait);
    counters.transfers = counters.transfers.saturating_add(u64::from(router.has_ridden_transit()));
    let window = journeys
        .fares
        .entry(human)
        .or_insert(FareWindow {
            first_board: now,
            highest_paid: Money::ZERO,
            last_board: None,
        });
    window.last_board = Some(key);
    let delta = window.charge(now, fare);
    transit_stats::on_board_with_fare(resources, line, delta, wait);
}

pub(crate) fn validate_journey_snapshot(sim: &crate::Simulation) -> Result<(), &'static str> {
    let stats = sim.read::<JourneyStats>();
    let world = sim.world();
    let now = super::elapsed_ticks(&sim.resources);
    let transit = sim.read::<Transit>();
    if stats.patterns.keys().chain(stats.lines.keys()).any(|line| !transit.lines.contains_key(*line)) {
        return Err("Orphaned saved passenger line statistics");
    }
    for (&id, fare) in &stats.fares {
        if !world.humans.contains_key(id)
            || fare.first_board > now
            || !(Money::ZERO..=Money::new_bucks(10)).contains(&fare.highest_paid)
            || fare.last_board.is_some_and(|key| key.trip > now || key.leg >= 2 || key.arrival.ordinal >= 30)
        {
            return Err("Invalid saved passenger fare");
        }
    }
    for (&id, plan) in &stats.plans {
        let human = world.humans.get(id).ok_or("Missing saved passenger journey owner")?;
        if plan.trip > now || plan.ride().is_none() || plan.rides[0].is_none()
            || plan.rides.iter().flatten().any(|ride| {
                !(2..=30).contains(&ride.calls)
                    || ride.board.line != ride.alight.line
                    || ride.board.pattern != ride.alight.pattern
                    || ride.board.ordinal >= ride.calls
                    || ride.alight.ordinal >= ride.calls
                    || ride.board.ordinal == ride.alight.ordinal
                    || ride.board.stop == ride.alight.stop
            })
            || plan.boarding.is_some_and(|key| {
                key.trip != plan.trip || key.leg != plan.leg
                    || Some(key.arrival.ordinal) != plan.ride().map(|ride| ride.board.ordinal)
                    || key.arrival.vehicle.location() != human.location
            })
        {
            return Err("Invalid saved passenger journey stage");
        }
    }
    for (&id, pending) in &stats.pending {
        if !world.humans.contains_key(id) || pending.born > now || !pending.valid() {
            return Err("Invalid saved pending passenger journey");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Color;
    use slotmapd::Key;

    #[test]
    fn p8_w2_reordered_pattern_invalidates_live_stop_ids() {
        let mut transit = Transit::default();
        let stops: Vec<_> = (1..=3).map(|offset| {
            let pos = geom::Vec3::X * offset as f32;
            transit.stops.insert_with_key(|id| super::super::TransitStop {
                id, pos, walk_pos: pos, berth: pos, dir: geom::Vec2::X,
            })
        }).collect();
        let line = transit.lines.insert_with_key(|id| super::super::TransitLine {
            id, stops: stops.clone(), target_vehicles: 1, fare: Money::ZERO,
            color: Color::WHITE, suspended: false,
        });
        let mut journeys = JourneyStats::default();
        let human = HumanID::null();
        let steps = [
            RoutingStep::WaitAtStop { stop: stops[0], line, since: Tick(1) },
            RoutingStep::RideTransit { alight: stops[1], line },
        ];
        journeys.install_plan(human, Tick(1), &steps, &transit, [Some((0, 1)), None]);
        assert!(journeys.valid_wait(human, line, stops[0], &stops));
        journeys.pattern_changed(line);
        assert!(!journeys.valid_wait(human, line, stops[0], &[stops[0], stops[2], stops[1]]));
        // Restoring the original call order cannot revive the first pattern.
        journeys.pattern_changed(line);
        assert!(!journeys.valid_wait(human, line, stops[0], &stops));
    }

    #[test]
    fn p8_journey_fare_window_includes_zero_and_expires_at_9000() {
        let mut fare = FareWindow {
            first_board: Tick(50),
            highest_paid: Money::ZERO,
            last_board: None,
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
