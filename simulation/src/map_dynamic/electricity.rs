use crate::map::{BuildingKind, ElectricityNetworkID, Map};
use crate::map_dynamic::BuildingInfos;
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::{SoulID, World};
use prototypes::{try_prototype, Power};
use serde::Deserialize;
use slotmapd::__impl::Serialize;
use std::collections::BTreeMap;

#[derive(Default, Serialize, Deserialize)]
pub struct ElectricityFlow {
    flowmap: BTreeMap<ElectricityNetworkID, NetworkFlow>,
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
    let mut flow = resources.write::<ElectricityFlow>();

    flow.flowmap.clear();

    for network in map.electricity.networks.values() {
        let mut consumed_power: Power = Power::ZERO;
        let mut produced_power: Power = Power::ZERO;

        for building in network.buildings.iter() {
            let building = map.buildings.get(*building).unwrap();

            match building.kind {
                BuildingKind::House => {
                    consumed_power += residences
                        .by_building
                        .get(&building.id)
                        .and_then(|home| try_prototype(home.prototype))
                        .and_then(|proto| proto.power_consumption)
                        .unwrap_or(Power::new(100));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::souls::residence::{self, Residence};
    use crate::tests::TestCtx;
    use geom::{vec2, vec3};
    use prototypes::ResidencePrototypeID;

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
