use crate::builders::{fill_houses, grant_money, place_company, seed_households};
use crate::builtin::perf;
use crate::runner::{PerfTimings, P50_MS_PER_TICK, P95_MS_PER_TICK};
use crate::{MetricSample, ScenarioCtx};
use geom::{vec2, OBB};
use prototypes::{
    try_prototype, GameTime, GoodsCompanyID, GoodsCompanyPrototype, Money, Tick, TICKS_PER_MINUTE,
};
use serde::Serialize;
use simulation::economy::population::PopulationScale;
use simulation::economy::ZoneDemand;
use simulation::map::{
    BuildingKind, LanePatternBuilder, LotLayout, MapProject, ProjectKind, ZoneKind,
};
use simulation::map_dynamic::BuildingInfos;
use simulation::transportation::Location;
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{HumanEnt, MapPreset, Simulation, SimulationOptions, SlotId, SnapshotRevision};
use std::collections::{BTreeMap, BTreeSet};
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
    run_paris(ctx, case).map(|_| ())
}

fn run_paris(ctx: &mut ScenarioCtx, case: PerfCase) -> Result<MeasuredWindow, String> {
    validate_case(case)?;
    setup_paris(ctx, case.agents)?;
    measure_windows(
        ctx,
        case.warmup_ticks,
        case.measured_ticks,
        Some(case.agents),
        false,
    )
}

pub fn benchmark_paris(case: PerfCase) -> Result<BenchmarkReport, String> {
    validate_case(case)?;
    let mut ctx = ScenarioCtx::new(perf::OPTIONS);
    let window = run_paris(&mut ctx, case)?;
    report(ctx, Workload::Paris { case }, window)
}

pub fn bench_slot(
    root: &Path,
    id: &SlotId,
    warmup_ticks: u64,
    measured_ticks: u64,
) -> Result<BenchmarkReport, String> {
    bench_slot_with_workers(root, id, warmup_ticks, measured_ticks, 1)
}

pub fn bench_slot_with_workers(
    root: &Path,
    id: &SlotId,
    warmup_ticks: u64,
    measured_ticks: u64,
    workers: usize,
) -> Result<BenchmarkReport, String> {
    validate_workers(workers)?;
    validate_window(measured_ticks)?;
    // Resolve paths before initialization changes the current working directory.
    let root = std::path::absolute(root).map_err(|error| error.to_string())?;
    ScenarioCtx::initialize();
    let start = Instant::now();
    let sim = Simulation::load_from_slot(&root, id)
        .map_err(|error| format!("{} ({id}): {error}", root.display()))?;
    let load_ms = milliseconds(start.elapsed());
    let mut ctx = ScenarioCtx::from_sim(sim);
    let window = measure_windows(&mut ctx, warmup_ticks, measured_ticks, None, false)?;

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
            workers,
            load_ms,
            save_encode_ms,
        },
        window,
    )
}

fn validate_case(case: PerfCase) -> Result<(), String> {
    validate_workers(case.workers)?;
    validate_window(case.measured_ticks)?;
    if case.agents == 0 {
        return Err("benchmark requires at least one resident".into());
    }
    Ok(())
}

fn validate_workers(workers: usize) -> Result<(), String> {
    match workers {
        1 => Ok(()),
        0 => Err("--sim-threads must be greater than zero".into()),
        _ => {
            Err("only one simulation worker is available until parallel schedules are wired".into())
        }
    }
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
    let proto = employer()?;
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
    let company_count = employer_count(houses, proto.n_workers);
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

fn employer() -> Result<&'static GoodsCompanyPrototype, String> {
    try_prototype(GoodsCompanyID::new(EMPLOYER))
        .filter(|proto| proto.n_workers > 0)
        .ok_or_else(|| format!("missing {EMPLOYER} employer with jobs"))
}

fn employer_count(agents: usize, workers_per_company: u32) -> usize {
    let minimum_jobs = agents.div_ceil(2);
    (minimum_jobs + minimum_jobs.div_ceil(10)).div_ceil(workers_per_company as usize)
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

#[derive(Serialize)]
struct MeasuredWindow {
    completed_steps: u64,
    measured_wall_seconds: f64,
    peak_agents: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    peak_active_vehicles: Option<usize>,
}

fn measure_windows(
    ctx: &mut ScenarioCtx,
    warmup_ticks: u64,
    measured_ticks: u64,
    expected_agents: Option<usize>,
    track_traffic: bool,
) -> Result<MeasuredWindow, String> {
    let start_tick = ctx.sim.get_tick();
    if start_tick
        .checked_add(warmup_ticks)
        .and_then(|tick| tick.checked_add(measured_ticks))
        .is_none()
    {
        return Err("benchmark window exceeds the simulation tick range".into());
    }
    sample_workload(ctx, "setup", expected_agents)?;
    let mut peak_agents = ctx.sim.world().humans.len();
    ctx.run_ticks_observed(warmup_ticks, |sim| {
        peak_agents = peak_agents.max(sim.world().humans.len())
    });
    sample_workload(ctx, "warmup", expected_agents)?;
    let count_active_vehicles = |sim: &Simulation| {
        sim.world()
            .vehicles
            .values()
            .filter(|vehicle| !vehicle.vehicle.state.is_parked())
            .count()
    };
    let mut peak_active_vehicles = track_traffic.then(|| count_active_vehicles(&ctx.sim));
    ctx.enable_perf_timings();
    let start = Instant::now();
    let completed_steps = ctx.run_ticks_observed(measured_ticks, |sim| {
        peak_agents = peak_agents.max(sim.world().humans.len());
        if let Some(peak) = &mut peak_active_vehicles {
            *peak = (*peak).max(count_active_vehicles(sim));
        }
    });
    let measured_wall_seconds = start.elapsed().as_secs_f64();
    sample_workload(ctx, "measured", expected_agents)?;
    Ok(MeasuredWindow {
        completed_steps,
        measured_wall_seconds,
        peak_agents,
        peak_active_vehicles,
    })
}

fn sample_workload(
    ctx: &mut ScenarioCtx,
    label: &str,
    expected_agents: Option<usize>,
) -> Result<(), String> {
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
    if let Some(expected) = expected_agents {
        let actual = world.humans.len();
        if actual != expected {
            return Err(format!(
                "workload calibration: {label} had {actual} residents, expected {expected}"
            ));
        }
    }
    let mut locations = [0; 4];
    let mut human_colliders = 0;
    for human in world.humans.values() {
        let index = match human.location {
            Location::Outside => 0,
            Location::Building(_) => 1,
            Location::Vehicle(_) => 2,
            Location::Transit(_) | Location::RailTransit(_) | Location::TransitHeld => 3,
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
    Ok(())
}

const MAX_LAYOUT_AGENTS: usize = 40_000;
const GRID_SPACING_M: f32 = 280.0;
const LAYOUT_START_TICK: u64 = 5 * TICKS_PER_MINUTE;
const EMPLOYER: &str = "clothes-store";

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PerfLayout {
    Compact,
    Growing,
}

#[derive(Serialize)]
struct GridFixture {
    layout: PerfLayout,
    grid_side: usize,
    grid_spacing_m: f32,
    road_footprint_m2: f64,
    workload_revision: u32,
    minimum_employed: usize,
    minimum_peak_active_vehicles: usize,
    activity_floors_checked: bool,
}

impl GridFixture {
    fn new(layout: PerfLayout, case: PerfCase) -> Self {
        // Use a small comparison family for smoke runs. Every normal compact
        // case uses the same road network independent of population.
        let grid_side = match layout {
            PerfLayout::Compact if case.agents <= 64 => 4,
            PerfLayout::Compact => 44,
            PerfLayout::Growing => (2..=44)
                .find(|&side| 24 * side * (side - 1) >= case.agents)
                .unwrap(),
        };
        Self {
            layout,
            grid_side,
            grid_spacing_m: GRID_SPACING_M,
            road_footprint_m2: ((grid_side - 1) as f64 * f64::from(GRID_SPACING_M)).powi(2),
            workload_revision: 1,
            minimum_employed: case.agents.div_ceil(2),
            minimum_peak_active_vehicles: case.agents.div_ceil(20),
            activity_floors_checked: case.warmup_ticks >= 2_000 && case.measured_ticks >= 10_000,
        }
    }
}

/// Build a flat, connected commute fixture with the same setup clock across populations.
pub fn benchmark_layout(case: PerfCase, layout: PerfLayout) -> Result<BenchmarkReport, String> {
    benchmark_layout_with_scale(case, layout, PopulationScale::REALISTIC)
}

pub fn benchmark_layout_with_scale(
    case: PerfCase,
    layout: PerfLayout,
    scale: PopulationScale,
) -> Result<BenchmarkReport, String> {
    validate_case(case)?;
    if case.agents > MAX_LAYOUT_AGENTS {
        return Err(format!("--agents must be at most {MAX_LAYOUT_AGENTS}"));
    }
    let fixture = GridFixture::new(layout, case);
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        terrain_size: if case.agents <= 64 { 16 } else { 36 },
        preset: MapPreset::Flat,
        persons_per_agent: scale.persons_per_agent(),
        ..perf::OPTIONS
    });
    setup_grid(&mut ctx, fixture.grid_side, case.agents)?;
    let window = measure_windows(
        &mut ctx,
        case.warmup_ticks,
        case.measured_ticks,
        Some(case.agents),
        true,
    )?;
    if fixture.activity_floors_checked {
        validate_activity(&ctx.samples, &fixture, window.peak_active_vehicles.unwrap())?;
    }
    report(ctx, Workload::Grid { case, fixture }, window)
}

fn setup_grid(ctx: &mut ScenarioCtx, side: usize, agents: usize) -> Result<(), String> {
    let origin = vec2(5_200.0, 900.0);
    let pattern = LanePatternBuilder::new().build();
    let mut points = Vec::with_capacity(side * side);
    let mut links = Vec::with_capacity(2 * side * (side - 1));
    let before = {
        let map = ctx.sim.map();
        for y in 0..side {
            for x in 0..side {
                let pos = origin + vec2(x as f32, y as f32) * GRID_SPACING_M;
                let height = map
                    .environment()
                    .height(pos)
                    .ok_or("grid outside terrain")?;
                let i = points.len();
                points.push(MapProject {
                    pos: pos.z(height),
                    kind: ProjectKind::Ground,
                });
                if x > 0 {
                    links.push((i - 1, i, None, pattern.clone()));
                }
                if y > 0 {
                    links.push((i - side, i, None, pattern.clone()));
                }
            }
        }
        map.roads().keys().collect::<BTreeSet<_>>()
    };
    let road_count = links.len();
    grant_money(ctx, Money::new_bucks(250 * road_count as i64));
    ctx.sim.apply_commands(&[
        WorldCommand::SetLotLayout(LotLayout::Uniform),
        WorldCommand::MapMakeMultipleConnections(points, links),
    ]);
    let mut roads = {
        let map = ctx.sim.map();
        let roads: Vec<_> = map
            .roads()
            .keys()
            .filter(|id| !before.contains(id))
            .collect();
        let junctions: BTreeSet<_> = roads
            .iter()
            .flat_map(|&id| [map.roads()[id].src, map.roads()[id].dst])
            .collect();
        if roads.len() != road_count
            || junctions.len() != side * side
            || !ctx.sim.read::<CommandOutcomes>().last.is_empty()
        {
            return Err(format!(
                "grid built {} of {road_count} road links and {} of {} junctions",
                roads.len(),
                junctions.len(),
                side * side
            ));
        }
        roads
    };
    roads.sort_unstable();

    let proto = employer()?;
    let employers = employer_count(agents, proto.n_workers);
    grant_money(ctx, proto.price * employers as i64);
    grant_money(ctx, Money::new_bucks(100 * agents as i64));
    let companies: Vec<_> = {
        let map = ctx.sim.map();
        spread(&roads, employers)
            .map(|id| {
                let road = &map.roads()[id];
                let points = road.points();
                let (mid, dir) = points.point_dir_along(points.length() * 0.5);
                let outward = dir.xy().normalize().perpendicular();
                let center = mid.xy() + outward * ((road.width + proto.size.w) * 0.5 + 1.0);
                WorldCommand::MapBuildSpecialBuilding {
                    pos: OBB::new(center, outward, proto.size.w, proto.size.h),
                    kind: BuildingKind::GoodsCompany(proto.id),
                    gen: proto.bgen,
                    zone: None,
                    connected_road: Some(id),
                }
            })
            .collect()
    };
    ctx.sim.apply_commands(&companies);
    require_accepted(&ctx.sim, "employers", employers)?;

    let homes: Vec<_> = {
        let map = ctx.sim.map();
        let mut lots: Vec<_> = map
            .lots()
            .values()
            .filter(|lot| roads.binary_search(&lot.parent).is_ok())
            .map(|lot| (lot.parent, lot.id))
            .collect();
        if lots.len() < agents {
            return Err(format!("grid has {} lots for {agents} houses", lots.len()));
        }
        lots.sort_unstable();
        spread(&lots, agents)
            .map(|(_, id)| WorldCommand::MapBuildHouse(id))
            .collect()
    };
    ctx.sim.apply_commands(&homes);
    require_accepted(&ctx.sim, "houses", agents)?;
    let mut houses: Vec<_> = ctx
        .sim
        .map()
        .buildings()
        .iter()
        .filter_map(|(id, building)| (building.kind == BuildingKind::House).then_some(id))
        .collect();
    houses.sort_unstable();
    seed_households(&mut ctx.sim, &houses)?;
    ctx.sim
        .apply_commands(&[WorldCommand::SetGameTime(GameTime::new(Tick(
            LAYOUT_START_TICK - 1,
        )))]);
    ctx.run_ticks(1);
    if ctx.sim.world().companies.len() != employers {
        return Err(format!(
            "grid spawned {} of {employers} employers",
            ctx.sim.world().companies.len()
        ));
    }
    Ok(())
}

fn require_accepted(sim: &Simulation, kind: &str, expected: usize) -> Result<(), String> {
    let outcomes = sim.read::<CommandOutcomes>();
    if outcomes.accepted.len() != expected {
        return Err(format!(
            "grid built {} of {expected} {kind}: {:?}",
            outcomes.accepted.len(),
            outcomes.last.first()
        ));
    }
    Ok(())
}

/// Spread occupied sites across the full ordered footprint, including its ends.
fn spread<T: Copy>(sites: &[T], count: usize) -> impl Iterator<Item = T> + '_ {
    (0..count).map(move |i| {
        let index = if count == 1 {
            sites.len() / 2
        } else {
            i * (sites.len() - 1) / (count - 1)
        };
        sites[index]
    })
}

fn validate_activity(
    samples: &[MetricSample],
    fixture: &GridFixture,
    peak: usize,
) -> Result<(), String> {
    let minimum_employed = fixture.minimum_employed;
    for sample in &samples[1..] {
        let employed = sample.extra["employed"] as usize;
        if employed < minimum_employed {
            return Err(format!("workload calibration: {} had {employed} employed residents, expected at least {minimum_employed}", sample.label));
        }
    }
    let minimum_traffic = fixture.minimum_peak_active_vehicles;
    if peak < minimum_traffic {
        return Err(format!("workload calibration: measured peak had {peak} active vehicles, expected at least {minimum_traffic}"));
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(tag = "benchmark")]
enum Workload {
    #[serde(rename = "perf_scale")]
    Paris {
        #[serde(flatten)]
        case: PerfCase,
    },
    #[serde(rename = "perf-scale")]
    Grid {
        #[serde(flatten)]
        case: PerfCase,
        #[serde(flatten)]
        fixture: GridFixture,
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
    simulation_options: SimulationOptions,
    lot_layout: LotLayout,
    initial_agents: usize,
    actual_agents: usize,
    persons_per_agent: u16,
    initial_population: u64,
    peak_population: u64,
    actual_population: u64,
    employed_population: u64,
    #[serde(flatten)]
    window: MeasuredWindow,
    tick_duration_ticks_per_sec: f64,
    setup: MetricSample,
    warmup: MetricSample,
    #[serde(flatten)]
    measured: MetricSample,
    p50_ms_per_tick: f64,
    p95_ms_per_tick: f64,
    #[serde(flatten)]
    timings: PerfTimings,
}

fn report(
    mut ctx: ScenarioCtx,
    workload: Workload,
    window: MeasuredWindow,
) -> Result<BenchmarkReport, String> {
    let timings = ctx
        .take_measured_timings()
        .ok_or("missing measured timings")?;
    let [setup, warmup, mut measured]: [MetricSample; 3] = ctx
        .samples
        .try_into()
        .map_err(|_| "missing benchmark samples")?;
    let tick_duration_ticks_per_sec = measured.ticks_per_sec;
    let scale = ctx.sim.population_scale();
    let employed = ctx
        .sim
        .world()
        .humans
        .values()
        .filter(|human| human.work.is_some())
        .count();
    measured.ticks_per_sec = if window.measured_wall_seconds > 0.0 {
        window.completed_steps as f64 / window.measured_wall_seconds
    } else {
        0.0
    };
    Ok(BenchmarkReport {
        workload,
        simulation_options: *ctx.sim.read::<SimulationOptions>(),
        lot_layout: ctx.sim.map().lot_layout(),
        initial_agents: setup.humans,
        actual_agents: measured.humans,
        persons_per_agent: scale.persons_per_agent(),
        initial_population: scale.persons(setup.humans),
        peak_population: scale.persons(window.peak_agents),
        actual_population: scale.persons(measured.humans),
        employed_population: scale.persons(employed),
        window,
        tick_duration_ticks_per_sec,
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
        let window = measure_windows(&mut ctx, 2, 1, None, false).unwrap();
        let case = PerfCase {
            agents: 1,
            warmup_ticks: 2,
            measured_ticks: 1,
            workers: 1,
        };
        let report =
            serde_json::to_value(report(ctx, Workload::Paris { case }, window).unwrap()).unwrap();
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
        assert_eq!(report["completed_steps"], 1);
        let wall = report["measured_wall_seconds"].as_f64().unwrap();
        assert_eq!(report["ticks_per_sec"].as_f64().unwrap(), wall.recip());
        let tick_rate = report["tick_duration_ticks_per_sec"].as_f64().unwrap();
        assert!(tick_rate >= wall.recip());
    }

    #[test]
    fn scale_m0_reports_options_and_peak_agents() {
        let mut ctx = ScenarioCtx::new(SimulationOptions {
            persons_per_agent: 4,
            ..smoke::OPTIONS
        });
        grid_city(&mut ctx, vec2(700.0, 700.0), 2, 2, 160.0);
        setup_houses(&mut ctx, 4).unwrap();
        let window = measure_windows(&mut ctx, 1, 1, None, false).unwrap();
        let case = PerfCase {
            agents: 4,
            warmup_ticks: 1,
            measured_ticks: 1,
            workers: 1,
        };
        let report =
            serde_json::to_value(report(ctx, Workload::Paris { case }, window).unwrap()).unwrap();
        assert_eq!(
            report["simulation_options"]["terrain_size"],
            smoke::OPTIONS.terrain_size
        );
        assert_eq!(report["initial_agents"], 4);
        assert_eq!(report["peak_agents"], 4);
        assert_eq!(report["actual_agents"], 4);
        assert_eq!(report["persons_per_agent"], 4);
        assert_eq!(report["initial_population"], 16);
        assert_eq!(report["peak_population"], 16);
        assert_eq!(report["actual_population"], 16);
    }

    #[test]
    fn scale_m0_rejects_population_drift() {
        let case = PerfCase {
            agents: 1,
            warmup_ticks: 1_000,
            measured_ticks: 1,
            workers: 1,
        };
        let err = benchmark_paris(case)
            .err()
            .expect("accepted a drifted population");
        assert!(err.contains("warmup had 2 residents, expected 1"), "{err}");
    }

    #[test]
    fn scale_m0_paris_preserves_the_tiny_checkpoint() {
        let mut ctx = ScenarioCtx::new(perf::OPTIONS);
        let case = PerfCase {
            agents: 4,
            warmup_ticks: 0,
            measured_ticks: 1,
            workers: 1,
        };
        let mut hashes = Vec::new();
        ctx.run_with_hashes("paris-smoke", &mut hashes, |ctx| run_perf_case(ctx, case))
            .unwrap();
        let checkpoint: serde_json::Value = serde_json::from_slice(&hashes).unwrap();
        // Keep the M0 gameplay projection. The 0.11.0 world format also
        // serializes each resident's education hours.
        assert_eq!(checkpoint["tick"], 5);
        assert_eq!(checkpoint["projection"], 7_202_952_127_034_493_285_u64);
        assert_eq!(
            checkpoint["hashes"]["world"],
            14_091_543_823_439_975_702_u64
        );
        // Procedural roofs serialize the completed face boundaries in canonical order.
        assert_eq!(checkpoint["hashes"]["map"], 10_765_074_735_981_337_578_u64);
    }

    #[test]
    #[ignore = "full Paris comparator, run on a quiet host"]
    fn scale_m0_paris_matches_frozen_perf_10k() {
        let mut legacy = ScenarioCtx::new(perf::OPTIONS);
        let mut paris = ScenarioCtx::new(perf::OPTIONS);
        let mut expected = Vec::new();
        let mut actual = Vec::new();
        legacy
            .run_with_hashes("perf_10k", &mut expected, perf::run_10k)
            .unwrap();
        let case = PerfCase {
            agents: 10_000,
            warmup_ticks: 2_000,
            measured_ticks: 10_000,
            workers: 1,
        };
        paris
            .run_with_hashes("perf_10k", &mut actual, |ctx| run_perf_case(ctx, case))
            .unwrap();
        assert_eq!(
            String::from_utf8(actual).unwrap(),
            String::from_utf8(expected).unwrap()
        );
    }

    #[test]
    fn scale_m0_layouts_hold_or_grow_the_network_and_align_the_clock() {
        let mut case = PerfCase {
            agents: 16,
            warmup_ticks: 2,
            measured_ticks: 1,
            workers: 1,
        };
        for layout in [PerfLayout::Compact, PerfLayout::Growing] {
            let first = serde_json::to_value(benchmark_layout(case, layout).unwrap()).unwrap();
            case.agents = 64;
            let second = serde_json::to_value(benchmark_layout(case, layout).unwrap()).unwrap();
            case.agents = 16;
            assert_eq!(first["setup"]["tick"], LAYOUT_START_TICK);
            assert_eq!(
                first["warmup"]["tick"],
                LAYOUT_START_TICK + case.warmup_ticks
            );
            assert_eq!(first["warmup"]["tick"], second["warmup"]["tick"]);
            assert_eq!(first["initial_agents"], 16);
            assert_eq!(second["initial_agents"], 64);
            assert_eq!(first["actual_agents"], 16);
            assert_eq!(second["actual_agents"], 64);
            let first_roads = first["extra"]["roads"].as_f64().unwrap();
            let second_roads = second["extra"]["roads"].as_f64().unwrap();
            match layout {
                PerfLayout::Compact => {
                    assert_eq!(first["road_footprint_m2"], second["road_footprint_m2"]);
                    assert_eq!(first_roads, second_roads);
                }
                PerfLayout::Growing => {
                    assert!(
                        first["road_footprint_m2"].as_f64().unwrap()
                            < second["road_footprint_m2"].as_f64().unwrap()
                    );
                    assert!(first_roads < second_roads);
                }
            }
        }
    }
}
