use geom::vec2;
use prototypes::{CompanyKind, GameTime, GoodsCompanyID, ItemID, RecTimeInterval, TICKS_PER_HOUR};
use simulation::economy::cargo::CargoBook;
use simulation::economy::{DailyEconomyMetrics, Market, TripPurpose, TICKS_PER_BUDGET_DAY};
use simulation::map::Density::{High, Low};
use simulation::map::ZoneKind::{Commercial, Industrial, Office, Residential};
use simulation::map::{BuildingID, BuildingKind};
use simulation::map_dynamic::BuildingInfos;
use simulation::souls::desire::{Work, WorkKind};
use simulation::souls::goods_company::enable_growable_freight;
use simulation::souls::human::spawn_human;
use simulation::transportation::Location;
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SimulationOptions, SlotId, SnapshotRevision, SoulID};

use crate::builders::{
    fill_houses, grid_city, paint_zone_blocks, place_company, place_freight_station,
    place_grid_power_plant, place_service_sites, CommandSink, UTILITY_SITES,
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
    if std::env::var_os("EGREGORIA_ECONOMY_WAREHOUSE").is_some() {
        return compare_warehouse(ctx);
    }
    let showcase = std::env::var_os("EGREGORIA_ECONOMY_SHOWCASE_ROOT");
    let demo_slot =
        std::env::var("EGREGORIA_ECONOMY_DEMO_SLOT").unwrap_or_else(|_| "showcase_city".into());
    if let Some(root) = &showcase {
        let slot = SlotId::new(&demo_slot).map_err(|error| error.to_string())?;
        ctx.sim = Simulation::load_from_slot(std::path::Path::new(root), &slot)
            .map_err(|error| error.to_string())?;
        // Slots saved before zoned trucks became the default still run the
        // instantaneous zoned lane.
        enable_growable_freight(&mut ctx.sim);
        if demo_slot == "starter_town" {
            // The opening town's first 46 workers fill closer jobs. Grow the
            // neighbourhood by the mine to exercise a staffed freight route.
            paint_zone_blocks(
                ctx,
                vec2(3630.0, 6120.0),
                100.0,
                [(2, 1, Residential, High), (2, 2, Residential, High)],
            );
            // Measure once the new residents staff the mine and its trucks run,
            // as the built fixture does after its two warm-up days.
            ctx.run_ticks(TICKS_PER_BUDGET_DAY);
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
    let mut total_cancelled = 0;
    let mut total_coal_stockout = 0;
    let mut total_coal_road = 0;
    let mut total_coal_station = 0;
    let coal = ItemID::new("coal");
    let goods = ItemID::new("goods");
    let plant = GoodsCompanyID::new("coal-power-plant");
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
        let coal_stockout = data.stockout_ticks_by_sku.get(&coal).copied().unwrap_or(0);
        // Separate the plant's coal by lane: verified truck unloads, and
        // station imports that filled an order freight left open.
        let (mut cancelled, mut coal_road, mut coal_station) = (0, 0, 0);
        let (mut store_road, mut store_station) = (0, 0);
        if let Some(freight) = ctx.sim.read::<CargoBook>().freight_day(day) {
            cancelled = freight.cancelled_loads;
            for (id, company) in &ctx.sim.world().companies {
                if company.comp.proto == plant {
                    let receipt = freight.receipt(SoulID::GoodsCompany(id), coal);
                    coal_road += receipt.road_units;
                    coal_station += receipt.station_units;
                }
                if company.comp.proto.prototype().kind == CompanyKind::Store {
                    let receipt = freight.receipt(SoulID::GoodsCompany(id), goods);
                    store_road += receipt.road_units;
                    store_station += receipt.station_units;
                }
            }
        }
        // Stores stock goods from trucks. A live station fills a stockout that
        // no truck is on its way to.
        println!(
            "economy_loop store goods day {day}: {store_road} units by road, {store_station} \
             station imports"
        );
        total_loads += loads;
        total_load_units += load_units;
        total_late += late;
        total_cancelled += cancelled;
        total_coal_stockout += coal_stockout;
        total_coal_road += coal_road;
        total_coal_station += coal_station;
        println!(
            "economy_loop freight day {day}: {loads} deliveries, {load_units} units, {late} late, \
             {cancelled} cancelled; plant received {coal_road} coal by road and {coal_station} \
             from stations, {coal_stockout} coal plant input stockout ticks",
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
    // A cancelled load is a missed delivery.
    let on_time = total_loads.saturating_sub(total_late) as f64
        / (total_loads + total_cancelled).max(1) as f64;
    let station_share =
        total_coal_station as f64 / (total_coal_road + total_coal_station).max(1) as f64;
    println!(
        "economy_loop freight totals: {total_loads} deliveries, {mean_load:.2} units/load, \
         {:.1}% on time, {total_cancelled} cancelled; plant received {total_coal_road} coal by road \
         and {total_coal_station} from stations ({:.1}% station share), \
         {total_coal_stockout} coal plant input stockout ticks",
        100.0 * on_time,
        100.0 * station_share,
    );
    if total_loads == 0
        || mean_load < 8.0
        || on_time < 0.9
        || !plant_supplied_by_road(total_coal_road, total_coal_station, total_coal_stockout)
    {
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

/// The coal plant runs on trucked coal. A live station fills any order the
/// trucks leave open, so station imports count as masked stockouts.
fn plant_supplied_by_road(road_units: u64, station_units: u64, stockout_ticks: u64) -> bool {
    road_units > 0 && station_units == 0 && stockout_ticks == 0
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

fn compare_warehouse(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let mut zoned = ScenarioCtx::new(SCENARIO.1);
    zoned_depot(&mut zoned)?;
    let mut direct = ScenarioCtx::new(SCENARIO.1);
    let direct = coal_supply_shock(&mut direct, false)?;
    let buffered = coal_supply_shock(ctx, true)?;
    println!(
        "economy_loop warehouse: after the mine's output is cut, the plant waits {} ticks and \
         receives {} coal directly; with the depot it waits {} ticks and receives {} coal \
         ({} coal staged at the depot before the cut)",
        direct.stockout_ticks,
        direct.units,
        buffered.stockout_ticks,
        buffered.units,
        buffered.staged,
    );
    if buffered.staged == 0
        || buffered.units == 0
        || buffered.stockout_ticks >= direct.stockout_ticks
    {
        return Err("Depot must restock from the mine and shorten the shortage".into());
    }
    ctx.sample("warehouse_recovery");
    Ok(())
}

/// A player places a depot beside the zoned industry and stores of a running,
/// fully employed city during work hours. Its first driver comes from the
/// hourly staffing pass. Staged goods give it something to move, because this
/// city's stores take all the zoned output directly.
fn zoned_depot(ctx: &mut ScenarioCtx) -> Result<(), String> {
    let origin = vec2(704.0, 704.0);
    build_fixture(ctx)?;
    ctx.run_ticks(2 * TICKS_PER_BUDGET_DAY);
    while ctx.sim.read::<GameTime>().daytime.hour != 9 {
        ctx.run_ticks(TICKS_PER_HOUR / 60);
    }
    // Below the grid's south edge, between the commercial and industrial columns.
    let depot = place_company(ctx, "small-depot", origin + vec2(400.0, -45.0))
        .ok_or("could not place the zoned depot")?;
    let soul = ctx
        .sim
        .read::<BuildingInfos>()
        .owner(depot)
        .ok_or("missing depot owner")?;
    let SoulID::GoodsCompany(company) = soul else {
        return Err("depot is not a company".into());
    };
    let goods = ItemID::new("goods");
    ctx.sim.write::<Market>().produce(soul, goods, 48);
    let (mut hired, mut first_truck) = (None, None);
    for minute in 1..=3 * 60 {
        ctx.run_ticks(TICKS_PER_HOUR / 60);
        let book = ctx.sim.read::<CargoBook>();
        if hired.is_none() && !ctx.sim.world().companies[company].workers.0.is_empty() {
            hired = Some(minute);
        }
        if first_truck.is_none() && book.outgoing_loads(depot).next().is_some() {
            first_truck = Some(minute);
        }
        if minute % 30 == 0 {
            println!(
                "economy_loop zoned depot after {minute} min: {} workers, {:?}, {} goods on hand",
                ctx.sim.world().companies[company].workers.0.len(),
                book.warehouse_activity(company),
                ctx.sim.read::<Market>().capital(soul, goods),
            );
        }
    }
    println!(
        "economy_loop zoned depot: first driver hired after {hired:?} min, first truck out \
         after {first_truck:?} min"
    );
    ctx.sample("zoned_depot");
    // The hourly pass hires within the hour. A driver without a car then walks
    // across town, so the truck leaves later.
    if hired.is_none_or(|minute| minute > 61) || first_truck.is_none() {
        return Err("The zoned depot must hire within the hour and then run a truck".into());
    }
    Ok(())
}

struct Recovery {
    staged: i32,
    stockout_ticks: u64,
    units: i32,
}

/// The mine runs with a staffed truck while the plant is stocked. A depot
/// near the plant restocks from it through normal matching. Then the mine's
/// stock is lost and the plant runs dry, and the plant's wait for coal is
/// measured with and without the buffer.
fn coal_supply_shock(ctx: &mut ScenarioCtx, buffered: bool) -> Result<Recovery, String> {
    let stations: Vec<_> = ctx
        .sim
        .map()
        .buildings()
        .iter()
        .filter_map(|(id, building)| {
            matches!(building.kind, BuildingKind::RailFreightStation(_))
                .then_some(WorldCommand::MapRemoveBuilding(id))
        })
        .collect();
    ctx.submit(&stations);
    ctx.flush();

    let origin = vec2(704.0, 704.0);
    grid_city(ctx, origin, 5, 2, 200.0);
    let mine = place_company(ctx, "coal-mine", origin + vec2(100.0, 25.0))
        .ok_or("could not place the coal mine")?;
    let plant = place_company(ctx, "coal-power-plant", origin + vec2(700.0, -100.0))
        .ok_or("could not place the coal consumer")?;
    let houses = fill_houses(ctx, 2);
    let [mine_home, depot_home] = houses[..] else {
        return Err("could not place the drivers' homes".into());
    };
    let depot = if buffered {
        Some(
            place_company(ctx, "small-depot", WAREHOUSE_SITE + origin)
                .ok_or("could not place the local depot")?,
        )
    } else {
        None
    };
    let owner = |sim: &Simulation, building| {
        sim.read::<BuildingInfos>()
            .owner(building)
            .ok_or("missing company owner")
    };
    let (mine_soul, consumer) = (owner(&ctx.sim, mine)?, owner(&ctx.sim, plant)?);
    let coal = ItemID::new("coal");
    {
        let mut market = ctx.sim.write::<Market>();
        market.produce(mine_soul, coal, 80);
        // Above the plant's reorder level, so the mine's surplus can be staged.
        market.produce(consumer, coal, 100);
    }
    staff_dispatch(&mut ctx.sim, mine_home, mine, mine_soul)?;
    if let Some(depot) = depot {
        let soul = owner(&ctx.sim, depot)?;
        staff_dispatch(&mut ctx.sim, depot_home, depot, soul)?;
    }
    ctx.run_ticks(3 * TICKS_PER_HOUR);

    let staged = match depot {
        Some(depot) => ctx
            .sim
            .read::<Market>()
            .capital(owner(&ctx.sim, depot)?, coal),
        None => 0,
    };
    {
        let mut market = ctx.sim.write::<Market>();
        let mine_stock = market.capital(mine_soul, coal);
        market.produce(mine_soul, coal, -mine_stock);
        let plant_stock = market.capital(consumer, coal);
        market.produce(consumer, coal, -plant_stock);
    }

    let snapshot = depot.and_then(|_| std::env::var_os("EGREGORIA_WAREHOUSE_CAPTURE"));
    let mut captured = false;
    let mut stockout_ticks = 0;
    for _ in 0..2 * TICKS_PER_HOUR {
        ctx.run_ticks(1);
        if let (Some(root), Some(depot), false) = (&snapshot, depot, captured) {
            let book = ctx.sim.read::<CargoBook>();
            if book.outgoing_loads(depot).next().is_some() {
                drop(book);
                let slot = SlotId::new("warehouse_capture").map_err(|error| error.to_string())?;
                let revision = SnapshotRevision::new(u128::from(ctx.sim.get_tick()));
                ctx.sim
                    .encode_slot(revision)
                    .and_then(|save| {
                        save.with_display_name("Warehouse capture".into())
                            .publish(std::path::Path::new(root), &slot)
                    })
                    .map_err(|error| error.to_string())?;
                let map = ctx.sim.map();
                let center = map.buildings()[depot].obb.center();
                println!("warehouse_capture site at {}, {}", center.x, center.y);
                captured = true;
            }
        }
        if ctx.sim.read::<Market>().capital(consumer, coal) <= 0 {
            stockout_ticks += 1;
        }
    }
    Ok(Recovery {
        staged,
        stockout_ticks,
        units: ctx.sim.read::<Market>().capital(consumer, coal),
    })
}

// Upstream of the plant on the same streets, so the depot's route to it is short.
const WAREHOUSE_SITE: geom::Vec2 = vec2(680.0, 24.0);

fn staff_dispatch(
    sim: &mut Simulation,
    house: BuildingID,
    building: BuildingID,
    source: SoulID,
) -> Result<(), String> {
    let SoulID::GoodsCompany(company) = source else {
        return Err("dispatch site is not a company".into());
    };
    let driver = spawn_human(sim, house).ok_or("could not seed the freight driver")?;
    let world = sim.world_mut_unchecked();
    let entity = &mut world.companies[company];
    let truck = *entity
        .comp
        .trucks
        .first()
        .ok_or("dispatch fleet has no truck")?;
    entity.comp.driver = Some(driver);
    entity.workers.0.push(driver);
    let mut work = Work::new(
        building,
        WorkKind::Driver {
            truck,
            load: None,
            deliver_order: None,
        },
        0.0,
    );
    work.work_inter = RecTimeInterval::always();
    world.humans[driver].work = Some(work);
    world.humans[driver].location = Location::Building(building);
    let mut infos = sim.write::<BuildingInfos>();
    infos.get_out(house, SoulID::Human(driver));
    infos.get_in(building, SoulID::Human(driver));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::plant_supplied_by_road;

    #[test]
    fn station_imports_cannot_pass_the_coal_freight_gate() {
        assert!(!plant_supplied_by_road(0, 0, 0), "no physical delivery");
        assert!(!plant_supplied_by_road(0, 20, 0), "station supply only");
        assert!(
            !plant_supplied_by_road(40, 2, 0),
            "a station import masked a stockout"
        );
        assert!(!plant_supplied_by_road(40, 0, 1), "the plant stalled");
        assert!(plant_supplied_by_road(40, 0, 0));
    }
}
