use crate::map::{BuildingKind, ElectricityNetworkID, Map};
use crate::map_dynamic::BuildingInfos;
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::world::CompanyID;
use crate::{SoulID, World};
use prototypes::{Power, ResidencePrototype, TICKS_PER_HOUR};
use serde::Deserialize;
use slotmapd::__impl::Serialize;
use std::collections::BTreeMap;

const HOUSE_FALLBACK_POWER: Power = Power::new(100);

#[derive(Default, Serialize, Deserialize)]
pub struct ElectricityFlow {
    flowmap: BTreeMap<ElectricityNetworkID, NetworkFlow>,
    #[serde(skip)]
    topology_generation: Option<u64>,
    #[serde(skip)]
    company_staffing: Vec<(CompanyID, usize)>,
}

impl ElectricityFlow {
    pub fn blackout(&self, network: ElectricityNetworkID) -> bool {
        self.flowmap
            .get(&network)
            .map(|f| f.blackout)
            .unwrap_or(false)
    }

    pub fn network_stats(&self, network: ElectricityNetworkID) -> NetworkFlow {
        self.flowmap.get(&network).cloned().unwrap_or(NetworkFlow {
            consumed_power: Power::ZERO,
            produced_power: Power::ZERO,
            blackout: false,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct NetworkFlow {
    pub consumed_power: Power,
    pub produced_power: Power,
    /// Whether the network is in a blackout
    pub blackout: bool,
}

/// Compute the electricity flow of the map and store it in the [`ElectricityFlow`] resource
/// All producing buildings will produce power, and all consuming buildings will consume power
/// If a network produces less power than it consumes, a blackout will occur
pub fn electricity_flow_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::electricity_flow");

    let map = resources.read::<Map>();
    let binfos = resources.read::<BuildingInfos>();
    let residences = resources.read::<Residences>();
    let homes_by_building = &residences.by_building;
    let mut flow = resources.write::<ElectricityFlow>();

    let mut house_power: Vec<_> = ResidencePrototype::iter()
        .map(|proto| {
            (
                proto.id,
                proto.power_consumption.unwrap_or(HOUSE_FALLBACK_POWER),
            )
        })
        .collect();
    house_power.sort_unstable_by_key(|&(id, _)| id);
    let mut last_prototype = None;
    let mut last_power = HOUSE_FALLBACK_POWER;

    flow.topology_generation = Some(map.electricity.generation);
    flow.company_staffing.clear();
    flow.company_staffing.extend(company_staffing(world));
    flow.flowmap.clear();

    for network in map.electricity.networks.values() {
        let mut consumed_power: Power = Power::ZERO;
        let mut produced_power: Power = Power::ZERO;
        let mut homes = network
            .buildings
            .first()
            .map(|&first| homes_by_building.range(first..))
            .unwrap_or_default();
        let mut home = homes.next();

        for &id in &network.buildings {
            let building = map.buildings.get(id).unwrap();

            match building.kind {
                BuildingKind::House => {
                    if home.is_some_and(|(&home_id, _)| home_id < id) {
                        home = homes.next();
                        // Seek across longer gaps so interleaved networks don't rescan each other.
                        if home.is_some_and(|(&home_id, _)| home_id < id) {
                            homes = homes_by_building.range(id..);
                            home = homes.next();
                        }
                    }
                    if let Some((_, residence)) = home.filter(|(&home_id, _)| home_id == id) {
                        if last_prototype != Some(residence.prototype) {
                            last_prototype = Some(residence.prototype);
                            last_power = house_power
                                .binary_search_by_key(&residence.prototype, |&(id, _)| id)
                                .map_or(HOUSE_FALLBACK_POWER, |index| house_power[index].1);
                        }
                        consumed_power += last_power;
                        home = homes.next();
                    } else {
                        consumed_power += HOUSE_FALLBACK_POWER;
                    }
                }
                BuildingKind::GoodsCompany(comp) => {
                    let proto = comp.prototype();

                    let Some(SoulID::GoodsCompany(owner)) = binfos.owner(building.id) else {
                        continue;
                    };

                    let Some(ent) = world.companies.get(owner) else {
                        continue;
                    };
                    let productivity = ent.raw_productivity(proto, building.zone.as_ref()) as f64;

                    consumed_power += proto.power_consumption.unwrap_or(Power::ZERO) * productivity;
                    produced_power += proto.power_production.unwrap_or(Power::ZERO) * productivity;
                }
                BuildingKind::RailFreightStation(_) => {}
                BuildingKind::TrainStation => {}
                BuildingKind::ExternalTrading => {}
            }
        }

        flow.flowmap.insert(
            network.id,
            NetworkFlow {
                consumed_power,
                produced_power,
                blackout: consumed_power > produced_power,
            },
        );
    }
}

fn company_staffing(world: &World) -> impl Iterator<Item = (CompanyID, usize)> + '_ {
    world
        .companies
        .iter()
        .map(|(id, company)| (id, company.workers.0.len()))
}

/// Refresh the snapshot before the utility and coverage passes.
pub(crate) fn refresh_electricity_flow_system(world: &mut World, resources: &mut Resources) {
    let generation = resources.read::<Map>().electricity.generation;
    // Residence upgrades run only on the hour, before this system. Hiring and
    // departures can change production or demand without touching the network.
    if resources.tick().0 % TICKS_PER_HOUR != 0 {
        let flow = resources.read::<ElectricityFlow>();
        if flow.topology_generation == Some(generation)
            && company_staffing(world).eq(flow.company_staffing.iter().copied())
        {
            return;
        }
    }
    electricity_flow_system(world, resources);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{market_update, Market};
    use crate::map::BuildingID;
    use crate::souls::goods_company::company_soul;
    use crate::souls::human::spawn_human;
    use crate::souls::residence::{self, Residence};
    use crate::tests::TestCtx;
    use crate::world::HumanID;
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{GameTime, GoodsCompanyID, ItemID, ResidencePrototypeID, Tick};

    fn company(test: &mut TestCtx, house: BuildingID, name: &str, x: f32) -> CompanyID {
        let road = test.g.map().get(house).unwrap().connected_road.unwrap();
        let proto = GoodsCompanyID::new(name).prototype();
        let building = test
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(vec2(x, 70.0), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto.id),
                proto.bgen,
                None,
                Some(road),
            )
            .unwrap();
        test.g.write::<BuildingInfos>().insert(building);
        let Some(SoulID::GoodsCompany(company)) = company_soul(&mut test.g, building, proto.id)
        else {
            panic!("could not start {name}");
        };
        company
    }

    #[test]
    fn pfb_late_refresh_observes_market_hiring() {
        let mut test = TestCtx::new();
        test.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
        let house = test.build_house_near(vec2(140.0, 125.0));
        let plant = company(&mut test, house, "coal-power-plant", 240.0);
        let worker = spawn_human(&mut test.g, house).unwrap();
        let door = test.g.map().get(house).unwrap().door_pos.xy();
        test.g.write::<Market>().buy_until(
            SoulID::Human(worker),
            door,
            ItemID::new("job-opening"),
            1,
        );
        let network = test.g.map().electricity.net_id(house).unwrap();

        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        assert!(resources.read::<ElectricityFlow>().blackout(network));
        market_update(world, resources);
        assert_eq!(world.companies[plant].workers.0.as_slice(), &[worker]);
        refresh_electricity_flow_system(world, resources);
        assert!(!resources.read::<ElectricityFlow>().blackout(network));
    }

    #[test]
    fn pfb_late_refresh_observes_hourly_house_upgrade() {
        let mut test = TestCtx::new();
        test.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
        let house = test.build_house_near(vec2(140.0, 125.0));
        let network = test.g.map().electricity.net_id(house).unwrap();
        *test.g.write::<GameTime>() = GameTime::new(Tick(TICKS_PER_HOUR));

        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        assert_eq!(
            resources
                .read::<ElectricityFlow>()
                .network_stats(network)
                .consumed_power,
            Power::new(200),
        );
        residence::register(
            &test.g,
            house,
            ResidencePrototypeID::new("residence-low-l2").prototype(),
        );
        let (world, resources) = test.g.world_res();
        refresh_electricity_flow_system(world, resources);
        assert_eq!(
            resources
                .read::<ElectricityFlow>()
                .network_stats(network)
                .consumed_power,
            ResidencePrototypeID::new("residence-low-l2")
                .prototype()
                .power_consumption
                .unwrap(),
        );
    }

    #[test]
    fn pfb_interleaved_networks_match_per_building_loads() {
        let mut test = TestCtx::new();
        test.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
        test.build_roads(&[vec3(80.0, 300.0, 0.0), vec3(440.0, 300.0, 0.0)]);
        let a1 = test.build_house_near(vec2(140.0, 125.0));
        let b1 = test.build_house_near(vec2(140.0, 325.0));
        let a2 = test.build_house_near(vec2(190.0, 125.0));
        let b2 = test.build_house_near(vec2(190.0, 325.0));
        residence::on_building_removed(&mut test.g, a2);
        test.g
            .write::<Residences>()
            .by_building
            .get_mut(&b1)
            .unwrap()
            .prototype = ResidencePrototypeID::new("missing-residence");
        residence::register(
            &test.g,
            b2,
            ResidencePrototypeID::new("residence-high-l3").prototype(),
        );

        let company_a = company(&mut test, a1, "clothes-store", 250.0);
        let company_b = company(&mut test, a1, "clothes-store", 300.0);
        for id in [company_a, company_b] {
            test.g.world.companies[id].workers.0 = vec![HumanID::default(); 7];
        }
        let map = test.g.map();
        let network_a = map.electricity.net_id(a1).unwrap();
        let network_b = map.electricity.net_id(b1).unwrap();
        assert_ne!(network_a, network_b);
        assert_eq!(map.electricity.net_id(a2), Some(network_a));
        assert_eq!(map.electricity.net_id(b2), Some(network_b));
        drop(map);

        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        let flow = resources.read::<ElectricityFlow>();
        // 7 / 10 in f32 is slightly less than 0.7, so each 1000 W load rounds to 699 W.
        for (network, watts) in [(network_a, 200 + 100 + 2 * 699), (network_b, 100 + 1200)] {
            let stats = flow.network_stats(network);
            assert_eq!(stats.consumed_power, Power::new(watts));
            assert_eq!(stats.produced_power, Power::ZERO);
            assert!(stats.blackout);
        }
    }

    #[test]
    fn p1bce_house_power_uses_active_prototype() {
        let mut test = TestCtx::new();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(300.0, 100.0, 0.0)]);
        let house = test.build_house_near(vec2(150.0, 125.0));
        let network = test.g.map().electricity.net_id(house).unwrap();
        for (prototype, watts) in [("residence-low-l1", 200), ("residence-high-l3", 1200)] {
            test.g.write::<Residences>().by_building.insert(
                house,
                Residence::from(ResidencePrototypeID::new(prototype).prototype()),
            );
            let (world, resources) = test.g.world_res();
            electricity_flow_system(world, resources);
            assert_eq!(
                resources
                    .read::<ElectricityFlow>()
                    .network_stats(network)
                    .consumed_power,
                Power::new(watts)
            );
        }
        residence::on_building_removed(&mut test.g, house);
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        assert_eq!(
            resources
                .read::<ElectricityFlow>()
                .network_stats(network)
                .consumed_power,
            Power::new(100)
        );
    }
}
