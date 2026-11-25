use crate::builders::{
    grid_city, paint_zone_blocks, place_freight_station, place_grid_power_plant,
    place_service_sites, CommandSink, UTILITY_SITES,
};
use crate::{Scenario, ScenarioCtx};
use geom::{vec2, vec3, Vec2};
use prototypes::{
    try_prototype, ItemID, ResidencePrototypeID, Size2D, Tick, TICKS_PER_DAY, TICKS_PER_HOUR,
};
use simulation::economy::cargo::CargoBook;
use simulation::economy::progression::{utility_served_fraction, ProgressionMode};
use simulation::economy::{Budget, DailyEconomyMetrics, Market, TripPurpose, ZoneDemand};
use simulation::map::Density::{self, High, Low, Medium};
use simulation::map::ZoneKind::{self, Commercial, Industrial, MixedUse, Office, Residential};
use simulation::map::{
    BuildingID, BuildingKind, GrowableBusinessState, LaneKind, LanePatternBuilder, PathKind,
    ProjectFilter, ProjectKind, TerraformKind, ZoneCell,
};
use simulation::map_dynamic::{BuildingInfos, EnvGrid, Itinerary};
use simulation::souls::{
    freight_station::train_stop, goods_company::company_soul, growth::check_growable_lot, human,
    lifecycle, residence::Residences,
};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{
    GrowableSummary, MapPreset, SavePhase, Simulation, SimulationOptions, SlotId, SnapshotRevision,
    SoulID,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const OPTIONS: SimulationOptions = SimulationOptions {
    save_replay: true,
    terrain_size: 16,
    preset: MapPreset::Flat,
    seed: 625,
    starting_money_bucks: 2_000_000,
    ..SimulationOptions::LEGACY
};

pub(super) const CITY: (Scenario, SimulationOptions) = (
    Scenario {
        name: "zones2_city",
        run: run_city,
    },
    OPTIONS,
);
pub(super) const CONTROLLED: (Scenario, SimulationOptions) = (
    Scenario {
        name: "zones2_controlled",
        run: run_controlled,
    },
    SimulationOptions {
        save_replay: false,
        ..OPTIONS
    },
);

pub(super) const ATTAINMENT: (Scenario, SimulationOptions) = (
    Scenario {
        name: "zones2_l3",
        run: run_attainment,
    },
    OPTIONS,
);

const ORIGIN: Vec2 = vec2(640.0, 640.0);
const BLOCK: f32 = 120.0;
const BOOTSTRAP_END: u64 = TICKS_PER_DAY;
const COMMISSION_END: u64 = 5 * TICKS_PER_DAY;
const OBSERVATION_DAYS: u64 = 2;
const PERF_POPULATION: usize = 10_000;

const FOOD_SUPPLY: [(&str, Vec2); 8] = [
    ("bakery", vec2(35.0, 140.0)),
    ("bakery", vec2(103.0, 279.0)),
    ("bakery", vec2(230.0, 424.0)),
    ("bakery", vec2(340.0, 511.0)),
    ("bakery", vec2(497.0, 375.0)),
    ("flour-factory", vec2(430.0, -48.0)),
    ("flour-factory", vec2(646.0, 300.0)),
    ("cereal-farm", vec2(630.0, 155.0)),
];

fn zone(kind: ZoneKind, density: Density) -> ZoneCell {
    ZoneCell { kind, density }
}

fn has_profiles() -> bool {
    try_prototype(ResidencePrototypeID::new("residence-high-l1"))
        .is_some_and(|proto| proto.massing.is_some())
}

fn mixed_use_enabled() -> bool {
    let registry = prototypes::prototypes();
    [Medium, High]
        .into_iter()
        .all(|density| registry.mixed_use_pair(density).is_some())
}

fn require_profiles() -> Result<(), String> {
    if !has_profiles() || !mixed_use_enabled() {
        return Err("ZONES2 requires profiled housing and both Mixed Use prototype pairs".into());
    }
    Ok(())
}

fn elapsed(sim: &Simulation) -> u64 {
    simulation::diagnostics::elapsed_tick(sim).0
}

fn advance_to(ctx: &mut ScenarioCtx, target: u64) -> Result<(), String> {
    let ticks = target
        .checked_sub(elapsed(&ctx.sim))
        .ok_or("elapsed checkpoint already passed")?;
    ctx.run_ticks(ticks);
    Ok(())
}

fn build_district(
    ctx: &mut ScenarioCtx,
    with_health: bool,
    extra_sites: &[(&'static str, Vec2)],
) -> Result<BuildingID, String> {
    let grid = grid_city(ctx, ORIGIN, 6, 6, BLOCK);
    if grid.roads.len() != 60 {
        return Err(format!(
            "connected {} of 60 district streets",
            grid.roads.len()
        ));
    }
    place_grid_power_plant(ctx, ORIGIN + vec2(0.0, 40.0))?;
    let freight = place_freight_station(ctx, ORIGIN + vec2(546.0, 534.0), Size2D::new(40.0, 50.0))
        .ok_or("could not connect freight to the district")?;
    let services = [
        ("school", vec2(210.0, 225.0)),
        ("clinic", vec2(275.0, 245.0)),
        ("police-station", vec2(465.0, 390.0)),
        ("park", vec2(310.0, 221.0)),
        ("fire-station", vec2(-40.0, 550.0)),
        ("coal-mine", vec2(540.0, 625.0)),
        ("bakery", vec2(570.0, 102.0)),
    ];
    place_service_sites(
        ctx,
        ORIGIN,
        UTILITY_SITES
            .into_iter()
            .chain(
                services
                    .into_iter()
                    .filter(|&(name, _)| with_health || name != "clinic"),
            )
            .chain(extra_sites.iter().copied()),
    )?;
    Ok(freight)
}

fn connect_freight(sim: &mut Simulation, terminal: BuildingID) -> Result<(), String> {
    let Some(SoulID::FreightStation(station)) = sim.read::<BuildingInfos>().owner(terminal) else {
        return Err("the district freight terminal has no station entity".into());
    };
    let stop = train_stop(&sim.world().freight_stations[station].trans);
    // The Flat bootstrap has a rail line ending at this north junction.
    let junction_x = 3036.0;
    let track = {
        let map = sim.map();
        let from = map.project(vec3(junction_x, 222.0, 0.0), 10.0, ProjectFilter::INTER);
        if !matches!(from.kind, ProjectKind::Intersection(_)) {
            return Err("the district's outside rail junction is missing".into());
        }
        let z = map
            .environment()
            .height(stop.xy())
            .ok_or("platform lies outside terrain")?;
        WorldCommand::MapMakeConnection {
            from,
            to: map.project(stop.xy().z(z), 0.0, ProjectFilter::INTER),
            inter: Some(vec2(junction_x, stop.y)),
            pat: LanePatternBuilder::new().rail(true).build(),
        }
    };
    sim.apply_commands([&track]);
    if !sim.read::<CommandOutcomes>().accepted.contains(&0) {
        return Err(format!(
            "freight branch was rejected: {:?}",
            sim.read::<CommandOutcomes>().last
        ));
    }
    {
        let map = sim.map();
        let platform = map
            .nearest_lane(stop, LaneKind::Rail, Some(50.0))
            .and_then(|lane| map.get(lane))
            .ok_or("the district freight terminal has no platform track")?;
        let outside = map
            .buildings()
            .values()
            .find(|building| building.kind == BuildingKind::ExternalTrading)
            .ok_or("the district has no outside trading building")?
            .obb
            .center()
            .z0();
        let platform = platform.points.project(stop);
        let tick = Tick(sim.get_tick());
        for (from, to) in [(outside, platform), (platform, outside)] {
            if Itinerary::route(tick, from, to, &map, PathKind::Rail).is_none() {
                return Err(
                    "district freight platform cannot complete an outside round trip".into(),
                );
            }
        }
    }
    super::demo_cities::spawn_freight_train(sim)
}

fn grow_city(ctx: &mut ScenarioCtx, mixed: bool, replay_root: Option<&Path>) -> Result<(), String> {
    grow_city_with_health(ctx, mixed, replay_root, true, &FOOD_SUPPLY)
}

fn grow_city_with_health(
    ctx: &mut ScenarioCtx,
    mixed: bool,
    replay_root: Option<&Path>,
    with_health: bool,
    extra_sites: &[(&'static str, Vec2)],
) -> Result<(), String> {
    let freight = build_district(ctx, with_health, extra_sites)?;
    connect_freight(&mut ctx.sim, freight)?;
    // The initial industrial and retail hiring keeps unemployment near zero.
    // Set the same office tax incentive in both districts so office demand clears the paint threshold.
    ctx.sim.apply_commands([&WorldCommand::SetTaxRate {
        zone: Office,
        rate_permille: 0,
    }]);
    paint_zone_blocks(
        ctx,
        ORIGIN,
        BLOCK,
        [
            (0, 0, Residential, Low),
            (0, 1, Residential, Low),
            (0, 2, Residential, Medium),
            (1, 1, Residential, Medium),
            (2, 0, Commercial, Low),
            (4, 0, Industrial, Low),
            (4, 1, Industrial, Low),
            (4, 2, Office, Low),
        ],
    );

    advance_to(ctx, BOOTSTRAP_END)?;
    let resumed = replay_root
        .map(|root| verify_checkpoint(&ctx.sim, root, "zones2_before_core"))
        .transpose()?;
    paint_core(ctx, mixed);
    if let Some((root, resumed)) = replay_root.zip(resumed) {
        let mut resumed = ScenarioCtx::from_sim(resumed);
        paint_core(&mut resumed, mixed);
        let uninterrupted = ctx.sim.hashes();
        let resumed_hashes = resumed.sim.hashes();
        if resumed_hashes != uninterrupted {
            let changed: BTreeSet<_> = uninterrupted
                .keys()
                .chain(resumed_hashes.keys())
                .filter(|key| uninterrupted.get(*key) != resumed_hashes.get(*key))
                .collect();
            return Err(format!(
                "resumed city diverged during the core paint wave: {changed:?}"
            ));
        }
        verify_checkpoint(&ctx.sim, root, "zones2_after_core")?;
    }
    if let Some(root) = replay_root {
        const EARLY_CORE: u64 = BOOTSTRAP_END + 6 * TICKS_PER_HOUR;
        advance_to(ctx, EARLY_CORE)?;
        let early = l1_core_towers(&ctx.sim);
        publish(
            &ctx.sim,
            root,
            "zones2_core_l1",
            "ZONES2 core before upgrades",
        )?;
        advance_to(ctx, COMMISSION_END)?;
        if !early.into_iter().any(|(id, door)| {
            ctx.sim
                .growable_summary(id)
                .is_some_and(|site| site.level == 3)
                && ctx
                    .sim
                    .map()
                    .get(id)
                    .is_some_and(|building| building.door_pos == door)
        }) {
            return Err("no L1 core tower reached L3 in place for the capture".into());
        }
        verify_checkpoint(&ctx.sim, root, "zones2_core_l3")?;
        Ok(())
    } else {
        advance_to(ctx, COMMISSION_END)
    }
}

fn l1_core_towers(sim: &Simulation) -> Vec<(BuildingID, geom::Vec3)> {
    let center = ORIGIN + vec2(2.5, 2.5) * BLOCK;
    let map = sim.map();
    map.buildings()
        .iter()
        .filter_map(|(id, building)| {
            let site = building.development()?;
            (site.zone == zone(Residential, High)
                && site.level == 1
                && building.obb.center().distance2(center) < (2.0 * BLOCK).powi(2))
            .then_some((id, building.door_pos))
        })
        .collect()
}

fn paint_core(ctx: &mut ScenarioCtx, mixed: bool) {
    paint_zone_blocks(
        ctx,
        ORIGIN,
        BLOCK,
        [
            (1, 2, Residential, High),
            (2, 1, Residential, High),
            (2, 2, Residential, High),
            (0, 3, Residential, Medium),
            (1, 3, Residential, High),
            (3, 1, Commercial, Low),
            (3, 2, Commercial, Low),
            (4, 3, Office, High),
            (4, 4, Industrial, Medium),
        ],
    );
    let infill = if mixed {
        [(2, 3, MixedUse, Medium), (3, 3, MixedUse, High)]
    } else {
        [(2, 3, Residential, Medium), (3, 3, Residential, High)]
    };
    paint_zone_blocks(ctx, ORIGIN, BLOCK, infill);
    if !mixed {
        paint_zone_blocks(ctx, ORIGIN, BLOCK, [(3, 4, Commercial, Low)]);
    }
}

fn run_attainment(ctx: &mut ScenarioCtx) -> Result<(), String> {
    use simulation::map::GrowableSiteState;
    use simulation::souls::development::requirements;

    require_profiles()?;
    grow_city_with_health(ctx, true, None, true, &[])?;
    check_commissioning(ctx, true)?;
    for kind in [Residential, MixedUse] {
        let count = summaries(&ctx.sim)
            .filter(|site| {
                site.zone.kind == kind
                    && site.level == 3
                    && site.occupants > 0
                    && site.site == GrowableSiteState::Active
            })
            .count();
        if count == 0 {
            return Err(format!("{kind:?} had no active L3 home"));
        }
        println!("{kind:?} reached level 3 at {count} active sites");
    }

    let mut control = ScenarioCtx::new(OPTIONS);
    grow_city_with_health(&mut control, true, None, false, &[])?;
    for kind in [Residential, MixedUse] {
        let ids: Vec<_> = control.sim.map().buildings().keys().collect();
        let mut health_blocked = 0;
        for id in ids {
            let Some(site) = control
                .sim
                .growable_summary(id)
                .filter(|site| site.zone.kind == kind && site.site == GrowableSiteState::Active)
            else {
                continue;
            };
            if site.level >= 3 {
                return Err(format!("{kind:?} control reached L3 without a clinic"));
            }
            if site.level == 2
                && requirements(&control.sim, id).is_some_and(|status| {
                    status
                        .requirements
                        .iter()
                        .filter(|need| !need.met)
                        .map(|need| need.name)
                        .eq(["Health coverage"])
                })
            {
                health_blocked += 1;
            }
        }
        if health_blocked == 0 {
            return Err(format!(
                "{kind:?} control had no L2 home blocked solely by health coverage"
            ));
        }
        println!(
            "{kind:?} control had {health_blocked} L2 sites blocked solely by health coverage"
        );
    }
    ctx.sample("ordinary_l3_attainment");
    Ok(())
}

fn run_city(ctx: &mut ScenarioCtx) -> Result<(), String> {
    require_profiles()?;
    let replay_root = export_root("EGR_ZONES2_REPLAY_ROOT")?;
    grow_city(ctx, true, replay_root.as_deref())?;
    check_commissioning(ctx, true)?;
    ctx.sample("commissioned");
    let mixed = observe_days(ctx)?;

    let mut control = ScenarioCtx::new(OPTIONS);
    grow_city(&mut control, false, None)?;
    check_commissioning(&control, false)?;
    let pure = observe_days(&mut control)?;
    ctx.samples
        .extend(control.samples.into_iter().map(|mut sample| {
            sample.label = format!("pure_control_{}", sample.label);
            sample
        }));
    compare_economies(&mixed, &pure)
}

fn verify_checkpoint(sim: &Simulation, root: &Path, label: &str) -> Result<Simulation, String> {
    publish(sim, root, label, label)?;
    let id = SlotId::new(label).map_err(|err| err.to_string())?;
    let loaded = Simulation::load_from_slot(root, &id).map_err(|err| err.to_string())?;
    let expected = sim.hashes();
    if loaded.hashes() != expected {
        return Err(format!(
            "{label}: snapshot diverged from the uninterrupted city"
        ));
    }
    let replay = Simulation::load_replay_from_slot(root, &id).map_err(|err| err.to_string())?;
    let (mut playback, mut loader) =
        Simulation::try_from_replay(replay).map_err(|err| err.to_string())?;
    let (_, steps) = loader
        .step_progress()
        .ok_or("city replay has no stepped timeline")?;
    let mut schedule = Simulation::schedule();
    let finished = (0..=steps).any(|_| loader.advance_tick(&mut playback, &mut schedule));
    if !finished || loader.step_progress() != Some((steps, steps)) || playback.hashes() != expected
    {
        return Err(format!(
            "{label}: decoded sidecar diverged from the uninterrupted city"
        ));
    }
    println!("{label}: snapshot and sidecar matched after {steps} steps");
    Ok(loaded)
}

fn summaries(sim: &Simulation) -> impl Iterator<Item = GrowableSummary> + '_ {
    let buildings: Vec<_> = sim.map().buildings().keys().collect();
    buildings
        .into_iter()
        .filter_map(|id| sim.growable_summary(id))
}

fn check_commissioning(ctx: &ScenarioCtx, mixed: bool) -> Result<(), String> {
    let sim = &ctx.sim;
    let sites: Vec<_> = summaries(sim).collect();
    for density in [Low, Medium, High] {
        if !sites
            .iter()
            .any(|site| site.zone == zone(Residential, density) && site.occupants > 0)
        {
            return Err(format!(
                "no occupied {density:?} housing by elapsed tick {}",
                elapsed(sim)
            ));
        }
    }
    for kind in [Commercial, Industrial, Office] {
        let filled: usize = sites
            .iter()
            .filter(|site| site.zone.kind == kind)
            .map(|site| site.filled_jobs)
            .sum();
        if filled == 0 {
            return Err(format!("no staffed pure {kind:?} zoning commissioned"));
        }
        println!("commissioned {filled} filled {kind:?} jobs");
    }
    let map = sim.map();
    let core = ORIGIN + vec2(2.5, 2.5) * BLOCK;
    let env = sim.read::<EnvGrid>();
    let mut towers = 0;
    let mut best_core_value = 0;
    for (id, building) in map.buildings().iter() {
        if building.obb.center().distance2(core) < (2.0 * BLOCK).powi(2)
            && sim.growable_summary(id).is_some_and(|site| {
                site.zone == zone(Residential, High) && site.occupants > 0 && site.floors >= 12
            })
        {
            towers += 1;
            best_core_value = best_core_value.max(env.land_value(id));
        }
    }
    if best_core_value < 50 {
        return Err(format!(
            "occupied core towers need measured land value ≥50, got {best_core_value}"
        ));
    }
    println!("commissioned {towers} occupied core towers, peak land value {best_core_value}");
    if mixed {
        for density in [Medium, High] {
            if !summaries(sim).any(|site| {
                site.zone == zone(MixedUse, density) && site.occupants > 0 && site.filled_jobs > 0
            }) {
                return Err(format!("no occupied and staffed {density:?} Mixed Use"));
            }
        }
        for kind in [Residential, MixedUse] {
            if !summaries(sim).any(|site| site.zone.kind == kind && site.level >= 3) {
                return Err(format!(
                    "{kind:?} did not attain level 3 through ordinary play"
                ));
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Observation {
    resident_days: f64,
    external_shops: u64,
    local_pickups: u64,
    baskets_due: u64,
    baskets_filled: u64,
    food_stockout_hours: f64,
    mixed_receipt_units: u64,
    mixed_shelf_peak: i32,
    productive_recipients: usize,
    housing_slots: f64,
    available_jobs: f64,
    filled_jobs: f64,
}

fn observe_days(ctx: &mut ScenarioCtx) -> Result<Observation, String> {
    let mut result = Observation::default();
    let mut totals = BTreeMap::new();
    let first_day = elapsed(&ctx.sim) / TICKS_PER_DAY;
    let shelf = ItemID::new("retail-goods");
    let input = ItemID::new("goods");
    let mut known_sellers = BTreeSet::new();
    let mut recipients = BTreeSet::new();
    let mut productive_recipients = BTreeSet::new();
    let mut utility_samples = 0;
    let mut utility_served = 0.0;
    for day in first_day..first_day + OBSERVATION_DAYS {
        for _ in 0..24 {
            sample_capacity(&ctx.sim, &mut result);
            let mut progress: Vec<_> = mixed_sellers(&ctx.sim)
                .filter_map(|seller| {
                    let SoulID::GoodsCompany(id) = seller else {
                        return None;
                    };
                    known_sellers.insert(seller);
                    Some((id, ctx.sim.get(id)?.comp.progress))
                })
                .collect();
            if progress.is_empty() {
                ctx.run_ticks(TICKS_PER_HOUR);
            } else {
                ctx.run_ticks_observed(TICKS_PER_HOUR, |sim| {
                    for (id, previous) in &mut progress {
                        let Some(company) = sim.get(*id) else {
                            continue;
                        };
                        let progress = company.comp.progress;
                        let seller = SoulID::GoodsCompany(*id);
                        // Company progress wraps only when the recipe produces a batch.
                        if progress < *previous {
                            let cargo = sim.read::<CargoBook>();
                            if cargo.freight_day(day).is_some_and(|day| {
                                let receipt = day.receipt(seller, input);
                                receipt.road_units > 0 || receipt.station_units > 0
                            }) {
                                recipients.insert(seller);
                            }
                            if recipients.contains(&seller) {
                                productive_recipients.insert(seller);
                            }
                        }
                        *previous = progress;
                    }
                });
            }
            if let Some(served) = utility_served_fraction(&ctx.sim) {
                utility_samples += 1;
                utility_served += served;
            }
            let market = ctx.sim.read::<Market>();
            for seller in mixed_sellers(&ctx.sim) {
                known_sellers.insert(seller);
                result.mixed_shelf_peak =
                    result.mixed_shelf_peak.max(market.capital(seller, shelf));
            }
        }
        let now = elapsed(&ctx.sim);
        let daily = ctx.sim.read::<DailyEconomyMetrics>();
        let data = daily
            .closed_day(day, now)
            .ok_or_else(|| format!("missing economy day {day}"))?;
        result.baskets_due += data.baskets_due.unwrap_or_default();
        result.baskets_filled += data.baskets_filled.unwrap_or_default();
        result.local_pickups += data.local_pickups;
        result.external_shops += data
            .trips_by_purpose
            .get(&TripPurpose::Shopping)
            .map_or(0, |trip| trip.completed);
        let food = ItemID::new("effective-food");
        result.food_stockout_hours += data
            .stockout_ticks_by_sku
            .get(&food)
            .copied()
            .unwrap_or_default() as f64
            / TICKS_PER_HOUR as f64;
        if let Some(entries) = ctx.sim.read::<Budget>().days().get(&day) {
            for (category, amount) in entries {
                *totals
                    .entry(format!("budget_{category:?}_inner"))
                    .or_default() += *amount as f64;
            }
        }
        if let Some(freight) = ctx.sim.read::<CargoBook>().freight_day(day) {
            *totals.entry("cancelled_loads".into()).or_default() += freight.cancelled_loads as f64;
            *totals.entry("road_delivered_loads".into()).or_default() +=
                freight.delivered_loads as f64;
            *totals.entry("road_delivered_units".into()).or_default() +=
                freight.delivered_units as f64;
            for &seller in &known_sellers {
                let receipt = freight.receipt(seller, input);
                if receipt.road_units > 0 || receipt.station_units > 0 {
                    recipients.insert(seller);
                }
                result.mixed_receipt_units += receipt.road_units + receipt.station_units;
                *totals.entry("mx_receipt_road_units".into()).or_default() +=
                    receipt.road_units as f64;
                *totals.entry("mx_receipt_station_units".into()).or_default() +=
                    receipt.station_units as f64;
            }
        }
    }
    result.productive_recipients = productive_recipients.len();
    let hours = (OBSERVATION_DAYS * 24) as f64;
    result.housing_slots /= hours;
    result.available_jobs /= hours;
    result.filled_jobs /= hours;
    println!(
        "operating window: {:.2} resident-days, {} filled/{} due baskets, {} external arrivals, {} local pickups, {:.2} food stockout hours, utilities {:.2}/{}",
        result.resident_days, result.baskets_filled, result.baskets_due, result.external_shops,
        result.local_pickups, result.food_stockout_hours, utility_served, utility_samples
    );
    if utility_samples == 0 || utility_served / f64::from(utility_samples) < 0.95 {
        return Err(format!(
            "city utilities served {utility_served:.2}/{utility_samples} hourly samples"
        ));
    }
    totals.extend([
        ("housing_slots_hourly_mean".into(), result.housing_slots),
        ("available_jobs_hourly_mean".into(), result.available_jobs),
        ("filled_jobs_hourly_mean".into(), result.filled_jobs),
        ("utility_served_demand".into(), utility_served),
        ("utility_total_demand".into(), f64::from(utility_samples)),
        (
            "mx_productive_recipients".into(),
            result.productive_recipients as f64,
        ),
        ("resident_days".into(), result.resident_days),
        ("local_pickups".into(), result.local_pickups as f64),
        (
            "external_shopping_arrivals".into(),
            result.external_shops as f64,
        ),
        ("baskets_due".into(), result.baskets_due as f64),
        ("baskets_filled".into(), result.baskets_filled as f64),
        ("food_stockout_hours".into(), result.food_stockout_hours),
        (
            "mx_shelf_stock_peak".into(),
            f64::from(result.mixed_shelf_peak),
        ),
        (
            "mixed_use_enabled".into(),
            f64::from(u8::from(mixed_use_enabled())),
        ),
        ("profiled_sites".into(), summaries(&ctx.sim).count() as f64),
    ]);
    ctx.sample("operating_days");
    ctx.samples.last_mut().unwrap().extra.extend(totals);
    Ok(result)
}

fn sample_capacity(sim: &Simulation, result: &mut Observation) {
    result.resident_days += sim.world().humans.len() as f64 / 24.0;
    let homes = sim.read::<Residences>();
    for (&id, home) in &homes.by_building {
        if !homes.is_abandoned(id) {
            result.housing_slots += f64::from(home.capacity);
        }
    }
    for company in sim.world().companies.values().filter(|company| {
        try_prototype(prototypes::GrowableCompanyID::from(company.comp.proto))
            .is_some_and(|proto| proto.growable)
    }) {
        result.available_jobs += f64::from(company.comp.max_workers);
        result.filled_jobs += company.workers.0.len() as f64;
    }
}

fn mixed_sellers(sim: &Simulation) -> impl Iterator<Item = SoulID> + '_ {
    sim.world().companies.iter().filter_map(|(id, company)| {
        try_prototype(prototypes::GrowableCompanyID::from(company.comp.proto))
            .filter(|proto| proto.zone_kind == MixedUse)
            .map(|_| SoulID::GoodsCompany(id))
    })
}

fn compare_economies(mixed: &Observation, pure: &Observation) -> Result<(), String> {
    let trips = |data: &Observation| data.external_shops as f64 / data.resident_days.max(1.0);
    let filled = |data: &Observation| data.baskets_filled as f64 / data.baskets_due.max(1) as f64;
    let stockout = |data: &Observation| data.food_stockout_hours / data.resident_days.max(1.0);
    let evidence = format!(
        "MX economy (mixed/pure): local={}, receipt units={}, producing recipients={}, external trips/resident-day={:.3}/{:.3}, fill={:.3}/{:.3}, stockout hours/resident-day={:.3}/{:.3}",
        mixed.local_pickups, mixed.mixed_receipt_units, mixed.productive_recipients, trips(mixed), trips(pure),
        filled(mixed), filled(pure), stockout(mixed), stockout(pure)
    );
    println!("{evidence}");
    let matched = |a: f64, b: f64| a > 0.0 && b > 0.0 && (a - b).abs() <= 0.05 * b;
    if !matched(mixed.housing_slots, pure.housing_slots)
        || !matched(mixed.available_jobs, pure.available_jobs)
        || !matched(mixed.filled_jobs, pure.filled_jobs)
    {
        return Err(format!(
            "pure control outside 5%: housing {}/{}, available jobs {}/{}, filled jobs {}/{} (MX/pure)",
            mixed.housing_slots, pure.housing_slots, mixed.available_jobs, pure.available_jobs,
            mixed.filled_jobs, pure.filled_jobs
        ));
    }
    if mixed.local_pickups < 4
        || mixed.mixed_receipt_units == 0
        || mixed.productive_recipients == 0
        || pure.external_shops == 0
        || trips(mixed) > 0.9 * trips(pure)
        || filled(mixed) < 0.8
        || filled(mixed) + 0.05 < filled(pure)
        || stockout(mixed) > 1.5
        || stockout(mixed) > stockout(pure) + 0.25
    {
        return Err(evidence);
    }
    Ok(())
}

fn run_controlled(ctx: &mut ScenarioCtx) -> Result<(), String> {
    require_profiles()?;
    let shots_root = export_root("EGR_ZONES2_SHOTS_ROOT")?;
    let perf_root = export_root("EGR_ZONES2_PERF_ROOT")?;
    build_district(ctx, true, &[])?;
    let parcels = [
        (vec2(754.0, 790.0), zone(Residential, Low)),
        (vec2(821.0, 790.0), zone(Residential, Medium)),
        (vec2(872.0, 790.0), zone(Residential, High)),
        (vec2(930.0, 790.0), zone(Residential, High)),
        (vec2(972.0, 790.0), zone(MixedUse, High)),
        (vec2(1069.0, 790.0), zone(MixedUse, Medium)),
        (vec2(1115.0, 790.0), zone(Commercial, High)),
        (vec2(966.0, 735.0), zone(Office, High)),
        (vec2(1100.0, 735.0), zone(Industrial, High)),
    ];
    let mut buildings = Vec::new();
    for (near, cell) in parcels {
        let lot = {
            let map = ctx.sim.map();
            map.lots()
                .values()
                .filter(|lot| check_growable_lot(&map, lot, cell).is_ok())
                .min_by(|a, b| {
                    a.shape
                        .center()
                        .distance2(near)
                        .total_cmp(&b.shape.center().distance2(near))
                        .then_with(|| a.id.cmp(&b.id))
                })
                .ok_or_else(|| format!("no lot fits {cell:?} near {near:?}"))?
                .id
        };
        let area = ctx.sim.map().get(lot).unwrap().shape;
        if cell == zone(MixedUse, Medium) {
            let frontage = (area.corners[0] + area.corners[1]) * 0.5;
            ctx.sim.apply_commands([&WorldCommand::Terraform {
                center: frontage,
                radius: 24.0,
                amount: -110.0,
                level: 0.0,
                kind: TerraformKind::Elevation,
                slope: None,
            }]);
            if !ctx.sim.read::<CommandOutcomes>().accepted.contains(&0) {
                return Err("sloped storefront terrain edit was rejected".into());
            }
        }
        ctx.sim.apply_commands([&WorldCommand::ZonePaint {
            area,
            kind: Some(cell.kind),
            density: cell.density,
        }]);
        let site = ctx
            .sim
            .place_zoned_growable(lot, cell)
            .map_err(|err| format!("{cell:?}: {err:?}"))?;
        if cell == zone(MixedUse, Medium) {
            let map = ctx.sim.map();
            let building = map.get(site.building).ok_or("missing sloped storefront")?;
            let shell = building
                .development()
                .and_then(|site| site.frozen_shell.as_ref())
                .ok_or("missing sloped storefront shell")?;
            let fit = shell.fit();
            if fit.base_z() - fit.frontage_landing().z <= 0.05 {
                return Err(format!(
                    "sloped storefront needs a graded connector, got {:.3} m of rise",
                    fit.base_z() - fit.frontage_landing().z
                ));
            }
        }
        buildings.push(site.building);
        if site.housing_slots > 0 {
            human::spawn_human(&mut ctx.sim, site.building)
                .ok_or("could not occupy the controlled home")?;
        }
        let summary = ctx
            .sim
            .growable_summary(site.building)
            .ok_or("missing committed profile")?;
        if cell.density == High && summary.floors < 12 {
            return Err(format!("{cell:?} has {} floors", summary.floors));
        }
        if site.housing_slots != summary.housing_slots || site.job_slots != summary.planned_jobs {
            return Err("realized slots differ from checked placement".into());
        }
    }
    let storefront = buildings[4];
    let origin = *ctx
        .sim
        .map()
        .lot_origin(storefront)
        .ok_or("missing storefront parcel")?;
    let business = {
        let proto = ctx
            .sim
            .map()
            .get(storefront)
            .unwrap()
            .kind
            .as_goods_company()
            .unwrap();
        company_soul(&mut ctx.sim, storefront, proto).ok_or("controlled storefront did not open")?
    };
    if let Some(root) = &shots_root {
        fill_skyline(&mut ctx.sim)?;
        // Let the first hourly environment pass populate the value overlay.
        ctx.run_ticks(TICKS_PER_HOUR);
        ctx.sim.write::<ZoneDemand>().values.extend([
            (Residential, 42.0),
            (Commercial, 4.0),
            (Industrial, -18.0),
            (Office, 21.0),
            (MixedUse, 4.0),
        ]);
        publish(&ctx.sim, root, "zones2_city", "ZONES2 dense core")?;
    }
    let SoulID::GoodsCompany(company) = business else {
        return Err("controlled storefront has no business role".into());
    };
    let before = ctx.sim.growable_summary(storefront).unwrap();
    if !lifecycle::close_business(&mut ctx.sim, company) {
        return Err("could not close the controlled storefront".into());
    }
    ctx.sim.apply_commands([]);
    let closed = ctx.sim.growable_summary(storefront).unwrap();
    if !matches!(
        closed.business,
        Some(GrowableBusinessState::ClosedUntil { .. })
    ) || closed.occupants != before.occupants
        || closed.housing_slots != before.housing_slots
        || closed.active_job_slots != 0
    {
        return Err("seller closure changed the controlled housing role".into());
    }
    ctx.reload()?;
    if let Some(root) = &shots_root {
        publish(
            &ctx.sim,
            root,
            "zones2_closed",
            "ZONES2 shuttered storefront",
        )?;
        let slot = SlotId::new("zones2_closed").map_err(|err| err.to_string())?;
        let sim = Simulation::load_from_slot(root, &slot).map_err(|err| err.to_string())?;
        let mut collapsed = ScenarioCtx::from_sim(sim);
        collapsed
            .sim
            .apply_commands([&WorldCommand::MapRemoveRoad(origin.parent_road)]);
        collapsed.run_ticks(TICKS_PER_HOUR);
        let abandoned = collapsed
            .sim
            .growable_summary(storefront)
            .ok_or("missing abandoned shell")?;
        if !matches!(
            abandoned.site,
            simulation::map::GrowableSiteState::Abandoned { .. }
        ) || abandoned.occupants > 0
            || abandoned.active_job_slots > 0
        {
            return Err("frontage removal did not abandon both roles".into());
        }
        publish(
            &collapsed.sim,
            root,
            "zones2_abandoned",
            "ZONES2 abandoned storefront",
        )?;
        collapsed.run_ticks(13 * TICKS_PER_HOUR);
        if collapsed.sim.map().get(storefront).is_some() {
            return Err("abandoned storefront did not collapse".into());
        }
        publish(
            &collapsed.sim,
            root,
            "zones2_collapsed",
            "ZONES2 timed collapse",
        )?;
    }
    ctx.sim
        .apply_commands([&WorldCommand::MapRemoveBuilding(storefront)]);
    let map = ctx.sim.map();
    if map.get(storefront).is_some() {
        return Err("ordinary bulldoze did not remove the shuttered storefront".into());
    }
    if !map.lots().values().any(|lot| {
        lot.parent == origin.parent_road
            && lot.shape.center().distance2(origin.shape.center()) < 0.01
            && check_growable_lot(&map, lot, zone(MixedUse, High)).is_ok()
    }) {
        return Err("ordinary bulldoze did not restore the usable storefront parcel".into());
    }
    drop(map);
    if let Some(root) = &shots_root {
        publish(
            &ctx.sim,
            root,
            "zones2_demolished",
            "ZONES2 restored parcel",
        )?;
        let guided = Simulation::new_with_options(SimulationOptions {
            progression: ProgressionMode::Guided,
            ..CONTROLLED.1
        });
        publish(&guided, root, "zones2_guided", "ZONES2 Guided locks")?;
    }
    ctx.sample("controlled");
    if let Some(root) = perf_root {
        export_perf(&root)?;
    }
    Ok(())
}

fn fill_skyline(sim: &mut Simulation) -> Result<(), String> {
    const INFILL_SITES: usize = 20;
    let core = vec2(960.0, 810.0);
    let mut lots: Vec<_> = sim
        .map()
        .lots()
        .values()
        .filter_map(|lot| {
            let pos = lot.shape.center();
            let distance = pos.distance2(core);
            // Keep the foreground and the oblique view of the storefront clear.
            (distance <= 230.0_f32.powi(2)
                && pos.y >= 714.0
                && pos.distance2(vec2(972.0, 790.0)) >= 42.0_f32.powi(2))
            .then_some((distance, lot.id, lot.shape))
        })
        .collect();
    lots.sort_unstable_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let cohorts = [
        zone(Residential, High),
        zone(MixedUse, High),
        zone(Residential, Medium),
        zone(Commercial, High),
        zone(Residential, Low),
    ];
    let mut added = 0;
    for (index, (_, lot, area)) in lots.into_iter().enumerate() {
        let cell = cohorts[index % cohorts.len()];
        let fallback = zone(Residential, Low);
        let Some(cell) = [cell, fallback]
            .into_iter()
            .find(|&cell| sim.place_zoned_growable(lot, cell).is_ok())
        else {
            continue;
        };
        sim.apply_commands([&WorldCommand::ZonePaint {
            area,
            kind: Some(cell.kind),
            density: cell.density,
        }]);
        added += 1;
        if added == INFILL_SITES {
            return Ok(());
        }
    }
    Err(format!(
        "only {added} of {INFILL_SITES} parcels fit the skyline infill"
    ))
}

fn export_root(var: &str) -> Result<Option<PathBuf>, String> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .map(|root| {
            reserve_export_root(&root)?;
            Ok(root)
        })
        .transpose()
}

fn reserve_export_root(root: &Path) -> Result<(), String> {
    std::fs::create_dir(root).map_err(|err| {
        format!(
            "fixture export requires a new directory {}: {err}",
            root.display()
        )
    })
}

fn publish(sim: &Simulation, root: &Path, label: &str, display: &str) -> Result<(), String> {
    let id = SlotId::new(label).map_err(|err| err.to_string())?;
    let phase = sim
        .encode_slot(SnapshotRevision::new(1))
        .map_err(|err| err.to_string())?
        .with_display_name(display.to_owned())
        .publish(root, &id)
        .map_err(|err| err.to_string())?;
    if !matches!(phase, SavePhase::Complete) {
        return Err(format!("{label}: incomplete slot publication: {phase:?}"));
    }
    Ok(())
}

fn export_perf(root: &Path) -> Result<(), String> {
    if !has_profiles() {
        return Err("the M0 tower workload requires the staged ZONES2 profiles".into());
    }
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        terrain_size: 24,
        starting_money_bucks: 8_000_000,
        ..CONTROLLED.1
    });
    let origin = vec2(224.0, 224.0);
    grid_city(&mut ctx, origin, 17, 17, 130.0);
    let lots = {
        let map = ctx.sim.map();
        let mut lots: Vec<_> = map
            .lots()
            .values()
            .map(|lot| (lot.id, lot.shape.center()))
            .collect();
        lots.sort_unstable_by_key(|(id, _)| *id);
        lots
    };
    let mut towers = 0;
    let mut jobs = 0;
    let mut residents = 0;
    for (lot, center) in lots {
        let column = ((center.x - origin.x) / 130.0).floor() as i32;
        let work = column.rem_euclid(3) == 2;
        if (work && jobs >= PERF_POPULATION / 2) || (!work && residents >= PERF_POPULATION) {
            continue;
        }
        let cell = zone(if work { Office } else { Residential }, High);
        let Ok(site) = ctx.sim.place_zoned_growable(lot, cell) else {
            continue;
        };
        if work {
            jobs += site.job_slots as usize;
        } else {
            let slots = (site.housing_slots as usize).min(PERF_POPULATION - residents);
            for _ in 0..slots {
                human::spawn_human(&mut ctx.sim, site.building)
                    .ok_or("could not fill tower slot")?;
            }
            residents += slots;
        }
        towers += 1;
        if residents >= PERF_POPULATION && jobs >= PERF_POPULATION / 2 {
            break;
        }
    }
    if residents != PERF_POPULATION || jobs < PERF_POPULATION / 2 || towers == 0 {
        return Err(format!(
            "M0 fixture incomplete: {residents} residents, {jobs} jobs, {towers} towers"
        ));
    }
    ctx.flush();
    let live_jobs: u32 = ctx
        .sim
        .world()
        .companies
        .values()
        .map(|company| company.comp.max_workers)
        .sum();
    let actual_agents = ctx.sim.world().humans.len();
    if actual_agents < PERF_POPULATION || live_jobs < (PERF_POPULATION / 2) as u32 {
        return Err(format!(
            "M0 fixture has {} residents and {live_jobs} active jobs",
            ctx.sim.world().humans.len()
        ));
    }
    println!(
        "zones2_towers_10k: actual agents={actual_agents}, towers={towers}, active jobs={live_jobs}"
    );
    publish(
        &ctx.sim,
        root,
        "zones2_towers_10k",
        "ZONES2 10,000 agents in towers",
    )
}
