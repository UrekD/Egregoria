use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize)]
pub struct MetricSample {
    pub tick: u64,
    pub label: String,
    pub humans: usize,
    pub vehicles: usize,
    pub companies: usize,
    pub trains: usize,
    pub money_cents: i64,
    pub ticks_per_sec: f64,
    pub extra: BTreeMap<String, f64>,
}
