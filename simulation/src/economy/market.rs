use std::cmp::Reverse;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

use geom::{Vec2, Vec3};
use prototypes::{
    prototypes_iter, try_prototype, GoodsCompanyID, GoodsCompanyPrototype, ItemPrototype, Money,
    Power, ServiceID, ServiceKind, Tick, TICKS_PER_HOUR,
};

use crate::economy::balance::{periodic_tick, shelf_delivery_item, shelf_delivery_units};
use crate::economy::{walking_component, JobRosters, Stamped, WalkingComponents};
use crate::economy::{
    Budget, ItemID, TripPurpose, TICKS_PER_BUDGET_DAY, WORKER_CONSUMPTION_PER_MINUTE,
};
use crate::map::{
    Building, BuildingID, BuildingKind, ElectricityNetworkID, LaneID, LaneKind, Map, MAX_ZONE_AREA,
};
use crate::map_dynamic::{BuildingInfos, Destination, ElectricityFlow, UtilityTotals};
use crate::souls::desire::WorkKind;
use crate::souls::household::{household_items, BasketId, BASKET_TRAVEL_LIMIT};
use crate::souls::human::HumanDecisionKind;
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::world::{CompanyEnt, CompanyID};
use crate::{HumanID, SoulID, World};

/// Freight by rail and station truck on an import, in percent of the item's value.
pub const IMPORT_FREIGHT_PERCENT: i64 = 25;

#[derive(Debug, Serialize, Deserialize)]
pub struct SellOrder {
    pub pos: Vec2,
    pub qty: u32,
    /// When selling less than stock, should not enable external trading
    pub stock: u32,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct BuyOrder {
    pub pos: Vec2,
    pub qty: u32,
}

#[derive(Default)]
struct ShelfIndex {
    // Keep premises indexed even when their shelves sell out.
    cells: BTreeMap<(i32, i32), BTreeSet<(SoulID, ItemID)>>,
    positions: BTreeMap<(SoulID, ItemID), (i32, i32)>,
}

impl ShelfIndex {
    fn cell(pos: Vec2) -> (i32, i32) {
        const CELL_SIZE: f32 = 256.0;
        (
            (pos.x / CELL_SIZE).floor() as i32,
            (pos.y / CELL_SIZE).floor() as i32,
        )
    }

    fn insert(&mut self, seller: SoulID, item: ItemID, pos: Vec2) {
        let cell = Self::cell(pos);
        if self.positions.get(&(seller, item)) != Some(&cell) {
            self.remove(seller, item);
            self.cells.entry(cell).or_default().insert((seller, item));
            self.positions.insert((seller, item), cell);
        }
    }

    fn remove(&mut self, seller: SoulID, item: ItemID) {
        if let Some(cell) = self.positions.remove(&(seller, item)) {
            if let Entry::Occupied(mut entries) = self.cells.entry(cell) {
                entries.get_mut().remove(&(seller, item));
                if entries.get().is_empty() {
                    entries.remove();
                }
            }
        }
    }

    fn nearby(&self, origin: Vec2, radius: f32) -> BTreeSet<SoulID> {
        let extent = Vec2::splat(radius);
        let (min_x, min_y) = Self::cell(origin - extent);
        let (max_x, max_y) = Self::cell(origin + extent);
        let mut sellers = BTreeSet::new();
        for x in min_x..=max_x {
            for (_, entries) in self.cells.range((x, min_y)..=(x, max_y)) {
                sellers.extend(entries.iter().map(|&(seller, _)| seller));
            }
        }
        sellers
    }
}

impl WalkingComponents {
    fn lane_component(&mut self, map: &Map, lane: LaneID) -> LaneID {
        if self.generation != map.walking_topology_generation {
            self.labels.clear();
            self.generation = map.walking_topology_generation;
        }
        walking_component(map, &mut self.labels, lane)
    }

    pub(crate) fn component_at(&mut self, map: &Map, pos: Vec3) -> Option<LaneID> {
        let lane = map.nearest_lane(pos, LaneKind::Walking, None)?;
        Some(self.lane_component(map, lane))
    }

    pub(crate) fn reaches_building(
        &mut self,
        map: &Map,
        origin: LaneID,
        target: &Building,
    ) -> bool {
        let Some(road) = target.connected_road.and_then(|road| map.get(road)) else {
            return false;
        };
        road.lanes_iter().any(|(lane, kind)| {
            kind == LaneKind::Walking && self.lane_component(map, lane) == origin
        }) && self.component_at(map, target.door_pos) == Some(origin)
    }
}

#[derive(Serialize, Deserialize)]
pub struct SingleMarket {
    // todo: change i32 to Quantity
    capital: BTreeMap<SoulID, i32>,
    buy_orders: Stamped<BTreeMap<SoulID, BuyOrder>>,
    sell_orders: BTreeMap<SoulID, SellOrder>,
    pub ext_value: Money,
    optout_exttrade: bool,
}

impl SingleMarket {
    pub fn new(ext_value: Money, optout_exttrade: bool) -> Self {
        Self {
            capital: Default::default(),
            buy_orders: Default::default(),
            sell_orders: Default::default(),
            ext_value,
            optout_exttrade,
        }
    }

    pub fn capital(&self, soul: SoulID) -> Option<i32> {
        self.capital.get(&soul).copied()
    }
    pub fn buy_order(&self, soul: SoulID) -> Option<&BuyOrder> {
        self.buy_orders.get(&soul)
    }
    pub(crate) fn buy_orders(&self) -> impl Iterator<Item = (SoulID, &BuyOrder)> {
        self.buy_orders.iter().map(|(&soul, order)| (soul, order))
    }
    pub fn optout_exttrade(&self) -> bool {
        self.optout_exttrade
    }
    pub fn sell_order(&self, soul: SoulID) -> Option<&SellOrder> {
        self.sell_orders.get(&soul)
    }

    pub fn capital_map(&self) -> &BTreeMap<SoulID, i32> {
        &self.capital
    }
}

/// Market handles good exchanging between souls themselves and the external market.
/// When goods are exchanges between souls, money is not involved.
/// When goods are exchanged with the external market, money is involved.
#[derive(Serialize, Deserialize)]
pub struct Market {
    markets: BTreeMap<ItemID, SingleMarket>,
    #[serde(default)]
    household_losses: BTreeMap<ItemID, u64>,
    seller_losses: BTreeMap<ItemID, u64>,
    basket_failures: BTreeMap<BasketFailure, u64>,
    shelf_refills: BTreeSet<SoulID>,
    reserved_stock: BTreeMap<(SoulID, ItemID), u32>,
    #[serde(skip)]
    shelves: Option<ShelfIndex>,
    #[serde(skip)]
    reservation_tick: u64,
    #[serde(skip)]
    reserved_this_tick: BTreeSet<SoulID>,
    #[serde(skip)]
    human_buyers: Option<BTreeMap<SoulID, BTreeSet<HumanID>>>,
    // reuse the trade vec to avoid allocations
    #[serde(skip)]
    all_trades: Vec<Trade>,
    // reuse the potential vec to avoid allocations
    #[serde(skip)]
    potential: Vec<(Trade, f32, (bool, HiringPriority))>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum HiringPriority {
    Power,
    Water,
    Sewage,
    Civic,
    /// A warehouse seats its first driver like a civic service, so a new
    /// depot runs trucks in a fully employed city.
    Freight,
    Support,
    Standard,
}

impl HiringPriority {
    fn for_company(id: GoodsCompanyID) -> Self {
        if id
            .prototype()
            .power_production
            .is_some_and(|power| power > Power::ZERO)
        {
            return Self::Power;
        }
        match try_prototype(ServiceID::from(id)).map(|service| service.service_kind) {
            Some(ServiceKind::WaterPump) => Self::Water,
            Some(ServiceKind::SewageOutlet) => Self::Sewage,
            Some(
                ServiceKind::Fire | ServiceKind::Health | ServiceKind::School | ServiceKind::Police,
            ) => Self::Civic,
            Some(_) => Self::Support,
            None if id.prototype().storage.is_some() => Self::Freight,
            None if id == GoodsCompanyID::new("coal-mine") => Self::Support,
            None => Self::Standard,
        }
    }
}

struct NetworkStaffing {
    power_supply: f64,
    power_demand: f64,
    pipes: UtilityTotals,
}

impl NetworkStaffing {
    fn balance(&self, priority: HiringPriority) -> Option<(f64, f64)> {
        let powered = self.power_supply >= self.power_demand && self.power_supply > 0.0;
        match priority {
            HiringPriority::Power => Some((self.power_supply, self.power_demand)),
            HiringPriority::Water if powered => {
                Some((self.pipes.water_production, self.pipes.water_demand))
            }
            HiringPriority::Sewage if powered && self.pipes.has_water() => {
                Some((self.pipes.sewage_capacity, self.pipes.water_demand))
            }
            _ => None,
        }
    }

    fn add_supply(&mut self, priority: HiringPriority, added: f64) {
        match priority {
            HiringPriority::Power => self.power_supply += added,
            HiringPriority::Water => self.pipes.water_production += added,
            HiringPriority::Sewage => self.pipes.sewage_capacity += added,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct BasketRequest {
    pub staples: u32,
    pub goods: u32,
    pub fallback: u32,
    pub excluded_seller: Option<SoulID>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BasketFailure {
    NoStockInRange,
    NoWalkingRoute,
    Expired,
    RouteInterrupted,
    SellerRemoved,
    ShopperRemoved,
    Throttled,
    SellerExcluded,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct BasketReservation {
    pub id: BasketId,
    pub shopper: HumanID,
    pub seller: SoulID,
    pub building: BuildingID,
    pub expires: u64,
    pub lines: Vec<(ItemID, u32)>,
    shelf_pos: Vec2,
}

#[derive(PartialOrd, Ord, PartialEq, Eq, Copy, Clone, Debug, Serialize, Deserialize)]
pub struct TradeTarget(pub SoulID);

debug_inspect_impl!(TradeTarget);

#[derive(Inspect, Copy, Clone, Debug, Serialize, Deserialize)]
pub struct Trade {
    pub buyer: TradeTarget,
    pub seller: TradeTarget,
    pub qty: i32,
    pub kind: ItemID,
    pub money_delta: Money, // money delta from the govt point of view, positive means we gained money
}

impl Trade {
    pub(crate) fn involves(&self, soul: SoulID) -> bool {
        self.buyer.0 == soul || self.seller.0 == soul
    }
}

pub fn find_trade_place(target: TradeTarget, binfos: &BuildingInfos) -> Option<BuildingID> {
    binfos.building_owned_by(target.0)
}

impl Default for Market {
    fn default() -> Self {
        let prices = calculate_prices(1.25);
        Self {
            markets: prototypes_iter::<ItemPrototype>()
                .map(|v| (v.id, SingleMarket::new(prices[&v.id], v.optout_exttrade)))
                .collect(),
            household_losses: Default::default(),
            seller_losses: Default::default(),
            basket_failures: Default::default(),
            shelf_refills: Default::default(),
            reserved_stock: Default::default(),
            shelves: None,
            reservation_tick: 0,
            reserved_this_tick: Default::default(),
            human_buyers: None,
            all_trades: Default::default(),
            potential: Default::default(),
        }
    }
}

impl Market {
    pub fn m(&mut self, kind: ItemID) -> &mut SingleMarket {
        self.markets.get_mut(&kind).unwrap()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ItemID, &SingleMarket)> {
        self.markets.iter()
    }

    /// Called when an agent tells the world it wants to sell something
    /// If an order is already placed, it will be updated.
    /// Beware that you need capital to sell anything, using produce.
    pub fn sell(&mut self, soul: SoulID, near: Vec2, kind: ItemID, qty: u32, stock: u32) {
        log::debug!("{:?} sell {:?} {:?} near {:?}", soul, qty, kind, near);
        if household_items().contains(&kind) {
            if let Some(shelves) = &mut self.shelves {
                shelves.insert(soul, kind, near);
            }
        }
        self.m(kind).sell_orders.insert(
            soul,
            SellOrder {
                pos: near,
                qty,
                stock,
            },
        );
    }

    pub fn sell_all(&mut self, soul: SoulID, near: Vec2, kind: ItemID, stock: u32) {
        let capital = self.markets[&kind].capital(soul).unwrap_or(0);
        if capital <= 0 {
            return;
        }
        let delivery = shelf_delivery_item();
        let [groceries, retail] = household_items();
        let mut stock = stock;
        let qty = if kind == delivery {
            self.unpack_delivery(soul, near, capital, stock)
        } else if kind == retail && stock > 0 && self.has_delivery_stockroom(soul) {
            // Both direct output and opened packages use the same shelf allowance.
            stock = self.shelf_allowance(soul, retail, stock);
            let advertised = self.markets[&kind]
                .sell_order(soul)
                .map_or(0, |order| order.qty);
            let limit = self
                .shelf_room(soul, retail, stock as i32, 0)
                .max(advertised as i32);
            self.discard_shelf_overflow(soul, kind, capital, limit)
        } else if kind == groceries && stock > 0 && !self.has_delivery_stockroom(soul) {
            // Direct output is advertised in full after each batch. A batch
            // can overshoot its limit when shoppers reserve part of a batch.
            // Keep the same one-batch headroom as the recipe's output gate.
            let advertised = self.markets[&kind]
                .sell_order(soul)
                .map_or(0, |order| order.qty) as i32;
            let batch = capital - advertised;
            if batch > 0 && self.reserved_stock.contains_key(&(soul, kind)) {
                let limit = self.shelf_room(soul, kind, stock as i32 + batch, 0);
                self.discard_shelf_overflow(soul, kind, capital, limit.max(advertised))
            } else {
                capital
            }
        } else {
            capital
        };
        if qty > 0 || (stock > 0 && household_items().contains(&kind)) {
            self.sell(soul, near, kind, qty as u32, stock);
        } else {
            self.m(kind).sell_orders.remove(&soul);
        }
    }

    fn has_delivery_stockroom(&self, seller: SoulID) -> bool {
        self.markets
            .get(&shelf_delivery_item())
            .is_some_and(|market| market.capital(seller).is_some())
    }

    fn shelf_allowance(&self, seller: SoulID, item: ItemID, default: u32) -> u32 {
        self.markets[&item]
            .sell_order(seller)
            .filter(|order| order.stock > 0)
            .map_or(default, |order| order.stock)
    }

    fn discard_shelf_overflow(
        &mut self,
        seller: SoulID,
        item: ItemID,
        stock: i32,
        limit: i32,
    ) -> i32 {
        let excess = (stock - limit).max(0);
        if excess > 0 {
            *self.seller_losses.entry(item).or_default() += excess as u64;
            return self.produce(seller, item, -excess);
        }
        stock
    }

    fn shelf_room(&self, seller: SoulID, item: ItemID, capacity: i32, stock: i32) -> i32 {
        let reserved = self
            .reserved_stock
            .get(&(seller, item))
            .copied()
            .unwrap_or(0) as i32;
        (capacity - reserved - stock).max(0)
    }

    /// Open only enough wholesale packages to fill free shelf space. Closed
    /// packages remain in the stockroom and pause the recipe when it fills.
    fn unpack_delivery(
        &mut self,
        seller: SoulID,
        pos: Vec2,
        mut packages: i32,
        capacity: u32,
    ) -> i32 {
        let [groceries, retail] = household_items();
        let [staples_per_package, goods_per_package] = shelf_delivery_units(1);
        let left = packages;
        for (item, per_package) in [
            (groceries, staples_per_package),
            (retail, goods_per_package),
        ] {
            let limit = self.shelf_allowance(seller, item, capacity * per_package as u32) as i32;
            let stock = self.markets[&item].capital(seller).unwrap_or(0);
            let opened = packages.min(self.shelf_room(seller, item, limit, stock) / per_package);
            if opened > 0 {
                packages -= opened;
                let qty = self.produce(seller, item, opened * per_package);
                self.sell(seller, pos, item, qty as u32, limit as u32);
            }
        }
        self.produce(seller, shelf_delivery_item(), packages - left)
    }

    fn refill_shelves(&mut self) {
        let delivery = shelf_delivery_item();
        for seller in std::mem::take(&mut self.shelf_refills) {
            let Some(order) = self
                .markets
                .get(&delivery)
                .and_then(|market| market.sell_order(seller))
            else {
                continue;
            };
            self.sell_all(seller, order.pos, delivery, order.stock);
        }
    }

    pub(crate) fn index_human_buyers(&mut self, world: &crate::World) {
        self.human_buyers.get_or_insert_with(|| {
            let mut by_seller = BTreeMap::<_, BTreeSet<_>>::new();
            let jobs = ItemID::new("job-opening");
            for (buyer, human) in world.humans.iter() {
                for trade in human
                    .bought
                    .0
                    .iter()
                    .filter(|(&item, _)| item != jobs)
                    .flat_map(|(_, trades)| trades)
                {
                    by_seller.entry(trade.seller.0).or_default().insert(buyer);
                }
            }
            by_seller
        });
    }

    pub(crate) fn take_human_buyers(&mut self, seller: SoulID) -> BTreeSet<HumanID> {
        self.human_buyers
            .as_mut()
            .and_then(|buyers| buyers.remove(&seller))
            .unwrap_or_default()
    }

    pub(crate) fn forget_human_buyer(&mut self, seller: SoulID, buyer: HumanID) {
        if let Some(index) = &mut self.human_buyers {
            if let Entry::Occupied(mut buyers) = index.entry(seller) {
                buyers.get_mut().remove(&buyer);
                if buyers.get().is_empty() {
                    buyers.remove();
                }
            }
        }
    }

    /// An agent was removed from the world, we need to clean after him
    pub fn remove(&mut self, soul: SoulID) {
        self.retain_cached_trades(|trade| !trade.involves(soul));
        self.remove_account(soul);
    }

    pub(crate) fn remove_cached_trades(&mut self, souls: &BTreeSet<SoulID>) {
        self.retain_cached_trades(|trade| {
            !souls.contains(&trade.buyer.0) && !souls.contains(&trade.seller.0)
        });
    }

    fn retain_cached_trades(&mut self, retain: impl Fn(&Trade) -> bool) {
        self.all_trades.retain(&retain);
        self.potential.retain(|(trade, _, _)| retain(trade));
    }

    pub(crate) fn remove_account(&mut self, soul: SoulID) {
        if let Some(buyers) = &mut self.human_buyers {
            buyers.remove(&soul);
        }
        if let Some(shelves) = &mut self.shelves {
            for item in household_items() {
                shelves.remove(soul, item);
            }
        }
        for item in household_items() {
            self.reserved_stock.remove(&(soul, item));
        }
        self.shelf_refills.remove(&soul);
        let household = household_items();
        let delivery = shelf_delivery_item();
        for (&item, market) in &mut self.markets {
            market.sell_orders.remove(&soul);
            market.buy_orders.remove(&soul);
            if let Some(stock) = market.capital.remove(&soul) {
                if stock > 0
                    && !matches!(soul, SoulID::Human(_))
                    && (household.contains(&item) || item == delivery)
                {
                    *self.seller_losses.entry(item).or_default() += stock as u64;
                }
            }
        }
    }

    /// Called when an agent tells the world it wants to buy something
    /// If an order is already placed, it will be updated.
    pub fn buy(&mut self, soul: SoulID, near: Vec2, kind: ItemID, qty: u32) {
        log::debug!("{:?} buy {:?} {:?} near {:?}", soul, qty, kind, near);

        self.m(kind)
            .buy_orders
            .insert(soul, BuyOrder { pos: near, qty });
    }

    pub fn buy_until(&mut self, soul: SoulID, near: Vec2, kind: ItemID, qty: u32) {
        let c = self.markets[&kind].capital(soul).unwrap_or(0);
        if c >= qty as i32 {
            return;
        }
        self.buy(soul, near, kind, (qty as i64 - i64::from(c)) as u32);
    }

    /// Stock owned by the agent. Direct grocery producers still hold basket
    /// reservations on their premises, so they occupy recipe output storage.
    pub fn capital(&self, soul: SoulID, kind: ItemID) -> i32 {
        let market = &self.markets[&kind];
        let capital = market.capital(soul).unwrap_or(0);
        if kind == household_items()[0] {
            if let Some(&reserved) = self.reserved_stock.get(&(soul, kind)) {
                if market.sell_order(soul).is_some_and(|order| order.stock > 0)
                    && !self.has_delivery_stockroom(soul)
                {
                    return capital + reserved as i32;
                }
            }
        }
        capital
    }

    /// Registers a soul to the market, not obligatory
    pub fn register(&mut self, soul: SoulID, kind: ItemID) {
        self.m(kind).capital.entry(soul).or_default();
    }

    /// Called whenever an agent (like a farm) produces something on it's own
    /// for example wheat is harvested or turned into flour. Returns the new quantity owned.
    pub fn produce(&mut self, soul: SoulID, kind: ItemID, delta: i32) -> i32 {
        log::debug!("{:?} produced {:?} {:?}", soul, delta, kind);

        let v = self.m(kind).capital.entry(soul).or_default();
        *v += delta;
        *v
    }

    pub(crate) fn hide_freight_stock(&mut self, source: SoulID, item: ItemID) {
        self.m(item).sell_orders.remove(&source);
    }

    pub(crate) fn receive_freight(&mut self, destination: SoulID, item: ItemID, units: u32) {
        self.produce(destination, item, units as i32);
        if let Entry::Occupied(mut buyer) = self.m(item).buy_orders.entry(destination) {
            let order = buyer.get_mut();
            order.qty = order.qty.saturating_sub(units);
            if order.qty == 0 {
                buyer.remove();
            }
        }
    }

    pub(crate) fn freight_export(
        &self,
        source: SoulID,
        station: SoulID,
        item: ItemID,
        units: u32,
    ) -> Trade {
        Trade {
            buyer: TradeTarget(station),
            seller: TradeTarget(source),
            qty: units as i32,
            kind: item,
            money_delta: self.markets[&item].ext_value * i64::from(units),
        }
    }

    /// An import delivered by a station truck. The price adds freight by rail
    /// and road to the item's value.
    pub(crate) fn freight_import(
        &self,
        station: SoulID,
        buyer: SoulID,
        item: ItemID,
        units: u32,
    ) -> Trade {
        let units_with_freight = i64::from(units) * (100 + IMPORT_FREIGHT_PERCENT);
        Trade {
            buyer: TradeTarget(buyer),
            seller: TradeTarget(station),
            qty: units as i32,
            kind: item,
            money_delta: -(self.markets[&item].ext_value * units_with_freight / 100),
        }
    }

    /// Writes off freight stock whose owner is gone: a cancelled load or queued
    /// batch of a removed source, or the on-hand stock of a removed company.
    pub(crate) fn record_freight_loss(&mut self, item: ItemID, units: u32) {
        *self.seller_losses.entry(item).or_default() += u64::from(units);
    }

    /// Match minimum staffing before the ordinary job market. Workers with a job
    /// can transfer only on the hourly staffing pass, after unemployed applicants.
    pub(super) fn prepare_job_matches(
        &mut self,
        world: &mut World,
        resources: &Resources,
        job: ItemID,
        tick: Tick,
    ) -> Vec<Trade> {
        let Some(market) = self.markets.get_mut(&job) else {
            return Vec::new();
        };
        if !market.buy_orders.is_empty() {
            // A roster can already contain an applicant before Work is assigned.
            let mut rosters = resources.write::<JobRosters>();
            let (_, rebuilt) = rosters.refresh(world);
            let JobRosters {
                sorted,
                orders,
                applicants,
                stale,
                ..
            } = &mut *rosters;
            // Unchanged orders and rosters give the same applicants.
            if rebuilt || *orders != Some(market.buy_orders.stamp()) {
                *orders = Some(market.buy_orders.stamp());
                applicants.clear();
                stale.clear();
                // Buyers ascend, so the sorted roster is merged in step.
                let mut rostered = sorted.iter().peekable();
                let mut on_roster = |id: HumanID| {
                    while rostered.next_if(|&&worker| worker < id).is_some() {}
                    rostered.peek() == Some(&&id)
                };
                for (&buyer, order) in &market.buy_orders {
                    match buyer {
                        SoulID::Human(id) if order.qty == 1 && !on_roster(id) => {
                            applicants.push(id)
                        }
                        _ => stale.push(buyer),
                    }
                }
            }
            let ineligible = applicants.iter().filter(|&&id| {
                !world.humans.get(id).is_some_and(|human| {
                    human.work.is_none() && human.bought.0.get(&job).is_none_or(Vec::is_empty)
                })
            });
            let stale: Vec<_> = stale
                .iter()
                .copied()
                .chain(ineligible.map(|&id| SoulID::Human(id)))
                .collect();
            // Removing an order renews the stamp, so the next tick collects again.
            for buyer in stale {
                market.buy_orders.remove(&buyer);
            }
        }
        let phase = periodic_tick(&resources.read::<Budget>(), tick);
        // Water and sewage sample at phase 1. Evaluate transfers just after that sample.
        let hourly = phase >= TICKS_PER_HOUR && phase % TICKS_PER_HOUR == 2;
        if market.buy_orders.is_empty() && !hourly {
            return Vec::new();
        }

        let map = resources.read::<Map>();
        let infos = resources.read::<BuildingInfos>();
        let electricity = resources.read::<ElectricityFlow>();
        let residences = resources.read::<Residences>();
        let mut vacancies = Vec::new();
        for (&seller, order) in &market.sell_orders {
            let SoulID::GoodsCompany(id) = seller else {
                continue;
            };
            let Some(company) = world.companies.get(id) else {
                continue;
            };
            if !market
                .capital(seller)
                .is_some_and(|capital| capital >= order.qty as i32)
                || !map
                    .get(company.comp.building)
                    .is_some_and(|building| !building.is_orphaned())
                || infos.owner(company.comp.building) != Some(seller)
            {
                continue;
            }
            let priority = HiringPriority::for_company(company.comp.proto);
            match priority {
                HiringPriority::Power | HiringPriority::Water | HiringPriority::Sewage => {
                    vacancies.push((priority, id));
                }
                HiringPriority::Civic | HiringPriority::Freight if company.workers.0.is_empty() => {
                    vacancies.push((priority, id));
                }
                _ => {}
            }
        }
        vacancies.sort_by_cached_key(|&(priority, id)| {
            let pos = market.sell_orders[&SoulID::GoodsCompany(id)].pos;
            let nearest = market
                .buy_orders
                .values()
                .map(|order| OrderedFloat(pos.distance2(order.pos)))
                .min();
            (priority, nearest, id)
        });

        let mut network_staffing = BTreeMap::new();
        let load_staffing = |world: &World, network| {
            Self::network_staffing(&map, world, &infos, &electricity, &residences, network)
        };
        let mut donors = None;
        let mut trades = Vec::new();
        for (priority, id) in vacancies {
            let seller = SoulID::GoodsCompany(id);
            let company = &world.companies[id];
            let workplace = company.comp.building;
            let building = &map.buildings()[workplace];
            let order = &market.sell_orders[&seller];
            let pos = order.pos;
            let mut open = (company.comp.max_workers as usize)
                .saturating_sub(company.workers.0.len())
                .min(order.qty as usize);

            let rate = Self::utility_rate(company, building, priority);
            let capacity = if matches!(priority, HiringPriority::Civic | HiringPriority::Freight) {
                open = open.min(1);
                None
            } else {
                let Some(network) = map.electricity.net_id(workplace) else {
                    continue;
                };
                let capacity = network_staffing
                    .entry(network)
                    .or_insert_with(|| load_staffing(world, network));
                let Some((supply, demand)) = capacity.balance(priority) else {
                    continue;
                };
                if rate <= 0.0 {
                    continue;
                }
                let tolerance = if priority == HiringPriority::Power {
                    0.0
                } else {
                    demand * 1e-6
                };
                let shortage = (demand - supply - tolerance).max(0.0);
                open = open.min((shortage / rate).ceil() as usize);
                Some(capacity)
            };

            let mut hired = 0;
            for _ in 0..open {
                let applicant = market
                    .buy_orders
                    .iter()
                    .min_by_key(|&(buyer, order)| (OrderedFloat(pos.distance2(order.pos)), *buyer))
                    .map(|(&buyer, _)| buyer);
                let buyer = match applicant {
                    Some(buyer) => {
                        market.buy_orders.remove(&buyer);
                        *market.capital.entry(buyer).or_default() += 1;
                        buyer
                    }
                    None if hourly => {
                        let donors = donors.get_or_insert_with(|| {
                            // Prefer the fullest rosters and preserve at least half of each
                            // employer's staffing capacity, including local delivery drivers.
                            world
                                .companies
                                .iter()
                                .filter_map(|(id, company)| {
                                    if HiringPriority::for_company(company.comp.proto)
                                        != HiringPriority::Standard
                                        || company.workers.0.len()
                                            <= (company.comp.max_workers as usize).div_ceil(2)
                                    {
                                        return None;
                                    }
                                    let building = map
                                        .get(company.comp.building)
                                        .filter(|building| !building.is_orphaned())?;
                                    let worker = Self::transferable_worker(world, company)?;
                                    Some((
                                        Reverse(company.workers.0.len()),
                                        id,
                                        worker,
                                        building.door_pos.xy(),
                                    ))
                                })
                                .collect::<Vec<_>>()
                        });
                        let Some((index, _)) = donors.iter().enumerate().min_by_key(
                            |&(_, &(staffing, id, _, door))| {
                                (staffing, OrderedFloat(pos.distance2(door)), id)
                            },
                        ) else {
                            break;
                        };
                        let (_, donor, worker, _) = donors.swap_remove(index);
                        Self::release_worker(market, world, &map, job, donor, worker, workplace);
                        SoulID::Human(worker)
                    }
                    None => break,
                };
                trades.push(Self::hire_worker(market, job, seller, buyer));
                hired += 1;
            }
            if let Some(capacity) = capacity {
                capacity.add_supply(priority, hired as f64 * rate);
            }
        }

        // Check for reverse transfers once a day. Keep at least one worker plus
        // a 25% network reserve after the move, so small fluctuations do not
        // trigger transfers back and forth.
        if phase >= TICKS_PER_BUDGET_DAY
            && phase % TICKS_PER_BUDGET_DAY == 2
            && trades.is_empty()
            && market.buy_orders.is_empty()
        {
            let recipient = market.sell_orders.iter().find_map(|(&seller, order)| {
                let SoulID::GoodsCompany(id) = seller else {
                    return None;
                };
                let company = world.companies.get(id)?;
                let workplace = company.comp.building;
                (HiringPriority::for_company(company.comp.proto) == HiringPriority::Standard
                    && company.workers.0.len() < company.comp.max_workers as usize
                    && market
                        .capital(seller)
                        .is_some_and(|capital| capital >= order.qty as i32)
                    && map
                        .get(workplace)
                        .is_some_and(|building| !building.is_orphaned()))
                .then_some((seller, workplace))
            });
            if let Some((seller, workplace)) = recipient {
                let surplus = world.companies.iter().find_map(|(id, company)| {
                    let priority = HiringPriority::for_company(company.comp.proto);
                    if company.workers.0.len() <= 1
                        || !matches!(
                            priority,
                            HiringPriority::Power | HiringPriority::Water | HiringPriority::Sewage
                        )
                    {
                        return None;
                    }
                    let building = map.get(company.comp.building)?;
                    let rate = Self::utility_rate(company, building, priority);
                    let network = map.electricity.net_id(building.id)?;
                    let capacity = network_staffing
                        .entry(network)
                        .or_insert_with(|| load_staffing(world, network));
                    let (supply, demand) = capacity.balance(priority)?;
                    if building.is_orphaned() || rate <= 0.0 || supply - rate < 1.25 * demand {
                        return None;
                    }
                    Some((id, Self::transferable_worker(world, company)?))
                });
                if let Some((donor, worker)) = surplus {
                    Self::release_worker(market, world, &map, job, donor, worker, workplace);
                    trades.push(Self::hire_worker(
                        market,
                        job,
                        seller,
                        SoulID::Human(worker),
                    ));
                }
            }
        }
        trades
    }

    fn transferable_worker(world: &World, company: &CompanyEnt) -> Option<HumanID> {
        company.workers.0.iter().copied().find(|worker| {
            company.comp.driver != Some(*worker)
                && world
                    .humans
                    .get(*worker)
                    .and_then(|human| human.work.as_ref())
                    .is_some_and(|work| {
                        work.workplace == company.comp.building
                            && matches!(work.kind, WorkKind::Worker)
                    })
        })
    }

    fn release_worker(
        market: &mut SingleMarket,
        world: &mut World,
        map: &Map,
        job: ItemID,
        donor: CompanyID,
        worker: HumanID,
        workplace: BuildingID,
    ) {
        let company = &mut world.companies[donor];
        let previous = company.comp.building;
        company.workers.0.retain(|&id| id != worker);
        let capital = market
            .capital
            .entry(SoulID::GoodsCompany(donor))
            .or_default();
        *capital += 1;
        market.sell_orders.insert(
            SoulID::GoodsCompany(donor),
            SellOrder {
                pos: map.buildings()[previous].door_pos.xy(),
                qty: *capital as u32,
                stock: 0,
            },
        );

        world.edit_human(worker, |human| {
            human.work = None;
            human.household.attended_shift = None;
            human.bought.0.remove(&job);

            let old_dest = Destination::Building(previous);
            let new_dest = Destination::Building(workplace);
            // A worker's commute is a plain GoTo. Household and leisure actions
            // own their own routes, including reservations and carried purchases.
            if matches!(human.decision.kind, HumanDecisionKind::GoTo(dest) if dest == old_dest) {
                human.decision.kind = HumanDecisionKind::GoTo(new_dest);
                human.decision.wait = 0;
                if human.router.target_dest == Some(old_dest)
                    && human.router.trip_purpose == TripPurpose::Work
                {
                    human.router.go_to_for(new_dest, TripPurpose::Work);
                }
            }
        });
    }

    fn hire_worker(market: &mut SingleMarket, job: ItemID, seller: SoulID, buyer: SoulID) -> Trade {
        let order = market.sell_orders.get_mut(&seller).unwrap();
        order.qty -= 1;
        if order.qty == 0 {
            market.sell_orders.remove(&seller);
        }
        *market.capital.get_mut(&seller).unwrap() -= 1;
        Trade {
            buyer: TradeTarget(buyer),
            seller: TradeTarget(seller),
            qty: 1,
            kind: job,
            money_delta: Money::ZERO,
        }
    }

    fn utility_rate(company: &CompanyEnt, building: &Building, priority: HiringPriority) -> f64 {
        let proto = company.comp.proto.prototype();
        let rate = match priority {
            HiringPriority::Power => {
                (proto.power_production.unwrap_or(Power::ZERO).watts()
                    - proto.power_consumption.unwrap_or(Power::ZERO).watts()) as f64
            }
            HiringPriority::Water => f64::from(proto.water_production.unwrap_or_default()),
            HiringPriority::Sewage => f64::from(proto.sewage_capacity.unwrap_or_default()),
            _ => 0.0,
        };
        let zone = building
            .zone
            .as_ref()
            .map_or(1.0, |zone| f64::from(zone.area) / f64::from(MAX_ZONE_AREA));
        rate * zone / f64::from(proto.n_workers.max(1))
    }

    fn network_staffing(
        map: &Map,
        world: &World,
        infos: &BuildingInfos,
        electricity: &ElectricityFlow,
        residences: &Residences,
        network: ElectricityNetworkID,
    ) -> NetworkStaffing {
        let power = electricity.network_stats(network);
        let mut staffing = NetworkStaffing {
            power_supply: power.produced_power.watts() as f64,
            power_demand: power.consumed_power.watts() as f64,
            pipes: UtilityTotals::default(),
        };
        // Calculate pipe demand and supply from the same live network. A road edit
        // can retain the network ID while changing which buildings it serves.
        for &id in &map.electricity.networks[&network].buildings {
            let building = &map.buildings()[id];
            if building.is_orphaned() {
                continue;
            }
            let demand = match building.kind {
                BuildingKind::House => residences
                    .by_building
                    .get(&id)
                    .and_then(|home| try_prototype(home.prototype))
                    .and_then(|proto| proto.water_consumption),
                BuildingKind::GoodsCompany(proto) => {
                    try_prototype(proto).and_then(|proto| proto.water_consumption)
                }
                _ => None,
            };
            if let Some(demand) = demand.filter(|rate| rate.is_finite() && *rate >= 0.0) {
                staffing.pipes.water_demand += f64::from(demand);
            }
            let Some(SoulID::GoodsCompany(owner)) = infos.owner(id) else {
                continue;
            };
            let Some(company) = world.companies.get(owner) else {
                continue;
            };
            let priority = HiringPriority::for_company(company.comp.proto);
            if matches!(priority, HiringPriority::Water | HiringPriority::Sewage) {
                let rate = Self::utility_rate(company, building, priority);
                staffing.add_supply(priority, rate.max(0.0) * company.workers.0.len() as f64);
            }
        }
        staffing
    }

    /// Returns a list of buy and sell orders matched together.
    /// A trade updates the buy and sell orders from the market, and the capital of the buyers and sellers.
    /// A trade can only be completed if the seller has enough capital.
    /// Please do not keep the trades around much, it needs to be destroyed by the next time you call this function.
    pub fn make_trades(&mut self, find_external: impl Fn(Vec2) -> Option<SoulID>) -> &[Trade] {
        self.make_trades_filtered(find_external, |_, _| true, &BTreeMap::new())
    }

    pub(super) fn make_trades_filtered(
        &mut self,
        find_external: impl Fn(Vec2) -> Option<SoulID>,
        mut can_pick_up: impl FnMut(SoulID, SoulID) -> bool,
        job_priorities: &BTreeMap<SoulID, (bool, HiringPriority)>,
    ) -> &[Trade] {
        self.refill_shelves();
        self.all_trades.clear();
        let local = household_items();
        let job = ItemID::new("job-opening");

        for (&kind, market) in &mut self.markets {
            if market.optout_exttrade && market.buy_orders.is_empty() {
                continue;
            }
            // Naive O(n²) alg
            // We don't immediatly apply the trades, because we want to find the nearest-positioned trades
            for (&seller, sorder) in &market.sell_orders {
                let qty_sell = sorder.qty as i32;

                let capital_sell = unwrap_or!(market.capital(seller), continue);
                if qty_sell > capital_sell {
                    continue;
                }
                let priority = if kind == job {
                    job_priorities
                        .get(&seller)
                        .copied()
                        .unwrap_or((true, HiringPriority::Standard))
                } else {
                    (true, HiringPriority::Standard)
                };
                for (&buyer, &border) in &market.buy_orders {
                    if seller == buyer {
                        log::warn!(
                            "{:?} is both selling and buying same commodity: {:?}",
                            seller,
                            kind
                        );
                        continue;
                    }
                    let qty_buy = border.qty as i32;
                    if qty_buy > qty_sell {
                        continue;
                    }
                    if local.contains(&kind) && !can_pick_up(buyer, seller) {
                        continue;
                    }
                    let score = sorder.pos.distance2(border.pos);
                    self.potential.push((
                        Trade {
                            buyer: TradeTarget(buyer),
                            seller: TradeTarget(seller),
                            qty: qty_buy,
                            kind,
                            money_delta: Money::ZERO,
                        },
                        score,
                        priority,
                    ))
                }
            }
            self.potential.sort_unstable_by_key(|(trade, distance, priority)| {
                (
                    *priority,
                    OrderedFloat(*distance),
                    trade.seller,
                    trade.buyer,
                    trade.kind,
                )
            });
            let SingleMarket {
                buy_orders,
                sell_orders,
                capital,
                optout_exttrade,
                ext_value,
                ..
            } = market;

            self.all_trades
                .extend(self.potential.drain(..).filter_map(|(trade, _, _)| {
                    let cap_seller = capital.entry(trade.seller.0).or_default();
                    if *cap_seller < trade.qty {
                        return None;
                    }

                    let Entry::Occupied(border) = buy_orders.entry(trade.buyer.0) else {
                        return None;
                    };

                    let sorderent = sell_orders.entry(trade.seller.0);

                    let mut sorderocc = match sorderent {
                        Entry::Vacant(_) => return None,
                        Entry::Occupied(o) => o,
                    };

                    let sorder = sorderocc.get_mut();

                    if sorder.qty < trade.qty as u32 {
                        return None;
                    }

                    border.remove();
                    sorder.qty -= trade.qty as u32;

                    if sorder.qty == 0 && !(local.contains(&kind) && sorder.stock > 0) {
                        sorderocc.remove();
                    }

                    // Safety: buyer cannot be the same as seller
                    *cap_seller -= trade.qty;
                    *capital.entry(trade.buyer.0).or_default() += trade.qty;

                    Some(trade)
                }));

            // External trading. Imports are physical: open buy orders wait for
            // the cargo book to order them by rail and truck them from a station.
            if !*optout_exttrade {
                // Seller surplus goes to external trading
                for (&seller, order) in sell_orders.iter_mut() {
                    let qty_sell = order.qty as i32 - order.stock as i32;
                    if qty_sell <= 0 {
                        continue;
                    }
                    if capital.get(&seller).copied().unwrap_or_default() < qty_sell {
                        log::warn!("{:?} is selling more than it has: {:?}", &seller, qty_sell);
                        continue;
                    }
                    let Some(ext @ SoulID::FreightStation(_)) = find_external(order.pos) else {
                        continue;
                    };
                    *capital.get_mut(&seller).unwrap() -= qty_sell;
                    order.qty -= qty_sell as u32;

                    self.all_trades.push(Trade {
                        buyer: TradeTarget(ext),
                        seller: TradeTarget(seller),
                        qty: qty_sell,
                        kind,
                        money_delta: *ext_value * qty_sell as i64,
                    });
                }
            }
        }

        for trade in &self.all_trades {
            if local.contains(&trade.kind) {
                self.shelf_refills.insert(trade.seller.0);
            }
            if trade.kind != job {
                if let (Some(by_seller), SoulID::Human(buyer)) =
                    (&mut self.human_buyers, trade.buyer.0)
                {
                    by_seller.entry(trade.seller.0).or_default().insert(buyer);
                }
            }
        }
        &self.all_trades
    }

    /// Reserves a basket from one shelf. Rank useful food and total quantity
    /// before distance, and perform the route preflight in that order.
    pub(crate) fn reserve_basket(
        &mut self,
        shopper: HumanID,
        origin: Vec2,
        radius: f32,
        request: BasketRequest,
        now: u64,
        reachable: impl FnMut(SoulID) -> Option<BuildingID>,
    ) -> Result<(BasketReservation, [u32; 2]), BasketFailure> {
        self.reserve_basket_preferred(shopper, origin, radius, request, now, None, reachable)
    }

    pub(crate) fn reserve_basket_preferred(
        &mut self,
        shopper: HumanID,
        origin: Vec2,
        radius: f32,
        request: BasketRequest,
        now: u64,
        preferred: Option<SoulID>,
        mut reachable: impl FnMut(SoulID) -> Option<BuildingID>,
    ) -> Result<(BasketReservation, [u32; 2]), BasketFailure> {
        struct Candidate {
            throttled: bool,
            seller: SoulID,
            pos: Vec2,
            distance: OrderedFloat<f32>,
            food: u32,
            goods: u32,
            missing: [u32; 2],
        }

        let [groceries, retail] = household_items();
        let shelves = self.shelves.get_or_insert_with(|| {
            let mut shelves = ShelfIndex::default();
            for item in [groceries, retail] {
                if let Some(market) = self.markets.get(&item) {
                    for (&seller, order) in &market.sell_orders {
                        shelves.insert(seller, item, order.pos);
                    }
                }
            }
            shelves
        });
        if now != self.reservation_tick {
            self.reservation_tick = now;
            self.reserved_this_tick.clear();
        }
        let available = |item, seller| {
            self.markets.get(&item).and_then(|market| {
                let order = market.sell_order(seller)?;
                let capital = market.capital(seller)?.max(0) as u32;
                Some((order.pos, order.qty.min(capital), order.stock > 0))
            })
        };
        let mut candidates = Vec::new();
        let mut excluded = false;
        for seller in shelves.nearby(origin, radius) {
            if seller == SoulID::Human(shopper) {
                continue;
            }
            let staple = available(groceries, seller);
            let other = available(retail, seller);
            let (staples_in_stock, goods_in_stock) = (
                staple.map_or(0, |(_, qty, _)| qty),
                other.map_or(0, |(_, qty, _)| qty),
            );
            let total_stock = staples_in_stock + goods_in_stock;
            let replenished_shelf =
                staple.is_some_and(|line| line.2) || other.is_some_and(|line| line.2);
            let basket_limit = if replenished_shelf {
                total_stock - total_stock / 4
            } else {
                total_stock
            };
            let reserve_for_goods =
                u32::from(request.goods > 0 && goods_in_stock > 0 && basket_limit >= 2);
            let food = staples_in_stock
                .min(request.staples)
                .min(basket_limit - reserve_for_goods);
            let goods_target = request.fallback.saturating_sub(food).max(request.goods);
            let goods = goods_in_stock.min(goods_target).min(basket_limit - food);
            if food + goods == 0 {
                continue;
            }
            let pos = if food > 0 {
                staple.unwrap().0
            } else {
                other.unwrap().0
            };
            let distance = OrderedFloat(origin.distance2(pos));
            if distance.0 <= radius.powi(2) {
                if request.excluded_seller == Some(seller) {
                    excluded = true;
                    continue;
                }
                candidates.push(Candidate {
                    throttled: self.reserved_this_tick.contains(&seller),
                    seller,
                    pos,
                    distance,
                    food,
                    goods,
                    missing: [
                        request.staples.saturating_sub(staples_in_stock),
                        request.goods.saturating_sub(goods_in_stock),
                    ],
                });
            }
        }
        if candidates.is_empty() {
            return Err(if excluded {
                BasketFailure::SellerExcluded
            } else {
                BasketFailure::NoStockInRange
            });
        }
        candidates.sort_unstable_by_key(|candidate| {
            (
                candidate.throttled,
                Reverse(candidate.food),
                Reverse(candidate.food + candidate.goods),
                preferred != Some(candidate.seller),
                candidate.distance,
                candidate.seller,
            )
        });
        let Some((candidate, building)) = candidates.into_iter().find_map(|candidate| {
            let building = reachable(candidate.seller)?;
            Some((candidate, building))
        }) else {
            return Err(BasketFailure::NoWalkingRoute);
        };
        if candidate.throttled {
            return Err(BasketFailure::Throttled);
        }

        let seller = candidate.seller;
        let lines: Vec<_> = [(groceries, candidate.food), (retail, candidate.goods)]
            .into_iter()
            .filter(|&(_, units)| units > 0)
            .collect();
        for &(item, units) in &lines {
            let market = self.m(item);
            *market.capital.get_mut(&seller).unwrap() -= units as i32;
            let order = market.sell_orders.get_mut(&seller).unwrap();
            order.qty -= units;
            if order.qty == 0 && order.stock == 0 {
                market.sell_orders.remove(&seller);
            }
            *self.reserved_stock.entry((seller, item)).or_default() += units;
        }
        // Allow one basket per seller per tick. Rounded whole units set aside
        // a quarter of a replenished shelf for later decisions.
        self.reserved_this_tick.insert(seller);
        Ok((
            BasketReservation {
                id: BasketId(common::hash_u64((shopper, now))),
                shopper,
                seller,
                building,
                expires: now + BASKET_TRAVEL_LIMIT,
                lines,
                shelf_pos: candidate.pos,
            },
            candidate.missing,
        ))
    }

    pub(crate) fn finish_basket(&mut self, basket: &BasketReservation) {
        for &(item, units) in &basket.lines {
            if let Entry::Occupied(mut reserved) = self.reserved_stock.entry((basket.seller, item))
            {
                *reserved.get_mut() -= units;
                if *reserved.get() == 0 {
                    reserved.remove();
                }
            }
        }
        self.shelf_refills.insert(basket.seller);
    }

    pub(crate) fn release_basket(&mut self, basket: BasketReservation, refund: bool) {
        self.finish_basket(&basket);
        for (item, units) in basket.lines {
            if refund {
                self.produce(basket.seller, item, units as i32);
                let (qty, stock) = self
                    .m(item)
                    .sell_order(basket.seller)
                    .map_or((0, 0), |order| (order.qty, order.stock));
                self.sell(basket.seller, basket.shelf_pos, item, qty + units, stock);
            } else {
                *self.seller_losses.entry(item).or_default() += u64::from(units);
            }
        }
    }

    /// Units written off by sellers, per item: shelf overflow and removed shelves,
    /// uncollected pickups, freight cancelled after its source was removed, and
    /// the recipe and storage stock of every removed company. Industrial items
    /// therefore include stock lost to demolition and growth churn, not only
    /// freight losses.
    pub fn seller_losses(&self) -> &BTreeMap<ItemID, u64> {
        &self.seller_losses
    }

    pub fn basket_failures(&self) -> &BTreeMap<BasketFailure, u64> {
        &self.basket_failures
    }

    pub(crate) fn record_basket_failure(&mut self, reason: BasketFailure) {
        *self.basket_failures.entry(reason).or_default() += 1;
    }

    pub(crate) fn record_household_losses(
        &mut self,
        lines: impl IntoIterator<Item = (ItemID, u32)>,
    ) {
        for (item, units) in lines {
            *self.household_losses.entry(item).or_default() += u64::from(units);
        }
    }

    pub fn household_losses(&self) -> &BTreeMap<ItemID, u64> {
        &self.household_losses
    }

    /// Reverses the stock transfer for a local pickup that was never collected.
    /// Stock from a removed seller is discarded along with its other inventory.
    pub(crate) fn cancel_retail_purchase(&mut self, trade: Trade, seller_pos: Option<Vec2>) {
        debug_assert_eq!(trade.money_delta, Money::ZERO);
        self.produce(trade.buyer.0, trade.kind, -trade.qty);
        if let Some(pos) = seller_pos {
            self.produce(trade.seller.0, trade.kind, trade.qty);
            let (qty, stock) = self
                .m(trade.kind)
                .sell_order(trade.seller.0)
                .map_or((0, 0), |order| (order.qty, order.stock));
            self.sell(
                trade.seller.0,
                pos,
                trade.kind,
                qty + trade.qty as u32,
                stock,
            );
        }
    }

    pub fn inner(&self) -> &BTreeMap<ItemID, SingleMarket> {
        &self.markets
    }
}

fn calculate_prices(price_multiplier: f32) -> BTreeMap<ItemID, Money> {
    use std::collections::BTreeSet;

    let mut item_graph: BTreeMap<ItemID, Vec<GoodsCompanyID>> = BTreeMap::new();
    for company in GoodsCompanyPrototype::iter() {
        let Some(ref recipe) = company.recipe else {
            continue;
        };
        for item in &recipe.production {
            item_graph.entry(item.id).or_default().push(company.id);
        }
    }

    fn calculate_price_inner(
        item_graph: &BTreeMap<ItemID, Vec<GoodsCompanyID>>,
        id: ItemID,
        cached_prices: &mut BTreeMap<ItemID, Money>,
        visiting: &mut BTreeSet<ItemID>,
        path_dependent: &mut bool,
        price_multiplier: f32,
    ) -> Option<Money> {
        if let Some(&price) = cached_prices.get(&id) {
            return Some(price);
        }
        let Some(companies) = item_graph.get(&id) else {
            return Some(Money::ZERO);
        };
        if !visiting.insert(id) {
            *path_dependent = true;
            return None;
        }

        let mut encountered_cycle = false;
        let mut minprice = None;
        'company: for &comp in companies {
            let company = &comp.prototype();
            let mut price_consumption = Money::ZERO;
            let Some(ref recipe) = company.recipe else {
                continue;
            };
            for recipe_item in &recipe.consumption {
                let Some(price) = calculate_price_inner(
                    item_graph,
                    recipe_item.id,
                    cached_prices,
                    visiting,
                    &mut encountered_cycle,
                    price_multiplier,
                ) else {
                    continue 'company;
                };
                price_consumption += price * recipe_item.amount as i64;
            }
            let qty = recipe
                .production
                .iter()
                .find_map(|x| (x.id == id).then_some(x.amount))
                .unwrap_or(0) as i64;

            let price_workers = recipe.duration.minutes()
                * company.n_workers as f64
                * WORKER_CONSUMPTION_PER_MINUTE;

            let newprice = (price_consumption
                + Money::new_inner((price_workers.inner() as f32 * price_multiplier) as i64))
                / qty;

            minprice = minprice.map(|x: Money| x.min(newprice)).or(Some(newprice));
        }

        visiting.remove(&id);
        if encountered_cycle {
            // Another root may make a different subset of these recipes usable.
            *path_dependent = true;
        } else if let Some(price) = minprice {
            cached_prices.insert(id, price);
        }
        minprice
    }

    let mut prices = BTreeMap::new();
    let mut cached_prices = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    for item in ItemPrototype::iter() {
        let price = calculate_price_inner(
            &item_graph,
            item.id,
            &mut cached_prices,
            &mut visiting,
            &mut false,
            price_multiplier,
        );
        prices.insert(item.id, price.unwrap_or(Money::ZERO));
    }

    prices
}

#[cfg(test)]
mod tests {
    use geom::{vec2, Vec2};
    use prototypes::test_prototypes;
    use prototypes::ItemID;

    use crate::economy::WORKER_CONSUMPTION_PER_MINUTE;
    use crate::world::CompanyID;
    use crate::{FreightStationID, SoulID};

    use super::Market;

    fn mk_ent(id: u64) -> CompanyID {
        CompanyID::from(slotmapd::KeyData::from_ffi(id))
    }

    #[test]
    fn test_match_orders() {
        let seller = SoulID::GoodsCompany(mk_ent((1 << 32) | 1));
        let seller_far = SoulID::GoodsCompany(mk_ent((1 << 32) | 2));
        let buyer = SoulID::GoodsCompany(mk_ent((1 << 32) | 3));
        let freight = SoulID::FreightStation(FreightStationID::from(slotmapd::KeyData::from_ffi(
            (1 << 32) | 4,
        )));

        test_prototypes(
            r#"
        data:extend {
          {
            type = "item",
            name = "cereal",
            label = "Cereal"
          },
          {
            type = "item",
            name = "wheat",
            label = "Wheat",
          }
        }
        "#,
        );

        let mut m = Market::default();

        let cereal = ItemID::new("cereal");

        m.produce(seller, cereal, 3);
        m.produce(seller_far, cereal, 3);

        m.buy(buyer, Vec2::ZERO, cereal, 2);
        m.sell(seller, Vec2::X, cereal, 3, 5);
        m.sell(seller_far, vec2(10.0, 10.0), cereal, 3, 5);

        let trades = m.make_trades(|_| Some(freight));

        assert_eq!(trades.len(), 1);
        let t0 = trades[0];
        assert_eq!(t0.seller.0, seller);
        assert_eq!(t0.buyer.0, buyer);
        assert_eq!(t0.qty, 2);
    }

    #[test]
    fn calculate_prices() {
        test_prototypes(
            r#"
        data:extend {
          {
            type = "item",
            name = "cereal",
            label = "Cereal"
          },
          {
            type = "item",
            name = "wheat",
            label = "Wheat",
          }
        }
        
        data:extend {{
            type = "goods-company",
            name = "cereal-farm",
            label = "Cereal farm",
            kind = "factory",
            bgen = "farm",
            recipe = {
                production = {
                    {"cereal", 3}
                },
                consumption = {},
                duration = "3m",
                storage_multiplier = 5,
            },
            n_trucks = 1,
            n_workers = 2,
            size = 0.0,
            asset = "no.jpg",
            price = 0,
        },
        {
            type = "goods-company",
            name = "wheat-factory",
            label = "Wheat factory",
            kind = "factory",
            bgen = "farm",
            recipe = {
                production = {
                    {"wheat", 2}
                },
                consumption = {
                    {"cereal", 2}
                },
                duration = "10m",
                storage_multiplier = 5,
            },
            n_trucks = 1,
            n_workers = 5,
            size = 0.0,
            asset = "no.jpg",
            price = 0,
        }}
        "#,
        );

        let cereal = ItemID::new("cereal");
        let wheat = ItemID::new("wheat");

        let prices = super::calculate_prices(1.0);

        assert_eq!(prices.len(), 2);
        let price_cereal = 2 * WORKER_CONSUMPTION_PER_MINUTE;
        assert_eq!(prices[&cereal], price_cereal);
        assert_eq!(
            prices[&wheat],
            (price_cereal * 2 + 5 * WORKER_CONSUMPTION_PER_MINUTE * 10) / 2
        );
    }

    #[test]
    fn p1c_price_graph_cycles() {
        test_prototypes(
            r#"
            local function item(name)
                return { type = "item", name = name, label = name }
            end
            local function company(name, output, inputs)
                return {
                    type = "goods-company", name = name, label = name,
                    kind = "factory", bgen = "farm", n_trucks = 1, n_workers = 1,
                    size = 1, asset = "no.jpg", price = 0,
                    recipe = {
                        production = {{output, 1}}, consumption = inputs,
                        duration = "1m", storage_multiplier = 1,
                    },
                }
            end
            data:extend {
                item("a"), item("b"), item("closed"),
                company("make-a", "a", {}),
                company("recycle-a", "a", {{"b", 1}}),
                company("make-b", "b", {{"a", 1}}),
                company("self-cycle", "closed", {{"closed", 1}}),
            }
            "#,
        );

        let prices = super::calculate_prices(1.0);
        assert_eq!(prices[&ItemID::new("a")], WORKER_CONSUMPTION_PER_MINUTE);
        assert_eq!(prices[&ItemID::new("b")], WORKER_CONSUMPTION_PER_MINUTE * 2);
        assert_eq!(prices[&ItemID::new("closed")], prototypes::Money::ZERO);
    }
}
