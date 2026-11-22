use serde::{Deserialize, Serialize};

use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::Router;
use crate::utils::resources::Resources;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TripMode {
    #[default]
    Walk,
    Car,
    Transit,
}

// The first three variants retain the serialized values of the original trip modes.
#[derive(Default, Serialize, Deserialize)]
pub(crate) enum TripProgress {
    #[default]
    Walk,
    Drove,
    RodeTransit,
    PlannedCar,
    PlannedTransit,
}

impl TripProgress {
    pub(crate) fn plan(&mut self, mode: TripMode) {
        if matches!(self, Self::Drove | Self::RodeTransit) {
            return;
        }
        *self = match mode {
            TripMode::Walk => Self::Walk,
            TripMode::Car => Self::PlannedCar,
            TripMode::Transit => Self::PlannedTransit,
        };
    }

    pub(crate) fn drove(&mut self) {
        if !matches!(self, Self::RodeTransit) {
            *self = Self::Drove;
        }
    }

    pub(crate) fn mode(&self) -> TripMode {
        match self {
            Self::Walk => TripMode::Walk,
            Self::Drove | Self::PlannedCar => TripMode::Car,
            Self::RodeTransit | Self::PlannedTransit => TripMode::Transit,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct TripModeStats {
    pub completed: u64,
    pub duration_ticks: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct TripStats {
    pub walk: TripModeStats,
    pub car: TripModeStats,
    pub transit: TripModeStats,
    /// Stop waits that exceeded the transit timeout.
    pub timeouts: u64,
    /// Passengers who abandoned transit after an expired wait.
    pub give_ups: u64,
}

pub(crate) fn on_start(router: &mut Router) {
    router.trip_start = None;
    router.trip_mode = TripProgress::Walk;
}

pub(crate) fn on_arrival(router: &mut Router, resources: &Resources) {
    let mode = std::mem::take(&mut router.trip_mode).mode();
    on_arrival_by_mode(router, resources, mode);
}

pub fn on_arrival_by_mode(router: &mut Router, resources: &Resources, mode: TripMode) {
    let Some(start) = router.trip_start.take() else {
        return;
    };
    let ticks = elapsed_ticks(resources).0.saturating_sub(start.0);
    let mut stats = resources.write::<TripStats>();
    let mode = match mode {
        TripMode::Walk => &mut stats.walk,
        TripMode::Car => &mut stats.car,
        TripMode::Transit => &mut stats.transit,
    };
    mode.completed += 1;
    mode.duration_ticks += ticks;
}

pub(crate) fn on_cancel(router: &mut Router) {
    on_start(router);
}

/// Record a wait that expired and forced the passenger to give up on transit.
/// The original trip clock remains active while a fallback route is attempted.
pub fn on_wait_timeout(resources: &Resources) {
    let mut stats = resources.write::<TripStats>();
    stats.timeouts += 1;
    stats.give_ups += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestCtx;
    use crate::{Simulation, INITIAL_TICK};
    use common::saveload::{Bincode, Encoder};
    use prototypes::{GameTime, Tick, TICKS_PER_HOUR};

    #[test]
    fn p3c_arrival_uses_the_planned_mode_and_keeps_modes_used_before_a_replan() {
        let mut sim = TestCtx::new_fast().g;
        let (_, resources) = sim.world_res();
        let mut router = Router::new(Some(crate::VehicleID::default()));
        resources.write::<GameTime>().tick = Tick(INITIAL_TICK + 200);

        router.trip_start = Some(Tick(20));
        router.trip_mode.plan(TripMode::Transit);
        router.trip_mode.plan(TripMode::Walk);
        on_arrival(&mut router, resources);

        router.trip_start = Some(Tick(150));
        router.trip_mode.plan(TripMode::Car);
        on_arrival(&mut router, resources);

        router.trip_start = Some(Tick(40));
        router.trip_mode = TripProgress::RodeTransit;
        router.trip_mode.plan(TripMode::Walk);
        on_arrival(&mut router, resources);

        let stats = resources.read::<TripStats>();
        assert_eq!((stats.walk.completed, stats.walk.duration_ticks), (1, 180));
        assert_eq!((stats.car.completed, stats.car.duration_ticks), (1, 50));
        assert_eq!(
            (stats.transit.completed, stats.transit.duration_ticks),
            (1, 160)
        );
    }

    #[test]
    fn p3c_trip_stats_complete_each_mode_once_and_discard_cancelled_trips() {
        let mut sim = TestCtx::new_fast().g;
        let (_, resources) = sim.world_res();
        let mut router = Router::new(None);
        for (mode, now, duration) in [
            (TripMode::Walk, 45, 20),
            (TripMode::Car, 73, 12),
            (TripMode::Transit, 18_200, 18_100),
        ] {
            resources.write::<GameTime>().tick = Tick(INITIAL_TICK + now);
            on_start(&mut router);
            router.trip_start = Some(Tick(now - duration));
            on_arrival_by_mode(&mut router, resources, mode);
            on_arrival_by_mode(&mut router, resources, mode);
        }
        router.trip_start = Some(Tick(1));
        on_cancel(&mut router);
        on_arrival(&mut router, resources);
        assert!(router.trip_start.is_none());

        router.trip_start = Some(Tick(200));
        on_wait_timeout(resources);
        assert_eq!(router.trip_start, Some(Tick(200)));
        resources.write::<GameTime>().tick = Tick(INITIAL_TICK + 200 + 2 * TICKS_PER_HOUR + 42);
        on_arrival(&mut router, resources);

        let saved: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
        let stats = saved.read::<TripStats>();
        assert_eq!(
            (stats.walk.completed, stats.walk.duration_ticks),
            (2, 2 * TICKS_PER_HOUR + 62)
        );
        assert_eq!((stats.car.completed, stats.car.duration_ticks), (1, 12));
        assert_eq!(
            (stats.transit.completed, stats.transit.duration_ticks),
            (1, 18_100)
        );
        assert_eq!((stats.timeouts, stats.give_ups), (1, 1));
    }
}
