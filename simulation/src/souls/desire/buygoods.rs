use crate::economy::{find_trade_place, Bought, EcoStats, Market, TradeTarget};
use crate::map::{BuildingID, Map};
use crate::map_dynamic::{BuildingInfos, Destination, Itinerary, ParkingManagement, Router};
use crate::souls::human::{HumanDecision, HumanDecisionKind};
use crate::transportation::Location;
use crate::utils::resources::Resources;
use crate::{HumanEnt, HumanID, ParCommandBuffer, SoulID, VehicleEnt, VehicleID};
use geom::{Transform, Vec3};
use prototypes::{GameTime, ItemID, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use slotmapd::HopSlotMap;

const PURCHASE_INTERVAL: u64 = 6 * TICKS_PER_HOUR;
const PICKUP_STALL_TICKS: u64 = TICKS_PER_HOUR;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuyGoods {
    last_order_period: Option<u64>,
    pickup: Option<Pickup>,
    tick: u64,
    timed_out_seller: Option<TimedOutSeller>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Pickup {
    store: BuildingID,
    seller: TradeTarget,
    started: u64,
    last_position: Vec3,
    last_progress_tick: Option<u64>,
    vehicle: Option<VehicleID>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TimedOutSeller {
    seller: SoulID,
    skipped_order: bool,
}

debug_inspect_impl!(BuyGoods);

impl BuyGoods {
    pub(crate) fn basket_score(stock: &crate::souls::household::HouseholdStock) -> f32 {
        let [groceries, retail] = crate::souls::household::household_items();
        if stock.units(retail) == 0 && stock.units(groceries) <= 3 {
            0.58
        } else {
            0.0
        }
    }

    #[inline]
    pub(crate) fn update_pickup(
        human: &mut HumanEnt,
        _: &GameTime,
        map: &Map,
        vehicles: &HopSlotMap<VehicleID, VehicleEnt>,
        resources: &Resources,
    ) {
        let Some(pickup) = human.goods.pickup else {
            return;
        };
        // Count simulation steps while en route, independent of calendar edits.
        human.goods.tick += 1;
        Self::poll_pickup(human, map, vehicles, resources, pickup);
    }

    fn poll_pickup(
        human: &mut HumanEnt,
        map: &Map,
        vehicles: &HopSlotMap<VehicleID, VehicleEnt>,
        resources: &Resources,
        mut pickup: Pickup,
    ) {
        if human.trans.pos != pickup.last_position {
            pickup.last_position = human.trans.pos;
            pickup.last_progress_tick = Some(human.goods.tick);
            human.goods.pickup = Some(pickup);
        }
        let last_progress = pickup.last_progress_tick.unwrap_or(pickup.started);
        let stalled = human.goods.tick - last_progress >= PICKUP_STALL_TICKS;
        let item = ItemID::new("retail-goods");
        let trade = human.bought.0.get_mut(&item).and_then(|trades| {
            let index = trades
                .iter()
                .rposition(|trade| trade.seller == pickup.seller)?;
            Some((trades, index))
        });
        let store = map.get(pickup.store).filter(|_| {
            resources
                .read::<BuildingInfos>()
                .building_owned_by(pickup.seller.0)
                == Some(pickup.store)
        });
        let arrived = store.is_some() && human.location == Location::Building(pickup.store);
        if trade.is_some()
            && !arrived
            && store.is_some_and(|store| !store.is_orphaned())
            && human.router.last_error.is_none()
            && !stalled
        {
            return;
        }

        if let Some((trades, index)) = trade {
            let trade = trades.remove(index);
            let mut market = resources.write::<Market>();
            if arrived {
                market.produce(trade.buyer.0, item, -trade.qty);
                resources
                    .write::<EcoStats>()
                    .internal_trade
                    .handle_trade(&trade);
            } else {
                market.cancel_retail_purchase(trade, store.map(|store| store.door_pos.xy()));
                if stalled {
                    human.goods.timed_out_seller = Some(TimedOutSeller {
                        seller: pickup.seller.0,
                        skipped_order: false,
                    });
                }
            }
        }
        human.goods.pickup = None;
        human.router.walk_only_pickup = false;
        human
            .router
            .cancel_route(&mut resources.write::<ParkingManagement>());
        human
            .router
            .use_vehicle(pickup.vehicle.filter(|&car| vehicles.contains_key(car)));
        human.it = Itinerary::NONE;
        human.decision = HumanDecision::default();
    }

    pub(crate) fn excluded_seller(&self) -> Option<SoulID> {
        self.timed_out_seller.as_ref().map(|retry| retry.seller)
    }

    fn next_order_period(&self, time: &GameTime, id: HumanID) -> Option<u64> {
        let offset = common::hash_u64(id) % PURCHASE_INTERVAL;
        let period = time.tick.0.checked_sub(offset)? / PURCHASE_INTERVAL;
        self.last_order_period
            .is_none_or(|last| period > last)
            .then_some(period)
    }

    pub fn score(&self, time: &GameTime, id: HumanID, _: &Location, bought: &Bought) -> f32 {
        if bought
            .0
            .get(&ItemID::new("retail-goods"))
            .is_some_and(|trades| !trades.is_empty())
        {
            0.45
        } else if self.next_order_period(time, id).is_some() {
            0.3
        } else {
            f32::NEG_INFINITY
        }
    }

    pub fn apply(
        &mut self,
        cbuf: &ParCommandBuffer<HumanEnt>,
        binfos: &BuildingInfos,
        time: &GameTime,
        id: HumanID,
        trans: &Transform,
        router: &mut Router,
        bought: &mut Bought,
    ) -> HumanDecisionKind {
        if let Some(pickup) = &self.pickup {
            router.walk_only_pickup = true;
            router.use_vehicle(None);
            return HumanDecisionKind::GoTo(Destination::Building(pickup.store));
        }

        let item = ItemID::new("retail-goods");
        let buyer = SoulID::Human(id);

        if let Some(trades) = bought.0.get_mut(&item) {
            if let Some(&trade) = trades.last() {
                if let Some(store) = find_trade_place(trade.seller, binfos) {
                    self.timed_out_seller = None;
                    self.pickup = Some(Pickup {
                        store,
                        seller: trade.seller,
                        started: self.tick,
                        last_position: trans.pos,
                        last_progress_tick: None,
                        vehicle: router.selected_vehicle(),
                    });
                    router.walk_only_pickup = true;
                    router.use_vehicle(None);
                    router.last_error = None;
                    return HumanDecisionKind::GoTo(Destination::Building(store));
                }
                trades.pop();
                cbuf.exec_on(id, move |market: &mut Market| {
                    market.cancel_retail_purchase(trade, None);
                });
                return HumanDecisionKind::Yield;
            }
        }

        if let Some(period) = self.next_order_period(time, id) {
            if let Some(retry) = &mut self.timed_out_seller {
                // Give alternatives one order period, then let a sole seller retry.
                if retry.skipped_order {
                    self.timed_out_seller = None;
                } else {
                    retry.skipped_order = true;
                }
            }
            let pos = trans.pos.xy();
            cbuf.exec_on(id, move |market: &mut Market| {
                market.buy(buyer, pos, item, 1);
            });
            self.last_order_period = Some(period);
        }
        HumanDecisionKind::Yield
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{BuildingID, BuildingKind, ProjectFilter, ProjectKind, RoadID};
    use crate::souls::goods_company::is_special_building_company;
    use crate::tests::TestCtx;
    use crate::{CompanyID, FreightStationID, WorldCommand};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{BuildingGen, FreightStationPrototypeID, GoodsCompanyID};

    fn city_street() -> TestCtx {
        let test = TestCtx::new();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
        test
    }

    fn place(test: &mut TestCtx, x: f32, kind: BuildingKind, gen: BuildingGen) -> BuildingID {
        let road = test.g.map().roads().keys().last().unwrap();
        test.apply(&[WorldCommand::MapBuildSpecialBuilding {
            pos: OBB::new(vec2(x, 145.0), Vec2::X, 20.0, 20.0),
            kind,
            gen,
            zone: None,
            connected_road: Some(road),
        }]);
        test.g
            .map()
            .buildings()
            .iter()
            .find_map(|(id, building)| {
                (building.kind == kind && building.obb.center().x == x).then_some(id)
            })
            .expect("test building placed")
    }

    fn company(test: &mut TestCtx, x: f32, name: &str) -> (BuildingID, CompanyID) {
        let ProjectKind::Road(road) = test
            .g
            .map()
            .project(vec3(x, 100.0, 0.0), 2.0, ProjectFilter::ROAD)
            .kind
        else {
            panic!("missing company frontage on the fixture street");
        };
        company_on_road(test, x, name, road)
    }

    fn company_on_road(
        test: &mut TestCtx,
        x: f32,
        name: &str,
        road: RoadID,
    ) -> (BuildingID, CompanyID) {
        let proto = GoodsCompanyID::new(name).prototype();
        let kind = BuildingKind::GoodsCompany(proto.id);
        // The routing fixtures need the store and nearby house at fixed positions.
        let building = test
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(vec2(x, 145.0), Vec2::X, 20.0, 20.0),
                kind,
                proto.bgen,
                None,
                Some(road),
            )
            .expect("test company placed");
        crate::souls::growth::on_growable_placed(&test.g, building, kind);
        test.tick_fast();
        let Some(SoulID::GoodsCompany(id)) = test.g.read::<BuildingInfos>().owner(building) else {
            panic!("company soul missing for {name}");
        };
        (building, id)
    }

    fn freight_station(test: &mut TestCtx) -> FreightStationID {
        let building = place(
            test,
            350.0,
            BuildingKind::RailFreightStation(FreightStationPrototypeID::new("freight-station")),
            BuildingGen::NoWalkway {
                door_pos: vec2(350.0, 145.0),
            },
        );
        test.tick_fast();
        let Some(SoulID::FreightStation(id)) = test.g.read::<BuildingInfos>().owner(building)
        else {
            panic!("freight station soul missing");
        };
        id
    }

    fn finish_batch(test: &mut TestCtx, company: CompanyID) {
        test.g.world_mut_unchecked().companies[company]
            .comp
            .progress = 1.0;
        test.tick_fast();
    }

    #[test]
    fn p1c_industrial_supplies_commercial() {
        let mut test = city_street();
        let (_, industry) = company(&mut test, 160.0, "industrial-l1");
        let (_, store) = company(&mut test, 230.0, "commercial-l1");
        let goods = ItemID::new("goods");
        let industry_soul = SoulID::GoodsCompany(industry);
        let store_soul = SoulID::GoodsCompany(store);

        finish_batch(&mut test, industry);
        let market = test.g.read::<Market>();
        assert!(
            market.capital(industry_soul, goods) > 0,
            "industry produced goods"
        );
        assert!(
            market.capital(store_soul, goods) > 0,
            "store acquired goods"
        );
        let trade = test.g.get(store).unwrap().bought.0[&goods][0];
        assert_eq!(trade.seller.0, industry_soul);
        drop(market);

        test.tick_fast();
        let industry = test.g.get(industry).unwrap();
        assert!(industry.sold.0.is_empty(), "local sale must be fulfilled");
        assert!(industry.comp.trucks.is_empty());
        assert!(industry.comp.driver.is_none());
    }

    #[test]
    fn p1c_growable_companies_and_retail() {
        let mut test = city_street();
        let (store_building, store) = company(&mut test, 190.0, "commercial-l1");
        let house = test.build_house_near(vec2(155.0, 122.0));
        test.set_residential_demand(40.0);
        test.tick_fast();
        let Some(SoulID::Human(human)) = test.g.read::<BuildingInfos>().owner(house) else {
            panic!("resident missing");
        };
        let buyer = SoulID::Human(human);
        let store_soul = SoulID::GoodsCompany(store);
        let retail = ItemID::new("retail-goods");

        assert!(retail.prototype().optout_exttrade);
        finish_batch(&mut test, store);
        assert!(test.g.read::<Market>().capital(store_soul, retail) > 0);
        let car = test.g.get(human).unwrap().router.personal_car.unwrap();

        // Exercise the household default path with real growable production.
        let now = test
            .g
            .read::<crate::economy::Budget>()
            .elapsed_ticks(test.g.read::<GameTime>().tick);
        let resident = &mut test.g.world_mut_unchecked().humans[human];
        resident.household.next_shop = now;
        resident.work = None;
        let pos = test.g.map().get(store_building).unwrap().door_pos.xy();
        test.g
            .write::<Market>()
            .sell(store_soul, pos, ItemID::new("job-opening"), 0, 0);

        for _ in 0..3 * TICKS_PER_HOUR {
            test.tick_fast();
            if !test.g.get(human).unwrap().household.on_hand.is_empty() {
                break;
            }
        }
        let resident = test.g.get(human).unwrap();
        assert_eq!(resident.location, Location::Building(house));
        assert_eq!(resident.router.personal_car, Some(car));
        assert_eq!(resident.router.selected_vehicle(), Some(car));
        assert!(resident.household.on_hand[&retail] > 0);
        assert!(resident.household.carried.is_empty());
        assert_eq!(test.g.read::<Market>().capital(buyer, retail), 0);
        assert!(test.g.get(store).unwrap().sold.0.is_empty());
    }

    #[test]
    fn p1bce_disconnected_retail_pickup_refunds_stock() {
        let mut test = city_street();
        let house = test.build_house_near(vec2(155.0, 122.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        test.build_roads(&[vec3(400.0, 100.0, 0.0), vec3(500.0, 200.0, 0.0)]);
        test.build_roads(&[vec3(500.0, 200.0, 0.0), vec3(800.0, 200.0, 0.0)]);
        let road_at = |x, y| {
            let ProjectKind::Road(id) = test
                .g
                .map()
                .project(vec3(x, y, 0.0), 2.0, ProjectFilter::ROAD)
                .kind
            else {
                panic!("no road at ({x}, {y})");
            };
            id
        };
        let connector = road_at(450.0, 150.0);
        let frontage = road_at(650.0, 200.0);
        let (store, company) = company_on_road(&mut test, 650.0, "commercial-l1", frontage);
        let buyer = SoulID::Human(human);
        let seller = SoulID::GoodsCompany(company);
        let retail = ItemID::new("retail-goods");
        let house_door = test.g.map().get(house).unwrap().door_pos;
        let store_door = test.g.map().get(store).unwrap().door_pos;
        {
            let mut market = test.g.write::<Market>();
            market.produce(seller, retail, 1);
            market.sell(seller, store_door.xy(), retail, 1, 0);
            market.sell(seller, store_door.xy(), ItemID::new("job-opening"), 0, 0);
            market.buy(buyer, house_door.xy(), retail, 1);
        }
        test.tick_fast();
        assert_eq!(test.g.read::<Market>().capital(buyer, retail), 1);
        test.apply(&[WorldCommand::MapRemoveRoad(connector)]);
        assert!(!test.g.map().get(store).unwrap().is_orphaned());
        for _ in 0..100 {
            test.tick_fast();
        }
        assert!(test.g.get(human).unwrap().router.references_building(store));
        for _ in 0..TICKS_PER_HOUR + 5_000 {
            test.tick_fast();
            if test.g.read::<Market>().capital(buyer, retail) == 0 {
                break;
            }
        }
        let market = test.g.read::<Market>();
        assert_eq!(
            market.capital(buyer, retail),
            0,
            "timed-out pickup refunded"
        );
        assert_eq!(market.capital(seller, retail), 1);
        let order = market.inner()[&retail].sell_order(seller).unwrap();
        assert_eq!(order.qty, 1);
        assert!(test
            .g
            .get(human)
            .unwrap()
            .bought
            .0
            .get(&retail)
            .is_none_or(Vec::is_empty));
        assert!(!test.g.get(human).unwrap().router.references_building(store));
        drop(market);

        // A new order must remain open when its only seller is on the disconnected road.
        test.g
            .write::<Market>()
            .buy(buyer, house_door.xy(), retail, 1);
        test.tick_fast();
        let market = test.g.read::<Market>();
        assert_eq!(market.capital(buyer, retail), 0);
        assert!(market.inner()[&retail].buy_order(buyer).is_some());
    }

    #[test]
    fn p1bce_external_orders_wait_for_freight_and_reconcile() {
        use crate::economy::{Budget, BudgetCategory, Government};

        let mut test = city_street();
        let starter_stations: Vec<_> = test
            .g
            .map()
            .buildings()
            .iter()
            .filter_map(|(id, building)| {
                matches!(building.kind, BuildingKind::RailFreightStation(_)).then_some(id)
            })
            .collect();
        for station in starter_stations {
            test.apply(&[WorldCommand::MapRemoveBuilding(station)]);
        }
        let (_, store) = company(&mut test, 160.0, "commercial-l1");
        let (_, office) = company(&mut test, 230.0, "office-l1");
        let goods = ItemID::new("goods");
        let services = ItemID::new("services");
        let importer = SoulID::GoodsCompany(store);
        let exporter = SoulID::GoodsCompany(office);
        let pos = vec2(160.0, 145.0);
        let opening = test.g.read::<Government>().money;
        let construction = test.g.read::<Budget>().total(BudgetCategory::Construction);
        let (import_cost, export_income) = {
            let mut market = test.g.write::<Market>();
            market.buy(importer, pos, goods, 2);
            market.produce(exporter, services, 4);
            market.sell(exporter, pos, services, 4, 1);
            (
                -market.m(goods).ext_value * 2,
                market.m(services).ext_value * 3,
            )
        };
        let (world, resources) = test.g.world_res();
        assert!(world.freight_stations.is_empty());
        crate::economy::market_update(world, resources);
        {
            let market = test.g.read::<Market>();
            assert_eq!(market.capital(importer, goods), 0);
            assert_eq!(market.capital(exporter, services), 4);
            assert_eq!(market.inner()[&goods].buy_order(importer).unwrap().qty, 2);
            assert_eq!(
                market.inner()[&services].sell_order(exporter).unwrap().qty,
                4
            );
        }
        assert_eq!(test.g.read::<Government>().money, opening);
        freight_station(&mut test);
        let (world, resources) = test.g.world_res();
        crate::economy::market_update(world, resources);
        let market = test.g.read::<Market>();
        assert_eq!(market.capital(importer, goods), 2);
        assert_eq!(market.capital(exporter, services), 1);
        assert!(market.inner()[&goods].buy_order(importer).is_none());
        assert_eq!(
            market.inner()[&services].sell_order(exporter).unwrap().qty,
            1
        );
        let budget = test.g.read::<Budget>();
        let construction_delta = budget.total(BudgetCategory::Construction) - construction;
        assert_eq!(
            i128::from(test.g.read::<Government>().money.inner()),
            i128::from((opening + import_cost + export_income).inner()) + construction_delta,
        );
        assert_eq!(
            budget.total(BudgetCategory::TradeImport),
            i128::from(import_cost.inner())
        );
        assert_eq!(
            budget.total(BudgetCategory::TradeExport),
            i128::from(export_income.inner())
        );
    }

    #[test]
    fn p1c_office_exports_services() {
        let mut test = city_street();
        let freight = freight_station(&mut test);
        let (_, office) = company(&mut test, 230.0, "office-l1");
        let proto = GoodsCompanyID::new("office-l1").prototype();
        let services = ItemID::new("services");
        assert!(!services.prototype().optout_exttrade);

        for _ in 0..=proto.recipe.as_ref().unwrap().storage_multiplier {
            finish_batch(&mut test, office);
        }
        test.tick_fast();
        assert!(
            test.g
                .read::<Market>()
                .capital(SoulID::GoodsCompany(office), services)
                < proto.recipe.as_ref().unwrap().production[0].amount
                    * (proto.recipe.as_ref().unwrap().storage_multiplier + 1),
            "services were exported after exceeding reserve stock",
        );
        assert!(
            test.g.get(freight).unwrap().f.waiting_cargo > 0,
            "export reached freight without a driver"
        );
        assert!(test.g.get(office).unwrap().sold.0.is_empty());
        assert!(test.g.get(office).unwrap().comp.trucks.is_empty());
    }

    #[test]
    fn p1c_growables_absent_from_special_buildings() {
        let _test = TestCtx::new();
        let special: Vec<_> = prototypes::prototypes_iter::<prototypes::GoodsCompanyPrototype>()
            .map(|proto| proto.id)
            .filter(|&id| is_special_building_company(id))
            .collect();
        assert!(special.contains(&GoodsCompanyID::new("bakery")));
        assert!(special.contains(&GoodsCompanyID::new("flour-factory")));
        for name in ["industrial-l1", "commercial-l1", "office-l1"] {
            assert!(
                !special.contains(&GoodsCompanyID::new(name)),
                "{name} is a growable"
            );
        }
    }

    #[test]
    fn special_building_command_rejects_growables() {
        use crate::economy::{Budget, Government};
        use crate::world_command::{CommandOutcomes, Rejection};

        let mut test = city_street();
        let buildings = test.g.map().buildings().len();
        let opening = test.g.read::<Government>().money;
        let ledger = test.g.read::<Budget>().days().clone();
        for name in ["industrial-l1", "commercial-l1", "office-l1"] {
            let proto = GoodsCompanyID::new(name).prototype();
            test.g
                .apply_commands([&WorldCommand::MapBuildSpecialBuilding {
                    pos: OBB::new(vec2(160.0, 145.0), Vec2::X, 20.0, 20.0),
                    kind: BuildingKind::GoodsCompany(proto.id),
                    gen: proto.bgen,
                    zone: None,
                    connected_road: None,
                }]);
            assert_eq!(
                test.g.read::<CommandOutcomes>().last,
                [("MapBuildSpecialBuilding", Rejection::InvalidBuilding)],
                "{name}"
            );
        }
        assert_eq!(test.g.map().buildings().len(), buildings);
        assert_eq!(test.g.read::<Government>().money, opening);
        assert_eq!(*test.g.read::<Budget>().days(), ledger);
    }

    #[test]
    fn assigning_work_consumes_pending_hire() {
        let mut test = city_street();
        let house = test.build_house_near(vec2(155.0, 122.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        let (building, _) = company(&mut test, 230.0, "commercial-l1");
        test.tick_fast();
        let jobs = ItemID::new("job-opening");
        let resident = test.g.get(human).unwrap();
        assert!(resident.work.is_none());
        assert_eq!(resident.bought.0[&jobs].len(), 1);

        test.tick_fast();
        let resident = test.g.get(human).unwrap();
        assert_eq!(resident.work.as_ref().unwrap().workplace, building);
        assert!(resident.bought.0.get(&jobs).is_none_or(Vec::is_empty));
    }
}
