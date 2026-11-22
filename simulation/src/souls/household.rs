use std::collections::BTreeMap;

use prototypes::{ItemID, Money, TICKS_PER_HOUR, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};

use crate::economy::balance::{
    BASKET_FOOD_TARGET, BASKET_GOODS_TARGET, FOOD_UNIT_TICKS, GOODS_UNIT_TICKS,
};
use crate::economy::{
    BasketRequest, BasketReservation, DailyEconomyMetrics, EcoStats, Market, Trade, TradeTarget,
    TICKS_PER_BUDGET_DAY,
};
use crate::map::BuildingID;
use crate::map_dynamic::transit::elapsed_ticks;
use crate::transportation::Location;
use crate::utils::resources::Resources;
use crate::{HumanEnt, HumanID, SoulID, World};

const NEEDS_INTERVAL: u64 = 5 * TICKS_PER_MINUTE;
const SHOP_RETRY: u64 = 2 * TICKS_PER_HOUR;
const SHOP_INTERVAL: u64 = 20 * TICKS_PER_HOUR;

#[derive(Default, Serialize, Deserialize)]
pub struct HouseholdStock {
    pub on_hand: BTreeMap<ItemID, u32>,
    pub carried: BTreeMap<ItemID, u32>,
    pub needs: Needs,
    pub basket: Option<BasketId>,
    pub last_shop: Option<u64>,
    pub(crate) reservation: Option<Box<BasketReservation>>,
    pub(crate) next_shop: u64,
    pub(crate) next_leisure: u64,
    pub(crate) leisure_visit: Option<BuildingID>,
    pub(crate) leisure_started: u64,
    pub(crate) leisure_until: Option<u64>,
    pub(crate) attended_day: Option<i32>,
    last_update: Option<u64>,
    next_food: u64,
    next_goods: u64,
    phase: u64,
}

#[derive(Serialize, Deserialize)]
pub struct Needs {
    pub food: f32,
    pub goods: f32,
    pub leisure: f32,
    pub rest: f32,
}

impl Default for Needs {
    fn default() -> Self {
        Self {
            food: 0.0,
            goods: 0.0,
            leisure: 1.0,
            rest: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BasketId(pub u64);

pub(crate) fn household_items() -> [ItemID; 2] {
    [ItemID::new("groceries"), ItemID::new("retail-goods")]
}

impl HouseholdStock {
    pub(crate) fn initialize(&mut self, id: HumanID, now: u64) {
        self.phase = common::hash_u64(id);
        self.next_shop = now + self.phase % (12 * TICKS_PER_HOUR);
        self.next_food = now + 1 + self.phase % FOOD_UNIT_TICKS;
        self.next_goods = now + 1 + self.phase % GOODS_UNIT_TICKS;
        self.next_leisure = now + self.phase % TICKS_PER_BUDGET_DAY;
        self.needs.leisure = 0.61 + (self.phase % 1_000) as f32 * 0.00039;
        self.last_update = Some(now);
    }

    pub(crate) fn units(&self, item: ItemID) -> u32 {
        self.on_hand.get(&item).copied().unwrap_or(0)
    }

    pub(crate) fn can_shop(&self, now: u64) -> bool {
        self.basket.is_none() && now >= self.next_shop
    }

    pub(crate) fn place_order(
        &mut self,
        now: u64,
        metrics: &mut DailyEconomyMetrics,
    ) -> BasketRequest {
        self.next_shop = now + SHOP_RETRY + self.phase % (20 * TICKS_PER_MINUTE);
        *metrics
            .days
            .entry(now / TICKS_PER_BUDGET_DAY)
            .or_default()
            .baskets_due
            .get_or_insert(0) += 1;
        let [groceries, retail] = household_items();
        let staples = BASKET_FOOD_TARGET.saturating_sub(self.units(groceries));
        BasketRequest {
            staples,
            goods: BASKET_GOODS_TARGET
                .saturating_sub(self.units(retail))
                .max(1),
            fallback: (staples + BASKET_GOODS_TARGET).saturating_sub(self.units(retail)),
        }
    }

    pub(crate) fn reserve(&mut self, basket: BasketReservation) {
        self.basket = Some(basket.id);
        self.reservation = Some(Box::new(basket));
    }

    pub(crate) fn collect(&mut self, id: BasketId, now: u64, resources: &Resources) {
        let Some(basket) = self.reservation.take_if(|basket| basket.id == id) else {
            return;
        };
        self.last_shop = Some(now);
        let units: u32 = basket.lines.iter().map(|&(_, units)| units).sum();
        // A partial basket has fewer days of supplies. Leave room for a second
        // outing later in the day without immediately retracing the same trip.
        self.next_shop = now + SHOP_INTERVAL.min((6 + u64::from(units) * 3) * TICKS_PER_HOUR);
        let mut metrics = resources.write::<DailyEconomyMetrics>();
        // Attribute fulfillment to the due cohort even when pickup crosses midnight.
        let due = metrics
            .days
            .entry(basket.due / TICKS_PER_BUDGET_DAY)
            .or_default();
        *due.baskets_filled.get_or_insert(0) += 1;
        let actual = metrics.days.entry(now / TICKS_PER_BUDGET_DAY).or_default();
        let mut stats = resources.write::<EcoStats>();
        for (item, units) in basket.lines {
            *self.carried.entry(item).or_default() += units;
            *actual.units_fulfilled_by_sku.entry(item).or_default() += u64::from(units);
            stats.internal_trade.handle_trade(&Trade {
                buyer: TradeTarget(SoulID::Human(basket.shopper)),
                seller: TradeTarget(basket.seller),
                qty: units as i32,
                kind: item,
                money_delta: Money::ZERO,
            });
        }
    }

    pub(crate) fn deposit(&mut self, now: u64, resources: &Resources) {
        self.advance(now, true, &mut resources.write::<DailyEconomyMetrics>());
        for (item, units) in std::mem::take(&mut self.carried) {
            *self.on_hand.entry(item).or_default() += units;
        }
        self.basket = None;
    }

    pub(crate) fn cancel(&mut self, resources: &Resources, refund: bool) -> bool {
        let Some(basket) = self.reservation.take() else {
            return false;
        };
        self.basket = None;
        self.next_shop =
            elapsed_ticks(resources).0 + SHOP_RETRY + self.phase % (20 * TICKS_PER_MINUTE);
        resources.write::<Market>().release_basket(*basket, refund);
        true
    }

    fn consume(&mut self, item: ItemID) -> bool {
        let std::collections::btree_map::Entry::Occupied(mut stock) = self.on_hand.entry(item)
        else {
            return false;
        };
        if *stock.get() <= 1 {
            return stock.remove() == 1;
        }
        *stock.get_mut() -= 1;
        true
    }

    fn advance(&mut self, now: u64, at_home: bool, metrics: &mut DailyEconomyMetrics) {
        let Some(mut from) = self.last_update.replace(now) else {
            self.next_food = now + FOOD_UNIT_TICKS;
            self.next_goods = now + GOODS_UNIT_TICKS;
            return;
        };
        let [groceries, retail] = household_items();
        while from < now {
            let day = from / TICKS_PER_BUDGET_DAY;
            let to = now
                .min((day + 1) * TICKS_PER_BUDGET_DAY)
                .min(self.next_food)
                .min(self.next_goods);
            let elapsed = to - from;
            let food_empty = self.units(groceries) == 0 && self.units(retail) == 0;
            let goods_empty = self.units(retail) == 0;
            if food_empty || goods_empty {
                let stockouts = &mut metrics.days.entry(day).or_default().stockout_ticks_by_sku;
                if food_empty {
                    *stockouts.entry(groceries).or_default() += elapsed;
                }
                if goods_empty {
                    *stockouts.entry(retail).or_default() += elapsed;
                }
            }

            self.needs.food = decay(self.needs.food, elapsed, FOOD_UNIT_TICKS);
            self.needs.goods = decay(self.needs.goods, elapsed, GOODS_UNIT_TICKS);
            if self.leisure_until.is_none() {
                self.needs.leisure = decay(self.needs.leisure, elapsed, 6 * TICKS_PER_BUDGET_DAY);
            }
            if at_home {
                self.needs.rest =
                    (self.needs.rest + elapsed as f32 / (6 * TICKS_PER_HOUR) as f32).min(1.0);
            } else {
                self.needs.rest = decay(self.needs.rest, elapsed, 19 * TICKS_PER_HOUR);
            }

            // Use retail as food when groceries are unavailable, including before
            // grocery-producing recipes are installed. Food takes priority at a tie.
            if to == self.next_food {
                if self.consume(groceries) || self.consume(retail) {
                    self.needs.food = 1.0;
                }
                self.next_food += FOOD_UNIT_TICKS;
            }
            if to == self.next_goods {
                if self.consume(retail) {
                    self.needs.goods = 1.0;
                }
                self.next_goods += GOODS_UNIT_TICKS;
            }
            from = to;
        }
    }
}

fn decay(value: f32, elapsed: u64, duration: u64) -> f32 {
    (value - elapsed as f32 / duration as f32).max(0.0)
}

/// Legacy explicitly submitted single-item orders still go through the old
/// pickup path. Baskets reserve shelf stock directly and settle on arrival.
pub fn settle_local_pickup(_world: &mut World, _resources: &Resources, _trade: Trade) -> bool {
    false
}

impl HouseholdStock {
    pub(crate) fn refresh_needs(&mut self, now: u64, at_home: bool, resources: &Resources) {
        if self
            .last_update
            .is_none_or(|last| now - last >= NEEDS_INTERVAL)
        {
            self.advance(now, at_home, &mut resources.write::<DailyEconomyMetrics>());
        }
    }
}

pub fn household_update(world: &mut World, resources: &mut Resources) {
    let now = elapsed_ticks(resources).0;
    if now % TICKS_PER_BUDGET_DAY != 0 {
        return;
    }
    // Flush the last partial interval before progression reads the closed day.
    let mut metrics = resources.write::<DailyEconomyMetrics>();
    for human in world.humans.values_mut() {
        let at_home = human.location == Location::Building(human.home.house);
        human.household.advance(now, at_home, &mut metrics);
    }
}

pub(crate) fn cancel_visit(human: &mut HumanEnt) {
    if human.household.leisure_visit.take().is_some() {
        human.household.leisure_until = None;
        human.router.walk_only_pickup = false;
    }
}
#[cfg(test)]
mod tests {
    use crate::economy::{DailyEconomyMetrics, Market, TripPurpose};
    use crate::map::BuildingKind;
    use crate::souls::desire::{Work, WorkKind};
    use crate::souls::human::spawn_human;
    use crate::souls::lifecycle;
    use crate::tests::TestCtx;
    use crate::transportation::Location;
    use crate::{Simulation, SoulID, WorldCommand};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{GameTime, GoodsCompanyID, ItemID, Tick, TICKS_PER_HOUR};

    fn street() -> (TestCtx, crate::HumanID, crate::map::BuildingID) {
        let mut test = TestCtx::new_fast();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(420.0, 100.0, 0.0)]);
        let home = test.build_house_near(vec2(155.0, 122.0));
        let human = spawn_human(&mut test.g, home).unwrap();
        (test, human, home)
    }

    fn business(test: &mut TestCtx, x: f32, name: &str) -> (crate::map::BuildingID, SoulID) {
        business_at(test, vec2(x, 145.0), name)
    }

    fn business_at(test: &mut TestCtx, at: Vec2, name: &str) -> (crate::map::BuildingID, SoulID) {
        use crate::map::{ProjectFilter, ProjectKind};
        let ProjectKind::Road(road) = test
            .g
            .map()
            .project(vec3(at.x, 100.0, 0.0), 2.0, ProjectFilter::ROAD)
            .kind
        else {
            panic!("missing frontage on the fixture street");
        };
        business_on_road(test, at, name, road)
    }

    fn business_on_road(
        test: &mut TestCtx,
        at: Vec2,
        name: &str,
        road: crate::map::RoadID,
    ) -> (crate::map::BuildingID, SoulID) {
        let proto = GoodsCompanyID::new(name).prototype();
        let kind = BuildingKind::GoodsCompany(proto.id);
        let building = test
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(at, Vec2::X, 20.0, 20.0),
                kind,
                proto.bgen,
                None,
                Some(road),
            )
            .unwrap();
        crate::souls::growth::on_growable_placed(&test.g, building, kind);
        crate::souls::add_souls_to_empty_buildings(&mut test.g);
        let seller = test
            .g
            .read::<crate::map_dynamic::BuildingInfos>()
            .owner(building)
            .unwrap();
        (building, seller)
    }

    fn shelf(test: &TestCtx, seller: SoulID, item: ItemID, units: u32) {
        let map = test.g.map();
        let infos = test.g.read::<crate::map_dynamic::BuildingInfos>();
        let door = map
            .get(infos.building_owned_by(seller).unwrap())
            .unwrap()
            .door_pos;
        let mut market = test.g.write::<Market>();
        market.produce(seller, item, units as i32);
        market.sell_all(seller, door.xy(), item, 0);
    }

    fn due(test: &mut TestCtx, id: crate::HumanID) {
        let stock = &mut test.g.world_mut_unchecked().humans[id].household;
        stock.next_shop = 0;
        stock.needs.food = 0.0;
        stock.needs.goods = 0.0;
        test.g.world_mut_unchecked().humans[id].decision.wait = 0;
    }

    #[test]
    fn e1_multi_sku_basket_uses_one_visit_then_deposits_and_consumes() {
        let (mut test, id, home) = street();
        let (store, seller) = business(&mut test, 233.0, "commercial-l1");
        let groceries = ItemID::new("groceries");
        let retail = ItemID::new("retail-goods");
        shelf(&test, seller, groceries, 3);
        shelf(&test, seller, retail, 2);
        due(&mut test, id);

        let mut picked = false;
        for _ in 0..3 * TICKS_PER_HOUR {
            test.tick_fast();
            let h = test.g.get(id).unwrap();
            if !picked && !h.household.carried.is_empty() {
                assert_eq!(h.location, Location::Building(store));
                assert_eq!(h.household.carried[&groceries], 3);
                assert!(h.household.carried[&retail] > 0);
                assert!(h.household.on_hand.is_empty());
                picked = true;
            }
            if h.household.on_hand.contains_key(&groceries) {
                assert_eq!(h.location, Location::Building(home));
                assert!(h.household.carried.is_empty());
                break;
            }
        }
        assert!(picked);
        let h = test.g.get(id).unwrap();
        assert_eq!(
            h.household.on_hand.get(&groceries),
            Some(&3),
            "return from the store: {:?}, {:?}",
            h.location,
            h.router.cur_step()
        );
        let metrics = test.g.read::<DailyEconomyMetrics>();
        let day = &metrics.days[&0];
        assert_eq!(day.trips_by_purpose[&TripPurpose::Shopping].completed, 1);
        assert_eq!(day.baskets_filled, Some(1));
        drop(metrics);

        let (world, res) = test.g.world_res();
        let h = &mut world.humans[id];
        let now = h.household.last_update.unwrap();
        h.household.advance(
            now + 9 * TICKS_PER_HOUR,
            true,
            &mut res.write::<DailyEconomyMetrics>(),
        );
        assert!(h.household.on_hand.get(&groceries).copied().unwrap_or(0) < 3);
        assert!(h.household.needs.food > 0.0);
    }

    #[test]
    fn e1_partial_fill_and_already_in_shop_use_retail_as_food() {
        let (mut test, id, _) = street();
        let (store, seller) = business(&mut test, 230.0, "commercial-l1");
        let retail = ItemID::new("retail-goods");
        shelf(&test, seller, retail, 2);
        let h = &mut test.g.world_mut_unchecked().humans[id];
        h.location = Location::Building(store);
        due(&mut test, id);
        test.tick_fast();
        assert_eq!(test.g.get(id).unwrap().household.carried[&retail], 2);
        let metrics = test.g.read::<DailyEconomyMetrics>();
        let day = &metrics.days[&0];
        assert_eq!(day.baskets_filled, Some(1));
        assert!(!day.trips_by_purpose.contains_key(&TripPurpose::Shopping));
    }

    #[test]
    fn e1_ignores_seller_with_disconnected_frontage() {
        use crate::map::{ProjectFilter, ProjectKind};
        let (mut test, id, _) = street();
        test.build_roads(&[vec3(100.0, 500.0, 0.0), vec3(420.0, 500.0, 0.0)]);
        let ProjectKind::Road(disconnected) = test
            .g
            .map()
            .project(vec3(250.0, 500.0, 0.0), 2.0, ProjectFilter::ROAD)
            .kind
        else {
            panic!("missing disconnected test road")
        };
        let (_, seller) =
            business_on_road(&mut test, vec2(230.0, 145.0), "commercial-l1", disconnected);
        let retail = ItemID::new("retail-goods");
        shelf(&test, seller, retail, 5);
        due(&mut test, id);
        test.tick_fast();
        assert!(test.g.get(id).unwrap().household.basket.is_none());
        assert_eq!(test.g.read::<Market>().capital(seller, retail), 5);
        assert_eq!(
            test.g.read::<DailyEconomyMetrics>().days[&0].baskets_due,
            Some(1)
        );
    }

    #[test]
    fn e1_expiry_and_despawn_refund_in_flight_pickups_once() {
        for failure in ["expiry", "despawn", "route"] {
            let (mut test, id, _) = street();
            let (_, seller) = business(&mut test, 230.0, "commercial-l1");
            let retail = ItemID::new("retail-goods");
            shelf(&test, seller, retail, 5);
            due(&mut test, id);
            test.tick_fast();
            assert!(test.g.get(id).unwrap().household.basket.is_some());
            assert_eq!(test.g.read::<Market>().capital(seller, retail), 0);
            match failure {
                "despawn" => lifecycle::despawn_human(&mut test.g, id),
                "expiry" => {
                    test.g.world_mut_unchecked().humans[id]
                        .household
                        .reservation
                        .as_mut()
                        .unwrap()
                        .expires = 0;
                }
                "route" => {
                    let (world, resources) = test.g.world_res();
                    let h = &mut world.humans[id];
                    h.router.cancel_route(
                        &mut resources.write::<crate::map_dynamic::ParkingManagement>(),
                    );
                    h.decision.wait = 0;
                }
                _ => unreachable!(),
            }
            test.tick_fast();
            test.tick_fast();
            assert_eq!(test.g.read::<Market>().capital(seller, retail), 5);
            assert_eq!(
                test.g.read::<Market>().capital(SoulID::Human(id), retail),
                0
            );
            assert_eq!(
                test.g.read::<DailyEconomyMetrics>().days[&0].baskets_filled,
                None
            );
        }
    }

    #[test]
    fn e1_rehome_keeps_pantry_demolition_writes_off_and_carry_is_deposited() {
        let (mut test, id, first_home) = street();
        let second_home = test.build_house_near(vec2(255.0, 122.0));
        let third_home = test.build_house_near(vec2(355.0, 122.0));
        let retail = ItemID::new("retail-goods");
        let h = &mut test.g.world_mut_unchecked().humans[id];
        h.household.on_hand.insert(retail, 3);
        h.household.carried.insert(retail, 2);

        // An intact replacement home keeps the existing pantry.
        test.g
            .write::<crate::map_dynamic::BuildingInfos>()
            .add_resident(second_home, SoulID::Human(id));
        test.g.world_mut_unchecked().humans[id].home.house = second_home;
        test.apply(&[WorldCommand::MapRemoveBuilding(first_home)]);
        assert_eq!(test.g.get(id).unwrap().household.on_hand[&retail], 3);
        assert!(test.g.read::<Market>().household_losses().is_empty());

        test.apply(&[WorldCommand::MapRemoveBuilding(second_home)]);
        let h = test.g.get(id).unwrap();
        assert_eq!(h.home.house, third_home);
        assert!(h.household.on_hand.is_empty());
        assert_eq!(h.household.carried[&retail], 2);
        assert_eq!(test.g.read::<Market>().household_losses()[&retail], 3);
        for _ in 0..3 * TICKS_PER_HOUR {
            test.tick_fast();
            if test.g.get(id).unwrap().household.carried.is_empty() {
                break;
            }
        }
        let h = test.g.get(id).unwrap();
        assert_eq!(h.location, Location::Building(third_home));
        assert_eq!(h.household.on_hand[&retail], 2);

        lifecycle::despawn_human(&mut test.g, id);
        test.tick_fast();
        assert_eq!(test.g.read::<Market>().household_losses()[&retail], 5);
    }

    #[test]
    fn e1_seller_demolition_writes_off_reserved_stock_once() {
        let (mut test, id, _) = street();
        let (store, seller) = business(&mut test, 230.0, "commercial-l1");
        let retail = ItemID::new("retail-goods");
        shelf(&test, seller, retail, 5);
        due(&mut test, id);
        test.tick_fast();
        assert!(test.g.get(id).unwrap().household.basket.is_some());

        test.apply(&[WorldCommand::MapRemoveBuilding(store)]);
        for _ in 0..5 {
            test.tick_fast();
        }
        assert!(test.g.get(id).unwrap().household.basket.is_none());
        assert_eq!(test.g.read::<Market>().household_losses()[&retail], 5);
    }

    #[test]
    fn e1_in_flight_basket_survives_save_resume() {
        let (mut test, id, _) = street();
        let (_, seller) = business(&mut test, 230.0, "commercial-l1");
        shelf(&test, seller, ItemID::new("retail-goods"), 5);
        due(&mut test, id);
        test.tick_fast();
        assert!(test.g.get(id).unwrap().household.basket.is_some());
        let mut restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
        let mut schedule = Simulation::schedule();
        for _ in 0..3 * TICKS_PER_HOUR {
            test.tick_fast();
            restored.tick(&mut schedule, &[]);
        }
        assert_eq!(restored.hashes(), test.g.hashes());
        let h = restored.get(id).unwrap();
        assert!(
            !h.household.on_hand.is_empty(),
            "{:?}, {:?}",
            h.location,
            h.router.cur_step()
        );
    }

    #[test]
    fn e1_work_attendance_and_park_dwell() {
        let (mut test, id, _) = street();
        let (job, employer) = business(&mut test, 230.0, "commercial-l1");
        let (park, park_owner) = business(&mut test, 320.0, "park");
        business_at(&mut test, vec2(275.0, 65.0), "solar-panel");
        let jobs = ItemID::new("job-opening");
        let mut market = test.g.write::<Market>();
        market.sell(employer, vec2(230.0, 145.0), jobs, 0, 0);
        market.sell(park_owner, vec2(320.0, 145.0), jobs, 0, 0);
        drop(market);
        let second_home = test.build_house_near(vec2(360.0, 122.0));
        let second = spawn_human(&mut test.g, second_home).unwrap();
        let owner = test
            .g
            .read::<crate::map_dynamic::BuildingInfos>()
            .owner(park)
            .unwrap();
        let SoulID::GoodsCompany(park_company) = owner else {
            panic!("missing park company")
        };
        test.g.world_mut_unchecked().companies[park_company]
            .workers
            .0
            .push(second);
        let mut staff = Work::new(park, WorkKind::Worker, 0.0);
        staff.work_inter = prototypes::RecTimeInterval::never();
        test.g.world_mut_unchecked().humans[second].work = Some(staff);
        test.g.world_mut_unchecked().humans[id].work = Some(Work::new(job, WorkKind::Worker, 0.0));
        test.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(
            TICKS_PER_HOUR,
        )))]);
        for _ in 0..TICKS_PER_HOUR {
            test.tick_fast();
        }
        assert_eq!(test.g.get(id).unwrap().location, Location::Building(job));
        assert_eq!(
            test.g.read::<DailyEconomyMetrics>().days[&0].trips_by_purpose[&TripPurpose::Work]
                .completed,
            1
        );
        test.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(
            11 * TICKS_PER_HOUR,
        )))]);
        let stock = &mut test.g.world_mut_unchecked().humans[id].household;
        stock.needs.leisure = 0.0;
        stock.next_leisure = 0;
        for _ in 0..TICKS_PER_HOUR {
            test.tick_fast();
            if test.g.get(id).unwrap().location == Location::Building(park) {
                break;
            }
        }
        assert_eq!(test.g.get(id).unwrap().location, Location::Building(park));
        for _ in 0..100 {
            test.tick_fast();
        }
        assert_eq!(test.g.get(id).unwrap().location, Location::Building(park));
        assert_eq!(
            test.g.read::<DailyEconomyMetrics>().days[&0].trips_by_purpose[&TripPurpose::Leisure]
                .completed,
            1
        );
    }
}
