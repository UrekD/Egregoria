use geom::vec2;
use simulation::economy::{DailyEconomyMetrics, TICKS_PER_BUDGET_DAY};
use simulation::map::Density::{High, Low};
use simulation::map::ZoneKind::{Commercial, Industrial, Office, Residential};
use simulation::SimulationOptions;

use crate::builders::{
    grid_city, paint_zone_blocks, place_freight_station, place_grid_power_plant,
    place_service_sites, UTILITY_SITES,
};
use crate::{Scenario, ScenarioCtx};

pub(super) const SCENARIO: (Scenario, SimulationOptions) = (
    Scenario {
        name: "economy_loop",
        run,
    },
    SimulationOptions {
        terrain_size: simulation::NewGameOptions::MIN_TERRAIN_CHUNKS,
        starting_money_bucks: 350_000,
        ..super::services_basic::OPTIONS
    },
);

fn run(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let origin = vec2(704.0, 704.0);
    let spacing = 100.0;
    grid_city(ctx, origin, 7, 7, spacing);
    place_freight_station(
        ctx,
        origin + vec2(450.0, 250.0),
        prototypes::Size2D::new(40.0, 50.0),
    )
    .ok_or("could not place the economy loop freight station")?;
    place_grid_power_plant(ctx, origin)?;
    place_service_sites(ctx, origin, UTILITY_SITES)?;

    paint_zone_blocks(
        ctx,
        origin,
        spacing,
        (0..6).flat_map(|y| {
            (0..6).map(move |x| {
                let (kind, density) = match x {
                    0 => (Residential, Low),
                    1..=2 => (Residential, High),
                    3 => (Commercial, Low),
                    4 => (Industrial, Low),
                    _ => (Office, Low),
                };
                (x, y, kind, density)
            })
        }),
    );

    ctx.run_ticks(TICKS_PER_BUDGET_DAY);
    let population = ctx.sim.world().humans.len();
    let metrics = ctx.sim.read::<DailyEconomyMetrics>();
    println!(
        "economy_loop: {population} residents, daily economy metrics {:?}",
        &*metrics
    );
    Ok(())
}
