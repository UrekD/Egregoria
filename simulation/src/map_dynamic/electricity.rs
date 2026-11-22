use crate::map::{
    Building, BuildingID, BuildingKind, ElectricityNetwork, ElectricityNetworkID, Map,
};
use crate::map_dynamic::BuildingInfos;
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::world::CompanyID;
use crate::{SoulID, World};
use prototypes::{prototypes, try_prototype, GoodsCompanyID, Power, TICKS_PER_HOUR};
use serde::Deserialize;
use slotmapd::__impl::Serialize;
use std::collections::BTreeMap;

const HOUSE_FALLBACK_POWER: Power = Power::new(100);

#[derive(Default, Serialize, Deserialize)]
pub struct ElectricityFlow {
    flowmap: BTreeMap<ElectricityNetworkID, NetworkFlow>,
    #[serde(skip)]
    power_cache: PowerCache,
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
        self.flowmap.get(&network).cloned().unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkFlow {
    pub consumed_power: Power,
    pub produced_power: Power,
    /// Whether the network is in a blackout
    pub blackout: bool,
}

impl Default for NetworkFlow {
    fn default() -> Self {
        Self {
            consumed_power: Power::ZERO,
            produced_power: Power::ZERO,
            blackout: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PowerCacheKey {
    map_identity: u64,
    topology_generation: u64,
    residence_generation: u64,
    prototype_registry: usize,
}

impl PowerCacheKey {
    fn new(map: &Map, residences: &Residences) -> Self {
        Self {
            map_identity: map.identity(),
            topology_generation: map.electricity.generation(),
            residence_generation: residences.generation(),
            prototype_registry: std::ptr::from_ref(prototypes()).addr(),
        }
    }
}

#[derive(Default)]
struct PowerCache {
    key: Option<PowerCacheKey>,
    networks: Vec<NetworkLoads>,
}

struct NetworkLoads {
    id: ElectricityNetworkID,
    houses: Vec<BuildingID>,
    companies: Vec<(BuildingID, GoodsCompanyID)>,
    residential_watts: i64,
}

impl NetworkLoads {
    fn from_network(map: &Map, network: &ElectricityNetwork) -> Self {
        let mut loads = Self {
            id: network.id,
            houses: Vec::new(),
            companies: Vec::new(),
            residential_watts: 0,
        };
        for &building in &network.buildings {
            match map.buildings[building].kind {
                BuildingKind::House => loads.houses.push(building),
                BuildingKind::GoodsCompany(company) => loads.companies.push((building, company)),
                _ => {}
            }
        }
        loads
    }
}

impl PowerCache {
    fn update(
        &mut self,
        key: PowerCacheKey,
        map: &Map,
        residences: &Residences,
        flowmap: &mut BTreeMap<ElectricityNetworkID, NetworkFlow>,
    ) {
        if self.key == Some(key) {
            return;
        }
        if self.key.is_none_or(|previous| {
            previous.map_identity != key.map_identity
                || previous.topology_generation != key.topology_generation
        }) {
            self.networks = map
                .electricity
                .networks()
                .map(|network| NetworkLoads::from_network(map, network))
                .collect();
            *flowmap = self
                .networks
                .iter()
                .map(|loads| (loads.id, NetworkFlow::default()))
                .collect();
        }
        for loads in &mut self.networks {
            loads.residential_watts = loads.houses.iter().fold(0i64, |watts, &id| {
                watts.wrapping_add(house_power(residences, id).watts())
            });
        }
        self.key = Some(key);
    }
}

fn house_power(residences: &Residences, id: BuildingID) -> Power {
    residences
        .by_building
        .get(&id)
        .and_then(|residence| try_prototype(residence.prototype))
        .and_then(|proto| proto.power_consumption)
        .unwrap_or(HOUSE_FALLBACK_POWER)
}

fn company_power(
    world: &World,
    binfos: &BuildingInfos,
    building: &Building,
    company: GoodsCompanyID,
) -> (Power, Power) {
    let proto = company.prototype();
    let Some(SoulID::GoodsCompany(owner)) = binfos.owner(building.id) else {
        return (Power::ZERO, Power::ZERO);
    };
    let Some(ent) = world.companies.get(owner) else {
        return (Power::ZERO, Power::ZERO);
    };
    let productivity = ent.raw_productivity(proto, building.zone.as_ref()) as f64;
    (
        proto.power_consumption.unwrap_or(Power::ZERO) * productivity,
        proto.power_production.unwrap_or(Power::ZERO) * productivity,
    )
}

/// Compute the electricity flow of the map and store it in the [`ElectricityFlow`] resource
/// All producing buildings will produce power, and all consuming buildings will consume power
/// If a network produces less power than it consumes, a blackout will occur
pub fn electricity_flow_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::electricity_flow");

    let map = resources.read::<Map>();
    let binfos = resources.read::<BuildingInfos>();
    let residences = resources.read::<Residences>();
    let mut flow = resources.write::<ElectricityFlow>();
    let ElectricityFlow {
        flowmap,
        power_cache,
        company_staffing: staffing,
    } = &mut *flow;

    power_cache.update(
        PowerCacheKey::new(&map, &residences),
        &map,
        &residences,
        flowmap,
    );
    staffing.clear();
    staffing.extend(company_staffing(world));

    for loads in &power_cache.networks {
        let mut consumed_watts = loads.residential_watts;
        let mut produced_power = Power::ZERO;
        for &(id, company) in &loads.companies {
            let (consumed, produced) = company_power(world, &binfos, &map.buildings[id], company);
            // Preserve integer totals when grouping house loads ahead of company loads.
            consumed_watts = consumed_watts.wrapping_add(consumed.watts());
            produced_power += produced;
        }
        let consumed_power = Power::new(consumed_watts);
        *flowmap.get_mut(&loads.id).unwrap() = NetworkFlow {
            consumed_power,
            produced_power,
            blackout: consumed_power > produced_power,
        };
    }

    #[cfg(debug_assertions)]
    assert_matches_full_recompute(world, &map, &binfos, &residences, flowmap);
}

#[cfg(any(debug_assertions, test))]
fn assert_matches_full_recompute(
    world: &World,
    map: &Map,
    binfos: &BuildingInfos,
    residences: &Residences,
    flowmap: &BTreeMap<ElectricityNetworkID, NetworkFlow>,
) {
    assert_eq!(flowmap.len(), map.electricity.networks().count());
    for network in map.electricity.networks() {
        let mut consumed_power = Power::ZERO;
        let mut produced_power = Power::ZERO;
        for &id in &network.buildings {
            let building = &map.buildings[id];
            let (consumed, produced) = match building.kind {
                BuildingKind::House => (house_power(residences, id), Power::ZERO),
                BuildingKind::GoodsCompany(company) => {
                    company_power(world, binfos, building, company)
                }
                _ => continue,
            };
            consumed_power += consumed;
            produced_power += produced;
        }
        assert_eq!(
            flowmap[&network.id],
            NetworkFlow {
                consumed_power,
                produced_power,
                blackout: consumed_power > produced_power,
            },
            "electricity network {:?}",
            network.id,
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
    {
        let key = PowerCacheKey::new(&resources.read::<Map>(), &resources.read::<Residences>());
        let flow = resources.read::<ElectricityFlow>();
        // Keep the hourly refresh for changes made by the systems between the two passes.
        if resources.tick().0 % TICKS_PER_HOUR != 0
            && flow.power_cache.key == Some(key)
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
    use crate::souls::goods_company::company_soul;
    use crate::souls::human::spawn_human;
    use crate::souls::residence::{self, Residence};
    use crate::tests::TestCtx;
    use crate::world::HumanID;
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{BuildingGen, GameTime, ItemID, ResidencePrototypeID, Tick};

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
    fn pfb2a3_map_replacement_reclassifies_buildings() {
        let mut test = TestCtx::new();
        let footprint = OBB::new(vec2(140.0, 125.0), Vec2::X, 20.0, 20.0);
        let mut original = Map::empty();
        let house = original
            .build_special_building(
                &footprint,
                BuildingKind::House,
                BuildingGen::House,
                None,
                None,
            )
            .unwrap();
        let network = original.electricity.net_id(house).unwrap();
        let electricity = original.electricity.clone();
        test.g.insert(original);

        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        assert_eq!(
            resources
                .read::<ElectricityFlow>()
                .network_stats(network)
                .consumed_power,
            HOUSE_FALLBACK_POWER,
        );

        let mut replacement = Map::empty();
        let station = replacement
            .build_special_building(
                &footprint,
                BuildingKind::TrainStation,
                BuildingGen::NoWalkway {
                    door_pos: Vec2::ZERO,
                },
                None,
                None,
            )
            .unwrap();
        assert_eq!(station, house);
        replacement.electricity = electricity;
        test.g.insert(replacement);

        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        assert_eq!(
            resources.read::<ElectricityFlow>().network_stats(network),
            NetworkFlow::default(),
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "attempt to add with overflow")]
    fn pfb2a3_power_consumption_overflow_panics_in_debug() {
        let mut test = TestCtx::new();
        prototypes::test_prototypes(
            r#"
            package.path = "../base_mod/?.lua"
            require("data")
            for _, prototype in ipairs(data) do
                if prototype.name == "residence-low-l1" then
                    prototype.power_consumption = "9223372036854775807 W"
                end
            end
            "#,
        );
        test.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
        let a = test.build_house_near(vec2(140.0, 125.0));
        let b = test.build_house_near(vec2(190.0, 125.0));
        assert_ne!(a, b);
        let map = test.g.map();
        assert_eq!(map.electricity.net_id(a), map.electricity.net_id(b));
        drop(map);
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
    }

    #[test]
    fn pfb2_cached_flow_tracks_residences_topology_and_load() {
        let mut test = TestCtx::new();
        test.build_roads(&[vec3(80.0, 100.0, 0.0), vec3(440.0, 100.0, 0.0)]);
        test.build_roads(&[vec3(80.0, 300.0, 0.0), vec3(440.0, 300.0, 0.0)]);
        let a = test.build_house_near(vec2(140.0, 125.0));
        let b = test.build_house_near(vec2(140.0, 325.0));
        let level_one = ResidencePrototypeID::new("residence-low-l1").prototype();
        let level_two = ResidencePrototypeID::new("residence-low-l2").prototype();
        residence::on_building_removed(&mut test.g, a);

        let check = |test: &TestCtx, houses: &[(BuildingID, i64)], merged: bool| {
            let map = test.g.map();
            let flow = test.g.read::<ElectricityFlow>();
            assert_matches_full_recompute(
                &test.g.world,
                &map,
                &test.g.read::<BuildingInfos>(),
                &test.g.read::<Residences>(),
                &flow.flowmap,
            );
            assert_eq!(
                map.electricity.net_id(a) == map.electricity.net_id(b),
                merged
            );
            for &(house, watts) in houses {
                let stats = flow.network_stats(map.electricity.net_id(house).unwrap());
                assert_eq!(stats.consumed_power, Power::new(watts));
                assert_eq!(stats.produced_power, Power::ZERO);
                assert!(stats.blackout);
            }
        };
        let refresh = |test: &mut TestCtx| {
            let (world, resources) = test.g.world_res();
            refresh_electricity_flow_system(world, resources);
        };

        test.tick_fast();
        assert_ne!(test.g.get_tick() % TICKS_PER_HOUR, 0);
        check(&test, &[(a, 100), (b, 200)], false);
        residence::register(&test.g, a, level_one);
        refresh(&mut test);
        check(&test, &[(a, 200), (b, 200)], false);

        *test.g.write::<GameTime>() = GameTime::new(Tick(TICKS_PER_HOUR - 1));
        test.tick_fast();
        test.g.write::<Residences>().upgrade(a, level_two);
        refresh(&mut test);
        check(&test, &[(a, 400), (b, 200)], false);

        let original_roads: Vec<_> = test.g.map().roads().keys().collect();
        test.build_roads(&[vec3(440.0, 100.0, 0.0), vec3(440.0, 300.0, 0.0)]);
        let bridge = test
            .g
            .map()
            .roads()
            .keys()
            .find(|road| !original_roads.contains(road))
            .unwrap();
        test.tick_fast();
        check(&test, &[(a, 600), (b, 600)], true);

        let bytes = Bincode::encode(&test.g).unwrap();
        test.g = Bincode::decode(&bytes).unwrap();
        check(&test, &[(a, 600), (b, 600)], true);
        test.tick_fast();
        check(&test, &[(a, 600), (b, 600)], true);

        test.g.map_mut().remove_road(bridge).unwrap();
        test.tick_fast();
        check(&test, &[(a, 400), (b, 200)], false);
        residence::on_building_removed(&mut test.g, a);
        refresh(&mut test);
        check(&test, &[(a, 100), (b, 200)], false);
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
        let mut missing =
            Residence::from(ResidencePrototypeID::new("residence-low-l1").prototype());
        missing.prototype = ResidencePrototypeID::new("missing-residence");
        test.g.write::<Residences>().insert(b1, missing);
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
            test.g.write::<Residences>().insert(
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
