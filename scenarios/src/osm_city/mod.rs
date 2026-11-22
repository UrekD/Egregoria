mod check;
mod lesce;
mod normalize;
mod publish;
mod source;
mod spatial;
mod zoning;

pub use normalize::{normalize, Feature, Forest, GeoBounds, LinkClass, NormalizedCity, Site};
pub use source::{read_overpass, OverpassExtract};

pub type ImportError = Box<dyn std::error::Error + Send + Sync>;

pub const LESCE_BOUNDS: GeoBounds = GeoBounds {
    south: 46.343,
    west: 14.133,
    north: 46.378,
    east: 14.188,
};

pub use publish::{generate, generate_with_options};

use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub source: Source,
    pub revision: String,
    pub feature_counts: BTreeMap<String, usize>,
    pub skipped: Vec<(String, String)>,
    pub synthetic: Vec<String>,
    pub route_checks: BTreeMap<String, bool>,
    pub flat_crossings: Vec<(String, String)>,
    pub short_lanes: Vec<(Vec<u64>, f32)>,
    /// Junctions whose control differs from the engine's automatic policy, as (source, change).
    pub junction_policies: Vec<(String, String)>,
    pub policies: Vec<&'static str>,
    pub opening_budget_cents: i64,
    pub opening: Option<CitySample>,
    pub generation_seconds: f64,
    pub snapshot_bytes: u64,
    pub headless: Option<HeadlessReport>,
    pub attribution: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Source {
    pub sha256: String,
    pub timestamp: Option<String>,
    pub requested_snapshot: &'static str,
    pub bounds: GeoBounds,
    pub query: &'static str,
}

#[derive(Debug, Serialize)]
pub struct HeadlessReport {
    pub ticks: u64,
    pub ticks_per_second: f64,
    pub samples: Vec<CitySample>,
    pub money_per_day_cents: Vec<i64>,
    pub developed: BTreeMap<String, usize>,
    pub below_perf_baseline: bool,
    pub freight_completed: bool,
}

#[derive(Debug, Serialize)]
pub struct CitySample {
    pub hours: u64,
    pub population: usize,
    pub unemployment_pct: f64,
    pub homes: usize,
    pub money_cents: i64,
    pub unserved_buildings: usize,
    pub water_demand: f64,
    pub water_production: f64,
    pub sewage_capacity: f64,
    pub utility_workers: BTreeMap<String, usize>,
}
