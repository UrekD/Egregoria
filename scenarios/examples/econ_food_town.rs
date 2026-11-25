use geom::{vec2, Vec2};
use prototypes::{ItemID, Money, TICKS_PER_HOUR};
use scenarios::builders::{grid_city, paint_zone_blocks, place_company};
use scenarios::ScenarioCtx;
use simulation::economy::cargo::CargoBook;
use simulation::economy::{
    Budget, DailyEconomyMetrics, DEFAULT_TAX_RATE_PERMILLE, TICKS_PER_BUDGET_DAY,
};
use simulation::map::{BuildingKind, Density, ZoneKind};
use simulation::souls::goods_company::enable_growable_freight;
use simulation::world_command::WorldCommand;
use simulation::{MapPreset, NewGameOptions, Simulation, SimulationOptions, SlotId, SoulID};
use std::{collections::BTreeMap, path::Path};

const SPACING: f32 = 100.0;
use Density::{High, Low, Medium};
use ZoneKind::{Commercial, Industrial, Office, Residential};

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [] => fresh_town().and_then(|mut ctx| score(&mut ctx, 2_500, 3_000)),
        [root] => {
            expand_showcase(Path::new(root)).and_then(|mut ctx| score(&mut ctx, 1_000, 1_400))
        }
        _ => Err("usage: econ_food_town [showcase-save-root]".into()),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

fn place(ctx: &mut ScenarioCtx, origin: Vec2, sites: &[(&str, Vec2)]) -> Result<(), String> {
    for &(name, offset) in sites {
        place_company(ctx, name, origin + offset)
            .ok_or_else(|| format!("could not place {name} at {offset:?}"))?;
    }
    Ok(())
}

fn disable_rail_imports(ctx: &mut ScenarioCtx) {
    let stations: Vec<_> = ctx
        .sim
        .map()
        .buildings()
        .values()
        .filter_map(|building| {
            matches!(building.kind, BuildingKind::RailFreightStation(_))
                .then_some(WorldCommand::MapRemoveBuilding(building.id))
        })
        .collect();
    ctx.submit(&stations);
    ctx.run_ticks(1);
    assert!(ctx.sim.world().freight_stations.is_empty());
    enable_growable_freight(&mut ctx.sim);
}

fn fresh_town() -> Result<ScenarioCtx, String> {
    let mut ctx = ScenarioCtx::new(SimulationOptions {
        terrain_size: NewGameOptions::MIN_TERRAIN_CHUNKS,
        preset: MapPreset::Flat,
        starting_money_bucks: 1_000_000,
        save_replay: false,
        ..SimulationOptions::LEGACY
    });
    disable_rail_imports(&mut ctx);
    let origin = vec2(704.0, 704.0);
    grid_city(&mut ctx, origin, 15, 17, SPACING);
    grid_city(&mut ctx, origin + vec2(-260.0, 200.0), 2, 1, 260.0);
    place(
        &mut ctx,
        origin,
        &[
            ("coal-power-plant", vec2(-130.0, 100.0)),
            ("coal-mine", vec2(275.0, 330.0)),
            ("water-pump", vec2(-30.0, 50.0)),
            ("water-pump", vec2(-30.0, 650.0)),
            ("water-pump", vec2(1_430.0, 750.0)),
            ("water-pump", vec2(-30.0, 1_450.0)),
            ("sewage-outlet", vec2(-30.0, 350.0)),
            ("sewage-outlet", vec2(-30.0, 1_050.0)),
            ("sewage-outlet", vec2(1_430.0, 650.0)),
            ("sewage-outlet", vec2(1_430.0, 1_450.0)),
            ("school", vec2(325.0, 250.0)),
            ("school", vec2(925.0, 750.0)),
            ("school", vec2(625.0, 1_350.0)),
            ("clinic", vec2(625.0, 250.0)),
            ("clinic", vec2(625.0, 950.0)),
            ("fire-station", vec2(425.0, 650.0)),
            ("fire-station", vec2(1_125.0, 650.0)),
            ("police-station", vec2(725.0, 350.0)),
            ("police-station", vec2(825.0, 850.0)),
            ("garbage-depot", vec2(560.0, 620.0)),
            ("park", vec2(325.0, 750.0)),
            ("park", vec2(925.0, 250.0)),
            ("park", vec2(825.0, 1_450.0)),
        ],
    )?;

    // Give offices time to create jobs ahead of the households moving in.
    let set_taxes = |ctx: &mut ScenarioCtx, rate_permille| {
        ctx.submit(&[Office, Residential].map(|zone| WorldCommand::SetTaxRate {
            zone,
            rate_permille,
        }));
        ctx.run_ticks(1);
    };
    set_taxes(&mut ctx, 0);

    for rows in [0..4, 4..8, 8..12, 12..16] {
        let zoning: Vec<_> = rows
            .flat_map(|y| {
                (0..14).map(move |x| {
                    let (kind, density) = match (x, y % 4) {
                        (2 | 7 | 11, 1) => (Commercial, High),
                        (1 | 6 | 12, 2) => (Industrial, High),
                        (0 | 4 | 8 | 13, _) => (Office, High),
                        _ => (Residential, High),
                    };
                    (x, y, kind, density)
                })
            })
            .collect();
        for housing in [true, false] {
            paint_zone_blocks(
                &mut ctx,
                origin,
                SPACING,
                zoning
                    .iter()
                    .copied()
                    .filter(|&(_, _, zone, _)| (zone == Residential) == housing),
            );
            if housing {
                ctx.run_ticks(3 * TICKS_PER_HOUR);
            }
        }
        ctx.run_ticks(9 * TICKS_PER_HOUR);
        println!(
            "Zoning phase ended with {} residents",
            ctx.sim.world().humans.len()
        );
    }
    for _ in 0..12 {
        if ctx.sim.world().humans.len() >= 2_600 {
            break;
        }
        ctx.run_ticks(6 * TICKS_PER_HOUR);
        println!(
            "Town commissioning: {} residents",
            ctx.sim.world().humans.len()
        );
    }
    set_taxes(&mut ctx, DEFAULT_TAX_RATE_PERMILLE);
    Ok(ctx)
}

fn expand_showcase(root: &Path) -> Result<ScenarioCtx, String> {
    let mut ctx = ScenarioCtx::new(SimulationOptions::LEGACY);
    let slot = SlotId::new("showcase_city").map_err(|error| error.to_string())?;
    ctx.sim = Simulation::load_from_slot(root, &slot).map_err(|error| error.to_string())?;
    disable_rail_imports(&mut ctx);
    let origin = vec2(3070.0, 6320.0);
    grid_city(&mut ctx, origin, 7, 4, SPACING);
    place(
        &mut ctx,
        origin,
        &[
            ("water-pump", vec2(-30.0, 150.0)),
            ("sewage-outlet", vec2(-30.0, 250.0)),
            ("park", vec2(225.0, 145.0)),
        ],
    )?;
    let zoning: Vec<_> = (0..3)
        .flat_map(|y| {
            (0..6).map(move |x| {
                let (kind, density) = match (x, y) {
                    (2, 0) | (4, 1) | (2, 2) => (Commercial, Low),
                    (5, 1 | 2) | (4, 2) => (Industrial, Medium),
                    (5, 0) => (Office, Low),
                    _ => (Residential, High),
                };
                (x, y, kind, density)
            })
        })
        .collect();
    paint_zone_blocks(
        &mut ctx,
        origin,
        SPACING,
        zoning
            .iter()
            .copied()
            .filter(|&(_, _, zone, _)| zone == Residential),
    );
    ctx.run_ticks(4 * TICKS_PER_HOUR);
    paint_zone_blocks(
        &mut ctx,
        origin,
        SPACING,
        zoning
            .into_iter()
            .filter(|&(_, _, zone, _)| zone != Residential),
    );
    Ok(ctx)
}

fn score(ctx: &mut ScenarioCtx, min: usize, max: usize) -> Result<(), String> {
    let elapsed = simulation::diagnostics::elapsed_tick(&ctx.sim).0;
    ctx.run_ticks(elapsed.next_multiple_of(TICKS_PER_BUDGET_DAY) - elapsed);
    let first_day = simulation::diagnostics::elapsed_tick(&ctx.sim).0 / TICKS_PER_BUDGET_DAY;

    let mut consecutive = 0;
    for day in first_day..first_day + 6 {
        let mut resident_hours = 0;
        let mut min_population = usize::MAX;
        let mut max_population = 0;
        for _ in 0..24 {
            let population = ctx.sim.world().humans.len();
            resident_hours += population;
            min_population = min_population.min(population);
            max_population = max_population.max(population);
            ctx.run_ticks(TICKS_PER_HOUR);
        }
        let end = simulation::diagnostics::elapsed_tick(&ctx.sim).0;
        let metrics = ctx.sim.read::<DailyEconomyMetrics>();
        let data = metrics
            .closed_day(day, end)
            .ok_or_else(|| format!("no closed metrics for day {day}"))?;
        let (filled, due) = (
            data.baskets_filled.unwrap_or(0),
            data.baskets_due.unwrap_or(0),
        );
        let empty_ticks = data
            .stockout_ticks_by_sku
            .get(&ItemID::new("effective-food"))
            .copied()
            .unwrap_or(0);
        let stockout_hours =
            24.0 * empty_ticks as f64 / (resident_hours.max(1) as f64 * TICKS_PER_HOUR as f64);
        let operating_balance: i64 = ctx
            .sim
            .read::<Budget>()
            .days()
            .get(&day)
            .into_iter()
            .flat_map(|entries| entries.iter())
            .filter(|(category, _)| category.is_operating())
            .map(|(_, amount)| i64::try_from(*amount).unwrap())
            .sum();
        let operating_balance = Money::new_inner(operating_balance);
        let mut shops = BTreeMap::new();
        let mut suppliers = BTreeMap::new();
        let cargo = ctx.sim.read::<CargoBook>();
        let mut road_goods = 0;
        let goods = ItemID::new("goods");
        for (id, company) in ctx.sim.world().companies.iter() {
            let name = company.comp.proto.prototype().name.as_str();
            if name.starts_with("commercial-") {
                *shops.entry(name).or_insert(0) += 1;
                if let Some(freight) = cargo.freight_day(day) {
                    road_goods += freight.receipt(SoulID::GoodsCompany(id), goods).road_units;
                }
            } else if name.starts_with("industrial-") {
                *suppliers.entry(name).or_insert(0) += 1;
            }
        }
        let population = ctx.sim.world().humans.len();
        min_population = min_population.min(population);
        max_population = max_population.max(population);
        let facilities = shops.values().sum::<usize>();
        println!(
            "Day {day}: {population} residents ({min_population}–{max_population}), \
             {filled}/{due} baskets ({:.2}%), {stockout_hours:.2} food stockout hours/resident-day, \
             operating balance {operating_balance}, shops {shops:?}, factories {suppliers:?}, \
             {road_goods} wholesale units by road",
            100.0 * filled as f64 / due.max(1) as f64
        );
        if min_population >= min
            && max_population <= max
            && due > 0
            && filled * 100 >= 95 * due
            && facilities <= 45
            && road_goods > 0
            && operating_balance >= Money::ZERO
        {
            consecutive += 1;
            if consecutive == 2 {
                return Ok(());
            }
        } else {
            consecutive = 0;
        }
    }
    Err(format!("expected two consecutive days at {min}–{max} residents, at least 95% baskets, at most 45 shops and nonnegative operations"))
}
