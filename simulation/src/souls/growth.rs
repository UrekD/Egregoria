use crate::economy::balance::{periodic_tick, CITY_UPDATE_INTERVAL_TICKS};
use crate::economy::{Budget, ZoneDemand};
use crate::map::{
    BuildingID, BuildingKind, LotID, Map, ProjectFilter, ZoneCell, ZoneGrid, ZoneKind,
};
use crate::map_dynamic::BuildingInfos;
use crate::souls::{lifecycle, residence};
use crate::Simulation;
use common::FastMap;
use geom::Vec2;
use ordered_float::OrderedFloat;
use prototypes::{GrowableCompanyPrototype, ResidencePrototype, Tick};

pub(crate) const GROWTH_INTERVAL: u64 = CITY_UPDATE_INTERVAL_TICKS;
const MAX_BUILDING_SEARCH_RADIUS: f32 = 2_048.0;

#[derive(Default)]
struct GrowthCache {
    zone_generation: u64,
    zones: FastMap<LotID, Option<ZoneCell>>,
    buildings: Vec<(BuildingID, Vec2)>,
    distances: FastMap<LotID, OrderedFloat<f32>>,
    zoned_lots: Vec<(LotID, ZoneCell)>,
    off_center_buildings: Vec<Vec2>,
    candidates: Vec<GrowthCandidate>,
}

struct GrowthCandidate {
    zone: ZoneKind,
    distance: OrderedFloat<f32>,
    lot: LotID,
    kind: BuildingKind,
    home: Option<&'static ResidencePrototype>,
    quota: u8,
}

pub(crate) const fn due(tick: u64) -> bool {
    tick >= GROWTH_INTERVAL && tick % GROWTH_INTERVAL == 0
}

pub fn growth_update(sim: &mut Simulation) {
    let tick = periodic_tick(&sim.read::<Budget>(), Tick(sim.get_tick()));
    if !due(tick) {
        return;
    }
    profiling::scope!("souls::growth_pass");

    let mut orphaned = {
        let map = sim.map();
        map.buildings()
            .iter()
            .filter_map(|(id, building)| {
                (building.is_orphaned()
                    && matches!(building.kind, BuildingKind::GoodsCompany(_))
                    && map.lot_origin(id).is_some())
                .then_some(id)
            })
            .collect::<Vec<_>>()
    };
    orphaned.sort_unstable();
    for id in orphaned {
        remove_growable(sim, id);
    }

    if sim.read::<ZoneGrid>().cells.is_empty() {
        return;
    }
    // A save from before staggering can reach the first growth phase without a demand sample.
    if !sim.read::<ZoneDemand>().has_sample() {
        return;
    }

    let mut zoned_lots = {
        let (_, resources) = sim.world_res();
        resources.write_or_default::<GrowthCache>();
        let map = resources.read::<Map>();
        let zones = resources.read::<ZoneGrid>();
        let mut cache = resources.write::<GrowthCache>();
        if cache.zone_generation != zones.generation {
            cache.zone_generation = zones.generation;
            cache.zones.clear();
        }
        // A lot's shape is immutable for its ID, even when frontage changes.
        cache.zones.retain(|&id, _| map.get(id).is_some());
        cache.distances.retain(|&id, _| map.get(id).is_some());
        let GrowthCache {
            zones: lot_zones,
            zoned_lots,
            ..
        } = &mut *cache;
        zoned_lots.extend(map.lots().iter().filter_map(|(id, lot)| {
            lot_zones
                .entry(id)
                .or_insert_with(|| zones.lot_zone(&lot.shape))
                .map(|zone| (id, zone))
        }));
        std::mem::take(zoned_lots)
    };
    grow_zoned_lots(sim, zoned_lots.drain(..));
    sim.write::<GrowthCache>().zoned_lots = zoned_lots;
}

fn grow_zoned_lots(
    sim: &mut Simulation,
    zoned_lots: impl IntoIterator<Item = (LotID, ZoneCell)>,
) {
    let mut candidates = {
        let (_, resources) = sim.world_res();
        resources.write_or_default::<GrowthCache>();
        let map = resources.read::<Map>();
        let demand = resources.read::<ZoneDemand>();
        let mut cache = resources.write::<GrowthCache>();
        let building_centers = || map.buildings().iter().map(|(id, b)| (id, b.obb.center()));
        if !building_centers().eq(cache.buildings.iter().copied()) {
            cache.buildings.clear();
            cache.buildings.extend(building_centers());
            cache.distances.clear();
        }
        let GrowthCache {
            distances,
            off_center_buildings,
            candidates,
            ..
        } = &mut *cache;
        let mut scanned_off_center_buildings = false;
        for (id, cell) in zoned_lots {
            let value = demand.values.get(&cell.kind).copied().unwrap_or_default();
            if value <= 5.0 || !value.is_finite() {
                continue;
            }
            let Some(lot) = map.get(id).filter(|lot| map.get(lot.parent).is_some()) else {
                continue;
            };
            let home = if cell.kind == ZoneKind::Residential {
                residence::prototype(cell.density, 1)
            } else {
                None
            };
            let (kind, lot_size_min) = if let Some(proto) = home {
                (BuildingKind::House, proto.lot_size_min)
            } else if let Some(proto) = GrowableCompanyPrototype::iter().find(|proto| {
                proto.growable
                    && proto.zone_kind == cell.kind
                    && proto.density == cell.density
                    && proto.level == 1
            }) {
                (
                    BuildingKind::GoodsCompany(proto.base.id),
                    proto.lot_size_min,
                )
            } else {
                continue;
            };

            let [width, depth] = lot.shape.axis();
            if width.mag() * depth.mag() + 0.001 < lot_size_min {
                continue;
            }
            let center = lot.shape.center();
            let distance = *distances.entry(id).or_insert_with(|| {
                if !scanned_off_center_buildings {
                    // A zoned building is indexed by its zone polygon, which can omit its center.
                    off_center_buildings.clear();
                    off_center_buildings.extend(map.buildings().values().filter_map(|building| {
                        let center = building.obb.center();
                        building
                            .zone
                            .as_ref()
                            .filter(|zone| !zone.poly.contains(center))?;
                        Some(center)
                    }));
                    scanned_off_center_buildings = true;
                }
                nearest_building_distance(&map, center, off_center_buildings)
            });
            let quota = ((value / 20.0).ceil() as u8).min(4);
            candidates.push(GrowthCandidate {
                zone: cell.kind,
                distance,
                lot: id,
                kind,
                home,
                quota,
            });
        }
        std::mem::take(candidates)
    };
    candidates.sort_unstable_by_key(|candidate| (candidate.zone, candidate.distance, candidate.lot));

    let mut current_zone = None;
    let mut remaining = 0;
    for GrowthCandidate {
        zone, lot, kind, home, quota, ..
    } in candidates.drain(..) {
        if current_zone != Some(zone) {
            current_zone = Some(zone);
            remaining = quota;
        }
        if remaining == 0 {
            continue;
        }
        let building = sim.map_mut().place_growable(lot, kind);
        if let Some(building) = building {
            on_growable_placed(sim, building, kind);
            if let Some(proto) = home.filter(|proto| proto.density != crate::map::Density::Low) {
                residence::register(sim, building, proto);
            }
            remaining -= 1;
        }
    }
    sim.write::<GrowthCache>().candidates = candidates;
}

fn nearest_building_distance(
    map: &Map,
    center: Vec2,
    off_center_buildings: &[Vec2],
) -> OrderedFloat<f32> {
    let mut nearest = off_center_buildings
        .iter()
        .map(|building| OrderedFloat(building.distance2(center)))
        .min()
        .unwrap_or(OrderedFloat(f32::INFINITY));
    if map.buildings().is_empty() {
        return nearest;
    }
    let mut radius = 32.0_f32;
    while radius <= MAX_BUILDING_SEARCH_RADIUS {
        // Footprints can intersect the radius before their centers are inside it.
        if let Some(distance) = map
            .spatial_map()
            .query_around(center, radius, ProjectFilter::BUILDING)
            .filter_map(|object| map.get(object.as_building()?))
            .map(|building| OrderedFloat(building.obb.center().distance2(center)))
            .min()
        {
            nearest = nearest.min(distance);
        }
        if nearest.0 <= radius * radius {
            return nearest;
        }
        radius *= 2.0;
    }

    // An expanding grid query visits empty cells too. A single scan keeps the
    // exact ordering without making the search cost depend on map distances.
    map.buildings()
        .values()
        .map(|building| OrderedFloat(building.obb.center().distance2(center)))
        .min()
        .unwrap_or(nearest)
}

/// Registers simulation state after successful map placement.
pub fn on_growable_placed(sim: &Simulation, building: BuildingID, kind: BuildingKind) {
    sim.write::<BuildingInfos>().insert(building);
    if kind == BuildingKind::House {
        residence::on_house_built(sim, building);
    }
}

/// Clears residence metadata after successful map removal.
pub fn on_growable_removed(sim: &mut Simulation, building: BuildingID) {
    residence::on_building_removed(sim, building);
}

pub(crate) fn remove_growable(sim: &mut Simulation, id: BuildingID) {
    let building = sim.map_mut().remove_building(id);
    if let Some(building) = building {
        on_growable_removed(sim, id);
        lifecycle::remove_building(sim, building, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{Market, ZoneDemand};
    use crate::map::{Density, LanePatternBuilder, Map, MapProject, RoadID};
    use crate::map_dynamic::{
        BuildingInfos, CoverageGrid, Destination, ParkingManagement, SpotReservation, UtilityFlow,
    };
    use crate::souls::desire::{Work, WorkKind};
    use crate::souls::{add_souls_to_empty_buildings, lifecycle};
    use crate::tests::TestCtx;
    use crate::{SoulID, WorldCommand};
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, vec3, OBB};
    use prototypes::{GameTime, GoodsCompanyID, ItemID, ServiceKind, TICKS_PER_HOUR};

    fn street() -> TestCtx {
        let test = TestCtx::new();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
        test
    }

    fn residential_demand(sim: &Simulation, demand: f32) {
        sim.write::<ZoneDemand>()
            .values
            .insert(ZoneKind::Residential, demand);
    }

    fn hourly_check(sim: &mut Simulation) {
        let tick = sim.get_tick();
        let mut next = tick - tick % TICKS_PER_HOUR + residence::UPDATE_OFFSET;
        if next <= tick {
            next += TICKS_PER_HOUR;
        }
        assert!(residence::due(next));
        sim.write::<GameTime>().tick.0 = next;
        residence::residence_update(sim);
        crate::utils::scheduler::drain_commands(sim);
    }

    fn serve_house(sim: &mut Simulation, house: BuildingID) {
        let road = sim.map().get(house).unwrap().connected_road.unwrap();
        for (name, x) in [
            ("coal-power-plant", 160.0),
            ("water-pump", 230.0),
            ("sewage-outlet", 300.0),
        ] {
            let proto = GoodsCompanyID::new(name).prototype();
            let site = sim
                .map_mut()
                .build_special_building(
                    &OBB::new(vec2(x, 70.0), geom::Vec2::X, 20.0, 20.0),
                    BuildingKind::GoodsCompany(proto.id),
                    proto.bgen,
                    None,
                    Some(road),
                )
                .unwrap();
            sim.write::<BuildingInfos>().insert(site);
            let Some(SoulID::GoodsCompany(owner)) =
                crate::souls::goods_company::company_soul(sim, site, proto.id)
            else {
                panic!("could not start {name}");
            };
            // The fixture only runs residence checks. Model staffing for the
            // utility snapshots without adding residents or running the labor market.
            sim.world.companies[owner]
                .workers
                .0
                .push(crate::HumanID::default());
        }
        let (world, resources) = sim.world_res();
        crate::map_dynamic::electricity_flow_system(world, resources);
        let tick = sim.get_tick();
        sim.write::<GameTime>().tick.0 = GROWTH_INTERVAL + UtilityFlow::UPDATE_OFFSET;
        assert!(UtilityFlow::due(sim.get_tick()));
        crate::map_dynamic::utility_flow_update(sim);
        sim.write::<GameTime>().tick.0 = tick;
        let utilities = sim.read::<UtilityFlow>();
        assert!(utilities.has_water(house) && utilities.has_sewage(house));
    }

    fn place_store(sim: &Simulation, lot: LotID) -> BuildingID {
        let kind = BuildingKind::GoodsCompany(prototypes::GoodsCompanyID::new("commercial-l1"));
        let building = sim.map_mut().place_growable(lot, kind).unwrap();
        on_growable_placed(sim, building, kind);
        building
    }

    #[test]
    fn p1bce_grows_high_density_company_zones() {
        for zone in [ZoneKind::Commercial, ZoneKind::Industrial, ZoneKind::Office] {
            let mut test = street();
            test.apply(&[WorldCommand::ZonePaint {
                area: OBB::new(vec2(250.0, 100.0), geom::Vec2::X, 320.0, 140.0),
                kind: Some(zone),
                density: Density::High,
            }]);
            test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL - 1;
            let (world, resources) = test.g.world_res();
            crate::economy::demand_update(world, resources);
            test.g.write::<ZoneDemand>().values.insert(zone, 40.0);
            assert!(due(GROWTH_INTERVAL));
            test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL;
            growth_update(&mut test.g);
            let map = test.g.map();
            assert!(
                map.buildings().values().any(|building| {
                    let Some(company) = building.kind.as_goods_company() else {
                        return false;
                    };
                    let proto = prototypes::GrowableCompanyID::from(company).prototype();
                    proto.density == Density::High && proto.zone_kind == zone && proto.level == 1
                }),
                "high-density {zone:?} lot did not grow"
            );
        }
    }

    #[test]
    fn p1bce_player_house_survives_low_occupancy() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        assert!(test.g.map().lot_origin(house).is_none());
        test.g.write::<GameTime>().tick.0 = 0;
        serve_house(&mut test.g, house);
        for _ in 0..36 {
            hourly_check(&mut test.g);
        }
        assert!(test.g.map().get(house).is_some());
        assert!(!test.g.read::<residence::Residences>().is_abandoned(house));
    }

    #[test]
    fn p1bce_unavailable_parking_keeps_car_if_parked() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        let car = test.g.get(human).unwrap().router.personal_car.unwrap();
        let destination = test.g.map().get(house).unwrap().door_pos;
        let spots = fill_parking(&test.g, destination, 400.0);
        test.g.world_mut_unchecked().humans[human]
            .router
            .target_dest = Some(Destination::Outside(destination));
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        crate::utils::scheduler::drain_commands(&mut test.g);
        let router = &test.g.get(human).unwrap().router;
        assert!(router.last_error.is_none());
        assert_eq!(router.personal_car, Some(car));
        assert!(test.g.get(car).is_some());
        for spot in spots {
            test.g.write::<ParkingManagement>().free(spot);
        }
    }

    #[test]
    fn p1bce_work_truck_uses_legacy_parking_search() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        let house_door = test.g.map().get(house).unwrap().door_pos;
        let truck = crate::transportation::spawn_parked_vehicle(
            &mut test.g,
            crate::transportation::VehicleKind::Truck,
            house_door,
        )
        .unwrap();
        let resident = &mut test.g.world_mut_unchecked().humans[human];
        resident.work = Some(Work::new(
            house,
            WorkKind::Driver {
                truck,
                deliver_order: None,
            },
            0.0,
        ));
        resident.router.use_vehicle(Some(truck));
        resident.router.target_dest = Some(Destination::Outside(vec3(175.0, 600.0, 0.0)));
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        assert!(test.g.get(human).unwrap().router.last_error.is_none());
    }

    #[test]
    fn p1b_growth_and_residence_lifecycle() {
        let mut test = street();
        residential_demand(&test.g, 0.0);
        let lot = front_lot(&test.g);
        test.apply(&[WorldCommand::MapBuildHouse(lot)]);
        let house = *test
            .g
            .read::<residence::Residences>()
            .by_building
            .keys()
            .next()
            .unwrap();
        add_souls_to_empty_buildings(&mut test.g);
        assert!(
            test.g.world().humans.is_empty(),
            "zero demand prevents immigration"
        );

        residential_demand(&test.g, 40.0);
        add_souls_to_empty_buildings(&mut test.g);
        assert_eq!(test.g.world().humans.len(), 1);
        add_souls_to_empty_buildings(&mut test.g);
        add_souls_to_empty_buildings(&mut test.g);
        assert_eq!(test.g.world().humans.len(), 2);

        let residents: Vec<_> = test.g.world().humans.keys().collect();
        let owner = test.g.read::<BuildingInfos>().owner(house);
        assert_eq!(owner, Some(SoulID::Human(residents[0])));
        assert_eq!(
            test.g
                .read::<BuildingInfos>()
                .building_owned_by(SoulID::Human(residents[1])),
            Some(house)
        );
        lifecycle::despawn_human(&mut test.g, residents[0]);
        crate::utils::scheduler::drain_commands(&mut test.g);
        assert_eq!(
            test.g.read::<BuildingInfos>().owner(house),
            Some(SoulID::Human(residents[1]))
        );
    }

    #[test]
    fn p1b_level_up_after_six_good_checks() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let mesh_top = |sim: &Simulation| {
            sim.map()
                .get(house)
                .unwrap()
                .mesh
                .faces
                .iter()
                .flat_map(|(face, _)| face.iter().map(|vertex| vertex.z))
                .fold(f32::NEG_INFINITY, f32::max)
        };
        let original_top = mesh_top(&test.g);
        residential_demand(&test.g, 40.0);
        for _ in 0..2 {
            add_souls_to_empty_buildings(&mut test.g);
        }
        let lot = test.g.map().lots().keys().next().unwrap();
        let store = place_store(&test.g, lot);

        for human in test.g.world_mut_unchecked().humans.values_mut() {
            human.work = Some(Work::new(store, WorkKind::Worker, 0.0));
        }
        test.g.write::<GameTime>().tick.0 = 0;
        serve_house(&mut test.g, house);
        test.g
            .write::<CoverageGrid>()
            .by_kind
            .entry(ServiceKind::School)
            .or_default()
            .insert(house, 30.0);
        for _ in 0..5 {
            hourly_check(&mut test.g);
        }
        let residences = test.g.read::<residence::Residences>();
        assert_eq!(residences.by_building.get(&house).map(|r| r.level), Some(1));
        drop(residences);
        hourly_check(&mut test.g);
        let residences = test.g.read::<residence::Residences>();
        let home = &residences.by_building[&house];
        assert_eq!((home.level, home.capacity), (2, 4));
        assert!(mesh_top(&test.g) > original_top);
    }

    #[test]
    fn p1b_orphan_rehomes_then_emigrates_and_reposts_job() {
        let mut test = street();
        residential_demand(&test.g, 40.0);
        let house = test.build_house_near(vec2(175.0, 125.0));
        add_souls_to_empty_buildings(&mut test.g);
        add_souls_to_empty_buildings(&mut test.g);
        let residents: Vec<_> = test.g.world().humans.keys().collect();
        assert_eq!(residents.len(), 2);

        let road = test.g.map().get(house).unwrap().connected_road.unwrap();
        test.apply(&[WorldCommand::MapRemoveRoad(road)]);
        test.build_roads(&[vec3(100.0, 300.0, 0.0), vec3(400.0, 300.0, 0.0)]);
        let neighbor = test.build_house_near(vec2(175.0, 325.0));
        add_souls_to_empty_buildings(&mut test.g);
        residential_demand(&test.g, 0.0);
        let lot = test.g.map().lots().keys().next().unwrap();
        place_store(&test.g, lot);
        add_souls_to_empty_buildings(&mut test.g);
        let company = test.g.world().companies.keys().next().unwrap();
        let workplace = test.g.get(company).unwrap().comp.building;
        let company_soul = SoulID::GoodsCompany(company);
        let job = ItemID::new("job-opening");
        let emigrant_soul = SoulID::Human(residents[1]);
        {
            let mut market = test.g.write::<Market>();
            market.produce(company_soul, job, -1);
            market.sell_all(company_soul, vec2(0.0, 0.0), job, 0);
        }
        let jobs_before = test
            .g
            .read::<Market>()
            .iter()
            .find(|(item, _)| **item == job)
            .unwrap()
            .1
            .sell_order(company_soul)
            .unwrap()
            .qty;
        if let Some(car) = test.g.get(residents[1]).unwrap().router.personal_car {
            test.g
                .read::<crate::ParCommandBuffer<crate::VehicleEnt>>()
                .kill(car);
            crate::utils::scheduler::drain_commands(&mut test.g);
        }
        let door = test.g.map().get(workplace).unwrap().door_pos;
        let car = crate::transportation::spawn_parked_vehicle(
            &mut test.g,
            crate::transportation::VehicleKind::Car,
            door,
        )
        .unwrap();
        let car_pos = test.g.get(car).unwrap().trans.pos;
        let spot = test
            .g
            .map()
            .parking
            .all_spots()
            .find_map(|(id, spot)| (spot.trans.pos == car_pos).then_some(id))
            .unwrap();
        assert!(!test.g.read::<ParkingManagement>().is_spot_free(spot));
        let world = test.g.world_mut_unchecked();
        world.companies[company].workers.0.push(residents[1]);
        world.humans[residents[1]].router = crate::map_dynamic::Router::new(Some(car));
        world.humans[residents[1]].work = Some(Work::new(workplace, WorkKind::Worker, 0.0));

        test.g.write::<GameTime>().tick.0 = 0;
        hourly_check(&mut test.g);
        assert_eq!(test.g.get(residents[0]).unwrap().home.house, neighbor);
        assert!(test.g.get(residents[1]).is_none());
        assert!(test.g.get(car).is_none());
        assert!(test.g.read::<ParkingManagement>().is_spot_free(spot));
        assert!(test.g.get(company).unwrap().workers.0.is_empty());
        let market = test.g.read::<Market>();
        for (_, item) in market.iter() {
            assert!(item.buy_order(emigrant_soul).is_none());
            assert!(item.sell_order(emigrant_soul).is_none());
            assert!(item.capital(emigrant_soul).is_none());
        }
        assert_eq!(
            market
                .iter()
                .find(|(item, _)| **item == job)
                .unwrap()
                .1
                .sell_order(company_soul)
                .unwrap()
                .qty,
            jobs_before + 1
        );
        drop(market);

        let infos = test.g.read::<BuildingInfos>();
        assert!(infos.get(house).unwrap().inside.is_empty());
        assert!(infos.owner(house).is_none());
        assert!(infos.building_owned_by(emigrant_soul).is_none());
        drop(infos);
        for _ in 0..12 {
            hourly_check(&mut test.g);
        }
        assert!(test.g.map().get(house).is_none());
        assert!(!test
            .g
            .read::<residence::Residences>()
            .by_building
            .contains_key(&house));
        assert!(test.g.read::<BuildingInfos>().get(house).is_none());
    }

    #[test]
    fn p1b_carless_resident_walks_to_work() {
        let mut test = street();
        residential_demand(&test.g, 40.0);
        let west = test.build_house_near(vec2(175.0, 125.0));
        let east = test.build_house_near(vec2(275.0, 125.0));
        for _ in 0..2 {
            add_souls_to_empty_buildings(&mut test.g);
        }
        let (human, home) = test
            .g
            .world()
            .humans
            .iter()
            .find_map(|(id, human)| {
                human
                    .router
                    .personal_car
                    .is_none()
                    .then_some((id, human.home.house))
            })
            .expect("a deterministic carless resident");
        let workplace = if home == west { east } else { west };
        let human_ent = &mut test.g.world_mut_unchecked().humans[human];
        human_ent.work = Some(Work::new(workplace, WorkKind::Worker, 0.0));
        human_ent.decision.kind =
            crate::souls::human::HumanDecisionKind::GoTo(Destination::Building(workplace));
        for _ in 0..10_000 {
            test.tick_fast();
            if test.g.get(human).unwrap().location
                == crate::transportation::Location::Building(workplace)
            {
                return;
            }
        }
        panic!("carless resident did not reach work");
    }

    #[test]
    fn p1b_parking_retry_keeps_car_when_a_spot_is_available_farther_away() {
        let mut test = street();
        let house = test.build_house_near(vec2(175.0, 125.0));
        let human = crate::souls::human::spawn_human(&mut test.g, house).unwrap();
        let car = test.g.get(human).unwrap().router.personal_car.unwrap();
        let destination = test.g.map().get(house).unwrap().door_pos;
        let spots = fill_parking(&test.g, destination, 100.0);
        assert!(!spots.is_empty());
        test.g.world_mut_unchecked().humans[human]
            .router
            .target_dest = Some(Destination::Outside(destination));
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        crate::utils::scheduler::drain_commands(&mut test.g);
        let router = &test.g.get(human).unwrap().router;
        assert!(router.last_error.is_none());
        assert_eq!(router.personal_car, Some(car));
        assert!(test.g.get(car).is_some());
        for spot in spots {
            test.g.write::<ParkingManagement>().free(spot);
        }
    }

    fn fill_parking(
        sim: &Simulation,
        destination: geom::Vec3,
        radius: f32,
    ) -> Vec<SpotReservation> {
        let map = sim.map();
        let mut parking = sim.write::<ParkingManagement>();
        let mut spots = vec![];
        while let Ok(spot) = parking.reserve_near_radius(destination, &map, radius) {
            spots.push(spot);
        }
        spots
    }

    #[test]
    fn p1b_mid_trip_parking_failure_walks() {
        let mut test = street();
        residential_demand(&test.g, 40.0);
        let house = test.build_house_near(vec2(175.0, 125.0));
        let workplace = test.build_house_near(vec2(275.0, 125.0));
        add_souls_to_empty_buildings(&mut test.g);
        let (human, body) = test
            .g
            .world()
            .humans
            .iter()
            .find(|(_, h)| h.home.house == house)
            .unwrap();
        let car = body
            .router
            .personal_car
            .expect("fixture resident has a car");
        crate::transportation::unpark(&mut test.g, car);
        let car_pos = test.g.get(car).unwrap().trans.pos;
        test.g
            .write::<BuildingInfos>()
            .get_out(house, SoulID::Human(human));
        let body = &mut test.g.world_mut_unchecked().humans[human];
        body.location = crate::transportation::Location::Vehicle(car);
        body.router.target_dest = Some(Destination::Building(workplace));
        body.decision.kind =
            crate::souls::human::HumanDecisionKind::GoTo(Destination::Building(workplace));
        let destination = test.g.map().get(workplace).unwrap().door_pos;
        let spots = fill_parking(&test.g, destination, 400.0);
        let (world, resources) = test.g.world_res();
        crate::map_dynamic::routing_changed_system(world, resources);
        crate::utils::scheduler::drain_commands(&mut test.g);
        let body = test.g.get(human).unwrap();
        assert!(body.router.last_error.is_none());
        assert!(body.router.personal_car.is_none());
        assert_eq!(body.location, crate::transportation::Location::Outside);
        assert_eq!(body.trans.pos, car_pos);
        assert!(body.collider.is_some());
        assert!(test.g.get(car).is_none());
        for spot in spots {
            test.g.write::<ParkingManagement>().free(spot);
        }
        for _ in 0..10_000 {
            test.tick_fast();
            if test.g.get(human).unwrap().location
                == crate::transportation::Location::Building(workplace)
            {
                return;
            }
        }
        panic!("resident did not reach the destination after leaving the car");
    }

    fn front_lot(sim: &Simulation) -> LotID {
        sim.map()
            .lots()
            .values()
            .min_by_key(|lot| OrderedFloat(lot.shape.center().distance2(vec2(170.0, 125.0))))
            .unwrap()
            .id
    }

    fn grow_house(sim: &mut Simulation, lot: LotID) -> BuildingID {
        residential_demand(sim, 20.0);
        grow_zoned_lots(
            sim,
            vec![(
                lot,
                ZoneCell {
                    kind: ZoneKind::Residential,
                    density: Density::Low,
                },
            )],
        );
        sim.read::<residence::Residences>()
            .by_building
            .keys()
            .copied()
            .next()
            .unwrap()
    }

    fn restored_lot(map: &Map, origin: OBB) -> RoadID {
        map.lots()
            .values()
            .find(|lot| {
                lot.shape
                    .corners
                    .iter()
                    .zip(origin.corners)
                    .all(|(actual, expected)| actual.is_close(expected, 0.001))
            })
            .expect("lot restored at the original footprint")
            .parent
    }

    #[test]
    fn p1b_zoned_growth_obeys_demand_quota_and_clusters() {
        let mut test = street();
        let anchor = test.build_house_near(vec2(175.0, 125.0));
        let (nearest, total, zones) = {
            let map = test.g.map();
            let anchor = map.get(anchor).unwrap().obb.center();
            let mut zones = map
                .lots()
                .keys()
                .map(|id| {
                    (
                        id,
                        ZoneCell {
                            kind: ZoneKind::Residential,
                            density: Density::High,
                        },
                    )
                })
                .collect::<Vec<_>>();
            zones.sort_unstable_by_key(|&(id, _)| {
                (
                    OrderedFloat(map.get(id).unwrap().shape.center().distance2(anchor)),
                    id,
                )
            });
            (map.get(zones[0].0).unwrap().shape, map.lots().len(), zones)
        };
        let money = test.g.read::<crate::economy::Government>().money;
        residential_demand(&test.g, 5.0);
        grow_zoned_lots(&mut test.g, zones.clone());
        assert_eq!(test.g.map().lots().len(), total);
        residential_demand(&test.g, 21.0);
        grow_zoned_lots(&mut test.g, zones);
        let residences = test.g.read::<residence::Residences>();
        assert_eq!(residences.by_building.len(), 3);
        let map = test.g.map();
        assert_eq!(map.lots().len(), total - 2);
        assert!(map
            .buildings()
            .iter()
            .any(|(id, building)| { id != anchor && building.obb.center() == nearest.center() }));
        for (&id, home) in &residences.by_building {
            if id != anchor {
                assert_eq!(
                    (home.density, home.level, home.capacity),
                    (Density::High, 1, 4)
                );
                assert!(map.lot_origin(id).is_some());
                assert!(test.g.read::<BuildingInfos>().get(id).is_some());
            }
        }
        assert_eq!(test.g.read::<crate::economy::Government>().money, money);
    }

    #[test]
    fn p1b_collapse_restores_lot_after_exactly_twelve_abandoned_hours() {
        let mut test = street();
        let lot = front_lot(&test.g);
        let origin = test.g.map().get(lot).unwrap().shape;
        let house = grow_house(&mut test.g, lot);
        residential_demand(&test.g, 0.0);
        test.g.write::<GameTime>().tick.0 = 0;
        serve_house(&mut test.g, house);
        for _ in 0..23 {
            hourly_check(&mut test.g);
        }
        assert!(!test.g.read::<residence::Residences>().is_abandoned(house));
        hourly_check(&mut test.g);
        assert!(test.g.read::<residence::Residences>().is_abandoned(house));
        residential_demand(&test.g, 40.0);
        add_souls_to_empty_buildings(&mut test.g);
        assert!(test.g.world().humans.is_empty());
        for _ in 0..11 {
            hourly_check(&mut test.g);
        }
        assert!(test.g.map().get(house).is_some());
        hourly_check(&mut test.g);
        assert!(test.g.map().get(house).is_none());
        assert!(test.g.map().lot_origin(house).is_none());
        assert!(!test
            .g
            .read::<residence::Residences>()
            .by_building
            .contains_key(&house));
        restored_lot(&test.g.map(), origin);
    }

    #[test]
    fn p1b_player_removal_restores_saved_lot_after_road_split_or_merge() {
        for merge in [false, true] {
            let mut test = street();
            let lot = front_lot(&test.g);
            let origin = test.g.map().get(lot).unwrap().clone();
            let house = grow_house(&mut test.g, lot);
            let encoded = Bincode::encode(&test.g).unwrap();
            test.g = Bincode::decode(&encoded).unwrap();
            let road = {
                let mut map = test.g.map_mut();
                let (_, spur) = map
                    .make_connection(
                        MapProject {
                            pos: vec3(285.0, 100.0, 0.0),
                            kind: crate::map::ProjectKind::Road(origin.parent),
                        },
                        MapProject::ground(vec3(285.0, 40.0, 0.0)),
                        None,
                        &LanePatternBuilder::default().sidewalks(false).build(),
                    )
                    .unwrap();
                let junction = map.remove_road(spur).unwrap().src;
                if merge {
                    let roads = &map.get(junction).unwrap().roads;
                    let (a, b) = (roads[0], roads[1]);
                    map.merge_road(a, b).unwrap()
                } else {
                    map.get(house).unwrap().connected_road.unwrap()
                }
            };
            assert_ne!(road, origin.parent);
            test.apply(&[WorldCommand::MapRemoveBuilding(house)]);
            assert_eq!(restored_lot(&test.g.map(), origin.shape), road);
        }
    }

    #[test]
    fn p1b_orphaned_company_restores_lot_after_frontage_rebuilt() {
        let mut test = street();
        let lot = front_lot(&test.g);
        let origin = test.g.map().get(lot).unwrap().clone();
        let store = prototypes::GoodsCompanyID::new("commercial-l1");
        let kind = BuildingKind::GoodsCompany(store);
        let building = test.g.map_mut().place_growable(lot, kind).unwrap();
        on_growable_placed(&test.g, building, kind);
        add_souls_to_empty_buildings(&mut test.g);
        let company = test.g.read::<BuildingInfos>().owner(building).unwrap();
        test.apply(&[WorldCommand::MapRemoveRoad(origin.parent)]);
        let replacement = test
            .g
            .map_mut()
            .make_connection(
                MapProject::ground(vec3(100.0, 100.0, 0.0)),
                MapProject::ground(vec3(400.0, 100.0, 0.0)),
                None,
                &LanePatternBuilder::default().sidewalks(false).build(),
            )
            .unwrap()
            .1;
        assert_ne!(replacement, origin.parent);
        assert!(test.g.read::<ZoneGrid>().cells.is_empty());
        assert!(due(GROWTH_INTERVAL));
        test.g.write::<GameTime>().tick.0 = GROWTH_INTERVAL;
        growth_update(&mut test.g);
        crate::utils::scheduler::drain_commands(&mut test.g);
        assert!(test.g.map().get(building).is_none());
        assert_eq!(restored_lot(&test.g.map(), origin.shape), replacement);
        assert!(test.g.world().companies.is_empty());
        assert!(test
            .g
            .read::<BuildingInfos>()
            .building_owned_by(company)
            .is_none());
        assert!(
            !test.g.world_res().1.contains::<GrowthCache>(),
            "empty zones should skip the lot cache even when orphan cleanup runs"
        );
    }
}
