use crate::economy::balance::{
    DEFAULT_TAX_RATE_PERMILLE, DEMAND_EMA_ALPHA, DEMAND_INTERVAL_TICKS, I_BASE,
    I_BOOTSTRAP_COMPANIES, K_GOODS, K_JOBS, K_RET, K_TAX, K_UNEMP, K_VAC, MIN_DEMAND_POPULATION,
    RETAIL_PER_CAPITA_PER_HOUR, R_BASE, R_BOOTSTRAP_POPULATION,
};
use crate::economy::{Budget, Market};
use crate::map::ZoneKind;
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::{SoulID, World};
use prototypes::{try_prototype, GrowableCompanyID, ItemID, RecipeItem, DELTA_F64, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Default, Serialize, Deserialize)]
pub struct ZoneDemand {
    pub values: BTreeMap<ZoneKind, f32>,
    drivers: BTreeMap<ZoneKind, DemandDrivers>,
}

/// Signed contributions to a zone's latest demand sample, before clamping and smoothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DemandDrivers {
    pub base: f32,
    /// Open job orders minus unemployed residents, weighted for residential demand.
    pub jobs: f32,
    pub vacancies: f32,
    pub tax: f32,
    pub shortfall: f32,
    pub unemployment: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemandDriver {
    Base,
    Jobs,
    Vacancies,
    Tax,
    Shortfall,
    Unemployment,
}

impl ZoneDemand {
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
        ]
        .into_iter()
        .rev()
        .filter(|&(_, value)| value != 0.0)
        .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
    }
}

pub fn demand_update(world: &mut World, resources: &mut Resources) {
    if resources.tick().0 % DEMAND_INTERVAL_TICKS != 0 {
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
    let mut occupants = BTreeMap::new();
    let mut unemployed = 0;
    for human in world.humans.values() {
        *occupants.entry(human.home.house).or_insert(0u32) += 1;
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
    let retail_need = population as f64 * RETAIL_PER_CAPITA_PER_HOUR;
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

    let budget = resources.read::<Budget>();
    let mut demand = resources.write::<ZoneDemand>();
    for (zone, mut drivers) in [
        (
            ZoneKind::Residential,
            DemandDrivers {
                base: r_base,
                jobs: K_JOBS * (open_jobs as f32 - unemployed as f32) * per_capita,
                vacancies: -K_VAC * vacancies as f32 * per_capita,
                ..Default::default()
            },
        ),
        (
            ZoneKind::Commercial,
            DemandDrivers {
                shortfall: K_RET * shortfall_norm(retail_need, rates.retail_supply),
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
    industrial_growables: usize,
}

fn company_rates(world: &World) -> CompanyRates {
    let goods = ItemID::new("goods");
    let retail_goods = ItemID::new("retail-goods");
    let mut rates = CompanyRates::default();
    let amount = |items: &[RecipeItem], kind| {
        items
            .iter()
            .filter(|item| item.id == kind)
            .map(|item| f64::from(item.amount))
            .sum::<f64>()
    };

    for company in world.companies.values() {
        let Some(proto) = try_prototype(GrowableCompanyID::from(company.comp.proto)) else {
            continue;
        };
        if proto.growable && proto.zone_kind == ZoneKind::Industrial {
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
        match proto.zone_kind {
            ZoneKind::Commercial => {
                rates.goods_need += amount(&recipe.consumption, goods) * full_batches;
                rates.retail_supply += amount(&recipe.production, retail_goods) * staffed_batches;
            }
            ZoneKind::Industrial => {
                rates.goods_supply += amount(&recipe.production, goods) * staffed_batches;
            }
            ZoneKind::Residential | ZoneKind::Office => {}
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

    fn sample(sim: &mut Simulation, tick: u64) -> BTreeMap<ZoneKind, f32> {
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
        ctx.apply(&[crate::world_command::WorldCommand::SetGameTime(
            GameTime::new(Tick(2_998)),
        )]);
        ctx.tick_fast();
        assert!(ctx.g.read::<ZoneDemand>().values.is_empty());
        ctx.tick_fast();
        let first = ctx.g.read::<ZoneDemand>().values.clone();
        assert_eq!(first.get(&ZoneKind::Residential), Some(&40.0));
        assert_eq!(first.get(&ZoneKind::Industrial), Some(&30.0));
        assert_eq!(first.get(&ZoneKind::Commercial), Some(&0.0));
        assert_eq!(first.get(&ZoneKind::Office), Some(&0.0));

        let house = home(&ctx, 1);
        assert_eq!(sample(&mut ctx.g, 5_999), first);
        let smoothed = sample(&mut ctx.g, 6_000);
        spawn_human(&mut ctx.g, house).unwrap();
        ctx.g.write::<ZoneDemand>().values.clear();
        let raw = sample(&mut ctx.g, 9_000);
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
        assert_eq!(sample(&mut ctx.g, 12_000)[&ZoneKind::Residential], -100.0);
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
            sample(&mut ctx.g, 3_000)[&ZoneKind::Industrial],
            I_BASE + goods_shortfall
        );

        let world = ctx.g.world_mut_unchecked();
        for _ in 0..I_BOOTSTRAP_COMPANIES {
            company(world, "industrial-l1", 6);
        }
        ctx.g.write::<ZoneDemand>().values.clear();
        assert_eq!(
            sample(&mut ctx.g, 6_000)[&ZoneKind::Industrial],
            goods_shortfall
        );
    }

    #[test]
    fn demand_drivers_store_signed_terms_before_smoothing() {
        use ZoneKind::{Commercial, Industrial, Office, Residential};

        let mut ctx = TestCtx::new_fast();
        assert!(ctx.g.read::<ZoneDemand>().drivers(Residential).is_none());
        sample(&mut ctx.g, 3_000);
        assert!(ctx.g.read::<ZoneDemand>().top_driver(Office).is_none());

        let house = home(&ctx, 4);
        spawn_human(&mut ctx.g, house).unwrap();
        ctx.g.write::<Budget>().set_tax_rate(Residential, 190);
        ctx.g.write::<Budget>().set_tax_rate(Office, 290);
        let values = sample(&mut ctx.g, 6_000);
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
            let initial = sample(&mut ctx.g, 3_000);
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
            let unstaffed = sample(&mut ctx.g, 6_000);
            let unstaffed_industrial = *ctx
                .g
                .read::<ZoneDemand>()
                .drivers(ZoneKind::Industrial)
                .unwrap();
            let (world, res) = ctx.g.world_res();
            market_update(world, res);
            assert!(world.get(human).unwrap().work.is_none());
            let pending_hire = sample(&mut ctx.g, 9_000);
            let pending_industrial = *ctx
                .g
                .read::<ZoneDemand>()
                .drivers(ZoneKind::Industrial)
                .unwrap();
            let (world, _) = ctx.g.world_res();
            let hired = world.companies[industrial].workers.0.pop().unwrap();
            world.companies[commercial].workers.0.push(hired);
            let staffed = sample(&mut ctx.g, 12_000);

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
}
