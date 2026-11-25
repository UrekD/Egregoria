use super::{active_business, effective_load, BuildingInfos};
use crate::map::{
    Building, BuildingID, BuildingKind, ElectricityNetwork, ElectricityNetworkID, Map, RoadID,
};
use crate::souls::residence::Residences;
use crate::utils::resources::Resources;
use crate::{CompanyID, SoulID, World};
use prototypes::{prototypes, Power, ResidencePrototypeID};
use serde::Deserialize;
use slotmapd::__impl::Serialize;
use std::collections::BTreeMap;

// The zoning spine owns registration in map_dynamic/mod.rs.
#[path = "site_load.rs"]
mod site_load;
pub use site_load::*;

#[derive(Default, Serialize, Deserialize)]
pub struct ElectricityFlow {
    flowmap: BTreeMap<ElectricityNetworkID, NetworkFlow>,
    #[serde(skip)]
    power_cache: PowerCache,
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
    #[cfg(test)]
    recalculated_networks: usize,
}

struct NetworkLoads {
    id: ElectricityNetworkID,
    houses: Vec<BuildingID>,
    live_sites: Vec<CachedSitePower>,
    residential_watts: i64,
    dirty: bool,
}

struct CachedSitePower {
    id: BuildingID,
    inputs: Option<SitePowerInputs>,
    consumed_watts: i64,
    produced_power: Power,
}

#[derive(PartialEq, Eq)]
struct SitePowerInputs {
    kind: BuildingKind,
    staffing: Option<(CompanyID, usize)>,
    legacy_area: Option<u32>,
    profile: Option<ProfilePowerInputs>,
}

#[derive(PartialEq, Eq)]
struct ProfilePowerInputs {
    road: Option<RoadID>,
    home: Option<(ResidencePrototypeID, u32)>,
    abandoned: bool,
    open: bool,
    level: u8,
    slots: u32,
}

impl SitePowerInputs {
    fn new(
        building: &Building,
        infos: &BuildingInfos,
        residences: &Residences,
        world: &World,
    ) -> Self {
        let profile = building.development().map(|site| ProfilePowerInputs {
            road: building.connected_road,
            home: residences
                .by_building
                .get(&building.id)
                .map(|home| (home.prototype, home.capacity)),
            abandoned: building.is_abandoned_growable(),
            open: site.business == Some(crate::map::GrowableBusinessState::Open),
            level: site.level,
            slots: site.planned_jobs,
        });
        let mut staffing = None;
        if profile
            .as_ref()
            .is_none_or(|site| site.open && !site.abandoned)
        {
            if let (Some(proto), Some(SoulID::GoodsCompany(owner))) =
                (building.kind.as_goods_company(), infos.owner(building.id))
            {
                staffing = world.companies.get(owner).and_then(|company| {
                    (company.comp.building == building.id && company.comp.proto == proto)
                        .then_some((owner, company.workers.0.len()))
                });
            }
        }
        let legacy_area = building
            .zone
            .as_ref()
            .filter(|_| profile.is_none())
            .map(|zone| zone.area.to_bits());
        Self {
            kind: building.kind,
            staffing,
            legacy_area,
            profile,
        }
    }
}

impl CachedSitePower {
    fn refresh(
        &mut self,
        map: &Map,
        infos: &BuildingInfos,
        residences: &Residences,
        world: &World,
    ) -> bool {
        let building = &map.buildings[self.id];
        let inputs = SitePowerInputs::new(building, infos, residences, world);
        if self.inputs.as_ref() == Some(&inputs) {
            return false;
        }
        self.inputs = Some(inputs);
        let (consumed, produced) = if building.development().is_some()
            && connected_site_network(map, building).is_none()
        {
            (0, Power::ZERO)
        } else {
            let load = effective_load(
                building,
                residences.by_building.get(&self.id),
                active_business(building, infos, world),
            );
            (
                load.residential_power
                    .watts()
                    .wrapping_add(load.business_power.watts()),
                load.power_production,
            )
        };
        let changed = self.consumed_watts != consumed || self.produced_power != produced;
        self.consumed_watts = consumed;
        self.produced_power = produced;
        changed
    }
}

impl NetworkLoads {
    fn from_network(map: &Map, network: &ElectricityNetwork) -> Self {
        let mut loads = Self {
            id: network.id,
            houses: Vec::new(),
            live_sites: Vec::new(),
            residential_watts: 0,
            dirty: true,
        };
        for &building in &network.buildings {
            let site = &map.buildings[building];
            if site.development().is_some() || site.kind.as_goods_company().is_some() {
                loads.live_sites.push(CachedSitePower {
                    id: building,
                    inputs: None,
                    consumed_watts: 0,
                    produced_power: Power::ZERO,
                });
            } else if site.kind == BuildingKind::House {
                loads.houses.push(building);
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
        let registry_changed = self
            .key
            .is_none_or(|previous| previous.prototype_registry != key.prototype_registry);
        for loads in &mut self.networks {
            if registry_changed {
                for site in &mut loads.live_sites {
                    site.inputs = None;
                }
            }
            let residential_watts = loads.houses.iter().fold(0i64, |watts, &id| {
                let load =
                    effective_load(&map.buildings[id], residences.by_building.get(&id), None);
                watts.wrapping_add(load.residential_power.watts())
            });
            loads.dirty |= loads.residential_watts != residential_watts;
            loads.residential_watts = residential_watts;
        }
        self.key = Some(key);
    }
}

/// Compute the electricity flow of the map and store it in the [`ElectricityFlow`] resource
/// All producing buildings will produce power, and all consuming buildings will consume power
/// If a network produces less power than it consumes, a blackout will occur
pub fn electricity_flow_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::electricity_flow");
    update_electricity_flow(world, resources);
    #[cfg(debug_assertions)]
    assert_matches_full_recompute(
        world,
        &resources.read::<Map>(),
        &resources.read::<BuildingInfos>(),
        &resources.read::<Residences>(),
        &resources.read::<ElectricityFlow>().flowmap,
    );
}

fn update_electricity_flow(world: &World, resources: &Resources) {
    let map = resources.read::<Map>();
    let infos = resources.read::<BuildingInfos>();
    let residences = resources.read::<Residences>();
    let mut flow = resources.write::<ElectricityFlow>();
    let ElectricityFlow {
        flowmap,
        power_cache,
    } = &mut *flow;

    power_cache.update(
        PowerCacheKey::new(&map, &residences),
        &map,
        &residences,
        flowmap,
    );

    for loads in &mut power_cache.networks {
        // Compare live inputs without resolving prototypes or recomputing loads.
        // Between the two power passes the market can change staffing directly.
        for site in &mut loads.live_sites {
            loads.dirty |= site.refresh(&map, &infos, &residences, world);
        }
        if !std::mem::take(&mut loads.dirty) {
            continue;
        }
        #[cfg(test)]
        {
            power_cache.recalculated_networks += 1;
        }
        let mut consumed_watts = loads.residential_watts;
        let mut produced_power = Power::ZERO;
        for site in &loads.live_sites {
            consumed_watts = consumed_watts.wrapping_add(site.consumed_watts);
            produced_power += site.produced_power;
        }
        let consumed_power = Power::new(consumed_watts);
        *flowmap.get_mut(&loads.id).unwrap() = NetworkFlow {
            consumed_power,
            produced_power,
            blackout: consumed_power > produced_power,
        };
    }
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
            if building.development().is_some() && connected_site_network(map, building).is_none() {
                continue;
            }
            let load = effective_load(
                building,
                residences.by_building.get(&id),
                active_business(building, binfos, world),
            );
            consumed_power += load.residential_power;
            consumed_power += load.business_power;
            produced_power += load.power_production;
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

/// Refresh the snapshot before the utility and coverage passes.
pub(crate) fn refresh_electricity_flow_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("map_dynamic::refresh_electricity_flow");
    update_electricity_flow(world, resources);
}

#[cfg(test)]
pub(crate) use tests::accounting_site;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{market_update, Market};
    use crate::souls::goods_company::company_soul;
    use crate::souls::human::spawn_human;
    use crate::souls::residence::{self, Residence};
    use crate::tests::TestCtx;
    use crate::{CompanyID, HumanID, SoulID};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{
        BuildingGen, GameTime, GoodsCompanyID, ItemID, ResidencePrototypeID, Tick, TICKS_PER_HOUR,
    };

    pub(crate) fn accounting_site(mixed: bool) -> (TestCtx, BuildingID, CompanyID) {
        let jobs = if mixed { 12 } else { 24 };
        accounting_site_with_jobs(mixed, jobs)
    }

    fn accounting_site_with_jobs(mixed: bool, jobs: u32) -> (TestCtx, BuildingID, CompanyID) {
        use crate::map::{
            Density, GrowableBusinessState, GrowableDevelopment, GrowableSiteState,
            PreparedGrowableAppearance, ZoneCell, ZoneKind,
        };
        use crate::souls::growth::check_growable_lot;
        use crate::GrowablePlan;

        let mut test = TestCtx::new_fast();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
        let zone = ZoneCell {
            kind: ZoneKind::Commercial,
            density: Density::Low,
        };
        let (plan, proto) = {
            let map = test.g.map();
            let site = map
                .lots()
                .values()
                .find_map(|lot| check_growable_lot(&map, lot, zone).ok())
                .unwrap();
            let proto = site.company.unwrap().base.id;
            let door = site.shape.center().z(0.1);
            let plan = GrowablePlan::prepare(
                &map,
                site,
                Some(PreparedGrowableAppearance {
                    development: GrowableDevelopment {
                        zone,
                        level: 1,
                        floors: 1,
                        roof_z: 4.0,
                        approved_value_band: 0,
                        frozen_shell: None,
                        home: None,
                        planned_jobs: jobs,
                        site: GrowableSiteState::Active,
                        business: Some(GrowableBusinessState::AwaitingSpawn),
                    },
                    base_z: 0.0,
                    door,
                    mesh: Default::default(),
                }),
                None,
                jobs,
            )
            .unwrap();
            (plan, proto)
        };
        let building = test.g.commit_prepared_growable(plan).unwrap().building;
        let Some(SoulID::GoodsCompany(company)) = company_soul(&mut test.g, building, proto) else {
            panic!("expected a company");
        };
        if mixed {
            // Mixed prototypes are gated in the stock registry. Give the checked
            // storefront the same mixed role state that a prepared site would carry.
            let home = residence::prototype(Density::Medium, 1).unwrap();
            let mut map = test.g.map_mut();
            let site = map.get(building).unwrap();
            let mut development = site.development().unwrap().clone();
            development.zone = ZoneCell {
                kind: ZoneKind::MixedUse,
                density: Density::Medium,
            };
            development.home = Some(home.id);
            let original = Bincode::encode(site.development().unwrap()).unwrap();
            let mut record = Bincode::encode(site).unwrap();
            assert!(record.ends_with(&original));
            record.truncate(record.len() - original.len());
            record.extend(Bincode::encode(&development).unwrap());
            map.buildings[building] = Bincode::decode(&record).unwrap();
            drop(map);
            residence::register(&test.g, building, home);
        }
        (test, building, company)
    }

    #[test]
    fn z3_accounting_mixed_power_and_late_role_change() {
        use crate::map::GrowableBusinessState;
        use crate::map_dynamic::{active_business, effective_load};

        let (mut test, id, company) = accounting_site(true);
        let network = test.g.map().electricity.net_id(id).unwrap();
        let used = |test: &TestCtx| {
            test.g
                .read::<ElectricityFlow>()
                .network_stats(network)
                .consumed_power
        };
        let expected = |test: &TestCtx| {
            let map = test.g.map();
            let building = map.get(id).unwrap();
            let infos = test.g.read::<BuildingInfos>();
            effective_load(
                building,
                test.g.read::<Residences>().by_building.get(&id),
                active_business(building, &infos, test.g.world()),
            )
        };
        let refresh = |test: &mut TestCtx| {
            let (world, resources) = test.g.world_res();
            refresh_electricity_flow_system(world, resources);
            assert_matches_full_recompute(
                world,
                &resources.read::<Map>(),
                &resources.read::<BuildingInfos>(),
                &resources.read::<Residences>(),
                &resources.read::<ElectricityFlow>().flowmap,
            );
        };

        let vacant = expected(&test);
        assert!(vacant.residential_power > Power::ZERO);
        assert!(vacant.business_water > 0.0);
        assert_eq!(vacant.business_power, Power::ZERO);
        refresh(&mut test);
        assert_eq!(used(&test), vacant.residential_power);

        test.g.world.companies[company].workers.0 = vec![HumanID::default(); 6];
        refresh(&mut test);
        let staffed = expected(&test);
        assert!(staffed.business_power > Power::ZERO);
        assert_eq!(
            used(&test),
            staffed.residential_power + staffed.business_power
        );

        test.g.write::<GameTime>().tick.0 += 1;
        assert_ne!(test.g.get_tick() % TICKS_PER_HOUR, 0);
        test.g.map_mut().set_growable_business_state(
            id,
            GrowableBusinessState::ClosedUntil {
                retry_elapsed_tick: u64::MAX,
            },
        );
        refresh(&mut test);
        assert_eq!(used(&test), staffed.residential_power);

        test.g.map_mut().mark_growable_abandoned(id, 0);
        refresh(&mut test);
        assert_eq!(used(&test), Power::ZERO);
    }

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
    fn z3_accounting_late_power_uses_changed_slots_with_the_same_staff() {
        use crate::map::PreparedGrowableAppearance;

        let (mut test, id, company) = accounting_site_with_jobs(false, 18);
        test.g.world.companies[company].workers.0 = vec![HumanID::default(); 9];
        let network = test.g.map().electricity.net_id(id).unwrap();
        let business_power = test.g.world.companies[company]
            .comp
            .proto
            .prototype()
            .power_consumption
            .unwrap();
        let refresh = |test: &mut TestCtx| {
            let (world, res) = test.g.world_res();
            refresh_electricity_flow_system(world, res);
            res.read::<ElectricityFlow>()
                .network_stats(network)
                .consumed_power
        };
        assert_eq!(refresh(&mut test), business_power * 0.5);
        {
            let mut map = test.g.map_mut();
            let building = map.get(id).unwrap();
            let mut development = building.development().unwrap().clone();
            development.level += 1;
            development.planned_jobs = 36;
            let appearance = PreparedGrowableAppearance {
                development,
                base_z: building.height,
                door: building.door_pos,
                mesh: Default::default(),
            };
            assert!(map.update_growable_appearance(id, appearance));
        }
        assert_eq!(refresh(&mut test), business_power * 0.25);
    }

    #[test]
    fn z3_fix_late_power_skips_clean_networks() {
        let (mut test, first, shop) = accounting_site(true);
        let ends = [vec3(100.0, 300.0, 0.0), vec3(400.0, 300.0, 0.0)];
        test.build_roads(&ends);
        let second = test.build_house_near(vec2(150.0, 325.0));
        assert_ne!(
            test.g.map().electricity.net_id(first),
            test.g.map().electricity.net_id(second)
        );
        let (world, res) = test.g.world_res();
        electricity_flow_system(world, res);
        let count = |res: &Resources| {
            res.read::<ElectricityFlow>()
                .power_cache
                .recalculated_networks
        };
        let initial = count(res);
        refresh_electricity_flow_system(world, res);
        assert_eq!(count(res), initial);
        world.companies[shop].workers.0.push(HumanID::default());
        refresh_electricity_flow_system(world, res);
        assert_eq!(count(res), initial + 1);
        res.write::<BuildingInfos>()
            .set_owner(first, SoulID::Human(HumanID::default()));
        refresh_electricity_flow_system(world, res);
        assert_eq!(count(res), initial + 2);
        assert_matches_full_recompute(
            world,
            &res.read::<Map>(),
            &res.read::<BuildingInfos>(),
            &res.read::<Residences>(),
            &res.read::<ElectricityFlow>().flowmap,
        );
    }

    #[test]
    fn z3_fix_inspector_follows_shop_closure_and_provider_capacity_between_samples() {
        use crate::diagnostics::building_diagnostics;
        use crate::map::{GrowableBusinessState, Zone, MAX_ZONE_AREA};
        use crate::map_dynamic::{utility_flow_update, UtilityStatus};

        let (mut test, home, _) = accounting_site(true);
        let plant = company(&mut test, home, "coal-power-plant", 220.0);
        let pump = company(&mut test, home, "water-pump", 270.0);
        let outlet = company(&mut test, home, "sewage-outlet", 320.0);
        let pump_building = test.g.world.companies[pump].comp.building;
        let water_status = |test: &TestCtx| {
            building_diagnostics(&test.g, home)
                .unwrap()
                .utilities
                .unwrap()
        };
        for id in [plant, pump, outlet] {
            let provider = &mut test.g.world.companies[id];
            provider.workers.0 = vec![HumanID::default(); provider.comp.max_workers as usize];
        }
        let water_between_roles = {
            let map = test.g.map();
            let homes = test.g.read::<Residences>();
            let infos = test.g.read::<BuildingInfos>();
            let network = connected_site_network(&map, map.get(home).unwrap()).unwrap();
            let live = NetworkLoad::live(&map, &homes, &infos, test.g.world(), network).unwrap();
            let shop = building_diagnostics(&test.g, home).unwrap().load.business_water;
            live.pipes.water_demand - shop / 2.0
        };
        let pump_output = f64::from(
            GoodsCompanyID::new("water-pump")
                .prototype()
                .water_production
                .unwrap(),
        );
        test.g.map_mut().buildings[pump_building].zone = Some(Zone {
            poly: Default::default(),
            area: (water_between_roles / pump_output) as f32 * MAX_ZONE_AREA,
            filldir: Vec2::X,
        });
        test.g.write::<GameTime>().tick = Tick(3001);
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        utility_flow_update(&mut test.g);
        assert_eq!(water_status(&test).water, UtilityStatus::Shortage);

        test.g.map_mut().buildings[pump_building]
            .zone
            .as_mut()
            .unwrap()
            .area = MAX_ZONE_AREA;
        assert_eq!(water_status(&test).water, UtilityStatus::Shortage);

        test.g.map_mut().set_growable_business_state(
            home,
            GrowableBusinessState::ClosedUntil {
                retry_elapsed_tick: u64::MAX,
            },
        );
        let closed = water_status(&test);
        test.g.world.companies[pump].workers.0.clear();
        let reduced = water_status(&test);
        assert_eq!(
            (closed.water, closed.sewage, reduced.water),
            (
                UtilityStatus::Served,
                UtilityStatus::Served,
                UtilityStatus::Shortage
            )
        );
        assert_eq!(reduced.totals.unwrap().sewage_demand, 0.0);
        let projected = building_diagnostics(&test.g, home).unwrap().load;
        assert_eq!(
            NetworkReservations::new(&test.g.map()).try_reserve(&test.g, home, projected),
            Err(UtilityAdmission::Water)
        );
    }

    #[test]
    fn z3_fix_inspector_rejects_sample_after_split_retains_network_id() {
        use crate::diagnostics::building_diagnostics;
        use crate::map_dynamic::{utility_flow_update, UtilityStatus};
        use common::saveload::{Bincode, Encoder};

        let (mut test, home, _) = accounting_site(true);
        test.build_roads(&[vec3(100.0, 300.0, 0.0), vec3(400.0, 300.0, 0.0)]);
        let other = test.build_house_near(vec2(150.0, 325.0));
        let original = test.g.map().roads().keys().collect::<Vec<_>>();
        test.build_roads(&[vec3(400.0, 100.0, 0.0), vec3(400.0, 300.0, 0.0)]);
        let bridge = test
            .g
            .map()
            .roads()
            .keys()
            .find(|road| !original.contains(road))
            .unwrap();
        for (name, x) in [
            ("coal-power-plant", 220.0),
            ("water-pump", 270.0),
            ("sewage-outlet", 320.0),
        ] {
            let id = company(&mut test, other, name, x);
            let provider = &mut test.g.world.companies[id];
            provider.workers.0 = vec![HumanID::default(); provider.comp.max_workers as usize];
        }
        let network = test.g.map().electricity.net_id(home).unwrap();
        assert_eq!(test.g.map().electricity.net_id(other), Some(network));
        test.g.write::<GameTime>().tick = Tick(3001);
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        utility_flow_update(&mut test.g);
        let status = |test: &TestCtx| {
            building_diagnostics(&test.g, home)
                .unwrap()
                .utilities
                .unwrap()
        };
        assert_eq!(status(&test).water, UtilityStatus::Served);
        assert_eq!(
            crate::economy::progression::utility_served_fraction(&test.g),
            Some(1.0)
        );
        let saved = Bincode::encode(&test.g).unwrap();
        let loaded = Bincode::decode(&saved).unwrap();
        assert_eq!(
            building_diagnostics(&loaded, home).unwrap().utilities.unwrap().water,
            UtilityStatus::Served,
            "loading a dry map must preserve the sampled service of unchanged network members"
        );
        test.g.map_mut().remove_road(bridge).unwrap();
        assert_eq!(test.g.map().electricity.net_id(home), Some(network));
        assert_ne!(test.g.map().electricity.net_id(other), Some(network));
        assert_eq!(status(&test).water, UtilityStatus::Unknown);
        assert_eq!(status(&test).sewage, UtilityStatus::Unknown);
        assert_eq!(
            crate::economy::progression::utility_served_fraction(&test.g),
            Some(0.0)
        );
    }

    #[test]
    fn z3_accounting_network_reservations_share_live_headroom() {
        use crate::map_dynamic::{utility_flow_update, UtilityFlow};

        let (mut test, first, shop) = accounting_site(true);
        let second = test.build_house_near(vec2(175.0, 125.0));
        for (name, x) in [
            ("coal-power-plant", 220.0),
            ("water-pump", 270.0),
            ("sewage-outlet", 320.0),
        ] {
            let provider = company(&mut test, first, name, x);
            let provider = &mut test.g.world.companies[provider];
            provider.workers.0 = vec![HumanID::default(); provider.comp.max_workers as usize];
        }
        test.g.write::<GameTime>().tick = Tick(3001);
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        utility_flow_update(&mut test.g);

        let (mut ledger, first_next, second_next) = {
            use crate::map_dynamic::{active_business, effective_load};

            let map = test.g.map();
            let homes = test.g.read::<Residences>();
            let infos = test.g.read::<BuildingInfos>();
            let world = test.g.world();
            let network = map.electricity.net_id(first).unwrap();
            assert_eq!(map.electricity.net_id(second), Some(network));
            let live = NetworkLoad::live(&map, &homes, &infos, world, network).unwrap();
            let electricity = test.g.read::<ElectricityFlow>();
            let power = electricity.network_stats(network);
            assert_eq!(
                live.power_demand_watts,
                i128::from(power.consumed_power.watts())
            );
            assert_eq!(
                live.power_supply_watts,
                i128::from(power.produced_power.watts())
            );
            let flow = test.g.read::<UtilityFlow>();
            let status =
                live_site_utility_status(&map, &homes, &infos, world, &flow, first)
                    .unwrap();
            assert_eq!(status.totals, Some(live.pipes));
            let add = (live.pipes.water_production - live.pipes.water_demand) * 0.75;
            assert!(add > 0.0);
            let increased = |id| {
                let building = map.get(id).unwrap();
                let active = active_business(building, &infos, world);
                let home = homes.by_building.get(&id);
                let mut next = projected_site_load(building, home, active);
                if id == first {
                    assert_eq!(active.unwrap().company, shop);
                    assert_eq!(
                        effective_load(building, home, active).business_power,
                        Power::ZERO
                    );
                    assert!(next.business_power > Power::ZERO);
                }
                next.residential_water += add;
                next.residential_sewage += add;
                next
            };
            (
                NetworkReservations::new(&map),
                increased(first),
                increased(second),
            )
        };

        assert_eq!(ledger.try_reserve(&test.g, first, first_next), Ok(()));
        assert_eq!(
            ledger.try_reserve(&test.g, second, second_next),
            Err(UtilityAdmission::Water)
        );
        let mut fresh = NetworkReservations::new(&test.g.map());
        assert_eq!(fresh.try_reserve(&test.g, second, second_next), Ok(()));

        test.g.map_mut().electricity.remove_object(second);
        assert_eq!(
            ledger.try_reserve(&test.g, second, second_next),
            Err(UtilityAdmission::StaleTopology)
        );
        assert_eq!(
            NetworkReservations::new(&test.g.map()).try_reserve(&test.g, second, second_next),
            Err(UtilityAdmission::Disconnected)
        );
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
            Power::new(100),
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
    fn pfb2a3_power_consumption_overflow_panics_in_debug() {
        if !crate::tests::in_prototype_fixture_process(
            module_path!(),
            "pfb2a3_power_consumption_overflow_panics_in_debug",
        ) {
            return;
        }
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
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            electricity_flow_system(world, resources);
        }))
        .expect_err("expected electricity consumption overflow");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(message, Some("attempt to add with overflow"));
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
