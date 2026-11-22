use crate::economy::balance::{
    periodic_tick, shelf_delivery_item, shelf_delivery_units, DEFAULT_TAX_RATE_PERMILLE,
    DEMAND_EMA_ALPHA, DEMAND_INTERVAL_TICKS, DEMAND_OFFSET_TICKS, FOOD_UNIT_TICKS,
    GOODS_UNIT_TICKS, I_BASE, I_BOOTSTRAP_COMPANIES, K_GOODS, K_HAPPY, K_JOBS, K_RET, K_TAX,
    K_UNEMP, K_VAC, MIN_DEMAND_POPULATION, NEUTRAL_HAPPINESS, R_BASE, R_BOOTSTRAP_POPULATION,
    SHELF_TARGET_UTILIZATION,
};
use crate::economy::{Budget, Market};
use crate::map::{BuildingID, ZoneKind};
use crate::map_dynamic::BuildingInfos;
use crate::souls::happiness::Happiness;
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::{SoulID, World};
use common::FastMap;
use prototypes::{try_prototype, GrowableCompanyID, ItemID, RecipeItem, DELTA_F64, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Default, Serialize, Deserialize)]
pub struct ZoneDemand {
    pub values: BTreeMap<ZoneKind, f32>,
    drivers: BTreeMap<ZoneKind, DemandDrivers>,
    #[serde(skip)]
    occupants: FastMap<BuildingID, u32>,
}

/// Signed contributions to a zone's latest demand sample, before clamping and smoothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DemandDrivers {
    /// Zone bootstrap contribution.
    pub base: f32,
    /// Open job orders minus unemployed residents, weighted for residential demand.
    pub jobs: f32,
    pub vacancies: f32,
    pub tax: f32,
    pub shortfall: f32,
    pub unemployment: f32,
    pub happiness: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemandDriver {
    Base,
    Jobs,
    Vacancies,
    Tax,
    Shortfall,
    Unemployment,
    Happiness,
}

impl ZoneDemand {
    pub(crate) const fn due(tick: u64) -> bool {
        tick % DEMAND_INTERVAL_TICKS == DEMAND_OFFSET_TICKS
    }

    pub(crate) fn has_sample(&self) -> bool {
        !self.drivers.is_empty()
    }

    pub fn drivers(&self, zone: ZoneKind) -> Option<&DemandDrivers> {
        self.drivers.get(&zone)
    }

    /// Returns the strongest nonzero contribution by magnitude, keeping its sign.
    /// Equal magnitudes use declaration order in `DemandDriver`.
    pub fn top_driver(&self, zone: ZoneKind) -> Option<(DemandDriver, f32)> {
        let drivers = self.drivers(zone)?;
        [
            (DemandDriver::Base, drivers.base),
            (DemandDriver::Jobs, drivers.jobs),
            (DemandDriver::Vacancies, drivers.vacancies),
            (DemandDriver::Tax, drivers.tax),
            (DemandDriver::Shortfall, drivers.shortfall),
            (DemandDriver::Unemployment, drivers.unemployment),
            (DemandDriver::Happiness, drivers.happiness),
        ]
        .into_iter()
        .rev()
        .filter(|&(_, value)| value != 0.0)
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
    }
}

pub fn demand_update(world: &mut World, resources: &mut Resources) {
    let budget = resources.read::<Budget>();
    if !ZoneDemand::due(periodic_tick(&budget, resources.tick())) {
        return;
    }

    let jobs = ItemID::new("job-opening");
    let market = resources.read::<Market>();
    let open_jobs: u64 = market.inner().get(&jobs).map_or(0, |job_market| {
        world
            .companies
            .keys()
            .filter_map(|id| job_market.sell_order(SoulID::GoodsCompany(id)))
            .map(|order| u64::from(order.qty))
            .sum()
    });

    let residences = resources.read::<Residences>();
    let mut demand = resources.write::<ZoneDemand>();
    let occupants = &mut demand.occupants;
    occupants.clear();
    let mut unemployed = 0;
    let mut unmet = [0_u64; 2];
    let now = budget.elapsed_ticks(resources.tick());
    for human in world.humans.values() {
        *occupants.entry(human.home.house).or_insert(0u32) += 1;
        for (total, request) in unmet
            .iter_mut()
            .zip(human.household.unmet_shelf_demand(now))
        {
            *total += u64::from(request);
        }
        let pending_hire = human
            .bought
            .0
            .get(&jobs)
            .is_some_and(|trades| !trades.is_empty());
        if human.work.is_none() && !pending_hire {
            unemployed += 1;
        }
    }
    let vacancies: u64 = residences
        .by_building
        .iter()
        .map(|(building, residence)| {
            let occupied = occupants.get(building).copied().unwrap_or(0);
            u64::from(residence.capacity.saturating_sub(occupied))
        })
        .sum();

    let population = world.humans.len();
    let per_capita = 100.0 / population.max(MIN_DEMAND_POPULATION) as f32;
    let unemployed_norm = unemployed as f32 * per_capita;
    let rates = company_rates(world);
    let r_base = if population < R_BOOTSTRAP_POPULATION {
        R_BASE
    } else {
        0.0
    };
    let i_base = if rates.industrial_growables < I_BOOTSTRAP_COMPANIES {
        I_BASE
    } else {
        0.0
    };
    let happiness = K_HAPPY
        * (resources
            .read::<Happiness>()
            .average(&resources.read::<BuildingInfos>())
            - NEUTRAL_HAPPINESS);

    for (zone, mut drivers) in [
        (
            ZoneKind::Residential,
            DemandDrivers {
                base: r_base,
                happiness,
                jobs: K_JOBS * (open_jobs as f32 - unemployed as f32) * per_capita,
                vacancies: -K_VAC * vacancies as f32 * per_capita,
                ..Default::default()
            },
        ),
        (
            ZoneKind::Commercial,
            DemandDrivers {
                shortfall: K_RET * rates.retail_shortfall(population, unmet),
                ..Default::default()
            },
        ),
        (
            ZoneKind::Industrial,
            DemandDrivers {
                base: i_base,
                shortfall: K_GOODS * shortfall_norm(rates.goods_need, rates.goods_supply),
                unemployment: K_UNEMP * unemployed_norm,
                ..Default::default()
            },
        ),
        (
            ZoneKind::Office,
            DemandDrivers {
                unemployment: K_UNEMP / 3.0 * unemployed_norm,
                ..Default::default()
            },
        ),
    ] {
        let tax = f32::from(budget.tax_rate(zone)) - f32::from(DEFAULT_TAX_RATE_PERMILLE);
        drivers.tax = -K_TAX * tax / 10.0;
        let raw = (drivers.base
            + drivers.jobs
            + drivers.vacancies
            + drivers.shortfall
            + drivers.unemployment
            + drivers.happiness
            + drivers.tax)
            .clamp(-100.0, 100.0);
        demand
            .values
            .entry(zone)
            .and_modify(|smoothed| *smoothed += DEMAND_EMA_ALPHA * (raw - *smoothed))
            .or_insert(raw);
        demand.drivers.insert(zone, drivers);
    }
}

fn shortfall_norm(need: f64, supply: f64) -> f32 {
    ((need - supply) / need.max(1.0) * 100.0).clamp(-100.0, 100.0) as f32
}

#[derive(Default)]
struct CompanyRates {
    goods_need: f64,
    goods_supply: f64,
    retail_supply: f64,
    groceries_supply: f64,
    industrial_growables: usize,
}

impl CompanyRates {
    fn retail_shortfall(&self, population: usize, unmet: [u64; 2]) -> f32 {
        let residents = population as f64;
        let staple_rate = TICKS_PER_HOUR as f64 / FOOD_UNIT_TICKS as f64;
        let goods_rate = TICKS_PER_HOUR as f64 / GOODS_UNIT_TICKS as f64;
        // Count live unmet lines by SKU. They can reveal empty or unevenly
        // distributed shelves even when nominal staffed production is high.
        let shortage = |rate, supply, missing| {
            let hourly_need: f64 = residents * rate;
            let nominal = shortfall_norm(hourly_need, supply);
            if missing == 0 {
                nominal
            } else {
                nominal.max(shortfall_norm(
                    hourly_need,
                    hourly_need - missing as f64 / 4.0,
                ))
            }
        };
        let staples = shortage(staple_rate, self.groceries_supply, unmet[0]);
        let goods = shortage(goods_rate, self.retail_supply, unmet[1]);
        staples.max(goods)
    }
}

fn company_rates(world: &World) -> CompanyRates {
    let goods = ItemID::new("goods");
    let [groceries, retail_goods] = crate::souls::household::household_items();
    let mut rates = CompanyRates::default();
    let amount = |items: &[RecipeItem], kind| {
        items
            .iter()
            .filter(|item| item.id == kind)
            .map(|item| f64::from(item.amount))
            .sum::<f64>()
    };

    for company in world.companies.values() {
        let proto = company.comp.proto.prototype();
        let growable = try_prototype(GrowableCompanyID::from(company.comp.proto))
            .filter(|proto| proto.growable);
        if growable.is_some_and(|proto| proto.zone_kind == ZoneKind::Industrial) {
            rates.industrial_growables += 1;
        }
        let Some(recipe) = &proto.recipe else {
            continue;
        };
        let duration = recipe.duration.seconds();
        if duration <= 0.0 {
            continue;
        }
        // The company system advances recipe progress by DELTA per simulation tick.
        let full_batches = TICKS_PER_HOUR as f64 * DELTA_F64 / duration;
        let staffed_batches = full_batches * f64::from(company.raw_productivity(proto, None));
        let delivery = amount(&recipe.production, shelf_delivery_item()) as i32;
        let [staples, _] = shelf_delivery_units(delivery);
        rates.groceries_supply += (amount(&recipe.production, groceries)
            + f64::from(staples) * SHELF_TARGET_UTILIZATION)
            * staffed_batches;
        let retail_output = amount(&recipe.production, retail_goods);
        let retail_utilization = if delivery > 0 {
            SHELF_TARGET_UTILIZATION
        } else {
            1.0
        };
        rates.retail_supply += retail_output * retail_utilization * staffed_batches;
        match growable.map(|proto| proto.zone_kind) {
            Some(ZoneKind::Commercial) => {
                rates.goods_need += amount(&recipe.consumption, goods) * full_batches;
            }
            Some(ZoneKind::Industrial) => {
                rates.goods_supply += amount(&recipe.production, goods) * staffed_batches;
            }
            _ => {}
        }
    }
    rates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::market_update;
    use crate::map::BuildingID;
    use crate::souls::goods_company::GoodsCompanyState;
    use crate::souls::human::spawn_human;
    use crate::souls::residence::Residence;
    use crate::tests::TestCtx;
    use crate::world::{CompanyEnt, CompanyID};
    use crate::Simulation;
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Transform, Vec3};
    use prototypes::{Density, GameTime, GoodsCompanyID, ResidencePrototypeID, Tick};

    fn sample(sim: &mut Simulation, cycle: u64) -> BTreeMap<ZoneKind, f32> {
        let tick = (cycle - 1) * DEMAND_INTERVAL_TICKS + DEMAND_OFFSET_TICKS;
        assert!(ZoneDemand::due(tick));
        sample_at(sim, tick)
    }

    fn sample_at(sim: &mut Simulation, tick: u64) -> BTreeMap<ZoneKind, f32> {
        *sim.write::<GameTime>() = GameTime::new(Tick(tick));
        let (world, resources) = sim.world_res();
        demand_update(world, resources);
        sim.read::<ZoneDemand>().values.clone()
    }

    fn home(ctx: &TestCtx, capacity: u32) -> BuildingID {
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
        let house = ctx.build_house_near(vec2(150.0, 125.0));
        ctx.g.write::<Residences>().by_building.insert(
            house,
            Residence {
                prototype: ResidencePrototypeID::new("residence-low-l1"),
                density: Density::Low,
                level: 1,
                capacity,
            },
        );
        house
    }

    fn company(world: &mut World, name: &str, max_workers: u32) -> CompanyID {
        world.insert(CompanyEnt {
            trans: Transform::new(Vec3::ZERO),
            comp: GoodsCompanyState {
                proto: GoodsCompanyID::new(name),
                building: BuildingID::default(),
                max_workers,
                progress: 0.0,
                driver: None,
                trucks: vec![],
            },
            workers: Default::default(),
            sold: Default::default(),
            bought: Default::default(),
        })
    }

    #[test]
    fn p1a_demand_bootstrap_and_ema_cadence() {
        let mut ctx = TestCtx::new_fast();
        *ctx.g.write::<GameTime>() = GameTime::new(Tick(DEMAND_OFFSET_TICKS - 2));
        ctx.tick_fast();
        assert!(ctx.g.read::<ZoneDemand>().values.is_empty());
        ctx.tick_fast();
        assert!(ZoneDemand::due(ctx.g.get_tick()));
        let first = ctx.g.read::<ZoneDemand>().values.clone();
        assert_eq!(first.get(&ZoneKind::Residential), Some(&40.0));
        assert_eq!(first.get(&ZoneKind::Industrial), Some(&30.0));
        assert_eq!(first.get(&ZoneKind::Commercial), Some(&0.0));
        assert_eq!(first.get(&ZoneKind::Office), Some(&0.0));

        let house = home(&ctx, 1);
        let off_tick = DEMAND_INTERVAL_TICKS + DEMAND_OFFSET_TICKS - 1;
        assert!(!ZoneDemand::due(off_tick));
        assert_eq!(sample_at(&mut ctx.g, off_tick), first);
        let smoothed = sample(&mut ctx.g, 2);
        spawn_human(&mut ctx.g, house).unwrap();
        ctx.g.write::<ZoneDemand>().values.clear();
        let raw = sample(&mut ctx.g, 3);
        assert_eq!(raw[&ZoneKind::Residential], 40.0 - K_JOBS * 10.0);
        assert!(raw[&ZoneKind::Office] > 0.0);
        assert_eq!(smoothed[&ZoneKind::Industrial], 30.0);
        let expected = 40.0 - 0.2 * K_VAC * 10.0;
        assert!((smoothed[&ZoneKind::Residential] - expected).abs() < 0.00001);

        ctx.g
            .write::<Residences>()
            .by_building
            .get_mut(&house)
            .unwrap()
            .capacity = 1_000;
        ctx.g.write::<ZoneDemand>().values.clear();
        assert_eq!(sample(&mut ctx.g, 4)[&ZoneKind::Residential], -100.0);
    }

    #[test]
    fn industrial_bootstrap_counts_only_industrial_growables() {
        let mut ctx = TestCtx::new_fast();
        let world = ctx.g.world_mut_unchecked();
        company(world, "bakery", 5);
        company(world, "commercial-l1", 4);
        company(world, "office-l1", 6);
        let goods_shortfall = K_GOODS * 100.0;
        assert_eq!(
            sample(&mut ctx.g, 1)[&ZoneKind::Industrial],
            I_BASE + goods_shortfall
        );

        let world = ctx.g.world_mut_unchecked();
        for _ in 0..I_BOOTSTRAP_COMPANIES {
            company(world, "industrial-l1", 6);
        }
        ctx.g.write::<ZoneDemand>().values.clear();
        assert_eq!(
            sample(&mut ctx.g, 2)[&ZoneKind::Industrial],
            goods_shortfall
        );
    }

    #[test]
    fn demand_drivers_store_signed_terms_before_smoothing() {
        use ZoneKind::{Commercial, Industrial, Office, Residential};

        let mut ctx = TestCtx::new_fast();
        assert!(ctx.g.read::<ZoneDemand>().drivers(Residential).is_none());
        sample(&mut ctx.g, 1);
        assert!(ctx.g.read::<ZoneDemand>().top_driver(Office).is_none());

        let house = home(&ctx, 4);
        spawn_human(&mut ctx.g, house).unwrap();
        ctx.g.write::<Budget>().set_tax_rate(Residential, 190);
        ctx.g.write::<Budget>().set_tax_rate(Office, 290);
        let values = sample(&mut ctx.g, 2);
        let demand = ctx.g.read::<ZoneDemand>();
        assert_eq!(
            demand.drivers(Residential),
            Some(&DemandDrivers {
                base: R_BASE,
                jobs: -K_JOBS * 10.0,
                vacancies: -K_VAC * 30.0,
                tax: -K_TAX * 10.0,
                ..Default::default()
            })
        );
        assert!(demand.drivers(Commercial).unwrap().shortfall > 0.0);
        assert_eq!(
            demand.drivers(Industrial).unwrap().unemployment,
            K_UNEMP * 10.0
        );
        assert_eq!(
            demand.top_driver(Office),
            Some((DemandDriver::Tax, -K_TAX * 20.0))
        );
        assert_eq!(
            values[&Office],
            DEMAND_EMA_ALPHA * (K_UNEMP / 3.0 * 10.0 - K_TAX * 20.0)
        );

        let encoded = Bincode::encode(&*demand).unwrap();
        let decoded: ZoneDemand = Bincode::decode(&encoded).unwrap();
        assert_eq!(demand.drivers, decoded.drivers);
        assert_eq!(demand.values, decoded.values);
    }

    #[test]
    fn p1a_demand_deterministic_two_runs() {
        fn trajectory() -> Vec<BTreeMap<ZoneKind, f32>> {
            let mut ctx = TestCtx::new_fast();
            let initial = sample(&mut ctx.g, 1);
            let house = home(&ctx, 1);
            let human = spawn_human(&mut ctx.g, house).unwrap();
            let (world, res) = ctx.g.world_res();
            let industrial = company(world, "industrial-l1", 6);
            let commercial = company(world, "commercial-l1", 4);
            company(world, "office-l1", 6);
            let jobs = ItemID::new("job-opening");
            let seller = SoulID::GoodsCompany(industrial);
            {
                let mut market = res.write::<Market>();
                market.produce(seller, jobs, 6);
                market.sell(seller, vec2(150.0, 125.0), jobs, 2, 0);
            }
            let unstaffed = sample(&mut ctx.g, 2);
            let unstaffed_industrial = *ctx
                .g
                .read::<ZoneDemand>()
                .drivers(ZoneKind::Industrial)
                .unwrap();
            let (world, res) = ctx.g.world_res();
            market_update(world, res);
            assert!(world.get(human).unwrap().work.is_none());
            let pending_hire = sample(&mut ctx.g, 3);
            let pending_industrial = *ctx
                .g
                .read::<ZoneDemand>()
                .drivers(ZoneKind::Industrial)
                .unwrap();
            let (world, _) = ctx.g.world_res();
            let hired = world.companies[industrial].workers.0.pop().unwrap();
            world.companies[commercial].workers.0.push(hired);
            let staffed = sample(&mut ctx.g, 4);

            let raw_residential = 40.0 + K_JOBS * 10.0;
            let first_ema = 40.0 + 0.2 * (raw_residential - 40.0);
            let second_ema = first_ema + 0.2 * (raw_residential - first_ema);
            assert_eq!(unstaffed[&ZoneKind::Residential], first_ema);
            assert_eq!(pending_hire[&ZoneKind::Residential], second_ema);
            assert!(unstaffed[&ZoneKind::Office] > 0.0);
            assert!(
                (pending_hire[&ZoneKind::Office] - 0.8 * unstaffed[&ZoneKind::Office]).abs()
                    < 0.00001
            );
            assert!(pending_industrial.shortfall < unstaffed_industrial.shortfall);
            assert!(pending_industrial.unemployment < unstaffed_industrial.unemployment);
            let raw_industrial = pending_industrial.base
                + pending_industrial.shortfall
                + pending_industrial.unemployment;
            let prior_industrial = unstaffed[&ZoneKind::Industrial];
            assert_eq!(
                pending_hire[&ZoneKind::Industrial],
                prior_industrial + DEMAND_EMA_ALPHA * (raw_industrial - prior_industrial)
            );
            assert!(staffed[&ZoneKind::Commercial] < pending_hire[&ZoneKind::Commercial]);
            vec![initial, unstaffed, pending_hire, staffed]
        }

        assert_eq!(trajectory(), trajectory());
    }

    #[test]
    fn e1_fix2_commercial_demand_uses_the_largest_sku_shortfall() {
        for (groceries_supply, retail_supply) in [(30.0, 0.0), (0.0, 30.0)] {
            let rates = super::CompanyRates {
                groceries_supply,
                retail_supply,
                ..Default::default()
            };
            assert_eq!(rates.retail_shortfall(100, [0, 0]), 100.0);
        }
        let rates = super::CompanyRates {
            groceries_supply: 30.0,
            retail_supply: 30.0,
            ..Default::default()
        };
        assert!(rates.retail_shortfall(100, [0, 0]) < 0.0);
        assert!(rates.retail_shortfall(100, [0, 1]) > 0.0);
    }
}
