pub use crate::economy::balance::DEFAULT_TAX_RATE_PERMILLE;
use crate::economy::Government;
use crate::map::{BuildingKind, Map, ZoneKind};
use crate::souls::residence::Residences;
use crate::{utils::resources::Resources, World, INITIAL_TICK};
use prototypes::{try_prototype, GrowableCompanyID, Money, Tick, HOURS_PER_DAY, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_TAX_RATE_PERMILLE: u16 = 290;
pub const TICKS_PER_BUDGET_DAY: u64 = TICKS_PER_HOUR * HOURS_PER_DAY as u64;

const TAX_BASE_PER_LEVEL_DAY: Money = Money::new_bucks(200);
const ROAD_UPKEEP_PER_LANE_KM_DAY: f64 = 5.0;
const SPECIAL_BUILDING_UPKEEP_DIVISOR: i128 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BudgetCategory {
    Construction,
    TradeImport,
    TradeExport,
    Tax(ZoneKind),
    RoadUpkeep,
    SpecialBuildingUpkeep,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Budget {
    // Offsets changes to the game clock so accrual follows actual simulation ticks.
    clock_offset: i128,
    last_accrued_hour: u64,
    // Cumulative turnover can exceed the range of the treasury balance.
    days: BTreeMap<u64, BTreeMap<BudgetCategory, i128>>,
    tax_rates: BTreeMap<ZoneKind, u16>,
    // Keep fractions of an inner unit across hours so daily proration does not lose money.
    hourly_remainders: BTreeMap<BudgetCategory, i128>,
}

impl Budget {
    fn record(&mut self, day: u64, category: BudgetCategory, delta: Money) {
        if delta != Money::ZERO {
            *self
                .days
                .entry(day)
                .or_default()
                .entry(category)
                .or_default() += i128::from(delta.inner());
        }
    }

    /// Nonempty elapsed-day buckets, in the same inner units as `Money`.
    pub fn days(&self) -> &BTreeMap<u64, BTreeMap<BudgetCategory, i128>> {
        &self.days
    }

    /// Cumulative amount in the same inner units as `Money`.
    pub fn total(&self, category: BudgetCategory) -> i128 {
        self.days
            .values()
            .filter_map(|day| day.get(&category))
            .sum()
    }

    pub fn tax_rate(&self, zone: ZoneKind) -> u16 {
        self.tax_rates
            .get(&zone)
            .copied()
            .unwrap_or(DEFAULT_TAX_RATE_PERMILLE)
    }

    pub fn set_tax_rate(&mut self, zone: ZoneKind, rate_permille: u16) {
        if rate_permille <= MAX_TAX_RATE_PERMILLE {
            self.tax_rates.insert(zone, rate_permille);
        }
    }

    pub(crate) fn shift_clock(&mut self, previous: Tick, next: Tick) {
        self.clock_offset += i128::from(previous.0) - i128::from(next.0);
    }

    fn elapsed_ticks(&self, tick: Tick) -> u64 {
        let elapsed = i128::from(tick.0) - i128::from(INITIAL_TICK) + self.clock_offset;
        elapsed.clamp(0, i128::from(u64::MAX)) as u64
    }

    /// Entry point for treasury transactions. Posts the signed change to both the
    /// treasury and the current day's budget ledger, with income positive.
    pub(crate) fn apply(resources: &Resources, category: BudgetCategory, delta: Money) {
        if delta != Money::ZERO {
            let tick = resources.tick();
            let mut government = resources.write::<Government>();
            let mut budget = resources.write::<Self>();
            let day = budget_day(budget.elapsed_ticks(tick));
            budget.post(&mut government, day, category, delta);
        }
    }

    fn post(
        &mut self,
        government: &mut Government,
        day: u64,
        category: BudgetCategory,
        delta: Money,
    ) {
        let opening = government.money.inner();
        let closing = opening.saturating_add(delta.inner());
        government.money = Money::new_inner(closing);
        self.record(day, category, Money::new_inner(closing - opening));
    }

    fn accrue_hour(
        &mut self,
        government: &mut Government,
        day: u64,
        category: BudgetCategory,
        daily_numerator: i128,
        daily_divisor: i128,
    ) {
        let remainder = self.hourly_remainders.entry(category).or_default();
        let hourly_divisor = daily_divisor * i128::from(HOURS_PER_DAY);
        let numerator = daily_numerator + *remainder;
        *remainder = numerator % hourly_divisor;
        let delta = (numerator / hourly_divisor).clamp(i128::from(i64::MIN), i128::from(i64::MAX));
        self.post(government, day, category, Money::new_inner(delta as i64));
    }
}

fn budget_day(tick: u64) -> u64 {
    // Entries at the end of a day close that day's ledger.
    tick.saturating_sub(1) / TICKS_PER_BUDGET_DAY
}

pub fn budget_update(world: &mut World, resources: &mut Resources) {
    let hour = {
        let budget = resources.read::<Budget>();
        let hour = budget.elapsed_ticks(resources.tick()) / TICKS_PER_HOUR;
        if hour <= budget.last_accrued_hour {
            return;
        }
        hour
    };

    profiling::scope!("economy::budget_update");
    let map = resources.read::<Map>();
    let residences = resources.read::<Residences>();
    let mut taxable_levels = BTreeMap::<ZoneKind, i128>::new();
    for human in world.humans.values().filter(|human| human.work.is_some()) {
        let house = human.home.house;
        if map
            .get(house)
            .is_some_and(|building| building.kind == BuildingKind::House)
        {
            let level = residences
                .by_building
                .get(&house)
                .map_or(1, |home| home.level);
            *taxable_levels.entry(ZoneKind::Residential).or_default() += i128::from(level);
        }
    }
    for company in world.companies.values() {
        let id = GrowableCompanyID::from(company.comp.proto);
        if let Some(proto) = try_prototype(id).filter(|proto| proto.growable) {
            *taxable_levels.entry(proto.zone_kind).or_default() +=
                company.workers.0.len() as i128 * i128::from(proto.level);
        }
    }

    let lane_metres: f64 = map
        .roads()
        .values()
        .map(|road| f64::from(road.length()) * road.n_lanes() as f64)
        .sum();
    let road_upkeep = Money::from_float_bucks(lane_metres * ROAD_UPKEEP_PER_LANE_KM_DAY / 1000.0);
    let special_building_value: i128 = map
        .buildings()
        .values()
        .filter(|building| match building.kind {
            BuildingKind::GoodsCompany(id) => {
                !try_prototype(GrowableCompanyID::from(id)).is_some_and(|proto| proto.growable)
            }
            BuildingKind::RailFreightStation(_) | BuildingKind::TrainStation => true,
            BuildingKind::House | BuildingKind::ExternalTrading => false,
        })
        .filter_map(|building| Government::special_building_price(building.kind))
        .map(|price| i128::from(price.inner()))
        .sum();

    let mut government = resources.write::<Government>();
    let mut budget = resources.write::<Budget>();
    let day = budget_day(hour * TICKS_PER_HOUR);
    for (zone, levels) in taxable_levels {
        let rate = i128::from(budget.tax_rate(zone));
        budget.accrue_hour(
            &mut government,
            day,
            BudgetCategory::Tax(zone),
            levels * rate * i128::from(TAX_BASE_PER_LEVEL_DAY.inner()),
            1000,
        );
    }
    budget.accrue_hour(
        &mut government,
        day,
        BudgetCategory::RoadUpkeep,
        -i128::from(road_upkeep.inner()),
        1,
    );
    budget.accrue_hour(
        &mut government,
        day,
        BudgetCategory::SpecialBuildingUpkeep,
        -special_building_value,
        SPECIAL_BUILDING_UPKEEP_DIVISOR,
    );
    budget.last_accrued_hour = hour;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::Density;
    use crate::souls::desire::{Work, WorkKind};
    use crate::souls::goods_company::company_soul;
    use crate::souls::growth::on_growable_placed;
    use crate::souls::human::spawn_human;
    use crate::souls::residence::Residence;
    use crate::tests::TestCtx;
    use crate::utils::scheduler::{RunnableFn, SeqSchedule};
    use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
    use crate::{Simulation, SoulID};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{
        BuildingGen, FreightStationPrototypeID, GameTime, GoodsCompanyID, ResidencePrototypeID,
    };

    fn advance_budget_ticks(sim: &mut Simulation, ticks: u64) {
        let mut schedule = SeqSchedule::default();
        schedule.add_system(Box::new(RunnableFn {
            name: "budget_update",
            f: |sim: &mut Simulation| {
                let (world, resources) = sim.world_res();
                budget_update(world, resources);
            },
        }));
        for _ in 0..ticks {
            sim.tick(&mut schedule, &[]);
        }
    }

    #[test]
    fn hourly_budget_ignores_clock_jumps() {
        let mut sim = TestCtx::new_fast().g;
        let category = BudgetCategory::SpecialBuildingUpkeep;
        let warp = |sim: &mut Simulation, tick| {
            sim.apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(tick)))]);
        };

        warp(&mut sim, 0);
        advance_budget_ticks(&mut sim, TICKS_PER_HOUR);
        let first_hour = sim.read::<Budget>().total(category);
        assert!(first_hour < 0);
        let money_after_first_hour = sim.read::<Government>().money;

        warp(&mut sim, TICKS_PER_HOUR - 1);
        advance_budget_ticks(&mut sim, 1);
        assert_eq!(sim.read::<Budget>().total(category), first_hour);
        assert_eq!(sim.read::<Government>().money, money_after_first_hour);

        warp(&mut sim, 20 * TICKS_PER_BUDGET_DAY - 1);
        advance_budget_ticks(&mut sim, 1);
        assert_eq!(sim.read::<Budget>().total(category), first_hour);
        assert_eq!(sim.read::<Government>().money, money_after_first_hour);

        advance_budget_ticks(&mut sim, TICKS_PER_HOUR - 3);
        assert_eq!(sim.read::<Budget>().total(category), first_hour);
        advance_budget_ticks(&mut sim, 1);
        let expected = -i128::from(Money::new_bucks(5).inner()) * 2 / 24;
        let budget = sim.read::<Budget>();
        assert_eq!(budget.total(category), expected);
        assert_eq!(budget.days().len(), 1);
        let ledger_sum: i128 = budget.days()[&0].values().sum();
        assert_eq!(
            i128::from(sim.read::<Government>().money.inner()),
            i128::from(Government::default().money.inner()) + ledger_sum
        );
    }

    #[test]
    fn hourly_budget_survives_mid_hour_save_across_day_boundary() {
        let mut continuous = TestCtx::new_fast().g;
        let category = BudgetCategory::SpecialBuildingUpkeep;
        continuous.apply_commands([&WorldCommand::SetGameTime(GameTime::new(Tick(123)))]);
        advance_budget_ticks(&mut continuous, TICKS_PER_HOUR + 456);
        assert_ne!(continuous.read::<Budget>().hourly_remainders[&category], 0);
        let encoded = Bincode::encode(&continuous).unwrap();
        let mut resumed: Simulation = Bincode::decode(&encoded).unwrap();

        let remaining = TICKS_PER_BUDGET_DAY - TICKS_PER_HOUR - 456;
        for sim in [&mut continuous, &mut resumed] {
            advance_budget_ticks(sim, remaining - 1);
            let expected = -i128::from(Money::new_bucks(5).inner()) * 23 / 24;
            assert_eq!(sim.read::<Budget>().total(category), expected);
            advance_budget_ticks(sim, TICKS_PER_HOUR + 1);
            assert_eq!(
                sim.read::<Budget>().days()[&0][&category],
                -i128::from(Money::new_bucks(5).inner())
            );
            assert!(sim.read::<Budget>().days()[&1][&category] < 0);
        }
        assert_eq!(
            continuous.read::<Budget>().days(),
            resumed.read::<Budget>().days()
        );
        assert_eq!(
            continuous.read::<Government>().money,
            resumed.read::<Government>().money
        );
    }

    #[test]
    fn p1d_set_tax_rate_bounds() {
        let mut ctx = TestCtx::new_fast();
        let opening = ctx.g.read::<Government>().money;
        for zone in [
            ZoneKind::Residential,
            ZoneKind::Commercial,
            ZoneKind::Industrial,
            ZoneKind::Office,
        ] {
            assert_eq!(ctx.g.read::<Budget>().tax_rate(zone), 90);
        }
        for (rate_permille, expected) in [
            (0, None),
            (290, None),
            (291, Some(Rejection::InvalidTaxRate)),
            (u16::MAX, Some(Rejection::InvalidTaxRate)),
        ] {
            let command = WorldCommand::SetTaxRate {
                zone: ZoneKind::Residential,
                rate_permille,
            };
            assert!(command.is_instant());
            ctx.g.apply_commands([&command]);
            assert_eq!(
                ctx.g.read::<CommandOutcomes>().last.as_slice(),
                expected.map(|r| ("SetTaxRate", r)).as_slice(),
            );
            assert_eq!(
                ctx.g.read::<Budget>().tax_rate(ZoneKind::Residential),
                rate_permille.min(290)
            );
            assert_eq!(ctx.g.read::<Budget>().tax_rate(ZoneKind::Commercial), 90);
            assert_eq!(ctx.g.read::<Government>().money, opening);
        }
    }

    #[test]
    fn p1d_budget_and_tax_rates() {
        let mut ctx = TestCtx::new_fast();
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
        let lane_metres: f64 = ctx
            .g
            .map()
            .roads()
            .values()
            .map(|road| f64::from(road.length()) * road.n_lanes() as f64)
            .sum();
        let house = ctx.build_house_near(vec2(150.0, 125.0));
        ctx.g.write::<Residences>().by_building.insert(
            house,
            Residence {
                prototype: ResidencePrototypeID::new("residence-low-l2"),
                density: Density::Low,
                level: 2,
                capacity: 4,
            },
        );
        let opening = ctx.g.read::<Government>().money;
        let site = vec2(150.0, 175.0);
        let kind =
            BuildingKind::RailFreightStation(FreightStationPrototypeID::new("freight-station"));
        ctx.g
            .apply_commands([&WorldCommand::MapBuildSpecialBuilding {
                pos: OBB::new(site, Vec2::X, 10.0, 10.0),
                kind,
                gen: BuildingGen::NoWalkway {
                    door_pos: Vec2::ZERO,
                },
                zone: None,
                connected_road: None,
            }]);
        let workplace = ctx
            .g
            .map()
            .buildings()
            .values()
            .find(|building| building.kind == kind && building.obb.center() == site)
            .unwrap()
            .id;
        let employed = spawn_human(&mut ctx.g, house).unwrap();
        spawn_human(&mut ctx.g, house).unwrap();
        ctx.g.world_mut_unchecked().humans[employed].work =
            Some(Work::new(workplace, WorkKind::Worker, 0.0));

        let (world, resources) = ctx.g.world_res();
        let post_construction = opening + Money::new_bucks(-1000);
        assert_eq!(resources.read::<Government>().money, post_construction);
        *resources.write::<GameTime>() = GameTime::new(Tick(INITIAL_TICK + TICKS_PER_HOUR - 1));
        budget_update(world, resources);
        assert_eq!(resources.read::<Government>().money, post_construction);

        *resources.write::<GameTime>() = GameTime::new(Tick(INITIAL_TICK + TICKS_PER_HOUR));
        budget_update(world, resources);
        let tax = Money::new_bucks(36).inner();
        let road_upkeep = Money::from_float_bucks(lane_metres * 5.0 / 1000.0).inner();
        // Both freight stations cost $1,000 each.
        let station_upkeep = Money::new_bucks(10).inner();
        assert_eq!(
            resources.read::<Government>().money,
            post_construction + Money::new_inner(tax / 24 - road_upkeep / 24 - station_upkeep / 24),
        );
        for hour in 2..=24 {
            *resources.write::<GameTime>() =
                GameTime::new(Tick(INITIAL_TICK + hour * TICKS_PER_HOUR));
            budget_update(world, resources);
        }
        assert_eq!(
            resources.read::<Government>().money,
            post_construction + Money::new_inner(tax - road_upkeep - station_upkeep),
        );

        let day_zero = resources.read::<Budget>();
        assert_eq!(day_zero.days().len(), 1);
        assert_eq!(
            day_zero.days()[&0][&BudgetCategory::Tax(ZoneKind::Residential)],
            i128::from(tax)
        );
        assert_eq!(
            day_zero.days()[&0][&BudgetCategory::RoadUpkeep],
            -i128::from(road_upkeep)
        );
        assert_eq!(
            day_zero.days()[&0][&BudgetCategory::SpecialBuildingUpkeep],
            -i128::from(station_upkeep)
        );
        drop(day_zero);

        *resources.write::<GameTime>() =
            GameTime::new(Tick(INITIAL_TICK + TICKS_PER_BUDGET_DAY + 1));
        Budget::apply(
            resources,
            BudgetCategory::TradeImport,
            Money::new_cents(-70),
        );
        Budget::apply(
            resources,
            BudgetCategory::TradeExport,
            Money::new_cents(150),
        );
        let budget = resources.read::<Budget>();
        assert_eq!(budget.days().len(), 2);
        assert_eq!(budget.days()[&1][&BudgetCategory::TradeImport], -7_000);
        assert_eq!(budget.days()[&1][&BudgetCategory::TradeExport], 15_000);
        let ledger_sum: i128 = budget.days().values().flat_map(|day| day.values()).sum();
        assert_eq!(
            i128::from(Government::default().money.inner()) + ledger_sum,
            i128::from(resources.read::<Government>().money.inner()),
        );
    }

    #[test]
    fn p1d_growable_company_tax_by_zone() {
        let mut ctx = TestCtx::new_fast();
        ctx.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
        let house = ctx.build_house_near(vec2(150.0, 125.0));
        ctx.g
            .write::<Residences>()
            .by_building
            .get_mut(&house)
            .unwrap()
            .capacity = 3;
        for (name, zone, rate_permille) in [
            ("commercial-l1", ZoneKind::Commercial, 120),
            ("industrial-l1", ZoneKind::Industrial, 240),
            ("office-l1", ZoneKind::Office, 180),
        ] {
            let proto = GoodsCompanyID::new(name);
            let lot = ctx.g.map().lots().keys().next().unwrap();
            let kind = BuildingKind::GoodsCompany(proto);
            let building = ctx.g.map_mut().place_growable(lot, kind).unwrap();
            on_growable_placed(&ctx.g, building, kind);
            let Some(SoulID::GoodsCompany(company)) = company_soul(&mut ctx.g, building, proto)
            else {
                panic!("company soul missing for {name}");
            };
            ctx.g.apply_commands([&WorldCommand::SetTaxRate {
                zone,
                rate_permille,
            }]);
            let worker = spawn_human(&mut ctx.g, house).unwrap();
            let world = ctx.g.world_mut_unchecked();
            world.humans[worker].work = Some(Work::new(building, WorkKind::Worker, 0.0));
            world.companies[company].workers.0.push(worker);
        }

        let (world, resources) = ctx.g.world_res();
        *resources.write::<GameTime>() = GameTime::new(Tick(INITIAL_TICK + TICKS_PER_HOUR));
        budget_update(world, resources);
        let budget = resources.read::<Budget>();
        for (zone, hourly_cents) in [
            (ZoneKind::Commercial, 100),
            (ZoneKind::Industrial, 200),
            (ZoneKind::Office, 150),
        ] {
            assert_eq!(
                budget.total(BudgetCategory::Tax(zone)),
                i128::from(Money::new_cents(hourly_cents).inner())
            );
        }
    }
}
