use geom::vec2;
use prototypes::{ItemID, TICKS_PER_HOUR};
use simulation::economy::{DailyEconomyMetrics, Market, TripPurpose, TICKS_PER_BUDGET_DAY};
use simulation::map::Density::{High, Low};
use simulation::map::ZoneKind::{Commercial, Industrial, Office, Residential};
use simulation::{Simulation, SimulationOptions, SlotId};

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
    if let Some(root) = std::env::var_os("EGREGORIA_ECONOMY_SHOWCASE_ROOT") {
        let slot = SlotId::new("showcase_city").map_err(|error| error.to_string())?;
        ctx.sim = Simulation::load_from_slot(std::path::Path::new(&root), &slot)
            .map_err(|error| error.to_string())?;
    } else {
        build_fixture(ctx)?;
        ctx.run_ticks(2 * TICKS_PER_BUDGET_DAY);
    }

    let now = simulation::map_dynamic::transit::elapsed_ticks(ctx.sim.world_res().1).0;
    let offset = now % TICKS_PER_BUDGET_DAY;
    if offset != 0 {
        ctx.run_ticks(TICKS_PER_BUDGET_DAY - offset);
    }
    let first_day = now.div_ceil(TICKS_PER_BUDGET_DAY);
    ctx.sample("warmup");

    for day in first_day..first_day + 2 {
        let mut resident_hours = 0;
        let mut employed_hours = 0;
        for _ in 0..24 {
            let humans = &ctx.sim.world().humans;
            resident_hours += humans.len();
            employed_hours += humans.values().filter(|human| human.work.is_some()).count();
            ctx.run_ticks(TICKS_PER_HOUR);
        }
        let residents = (resident_hours as f64 / 24.0).max(1.0);
        let employed = (employed_hours as f64 / 24.0).max(1.0);
        ctx.sample(&format!("day_{day}"));

        let metrics = ctx.sim.read::<DailyEconomyMetrics>();
        let data = metrics
            .closed_day(day, (day + 1) * TICKS_PER_BUDGET_DAY)
            .ok_or_else(|| format!("no measurements for economy day {day}"))?;
        let trips = |purpose| {
            data.trips_by_purpose
                .get(&purpose)
                .map_or(0, |stats| stats.completed)
        };
        let units: u64 = data.units_fulfilled_by_sku.values().sum();
        let bread = data
            .units_fulfilled_by_sku
            .get(&ItemID::new("bread"))
            .copied()
            .unwrap_or(0);
        let stockouts = |name| {
            data.stockout_ticks_by_sku
                .get(&ItemID::new(name))
                .map_or(0.0, |&ticks| {
                    ticks as f64 / TICKS_PER_HOUR as f64 / residents
                })
        };
        println!(
            "economy_loop day {day} ({residents:.2} resident-days, {employed:.2} employed-days): \
             {:.3} shopping trips/resident-day, {:.3} units/resident-day, {bread} bread units, \
             {} park visits, {} work arrivals ({:.3}/employed-day), \
             {:.3} effective food and {:.3} retail stockout hours/resident-day, \
             {} / {} baskets filled; units by SKU {:?}; trips by purpose {:?}",
            trips(TripPurpose::Shopping) as f64 / residents,
            units as f64 / residents,
            trips(TripPurpose::Leisure),
            trips(TripPurpose::Work),
            trips(TripPurpose::Work) as f64 / employed,
            stockouts("groceries"),
            stockouts("retail-goods"),
            data.baskets_filled.unwrap_or(0),
            data.baskets_due.unwrap_or(0),
            data.units_fulfilled_by_sku,
            data.trips_by_purpose,
        );
        println!(
            "economy_loop cumulative pantry losses {:?}",
            ctx.sim.read::<Market>().household_losses()
        );
    }
    Ok(())
}

fn build_fixture(ctx: &mut ScenarioCtx) -> Result<(), String> {
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

    Ok(())
}
