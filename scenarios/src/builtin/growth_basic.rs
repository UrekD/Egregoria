use crate::builders::{grid_city, paint_zone_blocks, place_company, place_freight_station};
use crate::{Scenario, ScenarioCtx};
use geom::{vec2, Vec2, OBB};
use prototypes::{GrowableCompanyID, GrowableCompanyPrototype, ItemID, ResidencePrototype, Size2D};
use simulation::economy::EcoStats;
use simulation::map::{Density, ProjectFilter, ProjectKind, ZoneGrid, ZoneKind};
use simulation::souls::residence::Residences;
use simulation::world_command::WorldCommand;
use simulation::SimulationOptions;
use std::time::Instant;

pub const SCENARIO: (Scenario, SimulationOptions) = (
    Scenario {
        name: "growth_basic",
        run,
    },
    SimulationOptions {
        terrain_size: 10,
        save_replay: false,
    },
);

const TICKS: u64 = 600_000;
const ORIGIN: Vec2 = vec2(704.0, 704.0);
const SPACING: f32 = 100.0;

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    run_for(ctx, false)
}

pub fn verify_twice(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let start = Instant::now();
    let mut resume = false;
    ctx.verify_twice(|run| {
        run_for(run, resume)?;
        resume = true;
        Ok(())
    })?;
    println!(
        "growth_basic: reloaded the second run after {} ticks; both runs took {:.2}s",
        TICKS / 2,
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn run_for(ctx: &mut ScenarioCtx, resume: bool) -> Result<(), String> {
    let start = ctx.sim.get_tick();
    let grid = grid_city(ctx, ORIGIN, 6, 6, SPACING);
    if grid.roads.len() != 60 {
        return Err(format!("built {} of 60 grid roads", grid.roads.len()));
    }
    let station = place_freight_station(ctx, ORIGIN + vec2(350.0, 250.0), Size2D::new(40.0, 50.0))
        .ok_or("could not place a road-connected freight station")?;
    {
        let map = ctx.sim.map();
        let station = &map.buildings()[station];
        if !station
            .connected_road
            .is_some_and(|road| grid.roads.contains(&road))
            || !station.obb.corners.iter().all(|&corner| {
                let offset = corner - ORIGIN;
                (0.0..5.0 * SPACING).contains(&offset.x) && (0.0..5.0 * SPACING).contains(&offset.y)
            })
        {
            return Err("freight station must be inside and connected to the grid".into());
        }
    }

    // A utility spur supplies the connected grid from an existing power plant.
    let spur = grid_city(ctx, ORIGIN + vec2(-260.0, 200.0), 2, 1, 260.0);
    let plant = place_company(ctx, "coal-power-plant", ORIGIN + vec2(-130.0, 100.0));
    if spur.roads.len() != 1
        || !plant.is_some_and(|id| {
            ctx.sim.map().buildings()[id].connected_road == spur.roads.first().copied()
        })
    {
        return Err("could not connect a power plant to the grid".into());
    }

    // Fifteen residential blocks, six industrial blocks, and four commercial
    // blocks. Repaint one quarter of a commercial block for a 60/25/15 area split.
    let blocks = (0..5).flat_map(|y| {
        (0..5).map(move |x| {
            let (kind, density) = match x {
                0..=2 => (ZoneKind::Residential, Density::High),
                3 if y < 4 => (ZoneKind::Commercial, Density::Low),
                _ => (ZoneKind::Industrial, Density::Low),
            };
            (x, y, kind, density)
        })
    });
    paint_zone_blocks(ctx, ORIGIN, SPACING, blocks);
    ctx.submit(&[WorldCommand::ZonePaint {
        area: OBB::new(ORIGIN + vec2(387.5, 350.0), Vec2::X, 25.0, SPACING),
        kind: Some(ZoneKind::Industrial),
        density: Density::Low,
    }]);
    ctx.run_ticks(1);
    preflight(ctx)?;

    let setup_ticks = ctx.sim.get_tick() - start;
    ctx.run_ticks(TICKS / 2 - setup_ticks);
    if resume {
        ctx.reload()?;
    }
    ctx.run_ticks(TICKS / 2);
    check_growth(ctx)
}

fn preflight(ctx: &ScenarioCtx) -> Result<(), String> {
    let map = ctx.sim.map();
    let zones = ctx.sim.read::<ZoneGrid>();
    let mut eligible = 0;
    let mut jobs = 0;
    for (id, lot) in map.lots().iter() {
        let Some(cell) = zones.lot_zone(&lot.shape) else {
            continue;
        };
        if !map.roads().contains_key(lot.parent)
            || map
                .spatial_map()
                .query(lot.shape, ProjectFilter::ALL)
                .any(|object| object != ProjectKind::Lot(id))
        {
            continue;
        }
        let [width, depth] = lot.shape.axis();
        let area = width.mag() * depth.mag() + 0.001;
        if cell.kind == ZoneKind::Residential {
            if ResidencePrototype::iter().any(|proto| {
                proto.density == cell.density && proto.level == 1 && area >= proto.lot_size_min
            }) {
                eligible += 1;
            }
        } else if let Some(proto) = GrowableCompanyPrototype::iter()
            .find(|proto| {
                proto.growable
                    && proto.zone_kind == cell.kind
                    && proto.density == cell.density
                    && proto.level == 1
            })
            .filter(|proto| area >= proto.lot_size_min)
        {
            eligible += 1;
            jobs += proto.n_workers;
        }
    }
    if eligible < 40 || jobs < 110 {
        return Err(format!(
            "preflight: {eligible} eligible zoned lots (need 40), {jobs} potential jobs (need 110)"
        ));
    }
    println!(
        "growth_basic: preflight found {eligible} eligible zoned lots and {jobs} potential jobs"
    );
    Ok(())
}

fn check_growth(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let (capacity, leveled_up) = {
        let homes = ctx.sim.read::<Residences>();
        (
            homes
                .by_building
                .iter()
                .filter(|&(&id, _)| !homes.is_abandoned(id))
                .map(|(_, home)| home.capacity as usize)
                .sum::<usize>(),
            homes
                .by_building
                .values()
                .filter(|home| home.level > 1)
                .count(),
        )
    };
    let world = ctx.sim.world();
    let population = world.humans.len();
    let unemployed = world.humans.values().filter(|h| h.work.is_none()).count();
    let jobs: u32 = world
        .companies
        .values()
        .filter(|company| {
            prototypes::try_prototype(GrowableCompanyID::from(company.comp.proto))
                .is_some_and(|proto| proto.growable)
        })
        .map(|company| company.comp.max_workers)
        .sum();
    // The longest history level spans the whole scenario. Citizens are the buyers
    // of retail-goods, which are local-only and unused as a recipe input.
    let retail_purchases = ctx
        .sim
        .read::<EcoStats>()
        .internal_trade
        .iter_histories(3)
        .find(|(id, _)| *id == ItemID::new("retail-goods"))
        .map_or(0, |(_, history)| {
            history.past_ring_items.iter().sum::<i64>()
        });
    let unemployment = 100.0 * unemployed as f64 / population.max(1) as f64;
    let summary = format!(
        "capacity={capacity} pop={population} jobs={jobs} unemployment={unemployment:.2}% level_ups={leveled_up} retail_purchases={retail_purchases}"
    );
    if capacity < 150
        || population < 150
        || jobs < 110
        || unemployed * 4 > population
        || leveled_up == 0
        || retail_purchases == 0
    {
        return Err(format!(
            "growth gate failed after {TICKS} ticks ({summary})"
        ));
    }
    println!("growth_basic: {summary}");
    ctx.sample("final");
    let sample = ctx.samples.last_mut().unwrap();
    sample.extra.extend([
        ("residential_capacity".into(), capacity as f64),
        ("jobs".into(), jobs as f64),
        ("unemployment_pct".into(), unemployment),
        ("leveled_up_residences".into(), leveled_up as f64),
        ("retail_purchases".into(), retail_purchases as f64),
    ]);
    Ok(())
}
