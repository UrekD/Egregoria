use serde::{Deserialize, Serialize};

use crate::map_dynamic::Router;
use crate::utils::resources::Resources;

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

pub(crate) fn on_start(_router: &mut Router) {}

pub(crate) fn on_arrival(_router: &mut Router, _resources: &Resources) {}

pub(crate) fn on_cancel(_router: &mut Router) {}
