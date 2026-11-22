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
    pub timeouts: u64,
    pub give_ups: u64,
}

pub(crate) fn on_start(router: &mut Router) {
    router.trip_start = None;
    router.trip_mode = TripMode::Walk;
}

pub(crate) fn on_arrival(router: &mut Router, resources: &Resources) {
    let mode = std::mem::take(&mut router.trip_mode);
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
    router.trip_start = None;
    router.trip_mode = TripMode::Walk;
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
