//! Balance constants for zoning demand and growth.

use prototypes::{ServiceKind, Tick};

use crate::economy::Budget;
use crate::INITIAL_TICK;

// Demand, growth, utilities, environment and fires use elapsed ticks. Anchor their
// phases at INITIAL_TICK to preserve the cadence of an uninterrupted calendar.
pub(crate) const fn elapsed_phase_tick(elapsed: u64) -> u64 {
    elapsed.saturating_add(INITIAL_TICK)
}

pub(crate) fn periodic_tick(budget: &Budget, calendar_tick: Tick) -> u64 {
    elapsed_phase_tick(budget.elapsed_ticks(calendar_tick))
}

// Demand samples one tick before growth, and utilities refresh one tick before
// environment. On an uninterrupted calendar, residence uses the preceding snapshots
// just before the boundary. Coverage refreshes independently by kind, and hourly
// happiness and fires follow the environment pass.
pub(crate) const CITY_UPDATE_INTERVAL_TICKS: u64 = 3_000;
pub const DEMAND_INTERVAL_TICKS: u64 = CITY_UPDATE_INTERVAL_TICKS;
pub(crate) const DEMAND_OFFSET_TICKS: u64 = DEMAND_INTERVAL_TICKS - 1;
pub const DEMAND_EMA_ALPHA: f32 = 0.2;
pub const DEFAULT_TAX_RATE_PERMILLE: u16 = 90;
pub const NEUTRAL_HAPPINESS: f32 = 50.0;

pub const R_BASE: f32 = 40.0;
pub const R_BOOTSTRAP_POPULATION: usize = 50;
pub const I_BASE: f32 = 30.0;
pub const I_BOOTSTRAP_COMPANIES: usize = 3;
pub const MIN_DEMAND_POPULATION: usize = 10;

pub const K_JOBS: f32 = 1.0;
pub const K_VAC: f32 = 0.5;
pub const K_TAX: f32 = 1.0;
pub const K_RET: f32 = 0.6;
pub const K_GOODS: f32 = 0.2;
pub const K_UNEMP: f32 = 1.0;
pub const K_HAPPY: f32 = 0.8;

// Weights are points at maximum satisfaction and sum to 100.
pub(crate) const HAPPINESS_POWER: f32 = 5.0;
pub(crate) const HAPPINESS_WATER: f32 = 5.0;
pub(crate) const HAPPINESS_SEWAGE: f32 = 5.0;
pub(crate) const HAPPINESS_COVERAGE: [(ServiceKind, f32); 6] = [
    (ServiceKind::Health, 9.0),
    (ServiceKind::Fire, 9.0),
    (ServiceKind::Police, 10.0),
    (ServiceKind::School, 12.0),
    (ServiceKind::Garbage, 9.0),
    (ServiceKind::Park, 9.0),
];
pub(crate) const HAPPINESS_CLEAN_AIR: f32 = 2.0;
pub(crate) const HAPPINESS_LAND_VALUE: f32 = 6.0;
pub(crate) const HAPPINESS_TAX: f32 = 3.0;
pub(crate) const HAPPINESS_RETAIL: f32 = 4.0;
pub(crate) const HAPPINESS_EMPLOYMENT: f32 = 8.0;
pub(crate) const HAPPINESS_COMMUTE: f32 = 4.0;
pub(crate) const HAPPINESS_COMMUTE_LIMIT_M: f32 = 1_500.0;
pub(crate) const RETAIL_ACCESS_RADIUS_M: f32 = 1_500.0;

/// One retail unit per resident per six game hours.
pub const RETAIL_PER_CAPITA_PER_HOUR: f64 = 1.0 / 6.0;
