use crate::builders::{
    grid_city, paint_zone_blocks, place_freight_station, place_grid_power_plant,
    place_service_sites, SCHOOL_SITE, UTILITY_SITES,
};
use crate::{Scenario, ScenarioCtx};
use geom::{vec2, Vec2};
use prototypes::Size2D;
use simulation::economy::ZoneDemand;
use simulation::map::{BuildingID, BuildingKind, Density, ZoneKind};
use simulation::map_dynamic::{BuildingInfos, ElectricityFlow, UtilityFlow};
use simulation::souls::{happiness::Happiness, residence::Residences};
use simulation::world_command::WorldCommand;
use simulation::{SimulationOptions, SoulID};
use std::sync::OnceLock;

pub const SCENARIO: (Scenario, SimulationOptions) = (
    Scenario {
        name: "services_basic",
        run,
    },
    OPTIONS,
);

pub(super) const OPTIONS: SimulationOptions = SimulationOptions {
    terrain_size: 10,
    save_replay: false,
};
pub(super) const TICKS: u64 = 600_000;

const PREFLIGHT_TICKS: u64 = TICKS / 2;
const ORIGIN: Vec2 = vec2(704.0, 704.0);
const SPACING: f32 = 100.0;
const COVERAGE_SITES: [(&str, Vec2); 6] = [
    SCHOOL_SITE,
    ("clinic", vec2(325.0, 150.0)),
    ("fire-station", vec2(125.0, 250.0)),
    ("police-station", vec2(125.0, 350.0)),
    ("garbage-depot", vec2(125.0, 450.0)),
    ("park", vec2(175.0, 350.0)),
];

static BASELINE: OnceLock<(u64, f32)> = OnceLock::new();

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let metrics = run_verified(ctx, true)?;
    metrics.check_basic()?;
    BASELINE.get_or_init(|| (metrics.tick, metrics.residential_demand));
    metrics.sample(ctx, SCENARIO.0.name);
    Ok(())
}

pub(super) fn reference_demand() -> Result<(u64, f32), String> {
    if let Some(&baseline) = BASELINE.get() {
        return Ok(baseline);
    }
    let mut ctx = ScenarioCtx::new(OPTIONS);
    let metrics = run_to(&mut ctx, true, false)?;
    metrics.check_basic()?;
    Ok(*BASELINE.get_or_init(|| (metrics.tick, metrics.residential_demand)))
}

pub(super) fn run_verified(
    ctx: &mut ScenarioCtx,
    coverage_services: bool,
) -> Result<Metrics, String> {
    let metrics = run_to(ctx, coverage_services, false)?;
    let mut resumed = ScenarioCtx::new(OPTIONS);
    run_to(&mut resumed, coverage_services, true)?;
    let resumed_hashes = resumed.sim.hashes();
    for (resource, hash) in ctx.sim.hashes() {
        if resumed_hashes.get(&resource) != Some(&hash) {
            return Err(format!(
                "midpoint resume changed final state for {resource}"
            ));
        }
    }
    Ok(metrics)
}

fn run_to(ctx: &mut ScenarioCtx, coverage_services: bool, resume: bool) -> Result<Metrics, String> {
    let start = ctx.sim.get_tick();
    // The fixture places its own connected terminal.
    let orphaned_terminals: Vec<_> = ctx
        .sim
        .map()
        .buildings()
        .values()
        .filter(|building| {
            matches!(building.kind, BuildingKind::RailFreightStation(_)) && building.is_orphaned()
        })
        .map(|building| WorldCommand::MapRemoveBuilding(building.id))
        .collect();
    ctx.sim.apply_commands(&orphaned_terminals);
    let grid = grid_city(ctx, ORIGIN, 7, 7, SPACING);
    if grid.roads.len() != 84 {
        return Err(format!("built {} of 84 grid roads", grid.roads.len()));
    }
    place_freight_station(ctx, ORIGIN + vec2(450.0, 250.0), Size2D::new(40.0, 50.0))
        .ok_or("could not place a road-connected freight station")?;
    let plant = place_grid_power_plant(ctx, ORIGIN)?;
    let coverage_sites = coverage_services
        .then_some(COVERAGE_SITES.as_slice())
        .unwrap_or(&[]);
    let services = place_service_sites(
        ctx,
        ORIGIN,
        UTILITY_SITES
            .into_iter()
            .chain(coverage_sites.iter().copied()),
    )?;
    let blocks = (0..6).flat_map(|y| {
        (0..6).filter_map(move |x| {
            let (kind, density) = match x {
                0..=3 => (ZoneKind::Residential, Density::High),
                4 if y < 4 => (ZoneKind::Commercial, Density::Low),
                5 if y < 2 => (ZoneKind::Industrial, Density::Low),
                _ => return None,
            };
            Some((x, y, kind, density))
        })
    });
    paint_zone_blocks(ctx, ORIGIN, SPACING, blocks);

    // Let the usual immigration and hiring systems staff the sites.
    let setup_ticks = ctx.sim.get_tick() - start;
    ctx.run_ticks(PREFLIGHT_TICKS - setup_ticks);
    check_service_network(ctx, plant, &services, "preflight")?;
    if resume {
        ctx.reload()?;
    }
    ctx.run_ticks(TICKS - PREFLIGHT_TICKS);
    check_service_network(ctx, plant, &services, "final")?;
    assess(ctx)
}

pub(super) fn check_service_network(
    ctx: &ScenarioCtx,
    plant: BuildingID,
    sites: &[BuildingID],
    stage: &str,
) -> Result<(), String> {
    let map = ctx.sim.map();
    let net = map
        .electricity
        .net_id(plant)
        .ok_or("power plant is disconnected")?;
    let electricity = ctx.sim.read::<ElectricityFlow>();
    if electricity.blackout(net) {
        return Err(format!("{stage}: service network is in a blackout"));
    }

    let infos = ctx.sim.read::<BuildingInfos>();
    for site in std::iter::once(plant).chain(sites.iter().copied()) {
        let prototype = map
            .get(site)
            .and_then(|building| building.kind.as_goods_company())
            .map(|id| id.prototype())
            .ok_or_else(|| format!("{stage}: provider site {site:?} is missing"))?;
        let company = match infos.owner(site) {
            Some(SoulID::GoodsCompany(id)) => ctx.sim.world().companies.get(id),
            _ => None,
        };
        let workers = match company {
            Some(company) => company.workers.0.len(),
            _ => 0,
        };
        if workers * 2 < prototype.n_workers as usize || map.electricity.net_id(site) != Some(net) {
            return Err(format!(
                "{stage}: {} has {workers}/{} workers or is disconnected",
                prototype.label, prototype.n_workers
            ));
        }
    }
    let utilities = ctx.sim.read::<UtilityFlow>();
    let totals = utilities
        .networks()
        .find_map(|(id, totals)| (id == net).then_some(totals))
        .ok_or("preflight: no utility flow on the service network")?;
    if totals.water_demand <= 0.0
        || totals.water_production < totals.water_demand
        || totals.sewage_capacity < totals.sewage_demand
    {
        return Err(format!("{stage}: inadequate utility capacity {totals:?}"));
    }
    println!(
        "services {stage}: {} providers staffed at least 50%, water {:.0}/{:.0}, sewage {:.0}/{:.0}",
        sites.len() + 1,
        totals.water_production,
        totals.water_demand,
        totals.sewage_capacity,
        totals.sewage_demand
    );
    Ok(())
}

pub(super) struct Metrics {
    pub(super) tick: u64,
    pub(super) residential_demand: f32,
    pub(super) happiness: f32,
    l2: usize,
    l3: usize,
}

fn assess(ctx: &ScenarioCtx) -> Result<Metrics, String> {
    let population = ctx.sim.world().humans.len();
    if population < 100 {
        return Err(format!("population {population} is below the floor of 100"));
    }

    let map = ctx.sim.map();
    let homes = ctx.sim.read::<Residences>();
    let flow = ctx.sim.read::<UtilityFlow>();
    let unserved = map
        .buildings()
        .values()
        .filter(|building| {
            building.kind != BuildingKind::ExternalTrading
                && !(flow.has_water(building.id) && flow.has_sewage(building.id))
        })
        .map(|building| (building.id, building.kind, building.connected_road))
        .collect::<Vec<_>>();
    if !unserved.is_empty() {
        return Err(format!(
            "final: buildings without water or sewage: {unserved:?}"
        ));
    }
    let (mut l2, mut l3) = (0, 0);
    for home in homes.by_building.values() {
        l2 += usize::from(home.level >= 2);
        l3 += usize::from(home.level == 3);
    }
    let residential_demand = ctx.sim.read::<ZoneDemand>().values[&ZoneKind::Residential];
    let happiness = ctx
        .sim
        .read::<Happiness>()
        .average(&ctx.sim.read::<BuildingInfos>());
    Ok(Metrics {
        tick: ctx.sim.get_tick(),
        residential_demand,
        happiness,
        l2,
        l3,
    })
}

impl Metrics {
    fn check_basic(&self) -> Result<(), String> {
        if self.l2 < 10 || self.happiness < 60.0 {
            return Err(format!(
                "expected at least 10 homes at L2 or higher and happiness of at least 60. Observed {}",
                self.summary()
            ));
        }
        Ok(())
    }

    pub(super) fn summary(&self) -> String {
        format!(
            "L2+={} L3={} happiness={:.2} unserved=0 R demand={:.2}",
            self.l2, self.l3, self.happiness, self.residential_demand
        )
    }

    pub(super) fn sample(&self, ctx: &mut ScenarioCtx, name: &str) {
        println!("{name}: {}; midpoint save/resume matched", self.summary());
        ctx.sample("final");
        ctx.samples.last_mut().unwrap().extra.extend([
            ("l2_residences".into(), self.l2 as f64),
            ("l3_residences".into(), self.l3 as f64),
            ("average_happiness".into(), f64::from(self.happiness)),
            ("unserved_buildings".into(), 0.0),
            (
                "residential_demand".into(),
                f64::from(self.residential_demand),
            ),
        ]);
    }
}
