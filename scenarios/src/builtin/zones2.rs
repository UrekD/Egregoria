use crate::builders::{
    grid_city, paint_zone_blocks, place_freight_station, place_grid_power_plant,
    place_service_sites, CommandSink, UTILITY_SITES,
};
use crate::{Scenario, ScenarioCtx};
use geom::{vec2, vec3, Vec2, Vec3};
use prototypes::{
    try_prototype, ItemID, ResidencePrototypeID, Size2D, Tick, TICKS_PER_DAY, TICKS_PER_HOUR,
};
use simulation::economy::balance::DEMAND_INTERVAL_TICKS;
use simulation::economy::cargo::{CargoBook, RoadDeadlineCounts};
use simulation::economy::progression::{utility_served_fraction, ProgressionMode};
use simulation::economy::{Budget, DailyEconomyMetrics, Market, TripPurpose, ZoneDemand};
use simulation::map::Density::{self, High, Low, Medium};
use simulation::map::ZoneKind::{self, Commercial, Industrial, MixedUse, Office, Residential};
use simulation::map::{
    BuildingID, BuildingKind, GrowableBusinessState, GrowableSiteState, LaneKind,
    LanePatternBuilder, Map, PathKind, Pathfinder, ProjectFilter, ProjectKind, RoadID,
    TerraformKind, TraverseDirection, ZoneCell, ZoneGrid,
};
use simulation::map_dynamic::{BuildingInfos, EnvGrid, Itinerary};
use simulation::souls::{
    freight_station::train_stop, goods_company::company_soul, growth::check_growable_lot, human,
    lifecycle, residence::Residences,
};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{
    CompanyID, GrowableSummary, MapPreset, SavePhase, Simulation, SimulationOptions, SlotId,
    SnapshotRevision, SoulID,
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
const PURE_CONTROL_OPTIONS: SimulationOptions = SimulationOptions {
    save_replay: false,
    ..OPTIONS
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
const STAFFED_TEARDOWN_SETUP_STEP: u64 = 672_004;
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

fn grow_city(ctx: &mut ScenarioCtx, replay_root: Option<&Path>) -> Result<BuildingID, String> {
    grow_city_with_health(ctx, replay_root, true, &FOOD_SUPPLY)
}

fn paint_commissioning_core(
    ctx: &mut ScenarioCtx,
    mixed: bool,
    replay_root: Option<&Path>,
    with_health: bool,
    extra_sites: &[(&'static str, Vec2)],
) -> Result<BuildingID, String> {
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
    Ok(freight)
}

fn grow_city_with_health(
    ctx: &mut ScenarioCtx,
    replay_root: Option<&Path>,
    with_health: bool,
    extra_sites: &[(&'static str, Vec2)],
) -> Result<BuildingID, String> {
    let freight = paint_commissioning_core(ctx, true, replay_root, with_health, extra_sites)?;
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
    } else {
        advance_to(ctx, COMMISSION_END)?;
    }
    Ok(freight)
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
        // Replace the Mixed Use blocks with pure housing at the same densities.
        [(2, 3, Residential, Medium), (3, 3, Residential, High)]
    };
    paint_zone_blocks(ctx, ORIGIN, BLOCK, infill);
    if !mixed {
        paint_zone_blocks(
            ctx,
            ORIGIN,
            BLOCK,
            [
                (3, 4, Commercial, Medium),
                (4, 2, Office, High),
                (5, 3, Industrial, Medium),
                (0, 4, Residential, Low),
                (3, 5, Commercial, Low),
            ],
        );
    }
}

fn grow_pure_control(ctx: &mut ScenarioCtx, target: DistrictCapacity) -> Result<BuildingID, String> {
    let terminal = paint_commissioning_core(ctx, false, None, true, &FOOD_SUPPLY)?;
    // Match against the live commissioned Mixed Use district before measuring trips.
    // The pure district grows normally first. Then choose residential and office
    // sites from the same district using realized capacity, with room inside the
    // 5% band for normal development during the observation window. Offices
    // adjust jobs while retaining the shopping destinations and freight geometry.
    // Two late commissioning passes leave time for residents to move in and
    // workers to take jobs, and account for growth after the first adjustment.
    for hours_left in [18, 6] {
        advance_to(ctx, COMMISSION_END - hours_left * TICKS_PER_HOUR)?;
        match_pure_sites(&mut ctx.sim, Residential, target.housing_slots)?;
        match_pure_sites(&mut ctx.sim, Office, target.available_jobs)?;
    }
    advance_to(ctx, COMMISSION_END)?;
    check_control_capacity(target, district_capacity(&ctx.sim))
        .map_err(|error| format!("at commissioning: {error}"))?;
    Ok(terminal)
}

fn match_pure_sites(sim: &mut Simulation, kind: ZoneKind, target: f64) -> Result<(), String> {
    let slots = |sim: &Simulation| {
        let capacity = district_capacity(sim);
        if kind == Residential {
            capacity.housing_slots
        } else {
            capacity.available_jobs
        }
    };
    let margin = 0.02 * target;
    let mut current = slots(sim);
    if current < target - margin {
        while current < target - margin {
            if !add_pure_site(sim, kind, target - current)? {
                break;
            }
            current = slots(sim);
        }
    } else {
        while current > target + margin {
            if !remove_pure_site(sim, kind, current - target)? {
                break;
            }
            current = slots(sim);
        }
    }
    Ok(())
}

fn add_pure_site(sim: &mut Simulation, kind: ZoneKind, missing: f64) -> Result<bool, String> {
    let focus = if kind == Residential {
        ORIGIN + vec2(2.5, 3.5) * BLOCK
    } else {
        ORIGIN + vec2(2.5, 4.5) * BLOCK
    };
    let candidate = {
        let map = sim.map();
        let zones = sim.read::<ZoneGrid>();
        map.lots()
            .values()
            .filter_map(|lot| {
                let offset = lot.shape.center() - ORIGIN;
                // Limit extra sites to the fixture streets and keep existing zoning.
                if !(0.0..=5.0 * BLOCK).contains(&offset.x)
                    || !(0.0..=5.0 * BLOCK).contains(&offset.y)
                    || zones.lot_zone(&lot.shape).is_some()
                {
                    return None;
                }
                let distance = lot.shape.center().distance2(focus);
                [High, Medium, Low]
                    .into_iter()
                    .filter_map(|density| {
                        let site = check_growable_lot(&map, lot, zone(kind, density)).ok()?;
                        let slots = site.home.map_or_else(
                            || site.company.map_or(0, |proto| proto.n_workers),
                            |proto| proto.capacity,
                        );
                        let error = (missing - f64::from(slots)).abs();
                        (error < missing).then_some((site, error, distance))
                    })
                    .min_by(|a, b| a.1.total_cmp(&b.1))
            })
            .min_by(|a, b| {
                a.1.total_cmp(&b.1)
                    .then_with(|| a.2.total_cmp(&b.2))
                    .then_with(|| a.0.lot.cmp(&b.0.lot))
            })
            .map(|(site, _, _)| site)
    };
    let Some(site) = candidate else {
        return Ok(false);
    };
    sim.apply_commands([&WorldCommand::ZonePaint {
        area: site.shape,
        kind: Some(kind),
        density: site.zone.density,
    }]);
    if !sim.read::<CommandOutcomes>().accepted.contains(&0) {
        return Err(format!("could not zone pure control parcel {:?}", site.lot));
    }
    let placed = sim
        .place_zoned_growable(site.lot, site.zone)
        .map_err(|error| format!("could not place pure control parcel {:?}: {error:?}", site.lot))?;
    if let Some(company) = site.company {
        company_soul(sim, placed.building, company.base.id)
            .ok_or_else(|| format!("could not open pure control office {:?}", placed.building))?;
    }
    Ok(true)
}

fn remove_pure_site(sim: &mut Simulation, kind: ZoneKind, surplus: f64) -> Result<bool, String> {
    let candidate = {
        let map = sim.map();
        map.buildings()
            .keys()
            .filter_map(|id| {
                let site = sim.growable_summary(id)?;
                if site.zone.kind != kind || site.site != GrowableSiteState::Active {
                    return None;
                }
                // A matched control can retire a different site of the same capacity.
                if sim.building_parking(id).is_some_and(|parking| parking.occupied != 0 || parking.held != 0) {
                    return None;
                }
                let (slots, users) = if kind == Residential {
                    (site.housing_slots, site.occupants)
                } else {
                    (site.active_job_slots, site.filled_jobs)
                };
                let error = (surplus - f64::from(slots)).abs();
                let area = map.lot_origin(id)?.shape;
                (error < surplus).then_some((id, area, error, users))
            })
            .min_by(|a, b| {
                a.2.total_cmp(&b.2)
                    .then_with(|| a.3.cmp(&b.3))
                    .then_with(|| a.0.cmp(&b.0))
            })
    };
    let Some((id, area, _, _)) = candidate else {
        return Ok(false);
    };
    // Clear the parcel's zoning so automatic growth cannot replace the removed site.
    sim.apply_commands([
        &WorldCommand::ZonePaint {
            area,
            kind: None,
            density: Low,
        },
        &WorldCommand::MapRemoveBuilding(id),
    ]);
    if sim.read::<CommandOutcomes>().accepted.as_slice() != [0, 1] {
        return Err(format!("could not retire pure control site {id:?}"));
    }
    Ok(true)
}

fn run_attainment(ctx: &mut ScenarioCtx) -> Result<(), String> {
    use simulation::map::GrowableSiteState;
    use simulation::souls::development::requirements;

    require_profiles()?;
    grow_city(ctx, None)?;
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
    grow_city_with_health(&mut control, None, false, &FOOD_SUPPLY)?;
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
    let export = export_root(ctx, "EGR_ZONES2_REPLAY_ROOT")?;
    let scratch;
    let replay_root = if let Some(path) = export.as_deref() {
        path
    } else {
        scratch = ReplayScratch::new()?;
        scratch.0.as_path()
    };
    let deadline_window = COMMISSION_END..COMMISSION_END + OBSERVATION_DAYS * TICKS_PER_DAY;
    ctx.sim.write::<CargoBook>().observe_road_deadlines(deadline_window.clone());
    let terminal = grow_city(ctx, Some(replay_root))?;
    check_commissioning(ctx, true)?;
    let mixed_freight = mean_retail_freight_distance(&ctx.sim, terminal)?;
    ctx.sample("commissioned");
    let mut control = ScenarioCtx::new(PURE_CONTROL_OPTIONS);
    control.sim.write::<CargoBook>().observe_road_deadlines(deadline_window);
    let terminal = grow_pure_control(&mut control, district_capacity(&ctx.sim))?;
    check_commissioning(&control, false)?;
    let pure_freight = mean_retail_freight_distance(&control.sim, terminal)?;
    // Average per-direction distance may differ by at most one fixture block.
    if (mixed_freight - pure_freight).abs() > BLOCK {
        return Err(format!(
            "mean terminal road distance differs by over {BLOCK} m (mixed {mixed_freight:.1} m, pure {pure_freight:.1} m)"
        ));
    }
    let mixed = observe_days(ctx)?;
    let pure = observe_days(&mut control)?;
    ctx.samples
        .extend(control.samples.into_iter().map(|mut sample| {
            sample.label = format!("pure_control_{}", sample.label);
            sample
        }));
    let match_result = compare_economies(&mixed, &pure);
    let after_paint = SlotId::new("zones2_after_core").map_err(|err| err.to_string())?;
    let commissioning =
        Simulation::load_from_slot(replay_root, &after_paint).map_err(|err| err.to_string())?;
    verify_mixed_frontage_replay(&mut ScenarioCtx::from_sim(commissioning), replay_root)?;
    match_result
}

fn mean_retail_freight_distance(sim: &Simulation, terminal: BuildingID) -> Result<f32, String> {
    use prototypes::CompanyKind;
    let map = sim.map();
    let terminal = map
        .get(terminal)
        .filter(|site| matches!(site.kind, BuildingKind::RailFreightStation(_)))
        .ok_or("the district has no rail freight terminal")?;
    let mut count = 0;
    let mut distance = 0.0;
    for company in sim.world().companies.values().filter(|company| {
        company.comp.proto.prototype().kind == CompanyKind::Store
            && try_prototype(prototypes::GrowableCompanyID::from(company.comp.proto))
                .is_some_and(|proto| proto.growable)
    }) {
        let store = map
            .get(company.comp.building)
            .ok_or("retailer has no building")?;
        for (source, destination) in [(terminal, store), (store, terminal)] {
            distance += freight_route_distance(&map, source.door_pos, destination.door_pos)
                .ok_or_else(|| {
                    format!("retailer {:?} has no round-trip terminal route", store.id)
                })?;
        }
        count += 1;
    }
    if count == 0 {
        return Err("the district has no zoned retailers to compare".into());
    }
    let mean = distance / (2 * count) as f32;
    println!("mean terminal road distance for {count} zoned retailers: {mean:.1} m per direction");
    Ok(mean)
}

fn freight_route_distance(map: &Map, start: Vec3, end: Vec3) -> Option<f32> {
    let itinerary = Itinerary::route(Tick(0), start, end, map, PathKind::Vehicle)?;
    let route = itinerary.get_route()?;
    let mut length = 0.0;
    let mut position = start;
    for &point in itinerary.local_path().iter().rev() {
        length += position.distance(point);
        position = point;
    }
    if let Some((last, intermediate)) = route.reversed_route.split_first() {
        for traversal in intermediate.iter().rev() {
            let points = traversal.raw_points(map)?;
            let (first, last) = match traversal.dir {
                TraverseDirection::Forward => (points.first(), points.last()),
                TraverseDirection::Backward => (points.last(), points.first()),
            };
            length += position.distance(first) + points.length();
            position = last;
        }
        let points = PathKind::Vehicle.local_route(map, last.destination_lane(), position, end)?;
        length += position.distance(points.first()) + points.length() + points.last().distance(end);
    }
    Some(length)
}

fn verify_mixed_frontage_replay(live: &mut ScenarioCtx, root: &Path) -> Result<(), String> {
    // Give property users time to drive out before the staffed frontage cut.
    advance_to(live, STAFFED_TEARDOWN_SETUP_STEP - 6 * TICKS_PER_HOUR)?;
    let close: Vec<_> = live.sim.map().buildings().keys()
        .filter(|&id| live.sim.building_parking(id).is_some_and(|parking| parking.open))
        .map(|building| WorldCommand::SetBuildingParkingOpen { building, open: false })
        .collect();
    if !close.is_empty() {
        run_teardown_batch(live, &close, 1)?;
    }
    advance_to(live, STAFFED_TEARDOWN_SETUP_STEP)?;
    let mut next_stage = elapsed(&live.sim);
    let staging_deadline = next_stage + 6 * TICKS_PER_HOUR;
    let mut staged = 0;
    let (roads, affected) = loop {
        let missing = match staffed_teardown_frontage(&live.sim) {
            Ok(frontage) => break frontage,
            Err(missing) => missing,
        };
        let now = elapsed(&live.sim);
        if now >= staging_deadline {
            return Err(format!("{missing} after {staged} worker housing changes"));
        }
        if staged < 4 && now >= next_stage {
            // A match can appear on the same tick as demolition or after the
            // replacement house submits its first applications.
            let changed = stage_teardown_hires(live)?;
            staged += usize::from(changed);
            next_stage = elapsed(&live.sim) + if changed { TICKS_PER_HOUR } else { 32 };
            if changed {
                continue;
            }
        }
        live.run_ticks(1);
    };
    let (endpoints, parcels) = {
        let map = live.sim.map();
        let endpoints: Vec<_> = roads
            .iter()
            .map(|&road| {
                let link = &map.roads()[road];
                [
                    map.intersections()[link.src].pos,
                    map.intersections()[link.dst].pos,
                ]
            })
            .collect();
        let parcels = affected
            .iter()
            .map(|&(id, _)| {
                let origin = map
                    .lot_origin(id)
                    .ok_or("Mixed Use fixture lacks parcel origin")?;
                let zone = live
                    .sim
                    .growable_summary(id)
                    .ok_or("Mixed Use fixture lacks roles")?
                    .zone;
                Ok((id, *origin, zone))
            })
            .collect::<Result<Vec<_>, String>>()?;
        (endpoints, parcels)
    };
    let (tenants, displaced) = {
        let map = live.sim.map();
        let infos = live.sim.read::<BuildingInfos>();
        let tenants: BTreeSet<_> = affected
            .iter()
            .filter_map(|&(id, _)| infos.get(id))
            .flat_map(|info| info.residents.iter().copied())
            .collect();
        let displaced: usize = roads
            .iter()
            .flat_map(|&road| &map.roads()[road].connected_buildings)
            .filter_map(|&id| infos.get(id))
            .map(|info| info.residents.len())
            .sum();
        (tenants, displaced)
    };
    let (loaded, before_cut) = snapshot_checkpoint(&live.sim, root, "zones2_staffed_cut")?;
    let mut resumed = ScenarioCtx::from_sim(loaded);

    let cuts: Vec<_> = roads.into_iter().map(WorldCommand::MapRemoveRoad).collect();
    let before_growth = DEMAND_INTERVAL_TICKS - (live.sim.get_tick() + 1) % DEMAND_INTERVAL_TICKS;
    for run in [&mut *live, &mut resumed] {
        run_teardown_batch(run, &cuts, 1)?;
        run.run_ticks(before_growth - 1);
    }

    // Provide vacancies for every home on the cut roads before growth settles
    // the disconnection. Ordinary homes can be processed before Mixed Use sites.
    let registry = prototypes::prototypes();
    let house = registry
        .legacy_residence(Low, 1)
        .or_else(|| {
            registry.residence_for_chain(prototypes::GrowableChainKey {
                zone_kind: prototypes::GrowableZoneKind::Pure(Residential),
                density: Low,
                level: 1,
            })
        })
        .filter(|home| home.capacity > 0)
        .ok_or("frontage fixture needs a manual house prototype with housing capacity")?;
    let homes_needed = displaced.div_ceil(house.capacity as usize);
    let replacement_homes = {
        let map = live.sim.map();
        let center = parcels[0].1.shape.center();
        let mut lots: Vec<_> = map
            .lots()
            .values()
            .filter(|lot| check_growable_lot(&map, lot, zone(Residential, Low)).is_ok())
            .collect();
        lots.sort_by(|a, b| {
            let distance = |lot: &simulation::map::Lot| lot.shape.center().distance2(center);
            distance(a)
                .total_cmp(&distance(b))
                .then_with(|| a.id.cmp(&b.id))
        });
        lots.into_iter()
            .take(homes_needed)
            .map(|lot| WorldCommand::MapBuildHouse(lot.id))
            .collect::<Vec<_>>()
    };
    if replacement_homes.len() != homes_needed {
        return Err(format!(
            "frontage fixture needs {homes_needed} rehousing lots for {displaced} tenants, found {}",
            replacement_homes.len()
        ));
    }
    for run in [&mut *live, &mut resumed] {
        run_teardown_batch(run, &replacement_homes, TICKS_PER_HOUR - before_growth)?;
        let map = run.sim.map();
        let infos = run.sim.read::<BuildingInfos>();
        let cargo = run.sim.read::<CargoBook>();
        for &(id, company) in &affected {
            let removed = SoulID::GoodsCompany(company);
            if !map.get(id).is_some_and(|site| site.is_abandoned_growable())
                || infos.owner(id).is_some()
                || run.sim.get(company).is_some()
                || cargo
                    .stock_for(id)
                    .values()
                    .any(|stock| stock.incoming > 0 || stock.reserved > 0)
                || run.sim.world().humans.values().any(|human| {
                    human.household.basket_seller() == Some(removed)
                        || human.work.as_ref().is_some_and(|work| work.workplace == id)
                        || human
                            .bought
                            .0
                            .values()
                            .flatten()
                            .any(|trade| trade.seller.0 == removed)
                })
            {
                return Err(format!(
                    "frontage loss left an active role or claim on {id:?}"
                ));
            }
        }
        let rehoused = tenants
            .iter()
            .filter_map(|&id| infos.residence_of(id))
            .any(|home| {
                !affected.iter().any(|&(id, _)| id == home)
                    && map.get(home).is_some_and(|site| {
                        site.has_residential_use() && !site.is_abandoned_growable()
                    })
            });
        if !rehoused {
            return Err("frontage loss did not rehouse a Mixed Use tenant".into());
        }
    }
    let abandoned = resumed_checkpoint(live, &resumed, root, "zones2_staffed_abandoned")?;

    let erasures: Vec<_> = parcels
        .iter()
        .map(|&(_, origin, _)| WorldCommand::ZonePaint {
            area: origin.shape,
            kind: None,
            density: Low,
        })
        .collect();
    let id = SlotId::new(abandoned.label).map_err(|err| err.to_string())?;
    let reload_abandoned = || {
        Simulation::load_from_slot(root, &id)
            .map(ScenarioCtx::from_sim)
            .map_err(|err| err.to_string())
    };
    let mut disconnected = reload_abandoned()?;
    for run in [&mut disconnected, &mut resumed] {
        run_teardown_batch(run, &erasures, 13 * TICKS_PER_HOUR)?;
        let map = run.sim.map();
        for &(id, origin, _) in &parcels {
            if map.get(id).is_some()
                || map
                    .lots()
                    .values()
                    .any(|lot| lot.shape.center().distance2(origin.shape.center()) < 0.01)
            {
                return Err(format!(
                    "collapse without frontage retained the shell or restored a parcel for {id:?}"
                ));
            }
        }
    }
    let collapsed_disconnected = resumed_checkpoint(
        &disconnected,
        &resumed,
        root,
        "zones2_disconnected_collapse",
    )?;
    verify_decoded(root, &[collapsed_disconnected])?;
    resumed = reload_abandoned()?;

    let mut repairs = erasures;
    {
        let map = live.sim.map();
        repairs.extend(endpoints.into_iter().map(|pair| {
            let [from, to] = pair.map(|pos| map.project(pos, 1.0, ProjectFilter::INTER));
            WorldCommand::MapMakeConnection {
                from,
                to,
                inter: None,
                pat: LanePatternBuilder::default().build(),
            }
        }));
    }
    for run in [&mut *live, &mut resumed] {
        run_teardown_batch(run, &repairs, 13 * TICKS_PER_HOUR)?;
        let map = run.sim.map();
        for &(id, origin, zone) in &parcels {
            if map.get(id).is_some()
                || !map.lots().values().any(|lot| {
                    lot.parent != origin.parent_road
                        && lot.shape.center().distance2(origin.shape.center()) < 0.01
                        && check_growable_lot(&map, lot, zone).is_ok()
                })
            {
                return Err(format!(
                    "timed collapse did not restore usable frontage for {id:?}"
                ));
            }
        }
    }
    let collapsed = resumed_checkpoint(live, &resumed, root, "zones2_staffed_collapsed")?;

    let repaint: Vec<_> = parcels
        .iter()
        .map(|&(_, origin, zone)| WorldCommand::ZonePaint {
            area: origin.shape,
            kind: Some(zone.kind),
            density: zone.density,
        })
        .collect();
    for run in [&mut *live, &mut resumed] {
        run_teardown_batch(run, &repaint, 1)?;
        prepare_regrowth_demand(run)?;
        run.run_ticks(TICKS_PER_DAY);
        let map = run.sim.map();
        if !parcels.iter().any(|&(_, origin, zone)| {
            map.buildings().values().any(|building| {
                building.obb.center().distance2(origin.shape.center()) < 0.01
                    && run.sim.growable_summary(building.id).is_some_and(|site| {
                        site.zone == zone && site.site == GrowableSiteState::Active
                    })
            })
        }) {
            return Err("repainting the restored frontage produced no Mixed Use regrowth".into());
        }
    }
    let regrown = resumed_checkpoint(live, &resumed, root, "zones2_staffed_regrown")?;
    verify_decoded(root, &[before_cut, abandoned, collapsed, regrown])
}

fn stage_teardown_hires(ctx: &mut ScenarioCtx) -> Result<bool, String> {
    // The district's housing is full. Record a demolition that frees storefront
    // jobs while baskets and deliveries are in flight. The next market pass can
    // then match replacement workers before the frontage cut.
    let home = {
        let infos = ctx.sim.read::<BuildingInfos>();
        let map = ctx.sim.map();
        let mut workers_by_home = BTreeMap::<_, (usize, usize, BTreeSet<CompanyID>)>::new();
        let mut removable = BTreeMap::new();
        for seller in mixed_sellers(&ctx.sim) {
            let SoulID::GoodsCompany(company) = seller else {
                continue;
            };
            let employer = &ctx.sim.world().companies[company];
            let Some(road) = map
                .get(employer.comp.building)
                .and_then(|site| site.connected_road)
            else {
                continue;
            };
            let removable_frontage = *removable.entry(road).or_insert_with(|| {
                WorldCommand::MapRemoveRoad(road)
                    .validate_and_price(&ctx.sim)
                    .is_ok()
            });
            for &worker in &employer.workers.0 {
                let home = infos
                    .residence_of(SoulID::Human(worker))
                    .filter(|&home| map.get(home).is_some_and(|site| !site.is_mixed_use()));
                if let Some(home) = home {
                    let (frontage, total, employers) = workers_by_home.entry(home).or_default();
                    if removable_frontage {
                        *frontage += 1;
                        employers.insert(company);
                    }
                    *total += 1;
                }
            }
        }
        workers_by_home
            .into_iter()
            .filter(|(home, _)| {
                map.lot_origin(*home).is_some()
                    && WorldCommand::MapRemoveBuilding(*home)
                        .validate_and_price(&ctx.sim)
                        .is_ok()
            })
            .min_by_key(|&(home, (frontage, workers, _))| {
                (std::cmp::Reverse((frontage, workers)), home)
            })
    };
    let Some((home, (frontage, workers, employers))) = home else {
        return Ok(false);
    };
    let parcel = *ctx
        .sim
        .map()
        .lot_origin(home)
        .ok_or("hire staging house has no parcel")?;
    run_teardown_batch(ctx, &[WorldCommand::MapRemoveBuilding(home)], 1)?;
    if staffed_teardown_frontage(&ctx.sim).is_ok() {
        return Ok(true);
    }

    // Build a legal house near a vacancy so its residents can apply through
    // the ordinary employment market.
    let lot = {
        let map = ctx.sim.map();
        let near = employers
            .iter()
            .filter_map(|&employer| {
                let company = ctx.sim.world().companies.get(employer)?;
                let vacancies =
                    (company.comp.max_workers as usize).saturating_sub(company.workers.0.len());
                (vacancies != 0).then_some((std::cmp::Reverse(vacancies), company.comp.building))
            })
            .min()
            .and_then(|(_, id)| map.get(id))
            .map_or(parcel.shape.center(), |site| site.door_pos.xy());
        let mut lots: Vec<_> = map.lots().values().collect();
        lots.sort_unstable_by(|a, b| {
            let distance = |lot: &simulation::map::Lot| lot.shape.center().distance2(near);
            distance(a).total_cmp(&distance(b)).then(a.id.cmp(&b.id))
        });
        lots.into_iter()
            .find(|lot| {
                WorldCommand::MapBuildHouse(lot.id)
                    .validate_and_price(&ctx.sim)
                    .is_ok()
            })
            .ok_or("no legal replacement lot near the demolished worker home")?
            .id
    };
    println!(
        "staged replacement home from {workers} Mixed Use workers ({frontage} on removable frontage)"
    );
    run_teardown_batch(ctx, &[WorldCommand::MapBuildHouse(lot)], 1)?;
    Ok(true)
}

fn prepare_regrowth_demand(ctx: &mut ScenarioCtx) -> Result<(), String> {
    // Open a new street with jobs near the district and homes beyond its existing
    // retail catchment. The restored storefronts can grow when both roles are needed.
    const OFFICE_LOTS: usize = 12;
    const HOMES: usize = 2;
    let start = ORIGIN + vec2(5.0 * BLOCK, 0.0);
    let length = 1_500.0;
    let grid = grid_city(ctx, start, 2, 1, length);
    let [road] = grid.roads.as_slice() else {
        return Err("could not connect the frontage expansion street".into());
    };
    let mut commands = vec![WorldCommand::SetTaxRate {
        zone: Residential,
        rate_permille: 0,
    }];
    {
        let map = ctx.sim.map();
        let lots = map.lots().values().filter(|lot| lot.parent == *road);
        let office = zone(Office, High);
        let offices = lots
            .clone()
            .filter(|lot| lot.shape.center().x < start.x + 600.0)
            .filter(|lot| check_growable_lot(&map, lot, office).is_ok())
            .take(OFFICE_LOTS)
            .map(|lot| WorldCommand::ZonePaint {
                area: lot.shape,
                kind: Some(office.kind),
                density: office.density,
            });
        commands.extend(offices);
        let residential = zone(Residential, Low);
        let houses = lots
            .filter(|lot| lot.shape.center().x > start.x + length - 100.0)
            .filter(|lot| check_growable_lot(&map, lot, residential).is_ok())
            .take(HOMES)
            .map(|lot| WorldCommand::MapBuildHouse(lot.id));
        commands.extend(houses);
    }
    if commands.len() != 1 + OFFICE_LOTS + HOMES {
        return Err("frontage expansion needs room for twelve offices and two homes".into());
    }
    run_teardown_batch(ctx, &commands, 1)
}

fn run_teardown_batch(
    ctx: &mut ScenarioCtx,
    commands: &[WorldCommand],
    steps: u64,
) -> Result<(), String> {
    ctx.submit(commands);
    ctx.run_ticks(1);
    {
        let result = ctx.sim.read::<CommandOutcomes>();
        if result.accepted.len() != commands.len() || !result.last.is_empty() {
            return Err(format!("teardown command batch failed: {:?}", result.last));
        }
    }
    ctx.run_ticks(steps - 1);
    Ok(())
}

type StaffedFrontage = (BTreeSet<RoadID>, Vec<(BuildingID, CompanyID)>);

fn staffed_teardown_frontage(sim: &Simulation) -> Result<StaffedFrontage, String> {
    let step = elapsed(sim);
    let failure = |claim| format!("staffed teardown at step {step} is missing {claim}");
    if (sim.get_tick() + 1) % DEMAND_INTERVAL_TICKS == 0 {
        return Err(failure("a tick between growth passes"));
    }
    let job = ItemID::new("job-opening");
    let hires: BTreeSet<_> = sim
        .world()
        .humans
        .values()
        .filter_map(|human| {
            human
                .bought
                .0
                .get(&job)?
                .first()
                .map(|trade| trade.seller.0)
        })
        .collect();
    if hires.is_empty() {
        return Err(failure("a pending Mixed Use hire"));
    }
    let map = sim.map();
    let sites: Vec<_> = mixed_sellers(sim)
        .filter_map(|seller| {
            let SoulID::GoodsCompany(company) = seller else {
                return None;
            };
            let id = sim.world().companies[company].comp.building;
            Some((id, company, map.get(id)?.connected_road?))
        })
        .collect();
    let legal_roads: BTreeSet<_> = sites.iter().map(|&(_, _, road)| road)
        .collect::<BTreeSet<_>>().into_iter()
        .filter(|&road| WorldCommand::MapRemoveRoad(road).validate_and_price(sim).is_ok())
        .collect();
    let job_road = sites
        .iter()
        .find_map(|&(_, company, road)| {
            (legal_roads.contains(&road) && hires.contains(&SoulID::GoodsCompany(company)))
                .then_some(road)
        })
        .ok_or_else(|| failure("a pending Mixed Use hire"))?;
    let cargo = sim.read::<CargoBook>();
    let freight_road = sites
        .iter()
        .filter(|&&(id, _, road)| legal_roads.contains(&road)
            && cargo.stock_for(id).values().any(|stock| stock.incoming > 0))
        .min_by_key(|&&(_, _, road)| (road != job_road, road))
        .map(|&(_, _, road)| road)
        .ok_or_else(|| failure("an incoming Mixed Use freight claim"))?;
    let baskets: BTreeSet<_> = sim
        .world()
        .humans
        .values()
        .filter_map(|human| human.household.basket_seller())
        .collect();
    let basket_road = sites
        .iter()
        .filter(|&&(_, company, road)| legal_roads.contains(&road)
            && baskets.contains(&SoulID::GoodsCompany(company)))
        .min_by_key(|&&(_, _, road)| (road != job_road && road != freight_road, road))
        .map(|&(_, _, road)| road)
        .ok_or_else(|| failure("a pending Mixed Use basket"))?;
    let roads = BTreeSet::from([job_road, freight_road, basket_road]);
    let affected: Vec<_> = sites
        .into_iter()
        .filter(|&(_, _, road)| roads.contains(&road))
        .map(|(id, company, _)| (id, company))
        .collect();
    if !affected.iter().any(|&(id, _)| {
        sim.growable_summary(id)
            .is_some_and(|site| site.filled_jobs > 1 && site.occupants > 0)
    }) {
        return Err(failure("an occupied and staffed Mixed Use site"));
    }
    println!(
        "staffed frontage cut across {} links at elapsed step {step} with hire, basket and freight claims",
        roads.len()
    );
    Ok((roads, affected))
}

struct ReplayCheckpoint {
    label: &'static str,
    steps: u64,
    hashes: BTreeMap<String, u64>,
}

fn resumed_checkpoint(
    live: &ScenarioCtx,
    resumed: &ScenarioCtx,
    root: &Path,
    label: &'static str,
) -> Result<ReplayCheckpoint, String> {
    let (_, checkpoint) = snapshot_checkpoint(&live.sim, root, label)?;
    if resumed.sim.hashes() != checkpoint.hashes {
        return Err(format!(
            "{label}: resumed world diverged from uninterrupted play"
        ));
    }
    Ok(checkpoint)
}

fn snapshot_checkpoint(
    sim: &Simulation,
    root: &Path,
    label: &'static str,
) -> Result<(Simulation, ReplayCheckpoint), String> {
    publish(sim, root, label, label)?;
    let id = SlotId::new(label).map_err(|err| err.to_string())?;
    let loaded = Simulation::load_from_slot(root, &id).map_err(|err| err.to_string())?;
    let hashes = sim.hashes();
    if loaded.hashes() != hashes {
        return Err(format!(
            "{label}: snapshot diverged from uninterrupted play"
        ));
    }
    Ok((
        loaded,
        ReplayCheckpoint {
            label,
            steps: elapsed(sim),
            hashes,
        },
    ))
}

fn verify_checkpoint(
    sim: &Simulation,
    root: &Path,
    label: &'static str,
) -> Result<Simulation, String> {
    let (loaded, checkpoint) = snapshot_checkpoint(sim, root, label)?;
    verify_decoded(root, &[checkpoint])?;
    Ok(loaded)
}

fn verify_decoded(root: &Path, checkpoints: &[ReplayCheckpoint]) -> Result<(), String> {
    let last = checkpoints.last().expect("at least one replay checkpoint");
    let id = SlotId::new(last.label).map_err(|err| err.to_string())?;
    let replay = Simulation::load_replay_from_slot(root, &id).map_err(|err| err.to_string())?;
    let (mut playback, mut loader) =
        Simulation::try_from_replay(replay).map_err(|err| err.to_string())?;
    let (mut current, steps) = loader
        .step_progress()
        .ok_or("city replay has no stepped timeline")?;
    if steps != last.steps {
        return Err(format!(
            "{}: expected {} replay steps, found {steps}",
            last.label, last.steps
        ));
    }
    let mut schedule = Simulation::schedule();
    for point in checkpoints {
        while current < point.steps {
            let finished = loader.advance_tick(&mut playback, &mut schedule);
            let (next, _) = loader.step_progress().unwrap();
            if next <= current || next > point.steps || (finished && next < point.steps) {
                return Err(format!(
                    "{}: decoded sidecar missed step {}",
                    point.label, point.steps
                ));
            }
            current = next;
        }
        if playback.hashes() != point.hashes {
            return Err(format!(
                "{}: decoded sidecar diverged from uninterrupted play",
                point.label
            ));
        }
        println!(
            "{}: snapshot and sidecar matched after {current} steps",
            point.label
        );
    }
    Ok(())
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
    road_loads: u64,
    late_road_loads: u64,
    cancelled_road_loads: u64,
    road_deadlines: RoadDeadlineCounts,
    mixed_receipt_units: u64,
    mixed_shelf_peak: i32,
    productive_recipients: usize,
    capacity: DistrictCapacity,
}

#[derive(Clone, Copy, Debug, Default)]
struct DistrictCapacity {
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
            let capacity = district_capacity(&ctx.sim);
            result.resident_days += ctx.sim.world().humans.len() as f64 / 24.0;
            result.capacity.housing_slots += capacity.housing_slots;
            result.capacity.available_jobs += capacity.available_jobs;
            result.capacity.filled_jobs += capacity.filled_jobs;
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
            result.road_loads += freight.delivered_loads;
            result.late_road_loads += freight.late_loads;
            result.cancelled_road_loads += freight.cancelled_loads;
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
    result.road_deadlines = ctx.sim.read::<CargoBook>().road_deadline_counts();
    result.productive_recipients = productive_recipients.len();
    let hours = (OBSERVATION_DAYS * 24) as f64;
    result.capacity.housing_slots /= hours;
    result.capacity.available_jobs /= hours;
    result.capacity.filled_jobs /= hours;
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
        ("cancelled_loads".into(), result.cancelled_road_loads as f64),
        ("road_delivered_loads".into(), result.road_loads as f64),
        ("road_late_loads".into(), result.late_road_loads as f64),
        ("road_due_loads".into(), result.road_deadlines.due as f64),
        ("road_on_time_loads".into(), result.road_deadlines.on_time as f64),
        ("housing_slots_hourly_mean".into(), result.capacity.housing_slots),
        ("available_jobs_hourly_mean".into(), result.capacity.available_jobs),
        ("filled_jobs_hourly_mean".into(), result.capacity.filled_jobs),
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

fn district_capacity(sim: &Simulation) -> DistrictCapacity {
    let mut capacity = DistrictCapacity::default();
    let homes = sim.read::<Residences>();
    for (&id, home) in &homes.by_building {
        if !homes.is_abandoned(id) {
            capacity.housing_slots += f64::from(home.capacity);
        }
    }
    for company in sim.world().companies.values().filter(|company| {
        try_prototype(prototypes::GrowableCompanyID::from(company.comp.proto))
            .is_some_and(|proto| proto.growable)
    }) {
        capacity.available_jobs += f64::from(company.comp.max_workers);
        capacity.filled_jobs += company.workers.0.len() as f64;
    }
    capacity
}

fn mixed_sellers(sim: &Simulation) -> impl Iterator<Item = SoulID> + '_ {
    sim.world().companies.iter().filter_map(|(id, company)| {
        try_prototype(prototypes::GrowableCompanyID::from(company.comp.proto))
            .filter(|proto| proto.chain_key().zone_kind == prototypes::GrowableZoneKind::MixedUse)
            .map(|_| SoulID::GoodsCompany(id))
    })
}

fn check_control_capacity(mixed: DistrictCapacity, pure: DistrictCapacity) -> Result<(), String> {
    let matched = |a: f64, b: f64| a > 0.0 && b > 0.0 && (a - b).abs() <= 0.05 * b;
    let capacities = format!(
        "housing {}/{}, available jobs {}/{}, filled jobs {}/{} (MX/pure)",
        mixed.housing_slots, pure.housing_slots, mixed.available_jobs, pure.available_jobs,
        mixed.filled_jobs, pure.filled_jobs
    );
    if !matched(mixed.housing_slots, pure.housing_slots)
        || !matched(mixed.available_jobs, pure.available_jobs)
        || !matched(mixed.filled_jobs, pure.filled_jobs)
    {
        return Err(format!("pure control outside 5%: {capacities}"));
    }
    println!("pure control within 5%: {capacities}");
    Ok(())
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
    let RoadDeadlineCounts { due, on_time } = mixed.road_deadlines;
    println!("MX road freight: {on_time}/{due} due loads on time");
    if on_time as f64 / (due.max(1) as f64) < 0.9 {
        return Err(format!(
            "MX road freight needs 90% on-time deliveries, got {on_time}/{due}"
        ));
    }
    check_control_capacity(mixed.capacity, pure.capacity)?;
    if mixed.local_pickups < 4
        || mixed.mixed_receipt_units == 0
        || mixed.productive_recipients == 0
        || pure.external_shops == 0
        // P6 owner decision (2026-11-26): lower the reduction gate from 10% to 8%.
        // Observed 9.2% (0.808 vs 0.890 trips/resident-day) with all MX baskets filled.
        || trips(mixed) > 0.92 * trips(pure)
        || filled(mixed) < 0.8
        || filled(mixed) + 0.05 < filled(pure)
        || stockout(mixed) > 1.5
        || stockout(mixed) > stockout(pure) + 0.25
    {
        return Err(evidence);
    }
    Ok(())
}

#[test]
#[ignore = "simulates multiple city days; run in release mode"]
fn zones2_pure_control_matches_capacity_and_freight_distance() {
    let mut mixed = ScenarioCtx::new(OPTIONS);
    let terminal = grow_city(&mut mixed, None).unwrap();
    let mixed_freight = mean_retail_freight_distance(&mixed.sim, terminal).unwrap();
    let mut pure = ScenarioCtx::new(PURE_CONTROL_OPTIONS);
    let terminal = grow_pure_control(&mut pure, district_capacity(&mixed.sim)).unwrap();
    let pure_freight = mean_retail_freight_distance(&pure.sim, terminal).unwrap();
    assert!(!summaries(&pure.sim).any(|site| site.zone.kind == MixedUse));
    let mixed = observe_days(&mut mixed).unwrap();
    let pure = observe_days(&mut pure).unwrap();

    check_control_capacity(mixed.capacity, pure.capacity).unwrap();
    assert!(
        (mixed_freight - pure_freight).abs() <= BLOCK,
        "mean terminal road distance differs by over {BLOCK} m (mixed {mixed_freight:.1} m, pure {pure_freight:.1} m)"
    );
}

#[test]
#[ignore = "simulates multiple city days; run in release mode"]
fn zones2_staffed_frontage_teardown_replays() {
    let scratch = ReplayScratch::new().unwrap();
    let mut city = ScenarioCtx::new(OPTIONS);
    paint_commissioning_core(&mut city, true, None, true, &FOOD_SUPPLY).unwrap();
    let (commissioning, _) = snapshot_checkpoint(&city.sim, &scratch.0, "zones2_after_core").unwrap();
    verify_mixed_frontage_replay(&mut ScenarioCtx::from_sim(commissioning), &scratch.0).unwrap();
}

#[test]
fn zones2_shopping_gate_accepts_eight_percent_reduction() {
    let pure = Observation {
        resident_days: 100.0,
        external_shops: 100,
        baskets_due: 10,
        baskets_filled: 10,
        capacity: DistrictCapacity {
            housing_slots: 100.0,
            available_jobs: 100.0,
            filled_jobs: 100.0,
        },
        ..Observation::default()
    };
    let mut mixed = Observation {
        external_shops: 92,
        local_pickups: 4,
        road_deadlines: RoadDeadlineCounts { due: 10, on_time: 10 },
        mixed_receipt_units: 10,
        productive_recipients: 1,
        ..pure
    };
    compare_economies(&mixed, &pure).unwrap();
    mixed.external_shops = 93;
    assert!(compare_economies(&mixed, &pure).is_err());
}

#[test]
fn zones2_freight_gate_counts_cancelled_loads() {
    let pure = Observation {
        resident_days: 10.0,
        external_shops: 10,
        baskets_due: 10,
        baskets_filled: 10,
        capacity: DistrictCapacity {
            housing_slots: 10.0,
            available_jobs: 10.0,
            filled_jobs: 10.0,
        },
        ..Observation::default()
    };
    let mut mixed = Observation {
        external_shops: 6,
        local_pickups: 4,
        road_deadlines: RoadDeadlineCounts { due: 10, on_time: 9 },
        mixed_receipt_units: 10,
        productive_recipients: 1,
        ..pure
    };
    compare_economies(&mixed, &pure).unwrap();
    mixed.road_deadlines.due = 11;
    let error = compare_economies(&mixed, &pure).unwrap_err();
    assert!(error.contains("road freight needs 90%"));
}

#[test]
fn zones2_freight_gate_counts_an_overdue_final_day_load() {
    let pure = Observation {
        resident_days: 10.0,
        external_shops: 10,
        baskets_due: 10,
        baskets_filled: 10,
        capacity: DistrictCapacity {
            housing_slots: 10.0,
            available_jobs: 10.0,
            filled_jobs: 10.0,
        },
        ..Observation::default()
    };
    let mixed = Observation {
        external_shops: 6,
        local_pickups: 4,
        road_loads: 10,
        late_road_loads: 1,
        road_deadlines: RoadDeadlineCounts { due: 11, on_time: 9 },
        mixed_receipt_units: 10,
        productive_recipients: 1,
        ..pure
    };
    // Nine on-time loads, one settled miss and one load still overdue at closing.
    let error = compare_economies(&mixed, &pure).unwrap_err();
    assert!(error.contains("got 9/11"), "{error}");
}

fn run_controlled(ctx: &mut ScenarioCtx) -> Result<(), String> {
    require_profiles()?;
    let shots_root = export_root(ctx, "EGR_ZONES2_SHOTS_ROOT")?;
    let perf_root = export_root(ctx, "EGR_ZONES2_PERF_ROOT")?;
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
    if !mixed_sellers(&ctx.sim).any(|seller| seller == business) {
        return Err("the Mixed Use storefront is missing from seller accounting".into());
    }
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
    if ctx.sim.building_parking(storefront).is_some() {
        run_teardown_batch(
            ctx,
            &[WorldCommand::SetBuildingParkingOpen {
                building: storefront,
                open: false,
            }],
            1,
        )?;
        let until = elapsed(&ctx.sim) + 2 * TICKS_PER_HOUR;
        while ctx.sim.building_parking(storefront).is_some_and(|parking| {
            parking.occupied != 0 || parking.held != 0
        }) {
            if elapsed(&ctx.sim) >= until {
                return Err("controlled storefront parking did not drain before demolition".into());
            }
            ctx.run_ticks(32);
        }
    }
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

fn export_root(ctx: &ScenarioCtx, var: &str) -> Result<Option<PathBuf>, String> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .map(|root| {
            if ctx.determinism_pass != Some(2) {
                reserve_export_root(&root)?;
            }
            let Some(pass) = ctx.determinism_pass else {
                return Ok(root);
            };
            let path = root.join(format!("run-{pass}"));
            reserve_export_root(&path)?;
            Ok(path)
        })
        .transpose()
}

struct ReplayScratch(PathBuf);

impl ReplayScratch {
    fn new() -> Result<Self, String> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        loop {
            let serial = NEXT.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("zones2-replay-{}-{serial}", std::process::id()));
            match std::fs::create_dir(&root) {
                Ok(()) => return Ok(Self(root)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(format!("could not create {}: {err}", root.display())),
            }
        }
    }
}

impl Drop for ReplayScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
