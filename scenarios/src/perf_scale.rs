use crate::builders::{fill_houses, grant_money, place_company, seed_households};
use crate::builtin::perf;
use crate::runner::{PerfTimings, P50_MS_PER_TICK, P95_MS_PER_TICK};
use crate::{MetricSample, ScenarioCtx};
use prototypes::{try_prototype, GoodsCompanyID, Money};
use serde::Serialize;
use simulation::economy::ZoneDemand;
use simulation::map::{BuildingKind, ZoneKind};
use simulation::map_dynamic::BuildingInfos;
use simulation::transportation::Location;
use simulation::world_command::WorldCommand;
use simulation::{HumanEnt, Simulation, SlotId, SnapshotRevision};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct PerfCase {
    pub agents: usize,
    pub warmup_ticks: u64,
    pub measured_ticks: u64,
    pub workers: usize,
}

/// Set up the Paris commute in a fresh context using `perf::OPTIONS` and record
/// setup, warm-up and measured samples in `ctx.samples`.
pub fn run_perf_case(ctx: &mut ScenarioCtx, case: PerfCase) -> Result<(), String> {
    if case.workers != 1 {
        return Err("perf_scale requires one simulation worker".into());
    }
    validate_window(case.measured_ticks)?;
    if case.agents == 0 {
        return Err("perf_scale requires at least one house".into());
    }
    setup_paris(ctx, case.agents)?;
    measure_windows(ctx, case.warmup_ticks, case.measured_ticks);
    Ok(())
}

pub fn benchmark_paris(case: PerfCase) -> Result<BenchmarkReport, String> {
    let mut ctx = ScenarioCtx::new(perf::OPTIONS);
    run_perf_case(&mut ctx, case)?;
    report(ctx, Workload::Paris { case })
}

pub fn bench_slot(
    root: &Path,
    id: &SlotId,
    warmup_ticks: u64,
    measured_ticks: u64,
) -> Result<BenchmarkReport, String> {
    validate_window(measured_ticks)?;
    // Resolve paths before initialization changes the current working directory.
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    ScenarioCtx::initialize();
    let start = Instant::now();
    let sim = Simulation::load_from_slot(&root, id)
        .map_err(|error| format!("{} ({id}): {error}", root.display()))?;
    let load_ms = milliseconds(start.elapsed());
    let mut ctx = ScenarioCtx::from_sim(sim);
    measure_windows(&mut ctx, warmup_ticks, measured_ticks);

    let start = Instant::now();
    let encoded = ctx
        .sim
        .encode_slot(SnapshotRevision::new(0))
        .map_err(|error| format!("{id}: {error}"))?;
    let save_encode_ms = milliseconds(start.elapsed());
    drop(encoded);

    report(
        ctx,
        Workload::Slot {
            slot: id.to_string(),
            warmup_ticks,
            measured_ticks,
            workers: 1,
            load_ms,
            save_encode_ms,
        },
    )
}

fn validate_window(measured_ticks: u64) -> Result<(), String> {
    if measured_ticks == 0 {
        return Err("--ticks must be greater than zero".into());
    }
    Ok(())
}

fn setup_paris(ctx: &mut ScenarioCtx, houses: usize) -> Result<(), String> {
    ctx.submit(&[WorldCommand::MapLoadParis]);
    ctx.run_ticks(1);

    // Match the perf_10k employer placement and allowance at every scale.
    const EMPLOYER: &str = "clothes-store";
    let proto = try_prototype(GoodsCompanyID::new(EMPLOYER))
        .filter(|proto| proto.n_workers > 0)
        .ok_or("missing clothes-store employer with jobs")?;
    let mut sites: Vec<_> = ctx
        .sim
        .map()
        .lots()
        .values()
        .map(|lot| (lot.id, lot.shape.center()))
        .collect();
    if houses > sites.len() {
        return Err(format!(
            "Paris has {} lots for {houses} houses",
            sites.len()
        ));
    }
    sites.sort_unstable_by_key(|&(id, _)| id);
    let minimum_jobs = houses.div_ceil(2);
    let company_count =
        (minimum_jobs + minimum_jobs.div_ceil(10)).div_ceil(proto.n_workers as usize);
    grant_money(ctx, proto.price * company_count as i64);

    let stride = (houses / company_count).max(1);
    let companies = sites
        .into_iter()
        .step_by(stride)
        .filter_map(|(_, near)| place_company(ctx, EMPLOYER, near))
        .take(company_count)
        .count();
    if companies != company_count {
        return Err(format!("built {companies} of {company_count} employers"));
    }

    setup_houses(ctx, houses)
}

fn setup_houses(ctx: &mut ScenarioCtx, houses: usize) -> Result<(), String> {
    // Keep the existing $1M allowance at 10k and below, and fund larger fixtures.
    grant_money(ctx, Money::new_bucks(100 * houses.max(10_000) as i64));
    let mut built = fill_houses(ctx, houses);
    if built.len() != houses {
        return Err(format!("built {} of {houses} houses", built.len()));
    }

    // Employer placement can leave positive demand from when the city had no residents.
    // Keep anyone already added on the construction tick, seed the empty houses, and
    // let the next demand update use the populated city.
    {
        let mut demand = ctx.sim.write::<ZoneDemand>();
        if let Some(value) = demand.values.get_mut(&ZoneKind::Residential) {
            if *value > 0.0 {
                *value = 0.0;
            }
        }
        let infos = ctx.sim.read::<BuildingInfos>();
        built.retain(|&house| {
            infos
                .get(house)
                .is_none_or(|info| info.residents.is_empty())
        });
    }
    seed_households(&mut ctx.sim, &built)
}

fn measure_windows(ctx: &mut ScenarioCtx, warmup_ticks: u64, measured_ticks: u64) {
    sample_workload(ctx, "setup");
    ctx.run_ticks(warmup_ticks);
    sample_workload(ctx, "warmup");
    ctx.enable_perf_timings();
    ctx.run_ticks(measured_ticks);
    sample_workload(ctx, "measured");
}

fn sample_workload(ctx: &mut ScenarioCtx, label: &str) {
    let mut extra = BTreeMap::new();
    if let Ok(load_avg) = std::fs::read_to_string("/proc/loadavg") {
        for (name, value) in ["load_avg_1m", "load_avg_5m", "load_avg_15m"]
            .into_iter()
            .zip(load_avg.split_whitespace())
        {
            if let Ok(value) = value.parse::<f64>() {
                extra.insert(name.to_string(), value);
            }
        }
    }

    let map = ctx.sim.map();
    let houses = map
        .buildings()
        .values()
        .filter(|building| building.kind == BuildingKind::House)
        .count();
    extra.insert("roads".into(), map.roads().len() as f64);
    drop(map);

    let world = ctx.sim.world();
    let mut locations = [0; 4];
    let mut human_colliders = 0;
    for human in world.humans.values() {
        let index = match human.location {
            Location::Outside => 0,
            Location::Building(_) => 1,
            Location::Vehicle(_) => 2,
            Location::Transit(_) => 3,
        };
        locations[index] += 1;
        human_colliders += usize::from(human.collider.is_some());
    }
    let vehicle_colliders = world
        .vehicles
        .values()
        .filter(|vehicle| vehicle.collider.is_some())
        .count();
    let jobs: usize = world
        .companies
        .values()
        .map(|company| company.comp.max_workers as usize)
        .sum();
    extra.extend([
        ("humans_outside".into(), locations[0] as f64),
        ("humans_in_buildings".into(), locations[1] as f64),
        ("humans_in_vehicles".into(), locations[2] as f64),
        ("humans_in_transit".into(), locations[3] as f64),
        ("human_colliders".into(), human_colliders as f64),
        ("vehicle_colliders".into(), vehicle_colliders as f64),
        ("job_capacity".into(), jobs as f64),
    ]);
    ctx.sample_perf_with_extra(label, houses, extra, true);
}

#[derive(Serialize)]
#[serde(tag = "benchmark")]
enum Workload {
    #[serde(rename = "perf_scale")]
    Paris {
        #[serde(flatten)]
        case: PerfCase,
    },
    #[serde(rename = "bench-slot")]
    Slot {
        slot: String,
        warmup_ticks: u64,
        measured_ticks: u64,
        workers: usize,
        load_ms: f64,
        save_encode_ms: f64,
    },
}

#[derive(Serialize)]
pub struct BenchmarkReport {
    #[serde(flatten)]
    workload: Workload,
    human_entity_bytes: usize,
    setup: MetricSample,
    warmup: MetricSample,
    #[serde(flatten)]
    measured: MetricSample,
    p50_ms_per_tick: f64,
    p95_ms_per_tick: f64,
    #[serde(flatten)]
    timings: PerfTimings,
}

fn report(mut ctx: ScenarioCtx, workload: Workload) -> Result<BenchmarkReport, String> {
    let timings = ctx
        .take_measured_timings()
        .ok_or("missing measured timings")?;
    let [setup, warmup, mut measured]: [MetricSample; 3] = ctx
        .samples
        .try_into()
        .map_err(|_| "missing benchmark samples")?;
    Ok(BenchmarkReport {
        workload,
        human_entity_bytes: std::mem::size_of::<HumanEnt>(),
        setup,
        warmup,
        p50_ms_per_tick: measured
            .extra
            .remove(P50_MS_PER_TICK)
            .ok_or("missing measured p50")?,
        p95_ms_per_tick: measured
            .extra
            .remove(P95_MS_PER_TICK)
            .ok_or("missing measured p95")?,
        measured,
        timings,
    })
}

fn milliseconds(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::grid_city;
    use crate::builtin::smoke;
    use geom::vec2;

    #[test]
    fn scale_m0_positive_demand_seeds_one_resident_per_house() {
        let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
        grid_city(&mut ctx, vec2(700.0, 700.0), 2, 2, 160.0);
        ctx.sim
            .write::<ZoneDemand>()
            .values
            .insert(ZoneKind::Residential, 100.0);

        setup_houses(&mut ctx, 4).unwrap();
        assert_eq!(ctx.sim.world().humans.len(), 4);
        ctx.run_ticks(1);
        assert_eq!(ctx.sim.world().humans.len(), 4);
    }

    #[test]
    fn scale_m0_measures_only_the_requested_window() {
        let mut ctx = ScenarioCtx::new(smoke::OPTIONS);
        ctx.run_ticks(3);
        let setup_tick = ctx.sim.get_tick();
        measure_windows(&mut ctx, 2, 1);
        let case = PerfCase {
            agents: 1,
            warmup_ticks: 2,
            measured_ticks: 1,
            workers: 1,
        };
        let report = serde_json::to_value(report(ctx, Workload::Paris { case }).unwrap()).unwrap();
        assert_eq!(report["benchmark"], "perf_scale");
        assert_eq!(report["agents"], 1);
        assert_eq!(report["setup"]["tick"], setup_tick);
        assert_eq!(report["warmup"]["tick"], setup_tick + 2);
        assert_eq!(report["tick"], setup_tick + 3);
        let p50 = report["p50_ms_per_tick"].as_f64().unwrap();
        let p95 = report["p95_ms_per_tick"].as_f64().unwrap();
        let max = report["max_ms_per_tick"].as_f64().unwrap();
        assert_eq!(p50, p95);
        assert_eq!(p95, max);
        assert!(report["ticks_per_sec"].as_f64().unwrap() > 0.0);
    }
}
