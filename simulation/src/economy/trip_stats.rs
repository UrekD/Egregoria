use serde::{Deserialize, Serialize};

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
