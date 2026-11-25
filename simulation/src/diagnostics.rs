//! Read-only explanations of building and citizen state.

use prototypes::{Power, Tick};

use crate::economy::cargo::CargoBook;
use crate::economy::Market;
use crate::map::{BuildingID, BuildingKind};
use crate::map_dynamic::{
    active_business, effective_load, live_site_utility_status, site_power_status, transit,
    ActiveRetailIndex, BuildingInfos, BuildingUtilityStatus, Destination, ElectricityFlow,
    RouterError, SiteLoad, UtilityFlow, UtilityStatus,
};
use crate::souls::events::Fires;
use crate::souls::goods_company::recipe_blockers;
use crate::souls::happiness::Happiness;
use crate::souls::human::HumanDecisionKind;
use crate::souls::residence::{self, Requirement, Residences};
use crate::transportation::Location;
use crate::{HumanID, Simulation, SoulID};

pub use crate::economy::cargo::ImportStatus;
pub use crate::souls::goods_company::RecipeBlocker;
pub use crate::souls::residence::PowerStatus;

#[derive(Debug)]
pub struct BuildingDiagnostics {
    pub load: SiteLoad,
    pub power: PowerStatus,
    pub utilities: Option<BuildingUtilityStatus>,
    pub retail_access: Option<bool>,
    pub happiness: Option<u8>,
    pub upgrade_requirements: Vec<Requirement>,
    pub blockers: Vec<BuildingBlocker>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildingBlocker {
    RoadDisconnected,
    MissingOwner,
    Power(PowerStatus),
    Water(UtilityStatus),
    Sewage(UtilityStatus),
    NoRetailAccess,
    Abandoned,
    OnFire,
    NoWorkers,
    Recipe(RecipeBlocker),
    /// A recipe input that no truck brings waits for an import, or has no import route.
    Import(ImportStatus),
}

pub fn building_diagnostics(sim: &Simulation, id: BuildingID) -> Option<BuildingDiagnostics> {
    let map = sim.map();
    let building = map.get(id)?;
    let residences = sim.read::<Residences>();
    let house = building.has_residential_use();
    let infos = sim.read::<BuildingInfos>();
    let business = active_business(building, &infos, sim.world());
    let load = effective_load(building, residences.by_building.get(&id), business);
    let power_flow = sim.read::<ElectricityFlow>();
    let utilities = live_site_utility_status(
        &map,
        &residences,
        &infos,
        sim.world(),
        &power_flow,
        &sim.read::<UtilityFlow>(),
        id,
    );
    let power = site_power_status(&map, &power_flow, building);
    let mut blockers = Vec::new();
    if building.is_orphaned() {
        blockers.push(BuildingBlocker::RoadDisconnected);
    }
    if sim.read::<Fires>().by_building.contains_key(&id) {
        blockers.push(BuildingBlocker::OnFire);
    }
    if building.is_abandoned_growable() || residences.is_abandoned(id) {
        blockers.push(BuildingBlocker::Abandoned);
    }

    let owner = infos.owner(id);
    let mut power_blocked = house && power != PowerStatus::Available;
    match building.kind {
        BuildingKind::GoodsCompany(_) => {
            if let Some(business) = business {
                let proto = business.prototype;
                power_blocked |= proto.power_consumption > Some(Power::ZERO)
                    && (power == PowerStatus::Blackout
                        || (building.development().is_some()
                            && power == PowerStatus::MissingNetwork));
                if business.slots > 0 && business.filled == 0 {
                    blockers.push(BuildingBlocker::NoWorkers);
                }
                if let Some(recipe) = &proto.recipe {
                    let owner = SoulID::GoodsCompany(business.company);
                    let market = sim.read::<Market>();
                    blockers.extend(
                        recipe_blockers(recipe, owner, &market).map(BuildingBlocker::Recipe),
                    );
                    if let Some(status) = sim.read::<CargoBook>().import_status(owner) {
                        blockers.push(BuildingBlocker::Import(status));
                    }
                }
            } else {
                blockers.push(BuildingBlocker::MissingOwner);
            }
        }
        BuildingKind::RailFreightStation(_) => {
            if !matches!(owner, Some(SoulID::FreightStation(owner))
                if sim.get(owner).is_some_and(|station| station.f.building == id))
            {
                blockers.push(BuildingBlocker::MissingOwner);
            }
        }
        _ => {}
    }
    if power_blocked {
        blockers.push(BuildingBlocker::Power(power));
    }
    if let Some(utilities) = utilities {
        if utilities.water != UtilityStatus::Served {
            blockers.push(BuildingBlocker::Water(utilities.water));
        }
        if utilities.sewage != UtilityStatus::Served {
            blockers.push(BuildingBlocker::Sewage(utilities.sewage));
        }
    }

    let (retail_access, upgrade_requirements) = if house {
        let index = ActiveRetailIndex::build(&map, &infos, sim.world());
        let nearby: Vec<_> = index.nearby(building.door_pos.xy()).collect();
        let has_retail = !nearby.is_empty();
        if !has_retail {
            blockers.push(BuildingBlocker::NoRetailAccess);
        }
        let mut requirements = residence::requirements_with_stores(sim, id, Some(&nearby));
        if building.development().is_some() {
            for requirement in &mut requirements {
                let met = match requirement.name {
                    "Power" => power == PowerStatus::Available,
                    "Water" => {
                        utilities.is_some_and(|status| status.water == UtilityStatus::Served)
                    }
                    "Sewage" => {
                        utilities.is_some_and(|status| status.sewage == UtilityStatus::Served)
                    }
                    _ => continue,
                };
                requirement.met = met;
                requirement.value = f32::from(met);
            }
            if building.is_mixed_use() {
                let staffed = business.is_some_and(|business| business.filled > 0);
                requirements.push(Requirement {
                    name: "Staffed storefront",
                    met: staffed,
                    value: f32::from(staffed),
                    threshold: 1.0,
                });
            }
        }
        (Some(has_retail), requirements)
    } else {
        (None, Vec::new())
    };
    Some(BuildingDiagnostics {
        load,
        power,
        utilities,
        retail_access,
        happiness: sim.read::<Happiness>().score(id),
        upgrade_requirements,
        blockers,
    })
}

/// Simulation steps elapsed in this city, independent of edits to the calendar.
pub fn elapsed_tick(sim: &Simulation) -> Tick {
    transit::elapsed_ticks(&sim.resources)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CitizenIntention {
    Idle,
    Travel,
    GoHome,
    GoToWork,
    PickUpGoods,
    ChangeVehicle,
    Deliver,
}

#[derive(Clone, Copy, Debug)]
pub enum CitizenBlocker {
    Route(RouterError),
    DestinationRemoved,
    DestinationDisconnected,
    PickupTimedOut,
}

#[derive(Debug)]
pub struct CitizenDiagnostics {
    pub intention: CitizenIntention,
    pub destination: Option<Destination>,
    pub blocker: Option<CitizenBlocker>,
    /// Remaining ticks before the active itinerary searches again.
    pub reroute_wait_ticks: Option<u16>,
}

pub fn citizen_diagnostics(sim: &Simulation, id: HumanID) -> Option<CitizenDiagnostics> {
    let citizen = sim.get(id)?;
    let mut decision = &citizen.decision.kind;
    while let HumanDecisionKind::MultiStack(steps) = decision {
        let Some(next) = steps.last() else { break };
        decision = next;
    }
    let intent_destination = match decision {
        HumanDecisionKind::GoTo(destination) => Some(*destination),
        HumanDecisionKind::DeliverAtBuilding(building)
        | HumanDecisionKind::DeliverLoad { destination: building, .. } => {
            Some(Destination::Building(*building))
        }
        HumanDecisionKind::PickupBasket(_)
        | HumanDecisionKind::DepositAtHome
        | HumanDecisionKind::VisitLeisure(_) => None,
        _ => None,
    };
    let destination = intent_destination.or(citizen.router.target_dest.filter(|destination| {
        !matches!(destination, Destination::Building(id) if citizen.location == Location::Building(*id))
    }));
    let intention = match decision {
        HumanDecisionKind::SetVehicle(_) => CitizenIntention::ChangeVehicle,
        HumanDecisionKind::DeliverAtBuilding(_) | HumanDecisionKind::DeliverLoad { .. } => {
            CitizenIntention::Deliver
        }
        _ if citizen.router.walk_only_pickup => CitizenIntention::PickUpGoods,
        _ => match destination {
            Some(Destination::Building(id)) if id == citizen.home.house => CitizenIntention::GoHome,
            Some(Destination::Building(id))
                if citizen
                    .work
                    .as_ref()
                    .is_some_and(|work| work.workplace == id) =>
            {
                CitizenIntention::GoToWork
            }
            Some(_) => CitizenIntention::Travel,
            None => CitizenIntention::Idle,
        },
    };
    let reroute_wait_ticks = match citizen.location {
        Location::Outside => citizen.it.is_wait_for_reroute(),
        Location::Vehicle(id) => sim
            .get(id)
            .and_then(|vehicle| vehicle.it.is_wait_for_reroute()),
        Location::Building(_) | Location::Transit(_) => None,
    };
    let blocker = citizen
        .router
        .last_error
        .map(CitizenBlocker::Route)
        .or_else(|| {
            if let Some(Destination::Building(id)) = destination {
                match sim.map().get(id) {
                    None => return Some(CitizenBlocker::DestinationRemoved),
                    Some(building) if building.is_orphaned() => {
                        return Some(CitizenBlocker::DestinationDisconnected)
                    }
                    Some(_) => {}
                }
            }
            if reroute_wait_ticks.is_some_and(|ticks| ticks > 0) {
                return Some(CitizenBlocker::DestinationDisconnected);
            }
            citizen
                .goods
                .excluded_seller()
                .map(|_| CitizenBlocker::PickupTimedOut)
        });
    Some(CitizenDiagnostics {
        intention,
        destination,
        blocker,
        reroute_wait_ticks,
    })
}

#[cfg(test)]
mod tests {
    use geom::{vec2, vec3, Vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID};

    use super::*;
    use crate::map_dynamic::electricity_flow_system;
    use crate::souls::goods_company::{company_soul, recipe_should_produce};
    use crate::souls::human::spawn_human;
    use crate::tests::TestCtx;

    fn city() -> (TestCtx, BuildingID) {
        let test = TestCtx::new_fast();
        test.build_roads(&[vec3(100.0, 150.0, 0.0), vec3(400.0, 150.0, 0.0)]);
        let house = test.build_house_near(vec2(160.0, 175.0));
        (test, house)
    }

    #[test]
    fn z3_accounting_mixed_diagnostics_follow_both_live_roles() {
        use crate::map::GrowableBusinessState;
        use crate::map_dynamic::accounting_site;

        let (mut test, site, company) = accounting_site(true);
        let vacant = building_diagnostics(&test.g, site).unwrap();
        assert_eq!(vacant.retail_access, Some(false));
        assert!(vacant.blockers.contains(&BuildingBlocker::NoWorkers));
        assert!(vacant.blockers.contains(&BuildingBlocker::NoRetailAccess));

        let worker = spawn_human(&mut test.g, site).unwrap();
        test.g.world.companies[company].workers.0.push(worker);
        assert_eq!(
            building_diagnostics(&test.g, site).unwrap().retail_access,
            Some(true)
        );

        test.g.map_mut().set_growable_business_state(
            site,
            GrowableBusinessState::ClosedUntil {
                retry_elapsed_tick: u64::MAX,
            },
        );
        let closed = building_diagnostics(&test.g, site).unwrap();
        assert_eq!(closed.retail_access, Some(false));
        assert!(closed.blockers.contains(&BuildingBlocker::MissingOwner));
        assert!(!closed.blockers.contains(&BuildingBlocker::NoWorkers));
        assert!(closed.utilities.is_some());
    }

    #[test]
    fn p4g_retail_access_uses_doors_and_is_read_only() {
        let (mut test, house) = city();
        let home_door = test.g.map().get(house).unwrap().door_pos.xy();
        let store = GoodsCompanyID::new("commercial-high-l1");
        let footprint = OBB::new(home_door + Vec2::x(1540.0), Vec2::Y, 50.0, 50.0);
        let store_endpoints = [
            (footprint.center() - Vec2::x(100.0)).z(0.0),
            (footprint.center() + Vec2::x(100.0)).z(0.0),
        ];
        let old_roads = test.g.map().roads().keys().collect::<Vec<_>>();
        test.build_roads(&store_endpoints);
        let road = test
            .g
            .map()
            .roads()
            .keys()
            .find(|id| !old_roads.contains(id))
            .unwrap();
        let worker = spawn_human(&mut test.g, house).unwrap();
        for (offset, met) in [(-41.0, true), (-39.0, false)] {
            let building = test
                .g
                .map_mut()
                .build_special_building(
                    &footprint,
                    BuildingKind::GoodsCompany(store),
                    BuildingGen::NoWalkway {
                        door_pos: Vec2::x(offset),
                    },
                    None,
                    Some(road),
                )
                .unwrap();
            test.g.write::<BuildingInfos>().insert(building);
            let Some(SoulID::GoodsCompany(company)) = company_soul(&mut test.g, building, store)
            else {
                panic!("expected a retail company");
            };
            test.g.world.companies[company].workers.0.push(worker);
            let before = test.g.hashes();
            let diagnostics = building_diagnostics(&test.g, house).unwrap();
            assert_eq!(diagnostics.retail_access, Some(met));
            assert_eq!(
                diagnostics
                    .upgrade_requirements
                    .iter()
                    .find(|requirement| requirement.name == "Retail access")
                    .unwrap()
                    .met,
                met
            );
            assert_eq!(test.g.hashes(), before);
            test.g.map_mut().remove_building(building);
            assert!(building_diagnostics(&test.g, building).is_none());
        }
    }

    #[test]
    fn z3_fix_road_cut_hides_staffed_retail_before_cleanup() {
        let (mut test, site, shop) = crate::map_dynamic::accounting_site(true);
        let worker = spawn_human(&mut test.g, site).unwrap();
        test.g.world.companies[shop].workers.0.push(worker);
        assert_eq!(
            building_diagnostics(&test.g, site).unwrap().retail_access,
            Some(true)
        );
        let road = test.g.map().get(site).unwrap().connected_road.unwrap();
        test.g.map_mut().remove_road(road).unwrap();
        assert!(test.g.map().get(site).unwrap().is_orphaned());
        assert_eq!(test.g.world.companies[shop].workers.0.as_slice(), &[worker]);
        assert_eq!(
            building_diagnostics(&test.g, site).unwrap().retail_access,
            Some(false)
        );
    }

    #[test]
    fn p4g_diagnostics_distinguish_missing_network_and_blackout() {
        let (mut test, house) = city();
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        assert_eq!(
            building_diagnostics(&test.g, house).unwrap().power,
            PowerStatus::Blackout
        );

        let road = test.g.map().get(house).unwrap().connected_road.unwrap();
        test.apply(&[crate::world_command::WorldCommand::MapRemoveRoad(road)]);
        let (world, resources) = test.g.world_res();
        electricity_flow_system(world, resources);
        let shared = |sim: &Simulation| {
            residence::power_status(&sim.map(), &sim.read::<ElectricityFlow>(), house)
        };
        assert_eq!(shared(&test.g), PowerStatus::Blackout);
        let diagnostics = building_diagnostics(&test.g, house).unwrap();
        assert_eq!(diagnostics.power, shared(&test.g));
        assert!(diagnostics
            .blockers
            .contains(&BuildingBlocker::RoadDisconnected));
        assert!(diagnostics
            .blockers
            .contains(&BuildingBlocker::Power(PowerStatus::Blackout)));

        test.g.map_mut().electricity.remove_object(house);
        assert_eq!(shared(&test.g), PowerStatus::MissingNetwork);
        assert_eq!(
            building_diagnostics(&test.g, house).unwrap().power,
            shared(&test.g)
        );
    }

    #[test]
    fn p4g_recipe_blockers_match_production_at_stock_thresholds() {
        let (mut test, house) = city();
        let road = test.g.map().get(house).unwrap().connected_road;
        let proto = GoodsCompanyID::new("commercial-high-l1").prototype();
        let store = test
            .g
            .map_mut()
            .build_special_building(
                &OBB::new(vec2(250.0, 100.0), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto.id),
                proto.bgen,
                None,
                road,
            )
            .unwrap();
        test.g.write::<BuildingInfos>().insert(store);
        assert!(building_diagnostics(&test.g, store)
            .unwrap()
            .blockers
            .contains(&BuildingBlocker::MissingOwner));

        let owner = company_soul(&mut test.g, store, proto.id).unwrap();
        let recipe = proto.recipe.as_ref().unwrap();
        let input = recipe.consumption.first().unwrap();
        let output = recipe.production.first().unwrap();
        let storage_limit = output.amount * (recipe.storage_multiplier + 1);
        {
            let mut market = test.g.write::<Market>();
            for item in &recipe.consumption {
                market.produce(owner, item.id, item.amount);
            }
            market.produce(owner, input.id, -1);
            assert!(!recipe_should_produce(recipe, owner, &market));
        }
        let blockers = building_diagnostics(&test.g, store).unwrap().blockers;
        assert!(
            blockers.contains(&BuildingBlocker::Recipe(RecipeBlocker::MissingInput {
                item: input.id,
                stock: input.amount - 1,
                required: input.amount,
            }))
        );

        {
            let mut market = test.g.write::<Market>();
            market.produce(owner, input.id, 1);
            market.produce(owner, output.id, storage_limit - 1);
            assert!(recipe_should_produce(recipe, owner, &market));
            market.produce(owner, output.id, 1);
            assert!(!recipe_should_produce(recipe, owner, &market));
        }
        let blockers = building_diagnostics(&test.g, store).unwrap().blockers;
        assert!(
            blockers.contains(&BuildingBlocker::Recipe(RecipeBlocker::OutputFull {
                item: output.id,
                stock: storage_limit,
                limit: storage_limit,
            }))
        );
    }

    #[test]
    fn p4g_citizen_why_reports_the_decision_destination_and_router_error() {
        let (mut test, house) = city();
        let citizen = spawn_human(&mut test.g, house).unwrap();
        let destination = Destination::Outside(vec3(300.0, 150.0, 0.0));
        let human = test.g.world.humans.get_mut(citizen).unwrap();
        human.decision.kind = HumanDecisionKind::GoTo(destination);
        human.router.last_error = Some(RouterError::LocatingVehicle);
        let before = test.g.hashes();

        let diagnostics = citizen_diagnostics(&test.g, citizen).unwrap();
        assert_eq!(diagnostics.intention, CitizenIntention::Travel);
        assert_eq!(diagnostics.destination, Some(destination));
        assert!(matches!(
            diagnostics.blocker,
            Some(CitizenBlocker::Route(RouterError::LocatingVehicle))
        ));
        assert_eq!(test.g.hashes(), before);
    }

    #[test]
    fn p4g_citizen_why_reports_a_failed_search_between_disconnected_roads() {
        let (mut test, house) = city();
        let a = vec3(175.0, 435.0, 0.0);
        let b = vec3(275.0, 435.0, 0.0);
        test.build_roads(&[a, b]);
        let citizen = spawn_human(&mut test.g, house).unwrap();
        let destination = Destination::Outside((a + b) * 0.5);
        let human = test.g.world.humans.get_mut(citizen).unwrap();
        human.router.use_vehicle(None);
        human.router.go_to(destination);
        human.decision.kind = HumanDecisionKind::GoTo(destination);

        for _ in 0..200 {
            test.tick_fast();
            let human = test.g.get(citizen).unwrap();
            if let Some(ticks) = human.it.is_wait_for_reroute().filter(|&ticks| ticks > 0) {
                assert!(human.router.last_error.is_none());
                let before = test.g.hashes();
                let diagnostics = citizen_diagnostics(&test.g, citizen).unwrap();
                assert_eq!(diagnostics.destination, Some(destination));
                assert!(matches!(
                    diagnostics.blocker,
                    Some(CitizenBlocker::DestinationDisconnected)
                ));
                assert_eq!(diagnostics.reroute_wait_ticks, Some(ticks));
                assert_eq!(test.g.hashes(), before);
                return;
            }
        }
        panic!("citizen did not enter a reroute wait after searching");
    }
}
