use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use ordered_float::OrderedFloat;
use serde::{Deserialize, Serialize};

use geom::Vec2;
use prototypes::{
    prototypes_iter, try_prototype, GoodsCompanyID, GoodsCompanyPrototype, ItemPrototype, Money,
    Power, ServiceID, ServiceKind,
};

use crate::economy::{ItemID, WORKER_CONSUMPTION_PER_MINUTE};
use crate::map::BuildingID;
use crate::map_dynamic::{BuildingInfos, ElectricityFlow, UtilityFlow};
use crate::souls::desire::WorkKind;
use crate::souls::household::{household_items, BasketId};
use crate::utils::resources::Resources;
use crate::{HumanID, SoulID, World};

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

#[derive(Serialize, Deserialize)]
pub struct SingleMarket {
    // todo: change i32 to Quantity
    capital: BTreeMap<SoulID, i32>,
    buy_orders: BTreeMap<SoulID, BuyOrder>,
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
            None if id == GoodsCompanyID::new("coal-mine") => Self::Support,
            None => Self::Standard,
        }
    }
}

pub(crate) struct BasketRequest {
    pub staples: u32,
    pub goods: u32,
    pub fallback: u32,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct BasketReservation {
    pub id: BasketId,
    pub shopper: HumanID,
    pub seller: SoulID,
    pub building: BuildingID,
    pub expires: u64,
    pub due: u64,
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
        let c = self.capital(soul, kind);
        if c <= 0 {
            return;
        }
        self.sell(soul, near, kind, c as u32, stock);
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
        for market in self.markets.values_mut() {
            market.sell_orders.remove(&soul);
            market.buy_orders.remove(&soul);
            market.capital.remove(&soul);
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
        let c = self.capital(soul, kind);
        if c >= qty as i32 {
            return;
        }
        self.buy(soul, near, kind, qty - c as u32);
    }

    /// Get the capital that this agent owns
    pub fn capital(&self, soul: SoulID, kind: ItemID) -> i32 {
        self.markets.get(&kind).unwrap().capital(soul).unwrap_or(0)
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

    /// Draw workers from ordinary employers when new applicants cannot start a priority
    /// workplace or cover a utility shortage. Existing drivers keep their assignments.
    pub(super) fn prepare_job_matches(
        &mut self,
        world: &mut World,
        resources: &Resources,
        job: ItemID,
    ) -> BTreeMap<SoulID, (bool, HiringPriority)> {
        let mut priorities = BTreeMap::new();
        let Some(market) = self.markets.get_mut(&job) else {
            return priorities;
        };
        // Keep a stale job application from hiring someone already at work.
        market.buy_orders.retain(|buyer, _| {
            matches!(*buyer, SoulID::Human(id) if world.humans.get(id).is_some_and(|human| human.work.is_none()))
        });
        let map = resources.read::<crate::map::Map>();
        let electricity = resources.read::<ElectricityFlow>();
        let utilities = resources.read::<UtilityFlow>();
        let mut urgent_vacancies = 0;
        for (&seller, order) in &market.sell_orders {
            let SoulID::GoodsCompany(id) = seller else {
                continue;
            };
            let Some(company) = world.companies.get(id) else {
                continue;
            };
            let priority = HiringPriority::for_company(company.comp.proto);
            if priority != HiringPriority::Standard
                && map.get(company.comp.building).is_some()
                && market.capital(seller).is_some_and(|capital| capital >= order.qty as i32)
            {
                let shortfall = map.electricity.net_id(company.comp.building).is_some_and(
                    |network| match priority {
                        HiringPriority::Power => electricity.blackout(network),
                        HiringPriority::Water | HiringPriority::Sewage => utilities
                            .networks()
                            .find(|&(id, _)| id == network)
                            .is_some_and(|(_, totals)| match priority {
                                HiringPriority::Water => !totals.has_water(),
                                _ => !totals.has_sewage(),
                            }),
                        _ => false,
                    },
                );
                let urgent = company.workers.0.is_empty() || shortfall;
                if urgent {
                    urgent_vacancies += order.qty as usize;
                }
                priorities.insert(seller, (!urgent, priority));
            }
        }
        let mut applicants = market.buy_orders.len();
        if urgent_vacancies <= applicants {
            return priorities;
        }

        for (id, company) in world.companies.iter_mut() {
            if applicants >= urgent_vacancies {
                break;
            }
            if company.workers.0.is_empty()
                || HiringPriority::for_company(company.comp.proto) != HiringPriority::Standard
            {
                continue;
            }
            let workplace = company.comp.building;
            let Some(building) = map.get(workplace) else {
                continue;
            };
            let before = applicants;
            let driver = company.comp.driver;
            company.workers.0.retain(|&worker| {
                if applicants >= urgent_vacancies || driver == Some(worker) {
                    return true;
                }
                let Some(human) = world.humans.get_mut(worker) else {
                    return true;
                };
                let buyer = SoulID::Human(worker);
                if market.buy_orders.contains_key(&buyer)
                    || human.work.as_ref().is_some_and(|work| {
                        work.workplace != workplace || !matches!(work.kind, WorkKind::Worker)
                    })
                {
                    return true;
                }
                human.work = None;
                human.bought.0.remove(&job);
                if let Some(capital) = market.capital.get_mut(&buyer) {
                    *capital = capital.saturating_sub(1).max(0);
                }
                market.buy_orders.insert(
                    buyer,
                    BuyOrder {
                        pos: human.trans.pos.xy(),
                        qty: 1,
                    },
                );
                applicants += 1;
                false
            });
            let released = applicants - before;
            if released > 0 {
                let seller = SoulID::GoodsCompany(id);
                let capital = market.capital.entry(seller).or_default();
                *capital += released as i32;
                market.sell_orders.insert(
                    seller,
                    SellOrder {
                        pos: building.door_pos.xy(),
                        qty: *capital as u32,
                        stock: 0,
                    },
                );
            }
        }
        priorities
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
        self.all_trades.clear();
        let local = household_items();
        let job = ItemID::new("job-opening");

        for (&kind, market) in &mut self.markets {
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
                    0_u64,
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

                    if sorder.qty == 0 {
                        sorderocc.remove();
                    }

                    // Safety: buyer cannot be the same as seller
                    *cap_seller -= trade.qty;
                    *capital.entry(trade.buyer.0).or_default() += trade.qty;

                    Some(trade)
                }));

            // External trading
            if !*optout_exttrade {
                buy_orders.retain(|&buyer, order| {
                    let Some(ext @ SoulID::FreightStation(_)) = find_external(order.pos) else {
                        return true;
                    };
                    let qty_buy = order.qty as i32;
                    *capital.entry(buyer).or_default() += qty_buy;

                    self.all_trades.push(Trade {
                        buyer: TradeTarget(buyer),
                        seller: TradeTarget(ext),
                        qty: qty_buy,
                        kind,
                        money_delta: -(*ext_value * qty_buy as i64), // we buy from external so we pay
                    });
                    false
                });

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

        &self.all_trades
    }

    /// Picks one reachable shelf and removes its accepted lines in one market operation.
    /// A useful basket is preferred to a tiny partial fill. Distance and IDs break ties
    /// within each group. There is no buyer market credit until or after collection.
    pub(crate) fn reserve_basket(
        &mut self,
        shopper: HumanID,
        origin: Vec2,
        request: BasketRequest,
        now: u64,
        mut reachable: impl FnMut(SoulID) -> Option<(BuildingID, Vec2)>,
    ) -> Option<BasketReservation> {
        let [groceries, retail] = household_items();
        let staples = self.markets.get(&groceries)?;
        let goods = self.markets.get(&retail)?;
        let id = BasketId(common::hash_u64((shopper, now)));
        let buyer = SoulID::Human(shopper);
        let available = |market: &SingleMarket, seller| {
            market.sell_order(seller).map_or(0, |order| {
                order
                    .qty
                    .min(market.capital(seller).unwrap_or(0).max(0) as u32)
            })
        };
        let mut best: Option<(_, BuildingID, Vec2, u32, u32)> = None;
        for &seller in staples.sell_orders.keys().chain(
            goods
                .sell_orders
                .keys()
                .filter(|seller| !staples.sell_orders.contains_key(seller)),
        ) {
            let food = available(staples, seller).min(request.staples);
            let goods_target = request.fallback.saturating_sub(food).max(request.goods);
            let other = available(goods, seller).min(goods_target);
            if food + other == 0 || seller == buyer {
                continue;
            }
            let Some((building, pos)) = reachable(seller) else {
                continue;
            };
            let item = if food > 0 { groceries } else { retail };
            let key = (
                food + other < 4,
                OrderedFloat(origin.distance2(pos)),
                seller,
                buyer,
                item,
                id,
            );
            if best.as_ref().is_none_or(|(best_key, ..)| key < *best_key) {
                best = Some((key, building, pos, food, other));
            }
        }

        let ((_, _, seller, _, _, _), building, shelf_pos, food, other) = best?;
        let lines: Vec<_> = [(groceries, food), (retail, other)]
            .into_iter()
            .filter(|&(_, units)| units > 0)
            .collect();
        for &(item, units) in &lines {
            let market = self.m(item);
            *market.capital.get_mut(&seller).unwrap() -= units as i32;
            let order = market.sell_orders.get_mut(&seller).unwrap();
            order.qty -= units;
            if order.qty == 0 {
                market.sell_orders.remove(&seller);
            }
        }
        Some(BasketReservation {
            id,
            shopper,
            seller,
            building,
            expires: now + 2 * prototypes::TICKS_PER_HOUR,
            due: now,
            lines,
            shelf_pos,
        })
    }

    pub(crate) fn release_basket(&mut self, basket: BasketReservation, refund: bool) {
        if refund {
            for (item, units) in basket.lines {
                self.produce(basket.seller, item, units as i32);
                self.m(item)
                    .sell_orders
                    .entry(basket.seller)
                    .or_insert(SellOrder {
                        pos: basket.shelf_pos,
                        qty: 0,
                        stock: 0,
                    })
                    .qty += units;
            }
        } else {
            self.record_household_losses(basket.lines);
        }
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
            let orders = &mut self.m(trade.kind).sell_orders;
            orders
                .entry(trade.seller.0)
                .or_insert(SellOrder {
                    pos,
                    qty: 0,
                    stock: 0,
                })
                .qty += trade.qty as u32;
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
