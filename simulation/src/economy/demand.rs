use crate::economy::balance::{
    periodic_tick, shelf_delivery_item, shelf_delivery_units, DEFAULT_TAX_RATE_PERMILLE,
    DEMAND_EMA_ALPHA, DEMAND_INTERVAL_TICKS, DEMAND_OFFSET_TICKS, FOOD_UNIT_TICKS,
    GOODS_UNIT_TICKS, I_BASE, I_BOOTSTRAP_COMPANIES, K_GOODS, K_HAPPY, K_JOBS, K_RET, K_TAX,
    K_UNEMP, K_VAC, MIN_DEMAND_POPULATION, NEUTRAL_HAPPINESS, R_BASE, R_BOOTSTRAP_POPULATION,
    SHELF_TARGET_UTILIZATION,
};
use crate::economy::{Budget, Market, WalkingComponents};
use crate::map::{BuildingID, LaneID, Map, ZoneKind};
use crate::map_dynamic::{active_business, BuildingInfos};
use crate::souls::happiness::Happiness;
use crate::souls::household::BASKET_TRAVEL_LIMIT;
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::{SoulID, World};
use common::FastMap;
use geom::Vec3;
use prototypes::{
    try_prototype, GrowableCompanyID, ItemID, RecipeItem, HOURS_PER_DAY, TICKS_PER_HOUR,
    TICKS_PER_REALTIME_SECOND,
};
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixedUseDemand {
    pub residential: f32,
    pub commercial: f32,
    pub limiting_zone: ZoneKind,
}

impl MixedUseDemand {
    pub const GROWTH_THRESHOLD: f32 = 5.0;

    pub fn score(self) -> f32 {
        self.residential.min(self.commercial)
    }

    pub fn may_grow(self) -> bool {
        self.residential.is_finite()
            && self.commercial.is_finite()
            && self.score() > Self::GROWTH_THRESHOLD
    }
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

    pub fn mixed_use(&self) -> Option<MixedUseDemand> {
        let residential = *self.values.get(&ZoneKind::Residential)?;
        let commercial = *self.values.get(&ZoneKind::Commercial)?;
        Some(MixedUseDemand {
            residential,
            commercial,
            limiting_zone: if residential <= commercial {
                ZoneKind::Residential
            } else {
                ZoneKind::Commercial
            },
        })
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
    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let mut walking = resources.write::<WalkingComponents>();
    let rates = company_rates(world, &map, &infos, &market, &mut walking);

    let residences = resources.read::<Residences>();
    let mut demand = resources.write::<ZoneDemand>();
    let occupants = &mut demand.occupants;
    occupants.clear();
    let mut unemployed = 0;
    let mut unmet = [0_u64; 2];
    let mut nearest_store = FastMap::default();
    let mut uncovered = 0;
    let now = budget.elapsed_ticks(resources.tick());
    for human in world.humans.values() {
        *occupants.entry(human.home.house).or_insert(0u32) += 1;
        let distance2 = *nearest_store.entry(human.home.house).or_insert_with(|| {
            map.get(human.home.house)
                .and_then(|home| {
                    let component = walking.component_at(&map, home.door_pos)?;
                    rates.shop_doors.get(&component).map(|doors| {
                        doors
                            .iter()
                            .map(|door| door.distance2(home.door_pos))
                            .fold(f32::INFINITY, f32::min)
                    })
                })
                .unwrap_or(f32::INFINITY)
        });
        let radius = human.pedestrian.walking_speed
            * (BASKET_TRAVEL_LIMIT / TICKS_PER_REALTIME_SECOND - 60) as f32;
        uncovered += usize::from(distance2 > radius.powi(2));
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
        .filter(|&(building, _)| {
            !residences.is_abandoned(*building)
                && map
                    .get(*building)
                    .is_some_and(|site| site.has_residential_use() && !site.is_abandoned_growable())
        })
        .map(|(building, residence)| {
            let occupied = occupants.get(building).copied().unwrap_or(0);
            u64::from(residence.capacity.saturating_sub(occupied))
        })
        .sum();

    let population = world.humans.len();
    let per_capita = 100.0 / population.max(MIN_DEMAND_POPULATION) as f32;
    let unemployed_norm = unemployed as f32 * per_capita;
    // Keep a retail signal for small disconnected neighbourhoods as well as large
    // gaps. The floor clears the commercial placement threshold after smoothing.
    let coverage_shortfall = if uncovered > 0 {
        (uncovered as f32 * per_capita).max(10.0)
    } else {
        -100.0
    };
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
    let happiness = K_HAPPY * (resources.read::<Happiness>().average(&infos) - NEUTRAL_HAPPINESS);

    for (zone, mut drivers) in [
        (
            ZoneKind::Residential,
            DemandDrivers {
                base: r_base,
                happiness,
                jobs: K_JOBS * (rates.open_jobs as f32 - unemployed as f32) * per_capita,
                vacancies: -K_VAC * vacancies as f32 * per_capita,
                ..Default::default()
            },
        ),
        (
            ZoneKind::Commercial,
            DemandDrivers {
                shortfall: K_RET
                    * rates
                        .retail_shortfall(population, unmet)
                        .max(coverage_shortfall),
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
    if let Some(mixed) = demand.mixed_use() {
        demand.values.insert(ZoneKind::MixedUse, mixed.score());
    }
}

fn shortfall_norm(need: f64, supply: f64) -> f32 {
    ((need - supply) / need.max(1.0) * 100.0).clamp(-100.0, 100.0) as f32
}

#[derive(Default)]
struct CompanyRates {
    open_jobs: u64,
    goods_need: f64,
    goods_supply: f64,
    retail_supply: f64,
    groceries_supply: f64,
    shelf_packages_supply: f64,
    industrial_growables: usize,
    shop_doors: BTreeMap<LaneID, Vec<Vec3>>,
}

impl CompanyRates {
    fn household_need(population: usize) -> [f64; 2] {
        [FOOD_UNIT_TICKS, GOODS_UNIT_TICKS]
            .map(|cadence| population as f64 * TICKS_PER_HOUR as f64 / cadence as f64)
    }

    fn retail_shortfall(&self, population: usize, unmet: [u64; 2]) -> f32 {
        let [staples_need, retail_need] = Self::household_need(population);
        let [staples_per_package, retail_per_package] = shelf_delivery_units(1).map(f64::from);
        // Count live unmet lines by SKU. They can reveal empty or unevenly
        // distributed shelves even when nominal staffed production is high.
        let shortage = |hourly_need, supply, missing| {
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
        let staples = shortage(
            staples_need,
            self.groceries_supply + self.shelf_packages_supply * staples_per_package,
            unmet[0],
        );
        let goods = shortage(
            retail_need,
            self.retail_supply + self.shelf_packages_supply * retail_per_package,
            unmet[1],
        );
        let shortage = staples.max(goods);
        // Each package can replenish either shelf. Account for their combined
        // need after crediting direct output against each SKU.
        let package_need = (staples_need - self.groceries_supply).max(0.0) / staples_per_package
            + (retail_need - self.retail_supply).max(0.0) / retail_per_package;
        if package_need == 0.0 {
            return shortage;
        }
        let package_equivalent =
            staples_need / staples_per_package + retail_need / retail_per_package;
        shortage.max(shortfall_norm(
            package_equivalent,
            package_equivalent - package_need + self.shelf_packages_supply,
        ))
    }
}

fn company_rates(
    world: &World,
    map: &Map,
    infos: &BuildingInfos,
    market: &Market,
    walking: &mut WalkingComponents,
) -> CompanyRates {
    let goods = ItemID::new("goods");
    let [groceries, retail_goods] = crate::souls::household::household_items();
    let mut rates = CompanyRates::default();
    let [staples_per_package, _] = shelf_delivery_units(1).map(f64::from);
    let mut wholesale_inputs = 0.0;
    let mut wholesale_output = 0.0;
    let mut other_packages = 0.0;
    let mut other_retail = 0.0;
    let mut other_groceries = 0.0;
    let amount = |items: &[RecipeItem], kind| {
        items
            .iter()
            .filter(|item| item.id == kind)
            .map(|item| f64::from(item.amount))
            .sum::<f64>()
    };

    let jobs = market.inner().get(&ItemID::new("job-opening"));
    for (id, company) in world.companies.iter() {
        let Some(building) = map.get(company.comp.building) else {
            continue;
        };
        let Some(business) =
            active_business(building, infos, world).filter(|business| business.company == id)
        else {
            continue;
        };
        rates.open_jobs += jobs
            .and_then(|jobs| jobs.sell_order(SoulID::GoodsCompany(id)))
            .map_or(0, |order| u64::from(order.qty));
        let proto = business.prototype;
        let zone = building
            .development()
            .map(|site| site.zone.kind)
            .or_else(|| {
                try_prototype(GrowableCompanyID::from(proto.id))
                    .filter(|proto| proto.growable)
                    .map(|proto| proto.zone_kind)
            });
        if zone == Some(ZoneKind::Industrial) {
            rates.industrial_growables += 1;
        }
        let Some(recipe) = &proto.recipe else {
            continue;
        };
        let business_rates = business.demand_rates(building);
        let Some(batch_ticks) = business_rates
            .full_speed_batch_ticks
            .filter(|ticks| *ticks > 0.0)
        else {
            continue;
        };
        let staffed_batches = TICKS_PER_HOUR as f64 / batch_ticks * business_rates.staffed_fraction;
        let packages = amount(&recipe.production, shelf_delivery_item());
        let staples = amount(&recipe.production, groceries);
        let retail = amount(&recipe.production, retail_goods);
        let shelf_utilization = if packages > 0.0 {
            SHELF_TARGET_UTILIZATION
        } else {
            1.0
        };
        let package_rate = packages * staffed_batches * shelf_utilization;
        let grocery_rate = staples * staffed_batches;
        let retail_rate = retail * staffed_batches * shelf_utilization;
        rates.shelf_packages_supply += package_rate;
        rates.groceries_supply += grocery_rate;
        rates.retail_supply += retail_rate;
        if packages > 0.0 || staples > 0.0 || retail > 0.0 {
            if let Some(component) = walking.component_at(map, building.door_pos) {
                if walking.reaches_building(map, component, building) {
                    rates
                        .shop_doors
                        .entry(component)
                        .or_default()
                        .push(building.door_pos);
                }
            }
        }

        let wholesale = amount(&recipe.consumption, goods);
        match zone {
            Some(ZoneKind::Commercial | ZoneKind::MixedUse) if wholesale > 0.0 => {
                if packages > 0.0 || staples > 0.0 || retail > 0.0 {
                    wholesale_inputs += wholesale * staffed_batches * shelf_utilization;
                    wholesale_output +=
                        package_rate + grocery_rate / staples_per_package + retail_rate;
                    // A new shop may need its first delivery before it has workers or sales.
                    let seller = SoulID::GoodsCompany(id);
                    let has_opened = [groceries, retail_goods].into_iter().any(|item| {
                        market
                            .inner()
                            .get(&item)
                            .is_some_and(|m| m.sell_order(seller).is_some())
                    });
                    if !has_opened {
                        rates.goods_need += wholesale / f64::from(HOURS_PER_DAY);
                    }
                } else {
                    rates.goods_need += wholesale * staffed_batches;
                }
            }
            zone => {
                other_packages += package_rate;
                other_groceries += grocery_rate;
                other_retail += retail_rate;
                if zone == Some(ZoneKind::Industrial) {
                    rates.goods_supply += amount(&recipe.production, goods) * staffed_batches;
                }
            }
        }
    }
    // Estimate sell-through in package equivalents, crediting each unit of
    // direct output against its own SKU before allocating shared packages.
    if wholesale_output > 0.0 {
        let [groceries, retail] = CompanyRates::household_need(world.humans.len());
        let remaining = (groceries - other_groceries).max(0.0) / staples_per_package
            + (retail - other_retail).max(0.0);
        let utilization = ((remaining - other_packages).max(0.0) / wholesale_output).min(1.0);
        rates.goods_need += wholesale_inputs * utilization;
    }
    rates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::market_update;
    use crate::map::{BuildingID, BuildingKind, ProjectFilter, ProjectKind, RoadID};
    use crate::souls::goods_company::GoodsCompanyState;
    use crate::souls::human::spawn_human;
    use crate::souls::residence::Residence;
    use crate::tests::TestCtx;
    use crate::world::{CompanyEnt, CompanyID};
    use crate::Simulation;
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Transform, Vec2, Vec3, OBB};
    use prototypes::{Density, GameTime, GoodsCompanyID, ResidencePrototypeID, Tick};

    #[test]
    fn z3_accounting_mixed_demand_uses_limiting_smoothed_source() {
        let mut ctx = TestCtx::new_fast();
        for (residential, commercial, expected) in [
            (25.0, 17.0, 17.0),
            (5.0, 20.0, 5.0),
            (20.0, 3.0, 3.0),
            (20.0, -9.0, -9.0),
        ] {
            let initial_for =
                |raw, value| (value - DEMAND_EMA_ALPHA * raw) / (1.0 - DEMAND_EMA_ALPHA);
            let mut demand = ctx.g.write::<ZoneDemand>();
            demand
                .values
                .insert(ZoneKind::Residential, initial_for(R_BASE, residential));
            demand
                .values
                .insert(ZoneKind::Commercial, initial_for(0.0, commercial));
            drop(demand);
            let values = sample(&mut ctx.g, 1);
            assert!((values[&ZoneKind::MixedUse] - expected).abs() < 0.001);
            let demand = ctx.g.read::<ZoneDemand>();
            let mixed = demand.mixed_use().unwrap();
            assert_eq!(
                mixed.limiting_zone,
                if residential < commercial {
                    ZoneKind::Residential
                } else {
                    ZoneKind::Commercial
                }
            );
            assert_eq!(
                mixed.may_grow(),
                expected > MixedUseDemand::GROWTH_THRESHOLD
            );
        }
    }

    #[test]
    fn z3_accounting_mixed_shop_counts_one_live_recipe_and_job_order() {
        use crate::map::GrowableBusinessState;
        use crate::map_dynamic::accounting_site;
        use crate::HumanID;

        let (mut ctx, id, company) = accounting_site(true);
        let entity = &mut ctx.g.world.companies[company];
        entity.workers.0 = vec![HumanID::default(); 6];
        let recipe = entity.comp.proto.prototype().recipe.as_ref().unwrap();
        let full_batches = TICKS_PER_HOUR as f64 / recipe.batch_ticks();
        let rates = |ctx: &TestCtx| {
            company_rates(
                ctx.g.world(),
                &ctx.g.map(),
                &ctx.g.read::<BuildingInfos>(),
                &ctx.g.read::<Market>(),
                &mut WalkingComponents::default(),
            )
        };
        let open = rates(&ctx);
        assert_eq!(open.open_jobs, 12);
        assert_eq!(open.goods_need, 2.0 / f64::from(HOURS_PER_DAY));
        assert_eq!(
            open.retail_supply,
            SHELF_TARGET_UTILIZATION * full_batches * 0.5
        );

        ctx.g.map_mut().set_growable_business_state(
            id,
            GrowableBusinessState::ClosedUntil {
                retry_elapsed_tick: u64::MAX,
            },
        );
        let closed = rates(&ctx);
        assert_eq!(
            (closed.open_jobs, closed.goods_need, closed.retail_supply),
            (0, 0.0, 0.0)
        );
    }

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

    fn company(ctx: &mut TestCtx, name: &str, max_workers: u32) -> CompanyID {
        let index = ctx.g.world.companies.len();
        company_at(
            ctx,
            name,
            max_workers,
            vec2(120.0 + 24.0 * index as f32, 60.0),
            None,
        )
    }

    fn company_at(
        ctx: &mut TestCtx,
        name: &str,
        max_workers: u32,
        center: Vec2,
        road: Option<RoadID>,
    ) -> CompanyID {
        let proto = GoodsCompanyID::new(name).prototype();
        let building = ctx
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(center, Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto.id),
                proto.bgen,
                None,
                road,
            )
            .unwrap();
        let company = ctx.g.world_mut_unchecked().insert(CompanyEnt {
            trans: Transform::new(Vec3::ZERO),
            comp: GoodsCompanyState {
                proto: proto.id,
                building,
                max_workers,
                progress: 0.0,
                driver: None,
                trucks: vec![],
            },
            workers: Default::default(),
            sold: Default::default(),
            bought: Default::default(),
        });
        let mut infos = ctx.g.write::<BuildingInfos>();
        infos.insert(building);
        infos.set_owner(building, SoulID::GoodsCompany(company));
        company
    }

    fn road_at(ctx: &TestCtx, pos: Vec3) -> RoadID {
        let ProjectKind::Road(road) = ctx.g.map().project(pos, 2.0, ProjectFilter::ROAD).kind
        else {
            panic!("no fixture road at {pos:?}");
        };
        road
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
        company(&mut ctx, "bakery", 5);
        company(&mut ctx, "commercial-l1", 4);
        company(&mut ctx, "office-l1", 6);
        let goods_shortfall = K_GOODS * shortfall_norm(2.0 / f64::from(HOURS_PER_DAY), 0.0);
        assert_eq!(
            sample(&mut ctx.g, 1)[&ZoneKind::Industrial],
            I_BASE + goods_shortfall
        );

        for _ in 0..I_BOOTSTRAP_COMPANIES {
            company(&mut ctx, "industrial-l1", 6);
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
            let industrial = company(&mut ctx, "industrial-l1", 6);
            let road = road_at(&ctx, vec3(220.0, 100.0, 0.0));
            let commercial =
                company_at(&mut ctx, "commercial-l1", 4, vec2(220.0, 130.0), Some(road));
            company(&mut ctx, "office-l1", 6);
            let jobs = ItemID::new("job-opening");
            let seller = SoulID::GoodsCompany(industrial);
            {
                let mut market = ctx.g.write::<Market>();
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
    fn econ_food_zoned_shops_can_supply_a_town_at_routine_utilization() {
        let _ctx = TestCtx::new_fast();
        let company = GoodsCompanyID::new("commercial-l1").prototype();
        let recipe = company.recipe.as_ref().unwrap();
        let shops = 20.0;
        let batches = shops * TICKS_PER_HOUR as f64 / recipe.batch_ticks();
        let amount = |id| {
            f64::from(
                recipe
                    .production
                    .iter()
                    .find(|item| item.id == id)
                    .unwrap()
                    .amount,
            )
        };
        let packages = amount(shelf_delivery_item()) * batches * SHELF_TARGET_UTILIZATION;
        let direct_retail =
            amount(ItemID::new("retail-goods")) * batches * SHELF_TARGET_UTILIZATION;
        let [food_per_package, goods_per_package] = shelf_delivery_units(1).map(f64::from);
        let population = 2_500.0;
        let groceries = population * TICKS_PER_HOUR as f64 / FOOD_UNIT_TICKS as f64;
        let goods = population * TICKS_PER_HOUR as f64 / GOODS_UNIT_TICKS as f64;
        let required =
            groceries / food_per_package + (goods - direct_retail).max(0.0) / goods_per_package;
        assert!(
            packages >= required,
            "{packages} wholesale packages/h < {required}"
        );
    }

    #[test]
    fn econ_food_counts_packages_as_shared_shelf_capacity() {
        // 240 residents need twenty groceries and ten retail goods
        // per hour. Ten packages cover either shelf on its own.
        let mut rates = super::CompanyRates {
            shelf_packages_supply: 10.0,
            ..Default::default()
        };
        assert!(rates.retail_shortfall(240, [0, 0]) > 0.0);
        rates.retail_supply = 10.0;
        assert!(rates.retail_shortfall(240, [0, 0]) < 0.0);
    }

    #[test]
    fn econ_food_stocked_shops_budget_wholesale_for_expected_sales() {
        let mut ctx = TestCtx::new_fast();
        let house = home(&ctx, 12);
        for _ in 0..12 {
            spawn_human(&mut ctx.g, house).unwrap();
        }
        let shop = company(&mut ctx, "commercial-l1", 12);
        let industry = company(&mut ctx, "industrial-l1", 18);
        for (id, staff) in [(shop, 12), (industry, 6)] {
            ctx.g.world.companies[id].workers.0 = vec![crate::HumanID::default(); staff];
        }
        let seller = SoulID::GoodsCompany(shop);
        let mut market = ctx.g.write::<Market>();
        for (item, capacity) in crate::souls::household::household_items()
            .into_iter()
            .zip([432, 12])
        {
            market.produce(seller, item, capacity as i32);
            market.sell(seller, Vec2::ZERO, item, capacity, capacity);
        }
        drop(market);
        sample(&mut ctx.g, 1);
        let demand = ctx.g.read::<ZoneDemand>();
        assert!(demand.drivers(ZoneKind::Industrial).unwrap().shortfall < 0.0);
    }

    #[test]
    fn econ_food_stocked_shop_across_a_walk_break_does_not_cover_homes() {
        let mut ctx = TestCtx::new_fast();
        let house = home(&ctx, 6);
        for _ in 0..6 {
            spawn_human(&mut ctx.g, house).unwrap();
        }
        ctx.build_roads(&[vec3(100.0, 260.0, 0.0), vec3(300.0, 260.0, 0.0)]);
        let road = road_at(&ctx, vec3(200.0, 260.0, 0.0));
        let shop = company_at(
            &mut ctx,
            "commercial-l1",
            12,
            vec2(200.0, 280.0),
            Some(road),
        );
        ctx.g.world.companies[shop].workers.0 = vec![crate::HumanID::default(); 12];
        let seller = SoulID::GoodsCompany(shop);
        let groceries = ItemID::new("groceries");
        let mut market = ctx.g.write::<Market>();
        market.produce(seller, groceries, 432);
        market.sell(seller, vec2(200.0, 280.0), groceries, 432, 432);
        drop(market);

        sample(&mut ctx.g, 1);
        assert!(
            ctx.g
                .read::<ZoneDemand>()
                .drivers(ZoneKind::Commercial)
                .unwrap()
                .shortfall
                > 5.0
        );
        ctx.build_roads(&[vec3(300.0, 100.0, 0.0), vec3(300.0, 260.0, 0.0)]);
        sample(&mut ctx.g, 2);
        assert!(
            ctx.g
                .read::<ZoneDemand>()
                .drivers(ZoneKind::Commercial)
                .unwrap()
                .shortfall
                < 0.0
        );
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
