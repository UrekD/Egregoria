use crate::economy::{find_trade_place, Bought, Market};
use crate::map_dynamic::{BuildingInfos, Destination};
use crate::souls::human::HumanDecisionKind;
use crate::transportation::Location;
use crate::{HumanEnt, HumanID, ParCommandBuffer, SoulID};
use geom::Transform;
use prototypes::{GameTime, ItemID, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};

const PURCHASE_INTERVAL: u64 = 6 * TICKS_PER_HOUR;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuyGoods {
    last_order_period: Option<u64>,
}

// The spawn hook uses the original unit struct's value syntax.
#[allow(non_upper_case_globals)]
pub const BuyGoods: BuyGoods = BuyGoods {
    last_order_period: None,
};

debug_inspect_impl!(BuyGoods);

impl BuyGoods {
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
        loc: &Location,
        bought: &mut Bought,
    ) -> HumanDecisionKind {
        let item = ItemID::new("retail-goods");
        let buyer = SoulID::Human(id);

        // Keep the match in Bought until collection so seller removal can cancel it.
        if let Some(trades) = bought.0.get_mut(&item) {
            if let Some(trade) = trades.last() {
                if let Some(store) = find_trade_place(trade.seller, binfos) {
                    if *loc != Location::Building(store) {
                        return HumanDecisionKind::GoTo(Destination::Building(store));
                    }
                }
                let qty = trades.pop().unwrap().qty;
                cbuf.exec_on(id, move |market: &mut Market| {
                    market.produce(buyer, item, -qty);
                });
                return HumanDecisionKind::Yield;
            }
        }

        if let Some(period) = self.next_order_period(time, id) {
            let pos = trans.pos.xy();
            cbuf.exec_on(id, move |market: &mut Market| {
                // A removed seller's match may have been dropped while its capital was credited.
                let uncollected = market.capital(buyer, item);
                if uncollected > 0 {
                    market.produce(buyer, item, -uncollected);
                }
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
    use crate::map::{BuildingID, BuildingKind};
    use crate::souls::goods_company::is_special_building_company;
    use crate::tests::TestCtx;
    use crate::{CompanyID, FreightStationID, WorldCommand};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{BuildingGen, FreightStationPrototypeID, GoodsCompanyID, Tick};

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
        let proto = GoodsCompanyID::new(name).prototype();
        let building = place(test, x, BuildingKind::GoodsCompany(proto.id), proto.bgen);
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
        test.g.world_mut_unchecked().humans[human]
            .router
            .use_vehicle(None);

        let period = 6 * TICKS_PER_HOUR;
        let offset = common::hash_u64(human) % period;
        let due = offset + 3 * period;
        test.apply(&[WorldCommand::SetGameTime(GameTime::new(Tick(due)))]);
        let bought_at = (0..200)
            .find_map(|_| {
                test.tick_fast();
                (test.g.read::<Market>().capital(buyer, retail) == 1).then(|| test.g.get_tick())
            })
            .expect("resident bought retail goods at the six-hour cadence");
        assert!(bought_at >= due);
        assert_ne!(
            test.g.get(human).unwrap().location,
            Location::Building(store_building)
        );
        test.tick();

        for _ in 0..5_000 {
            test.tick_fast();
            if test.g.read::<Market>().capital(buyer, retail) == 0 {
                break;
            }
        }
        let resident = test.g.get(human).unwrap();
        assert_eq!(resident.location, Location::Building(store_building));
        assert_eq!(
            test.g.read::<Market>().capital(buyer, retail),
            0,
            "resident collected the unit"
        );
        assert!(resident.bought.0.get(&retail).is_none_or(Vec::is_empty));
        for (tick, order_due) in [(due + period - 1, false), (due + period, true)] {
            let score = resident.goods.score(
                &GameTime::new(Tick(tick)),
                human,
                &resident.location,
                &resident.bought,
            );
            assert_eq!(score.is_sign_positive(), order_due);
        }
        assert!(test.g.get(store).unwrap().sold.0.is_empty());
        for _ in 0..100 {
            test.tick_fast();
        }
        assert_eq!(
            test.g.read::<Market>().capital(buyer, retail),
            0,
            "one purchase per period"
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
}
