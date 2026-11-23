use std::collections::{BTreeMap, BTreeSet};

use prototypes::{ItemID, Money, TICKS_PER_HOUR, TICKS_PER_MINUTE, TICKS_PER_REALTIME_SECOND};
use serde::{Deserialize, Serialize};

use crate::economy::balance::{
    BASKET_FOOD_TARGET, BASKET_GOODS_TARGET, FOOD_UNIT_TICKS, GOODS_UNIT_TICKS,
};
use crate::economy::{
    BasketFailure, BasketRequest, BasketReservation, DailyEconomyMetrics, EcoStats, Market, Trade,
    TradeTarget, TICKS_PER_BUDGET_DAY,
};
use crate::map::BuildingID;
use crate::map_dynamic::transit::elapsed_ticks;
use crate::map_dynamic::BuildingInfos;
use crate::transportation::Location;
use crate::utils::resources::Resources;
use crate::{HumanEnt, HumanID, SoulID, World};

const NEEDS_INTERVAL: u64 = 5 * TICKS_PER_MINUTE;
const SHOP_RETRY: u64 = 30 * TICKS_PER_MINUTE;
const SHOP_EPISODE_LIMIT: u64 = 8 * TICKS_PER_HOUR;
const SHELF_SHORTFALL_WINDOW: u64 = 4 * TICKS_PER_HOUR;
// Travel budgets bound how far an agent moves, so they are real time
// (12 and 18 real minutes, two and three game hours at the old 10× clock).
pub(crate) const BASKET_TRAVEL_LIMIT: u64 = 12 * 60 * TICKS_PER_REALTIME_SECOND;
pub(crate) const HOMEBOUND_TRAVEL_LIMIT: u64 = 18 * 60 * TICKS_PER_REALTIME_SECOND;
pub(crate) const HOMEBOUND_RETRY: u64 = 2 * TICKS_PER_HOUR;

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
    pub(crate) attended_shift: Option<(BuildingID, i32)>,
    pub(crate) next_deposit: u64,
    pub(crate) homebound_until: Option<u64>,
    shop_due: Option<u64>,
    shop_failures: u8,
    excluded_seller: Option<(SoulID, u64)>,
    shelf_shortfall: Option<(u64, [u32; 2])>,
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

/// Derived indexes for active reservations and park visits, rebuilt after load.
#[derive(Default)]
pub(crate) struct HouseholdClaims {
    pub(crate) baskets: BTreeMap<SoulID, BTreeSet<HumanID>>,
    pub(crate) parks: BTreeMap<BuildingID, BTreeSet<(bool, u64, HumanID)>>,
}

impl HouseholdClaims {
    pub(crate) fn from_world(world: &World) -> Self {
        let mut claims = Self::default();
        for (id, human) in world.humans.iter() {
            if let Some(basket) = &human.household.reservation {
                claims.baskets.entry(basket.seller).or_default().insert(id);
            }
            if let Some(park) = human.household.leisure_visit {
                claims.add_park_claim(park, id, &human.household);
            }
        }
        claims
    }

    pub(crate) fn add_park_claim(&mut self, park: BuildingID, id: HumanID, stock: &HouseholdStock) {
        self.parks
            .entry(park)
            .or_default()
            .insert(stock.park_claim(id));
    }

    pub(crate) fn remove_park_claim(
        &mut self,
        park: BuildingID,
        id: HumanID,
        stock: &HouseholdStock,
    ) {
        remove_claim(&mut self.parks, park, stock.park_claim(id));
    }
}

fn remove_claim<K: Ord, V: Ord>(claims: &mut BTreeMap<K, BTreeSet<V>>, key: K, id: V) {
    if let std::collections::btree_map::Entry::Occupied(mut claim) = claims.entry(key) {
        claim.get_mut().remove(&id);
        if claim.get().is_empty() {
            claim.remove();
        }
    }
}

impl HouseholdStock {
    fn park_claim(&self, id: HumanID) -> (bool, u64, HumanID) {
        // A visitor already dwelling keeps precedence over someone en route.
        match self.leisure_until {
            Some(until) => (false, until, id),
            None => (true, self.leisure_started, id),
        }
    }

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
        self.basket.is_none()
            && self.reservation.is_none()
            && self.carried.is_empty()
            && now >= self.next_shop
    }

    pub(crate) fn place_order(&mut self, now: u64) -> BasketRequest {
        self.shop_due.get_or_insert(now);
        let [groceries, retail] = household_items();
        let staples = BASKET_FOOD_TARGET.saturating_sub(self.units(groceries));
        BasketRequest {
            staples,
            goods: BASKET_GOODS_TARGET.saturating_sub(self.units(retail)),
            fallback: (staples + BASKET_GOODS_TARGET).saturating_sub(self.units(retail)),
            excluded_seller: self.excluded_seller(now),
        }
    }

    pub(crate) fn reserve(&mut self, basket: BasketReservation, resources: &Resources) -> bool {
        if self.basket.is_some() || self.reservation.is_some() {
            Self::release(basket, resources, false);
            return false;
        }
        resources
            .write::<HouseholdClaims>()
            .baskets
            .entry(basket.seller)
            .or_default()
            .insert(basket.shopper);
        self.basket = Some(basket.id);
        self.reservation = Some(Box::new(basket));
        true
    }

    fn clear_basket_claim(basket: &BasketReservation, resources: &Resources) {
        let mut claims = resources.write::<HouseholdClaims>();
        remove_claim(&mut claims.baskets, basket.seller, basket.shopper);
    }

    pub(crate) fn collect(&mut self, id: BasketId, now: u64, resources: &Resources) {
        let Some(basket) = self.reservation.take_if(|basket| basket.id == id) else {
            return;
        };
        Self::clear_basket_claim(&basket, resources);
        resources.write::<Market>().finish_basket(&basket);
        self.last_shop = Some(now);
        self.shop_failures = 0;
        self.excluded_seller = None;
        let [groceries, retail] = household_items();
        let delivered = |item| {
            basket
                .lines
                .iter()
                .find(|line| line.0 == item)
                .map_or(0, |line| line.1)
        };
        let staples = delivered(groceries);
        let goods = delivered(retail);
        let request = self.place_order(now);
        // A short fill can be retried before its next meal runs out.
        let food = staples + goods.saturating_sub(request.goods);
        self.next_shop = now + (2 + 3 * u64::from(food).min(4)) * TICKS_PER_HOUR;
        let mut metrics = resources.write::<DailyEconomyMetrics>();
        self.finish_order(now, true, &mut metrics);
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
        if self.reservation.is_none() {
            self.basket = None;
        }
        self.homebound_until = None;
        self.next_deposit = 0;
    }

    pub(crate) fn cancel(&mut self, resources: &Resources, reason: BasketFailure) -> bool {
        let Some(basket) = self.reservation.take() else {
            return false;
        };
        self.basket = None;
        let now = elapsed_ticks(resources).0;
        if matches!(
            reason,
            BasketFailure::Expired | BasketFailure::RouteInterrupted
        ) {
            self.excluded_seller = Some((basket.seller, now + SHELF_SHORTFALL_WINDOW));
        }
        Self::clear_basket_claim(&basket, resources);
        Self::release(*basket, resources, reason == BasketFailure::SellerRemoved);
        self.shop_failed(now, reason, resources);
        true
    }

    fn release(basket: BasketReservation, resources: &Resources, removed: bool) {
        let refund = !removed
            && resources.read::<BuildingInfos>().owner(basket.building) == Some(basket.seller);
        resources.write::<Market>().release_basket(basket, refund);
    }

    fn excluded_seller(&self, now: u64) -> Option<SoulID> {
        self.excluded_seller
            .filter(|&(_, until)| now < until)
            .map(|(seller, _)| seller)
    }

    pub(crate) fn shop_failed(&mut self, now: u64, reason: BasketFailure, resources: &Resources) {
        resources.write::<Market>().record_basket_failure(reason);
        match reason {
            BasketFailure::Throttled => {
                // A seller takes one reservation per tick: retry after 1.5–4.5 real seconds.
                self.next_shop = now
                    + 3 * TICKS_PER_REALTIME_SECOND / 2
                    + self.phase % (3 * TICKS_PER_REALTIME_SECOND);
            }
            BasketFailure::SellerRemoved => {
                self.next_shop = now + 5 * TICKS_PER_MINUTE + self.phase % (5 * TICKS_PER_MINUTE);
            }
            BasketFailure::SellerExcluded => {
                self.next_shop = now + SHOP_RETRY + self.phase % SHOP_RETRY;
            }
            BasketFailure::ShopperRemoved => {}
            _ => {
                let retry = SHOP_RETRY << self.shop_failures.min(3);
                self.shop_failures = self.shop_failures.saturating_add(1);
                self.next_shop = now + retry + self.phase % SHOP_RETRY;
            }
        }
        self.expire_order(now, resources);
    }

    pub(crate) fn expire_order(&mut self, now: u64, resources: &Resources) {
        if self.reservation.is_none()
            && self
                .shop_due
                .is_some_and(|due| now - due >= SHOP_EPISODE_LIMIT)
        {
            self.finish_order(now, false, &mut resources.write::<DailyEconomyMetrics>());
        }
    }

    pub(crate) fn discard_order(&mut self, resources: &Resources) {
        if self.shop_due.is_some() {
            let now = elapsed_ticks(resources).0;
            self.finish_order(now, false, &mut resources.write::<DailyEconomyMetrics>());
        }
    }

    fn finish_order(&mut self, now: u64, filled: bool, metrics: &mut DailyEconomyMetrics) {
        if self.shop_due.take().is_none() {
            return;
        }
        // Settle both counters on the result day so a closed day is immutable,
        // even when a reservation is picked up after midnight.
        let day = metrics.days.entry(now / TICKS_PER_BUDGET_DAY).or_default();
        *day.baskets_due.get_or_insert(0) += 1;
        *day.baskets_filled.get_or_insert(0) += u64::from(filled);
    }

    pub(crate) fn note_shelf_shortfall(&mut self, now: u64, missing: [u32; 2]) {
        self.shelf_shortfall = (missing != [0, 0]).then_some((now, missing));
    }

    pub(crate) fn unmet_shelf_demand(&self, now: u64) -> [u32; 2] {
        self.shelf_shortfall
            .filter(|&(at, _)| now.saturating_sub(at) < SHELF_SHORTFALL_WINDOW)
            .map_or([0, 0], |(_, missing)| missing)
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
            let staples_empty = self.units(groceries) == 0;
            let goods_empty = self.units(retail) == 0;
            if staples_empty || goods_empty {
                let stockouts = &mut metrics.days.entry(day).or_default().stockout_ticks_by_sku;
                if staples_empty {
                    *stockouts.entry(groceries).or_default() += elapsed;
                }
                if goods_empty {
                    *stockouts.entry(retail).or_default() += elapsed;
                }
                if staples_empty && goods_empty {
                    *stockouts.entry(ItemID::new("effective-food")).or_default() += elapsed;
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

pub(crate) fn cancel_visit(id: HumanID, human: &mut HumanEnt, resources: &Resources) {
    if let Some(park) = human.household.leisure_visit.take() {
        resources
            .write::<HouseholdClaims>()
            .remove_park_claim(park, id, &human.household);
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
    fn e1_fix_retries_count_once_and_closed_days_stay_closed() {
        let (mut test, id, _) = street();
        let (_, seller) = business(&mut test, 230.0, "commercial-l1");
        let (world, resources) = test.g.world_res();
        let h = &mut world.humans[id];
        let due = super::TICKS_PER_BUDGET_DAY - 100;
        h.household.place_order(due - TICKS_PER_HOUR);
        let request = h.household.place_order(due);
        let retail = ItemID::new("retail-goods");
        let pos = vec2(230.0, 145.0);
        let (basket, _) = {
            let mut market = resources.write::<Market>();
            market.produce(seller, retail, 5);
            market.sell_all(seller, pos, retail, 0);
            market
                .reserve_basket(id, pos, 600.0, request, due, |_| Some(h.home.house))
                .unwrap()
        };
        let basket_id = basket.id;
        assert!(h.household.reserve(basket, resources));
        h.household.collect(basket_id, due + 200, resources);
        let metrics = resources.read::<DailyEconomyMetrics>();
        assert!(metrics
            .days
            .get(&0)
            .is_none_or(|day| day.baskets_due.is_none() && day.baskets_filled.is_none()));
        assert_eq!(metrics.days[&1].baskets_due, Some(1));
        assert_eq!(metrics.days[&1].baskets_filled, Some(1));
    }

    #[test]
    fn e1_fix_full_retail_shelf_does_not_block_staples() {
        use crate::souls::goods_company::{recipe_act, recipe_should_produce};

        let (mut test, _, _) = street();
        let (_, seller) = business(&mut test, 230.0, "commercial-l1");
        let proto = GoodsCompanyID::new("commercial-l1").prototype();
        let recipe = proto.recipe.as_ref().unwrap();
        let groceries = ItemID::new("groceries");
        let retail = ItemID::new("retail-goods");
        assert!(crate::souls::residence::is_retail_store(
            BuildingKind::GoodsCompany(proto.id)
        ));
        let retail_batch = recipe
            .production
            .iter()
            .find(|item| item.id == retail)
            .unwrap();
        let retail_limit = retail_batch.amount * recipe.storage_multiplier;
        let pos = vec2(230.0, 145.0);
        let mut market = test.g.write::<Market>();
        for input in &recipe.consumption {
            market.produce(seller, input.id, input.amount);
        }
        market.produce(seller, retail, retail_limit);
        assert!(recipe_should_produce(recipe, seller, &market));
        recipe_act(recipe, seller, pos, &mut market);
        assert!(market.capital(seller, groceries) > 0);
        assert_eq!(market.capital(seller, retail), retail_limit);
        assert_eq!(
            market.seller_losses().get(&retail),
            Some(&(retail_batch.amount as u64))
        );
    }

    #[test]
    fn e1_fix2_bakery_counts_reserved_stock_until_pickup_or_refund() {
        use crate::economy::BasketRequest;
        use crate::souls::goods_company::{recipe_act, recipe_should_produce};

        let (mut test, shopper, _) = street();
        let (store, seller) = business(&mut test, 230.0, "bakery");
        let pos = test.g.map().get(store).unwrap().door_pos.xy();
        let recipe = GoodsCompanyID::new("bakery")
            .prototype()
            .recipe
            .as_ref()
            .unwrap();
        let groceries = ItemID::new("groceries");
        let flour = ItemID::new("flour");
        let batch = recipe.production[0].amount;
        let allowance = batch * recipe.storage_multiplier;
        let capacity = allowance + batch;
        let mut market = test.g.write::<Market>();
        market.produce(seller, groceries, allowance);
        market.produce(seller, flour, 2);
        market.sell_all(seller, pos, groceries, allowance as u32);
        let (basket, _) = market
            .reserve_basket(
                shopper,
                pos,
                100.0,
                BasketRequest {
                    staples: batch as u32,
                    ..Default::default()
                },
                1,
                |_| Some(store),
            )
            .unwrap();

        assert!(recipe_should_produce(recipe, seller, &market));
        recipe_act(recipe, seller, pos, &mut market);
        assert!(!recipe_should_produce(recipe, seller, &market));
        assert_eq!(
            market.m(groceries).sell_order(seller).unwrap().qty,
            allowance as u32
        );
        market.release_basket(basket, true);
        assert_eq!(
            market.m(groceries).sell_order(seller).unwrap().qty,
            capacity as u32
        );
        assert!(!recipe_should_produce(recipe, seller, &market));
    }

    #[test]
    fn e1_fix2_synchronized_shoppers_retry_without_reporting_stockout() {
        let (mut test, first, home) = street();
        let second = spawn_human(&mut test.g, home).unwrap();
        let (_, seller) = business(&mut test, 230.0, "commercial-l1");
        shelf(&test, seller, ItemID::new("groceries"), 20);
        for id in [first, second] {
            due(&mut test, id);
            test.g.world_mut_unchecked().humans[id]
                .household
                .last_update = Some(1);
        }
        test.tick_fast();
        let stock = &test.g.get(second).unwrap().household;
        assert!(test.g.get(first).unwrap().household.reservation.is_some());
        assert!(stock.reservation.is_none());
        assert_eq!(stock.shop_failures, 0);
        assert_eq!(stock.unmet_shelf_demand(1), [0, 0]);
        assert!(stock.next_shop < super::SHOP_RETRY);
        assert_eq!(
            test.g.read::<Market>().basket_failures()[&super::BasketFailure::Throttled],
            1,
        );
        for _ in 0..2 * prototypes::TICKS_PER_MINUTE {
            test.tick_fast();
        }
        assert!(test.g.get(second).unwrap().household.reservation.is_some());
    }

    #[test]
    fn e1_fix2_removing_a_pending_employer_reopens_the_job_search() {
        let (mut test, human, _) = street();
        let (_, seller) = business(&mut test, 230.0, "commercial-l1");
        let SoulID::GoodsCompany(company) = seller else {
            unreachable!();
        };
        test.g.write::<Market>().index_human_buyers(test.g.world());
        test.tick_fast();
        let jobs = ItemID::new("job-opening");
        assert_eq!(
            test.g.get(human).unwrap().bought.0[&jobs][0].seller.0,
            seller
        );
        let mut restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();

        for sim in [&mut test.g, &mut restored] {
            lifecycle::despawn_company(sim, company);
            crate::utils::scheduler::drain_commands(sim);
            assert!(!sim.get(human).unwrap().bought.0.contains_key(&jobs));
            assert!(sim
                .write::<Market>()
                .m(jobs)
                .buy_order(SoulID::Human(human))
                .is_some());
        }
        assert!(test.g.is_equal(&restored));
    }

    #[test]
    fn e1_fix_same_day_replacement_job_requires_attendance() {
        let (mut test, id, home) = street();
        let (old_job, _) = business(&mut test, 230.0, "commercial-l1");
        let (new_job, _) = business(&mut test, 330.0, "commercial-l1");
        let time = GameTime::new(Tick(2 * TICKS_PER_HOUR));
        let h = &mut test.g.world_mut_unchecked().humans[id];
        h.work = Some(Work::new(old_job, WorkKind::Worker, 0.0));
        h.attend_work(&time, old_job);
        h.work = Some(Work::new(new_job, WorkKind::Worker, 0.0));
        let shift_score = |h: &crate::HumanEnt, time| {
            h.work.as_ref().unwrap().shift_score(
                time,
                &Location::Building(home),
                h.household.attended_shift,
            )
        };
        assert!(shift_score(h, &time) > 0.0);
        let following_day = GameTime::new(Tick(26 * TICKS_PER_HOUR));
        h.attend_work(&following_day, new_job);
        assert_eq!(shift_score(h, &following_day), 0.0);
        assert!(shift_score(h, &time) > 0.0);
    }

    #[test]
    fn e1_fix_disconnected_home_deposit_yields_with_stock_still_carried() {
        use crate::map_dynamic::{Destination, Router};
        use crate::souls::human::HumanDecisionKind;

        let (mut test, id, _) = street();
        let (workplace, _) = business(&mut test, 230.0, "commercial-l1");
        test.build_roads(&[vec3(100.0, 500.0, 0.0), vec3(420.0, 500.0, 0.0)]);
        let home = test.build_house_near(vec2(155.0, 522.0));
        let groceries = ItemID::new("groceries");
        let human = &mut test.g.world_mut_unchecked().humans[id];
        human.location = Location::Building(workplace);
        human.home.house = home;
        human.household.carried.insert(groceries, 3);
        human.router = Router::new(None);
        human.decision.kind = HumanDecisionKind::DepositAtHome;
        human.decision.wait = 0;
        for _ in 0..2 * crate::souls::human::HOMEBOUND_STALL {
            test.tick_fast();
        }
        let human = test.g.get(id).unwrap();
        assert!(!matches!(
            human.decision.kind,
            HumanDecisionKind::DepositAtHome
        ));
        assert_ne!(human.router.target_dest, Some(Destination::Building(home)));
        assert_eq!(human.household.carried.get(&groceries), Some(&3));
        assert!(human.household.on_hand.is_empty());
    }

    #[test]
    fn e1_fix_after_work_basket_returns_via_the_car_parked_away_from_home() {
        use crate::map_dynamic::Destination;
        use crate::souls::human::HumanDecisionKind;

        let (mut test, id, home) = street();
        let (workplace, _) = business(&mut test, 330.0, "commercial-l1");
        let workplace_door = test.g.map().get(workplace).unwrap().door_pos;
        let world = test.g.world_mut_unchecked();
        let human = &mut world.humans[id];
        let car = human.router.personal_car.unwrap();
        world.vehicles[car].trans.pos = workplace_door;
        human.location = Location::Building(workplace);
        human.household.carried.insert(ItemID::new("groceries"), 3);
        human.decision.kind = HumanDecisionKind::DepositAtHome;
        human.decision.wait = 0;
        test.tick_fast();

        let human = test.g.get(id).unwrap();
        assert!(!human.router.walk_only_pickup);
        assert_eq!(human.router.target_dest, Some(Destination::Building(home)));
        assert_eq!(human.router.selected_vehicle(), Some(car));
    }

    #[test]
    fn e1_fix2_stalled_car_deposit_yields_with_stock_still_carried() {
        use crate::map::{LaneKind, PathKind};
        use crate::map_dynamic::{Destination, Itinerary, RoutingStep};
        use crate::souls::human::HumanDecisionKind;

        let (mut test, id, home) = street();
        test.build_roads(&[vec3(100.0, 500.0, 0.0), vec3(420.0, 500.0, 0.0)]);
        let car_pos = {
            let map = test.g.map();
            let pos = vec3(200.0, 500.0, 0.0);
            map.get(map.nearest_lane(pos, LaneKind::Driving, None).unwrap())
                .unwrap()
                .points
                .project_segment(pos)
                .0
        };
        let home_pos = test.g.map().get(home).unwrap().door_pos;
        let car = test.g.get(id).unwrap().router.personal_car.unwrap();
        crate::transportation::unpark(&mut test.g, car);
        let world = test.g.world_mut_unchecked();
        let human = &mut world.humans[id];
        let groceries = ItemID::new("groceries");
        human.location = Location::Vehicle(car);
        human.router.set_test_route(
            Destination::Building(home),
            vec![RoutingStep::DriveTo(car, home_pos)],
        );
        human.household.carried.insert(groceries, 3);
        human.household.homebound_until = Some(super::HOMEBOUND_TRAVEL_LIMIT);
        human.decision.kind = HumanDecisionKind::DepositAtHome;
        human.decision.wait = 0;
        world.vehicles[car].trans.pos = car_pos;
        world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Vehicle, home_pos);
        let stall = crate::souls::human::HOMEBOUND_STALL;
        for _ in 0..stall * 4 / 5 {
            test.tick_fast();
        }
        assert!(test.g.get(car).unwrap().it.is_wait_for_reroute().is_some());
        for _ in 0..stall * 6 / 5 {
            test.tick_fast();
        }
        let human = test.g.get(id).unwrap();
        assert!(!matches!(
            human.decision.kind,
            HumanDecisionKind::DepositAtHome
        ));
        assert_eq!(human.location, Location::Outside);
        assert_ne!(human.router.target_dest, Some(Destination::Building(home)));
        assert_eq!(human.household.carried.get(&groceries), Some(&3));
        // The car was parked before its owner left it, not abandoned on the lane.
        assert!(test.g.get(car).unwrap().vehicle.state.is_parked());
        assert!(crate::map_dynamic::abandoned_cars(test.g.world()).is_empty());
    }

    #[test]
    fn parkfix_homebound_deadline_parks_the_car_before_leaving_it() {
        use crate::map::{LaneKind, PathKind};
        use crate::map_dynamic::{Destination, Itinerary, RoutingStep};
        use crate::souls::human::HumanDecisionKind;

        let (mut test, id, home) = street();
        let car_pos = {
            let map = test.g.map();
            let pos = vec3(400.0, 100.0, 0.0);
            map.get(map.nearest_lane(pos, LaneKind::Driving, None).unwrap())
                .unwrap()
                .points
                .project_segment(pos)
                .0
        };
        let home_pos = test.g.map().get(home).unwrap().door_pos;
        let car = test.g.get(id).unwrap().router.personal_car.unwrap();
        crate::transportation::unpark(&mut test.g, car);
        test.g
            .write::<crate::map_dynamic::BuildingInfos>()
            .get_out(home, SoulID::Human(id));
        let world = test.g.world_mut_unchecked();
        let human = &mut world.humans[id];
        human.location = Location::Vehicle(car);
        if let Some(collider) = human.collider.take() {
            test.g
                .write::<crate::transportation::TransportGrid>()
                .remove_maintain(collider.0);
        }
        let world = test.g.world_mut_unchecked();
        let human = &mut world.humans[id];
        human.router.set_test_route(
            Destination::Building(home),
            vec![RoutingStep::DriveTo(car, home_pos)],
        );
        human.household.carried.insert(ItemID::new("groceries"), 3);
        // The homebound deadline has already passed.
        human.household.homebound_until = Some(0);
        human.decision.kind = HumanDecisionKind::DepositAtHome;
        human.decision.wait = 0;
        world.vehicles[car].trans.pos = car_pos;
        world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Vehicle, home_pos);
        test.tick_fast();
        assert!(!matches!(
            test.g.get(id).unwrap().decision.kind,
            HumanDecisionKind::DepositAtHome
        ));
        let parked = (0..prototypes::TICKS_PER_MINUTE).any(|_| {
            test.tick_fast();
            test.g.get(car).unwrap().vehicle.state.is_parked()
        });
        let v = test.g.get(car).unwrap();
        assert!(
            parked,
            "the car is still on the road: {:?} at {:?}",
            v.vehicle.state, v.trans.pos
        );
        assert!(v.collider.is_none());
        assert!(crate::map_dynamic::abandoned_cars(test.g.world()).is_empty());
    }

    #[test]
    fn e1_fix_second_basket_refunds_without_overwriting_the_live_reservation() {
        let (mut test, id, _) = street();
        let (first, seller) = business(&mut test, 230.0, "commercial-l1");
        let (second, other) = business(&mut test, 330.0, "commercial-l1");
        let groceries = ItemID::new("groceries");
        let retail = ItemID::new("retail-goods");
        shelf(&test, seller, groceries, 4);
        shelf(&test, other, retail, 5);
        let (world, resources) = test.g.world_res();
        let stock = &mut world.humans[id].household;
        let request = stock.place_order(0);
        let pos = vec2(230.0, 145.0);
        let (first_basket, _) = resources
            .write::<Market>()
            .reserve_basket(id, pos, 600.0, request, 0, |candidate| {
                (candidate == seller).then_some(first)
            })
            .unwrap();
        let first_id = first_basket.id;
        assert!(stock.reserve(first_basket, resources));
        let (second_basket, _) = resources
            .write::<Market>()
            .reserve_basket(id, pos, 600.0, request, 1, |candidate| {
                (candidate == other).then_some(second)
            })
            .unwrap();
        assert!(!stock.reserve(second_basket, resources));
        assert_eq!(stock.basket, Some(first_id));
        let market = resources.read::<Market>();
        assert_eq!(market.capital(seller, groceries), 0);
        assert_eq!(market.capital(other, retail), 5);
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
            test.g.read::<Market>().basket_failures()[&super::BasketFailure::NoWalkingRoute],
            1
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
                test.g
                    .read::<DailyEconomyMetrics>()
                    .days
                    .get(&0)
                    .and_then(|day| day.baskets_filled),
                if failure == "despawn" { Some(0) } else { None }
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
        let market = test.g.read::<Market>();
        assert_eq!(market.seller_losses()[&retail], 5);
        assert!(market.household_losses().is_empty());
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
        assert_eq!(
            restored.read::<super::HouseholdClaims>().baskets,
            test.g.read::<super::HouseholdClaims>().baskets,
        );
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
        // Walking takes real time.
        let travel = 6 * 60 * prototypes::TICKS_PER_REALTIME_SECOND;
        for _ in 0..travel {
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
        for _ in 0..travel {
            test.tick_fast();
            if test.g.get(id).unwrap().location == Location::Building(park) {
                break;
            }
        }
        assert_eq!(test.g.get(id).unwrap().location, Location::Building(park));
        let restored: Simulation = Bincode::decode(&Bincode::encode(&test.g).unwrap()).unwrap();
        assert_eq!(
            restored.read::<super::HouseholdClaims>().parks,
            test.g.read::<super::HouseholdClaims>().parks,
        );
        for _ in 0..100 {
            test.tick_fast();
        }
        assert_eq!(test.g.get(id).unwrap().location, Location::Building(park));
        assert_eq!(
            test.g.read::<DailyEconomyMetrics>().days[&0].trips_by_purpose[&TripPurpose::Leisure]
                .completed,
            1
        );
        test.g.world_mut_unchecked().companies[park_company]
            .workers
            .0
            .clear();
        for _ in 0..100 {
            test.tick_fast();
        }
        let stock = &test.g.get(id).unwrap().household;
        assert!(stock.leisure_visit.is_none());
        assert!(stock.needs.leisure < 0.59);
        assert!(!test
            .g
            .read::<super::HouseholdClaims>()
            .parks
            .contains_key(&park));
    }
}
