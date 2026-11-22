use geom::vec2;
use prototypes::{ItemID, TICKS_PER_HOUR};
use simulation::economy::{DailyEconomyMetrics, Market, TripPurpose, TICKS_PER_BUDGET_DAY};
use simulation::map::Density::{High, Low};
use simulation::map::ZoneKind::{Commercial, Industrial, Office, Residential};
use simulation::{Simulation, SimulationOptions, SlotId};

use crate::builders::{
    grid_city, paint_zone_blocks, place_company, place_freight_station, place_grid_power_plant,
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
    let showcase = std::env::var_os("EGREGORIA_ECONOMY_SHOWCASE_ROOT");
    let demo_slot =
        std::env::var("EGREGORIA_ECONOMY_DEMO_SLOT").unwrap_or_else(|_| "showcase_city".into());
    if let Some(root) = &showcase {
        let slot = SlotId::new(&demo_slot).map_err(|error| error.to_string())?;
        ctx.sim = Simulation::load_from_slot(std::path::Path::new(root), &slot)
            .map_err(|error| error.to_string())?;
        if demo_slot == "starter_town" {
            // The opening town's first 46 workers fill closer jobs. Grow the
            // neighbourhood by the mine to exercise a staffed freight route.
            paint_zone_blocks(
                ctx,
                vec2(3630.0, 6120.0),
                100.0,
                [(2, 1, Residential, High), (2, 2, Residential, High)],
            );
        }
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

    let mut total_resident_days = 0.0;
    let mut total_outings = 0;
    let mut total_units = 0;
    let mut total_groceries = 0;
    let mut total_due = 0;
    let mut total_filled = 0;
    let mut total_food_stockout = 0.0;
    let mut total_loads = 0;
    let mut total_load_units = 0;
    let mut total_late = 0;
    let mut total_coal_stockout = 0;
    let failures_before = ctx.sim.read::<Market>().basket_failures().clone();

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
        let loads = data.loads.unwrap_or(0);
        let load_units = data.load_units.unwrap_or(0);
        let late = data.late_loads.unwrap_or(0);
        let coal_stockout = data
            .stockout_ticks_by_sku
            .get(&ItemID::new("coal"))
            .copied()
            .unwrap_or(0);
        total_loads += loads;
        total_load_units += load_units;
        total_late += late;
        total_coal_stockout += coal_stockout;
        println!(
            "economy_loop freight day {day}: {loads} loads, {load_units} units, {late} late or cancelled; \
             {coal_stockout} coal plant input stockout ticks",
        );
        let trips = |purpose| {
            data.trips_by_purpose
                .get(&purpose)
                .map_or(0, |stats| stats.completed)
        };
        let units: u64 = data.units_fulfilled_by_sku.values().sum();
        let groceries = data
            .units_fulfilled_by_sku
            .get(&ItemID::new("groceries"))
            .copied()
            .unwrap_or(0);
        let stockouts = |name| {
            data.stockout_ticks_by_sku
                .get(&ItemID::new(name))
                .map_or(0.0, |&ticks| {
                    ticks as f64 / TICKS_PER_HOUR as f64 / residents
                })
        };
        let food_stockout = stockouts("effective-food");
        let due = data.baskets_due.unwrap_or(0);
        let filled = data.baskets_filled.unwrap_or(0);
        let outings = trips(TripPurpose::Shopping);
        total_resident_days += residents;
        total_outings += outings;
        total_units += units;
        total_groceries += groceries;
        total_due += due;
        total_filled += filled;
        total_food_stockout += food_stockout * residents;
        println!(
            "economy_loop day {day} ({residents:.2} resident-days, {employed:.2} employed-days): \
             {:.3} shopping trips/resident-day, {:.3} units/resident-day, {groceries} grocery units, \
             {} park visits, {} work arrivals ({:.3}/employed-day), \
             {food_stockout:.3} effective food, {:.3} groceries and {:.3} retail stockout hours/resident-day, \
             {filled} / {due} baskets filled; units by SKU {:?}; trips by purpose {:?}",
            outings as f64 / residents,
            units as f64 / residents,
            trips(TripPurpose::Leisure),
            trips(TripPurpose::Work),
            trips(TripPurpose::Work) as f64 / employed,
            stockouts("groceries"),
            stockouts("retail-goods"),
            data.units_fulfilled_by_sku,
            data.trips_by_purpose,
        );
    }

    let market = ctx.sim.read::<Market>();
    let failures: Vec<_> = market
        .basket_failures()
        .iter()
        .filter_map(|(reason, count)| {
            let delta = count - failures_before.get(reason).copied().unwrap_or(0);
            (delta > 0).then_some((reason, delta))
        })
        .collect();
    let food_stockout = total_food_stockout / total_resident_days;
    let outings = total_outings as f64 / total_resident_days;
    let units = total_units as f64 / total_resident_days;
    println!(
        "economy_loop two-day totals: {total_filled}/{total_due} baskets filled, \
         {food_stockout:.3} food stockout hours/resident-day, {outings:.3} outings/resident-day, \
         {units:.3} units/resident-day, {total_groceries} grocery units; failed attempts {failures:?}; \
         cumulative household losses {:?}, seller write-offs {:?}",
        market.household_losses(),
        market.seller_losses(),
    );
    let mean_load = total_load_units as f64 / total_loads.max(1) as f64;
    let on_time = total_loads.saturating_sub(total_late) as f64 / total_loads.max(1) as f64;
    println!(
        "economy_loop freight totals: {total_loads} loads, {mean_load:.2} units/load, \
         {:.1}% on time, {total_coal_stockout} coal plant input stockout ticks",
        100.0 * on_time,
    );
    if total_loads == 0 || mean_load < 8.0 || on_time < 0.9 || total_coal_stockout != 0 {
        return Err("City missed one or more road freight targets".into());
    }
    if showcase.is_some()
        && demo_slot == "showcase_city"
        && (total_due == 0
            || total_filled as f64 / (total_due as f64) < 0.8
            || food_stockout > 1.5
            || !(0.3..=1.2).contains(&outings)
            || !(2.9..=4.5).contains(&units)
            || total_groceries == 0)
    {
        return Err("Showcase City missed one or more household economy targets".into());
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
    place_company(ctx, "coal-mine", origin + vec2(275.0, 330.0))
        .ok_or("could not place the economy loop coal mine")?;
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
