//! Balance constants for zoning demand and growth.

pub const DEMAND_INTERVAL_TICKS: u64 = 3_000;
pub const DEMAND_EMA_ALPHA: f32 = 0.2;
pub const DEFAULT_TAX_RATE_PERMILLE: u16 = 90;
pub const HAPPINESS: u8 = 50;

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

/// One retail unit per resident per six game hours.
pub const RETAIL_PER_CAPITA_PER_HOUR: f64 = 1.0 / 6.0;
