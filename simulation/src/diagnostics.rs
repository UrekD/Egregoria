//! Read-only explanations of building and citizen state.

use prototypes::{try_prototype, Power, Tick};

use crate::economy::Market;
use crate::map::{BuildingID, BuildingKind};
use crate::map_dynamic::{
    transit, BuildingInfos, BuildingUtilityStatus, Destination, ElectricityFlow, RouterError,
    UtilityFlow, UtilityStatus,
};
use crate::souls::events::Fires;
use crate::souls::goods_company::recipe_blockers;
use crate::souls::happiness::Happiness;
use crate::souls::human::HumanDecisionKind;
use crate::souls::residence::{self, Requirement, Residences};
use crate::transportation::Location;
use crate::{HumanID, Simulation, SoulID};

pub use crate::souls::goods_company::RecipeBlocker;
pub use crate::souls::residence::PowerStatus;

#[derive(Debug)]
pub struct BuildingDiagnostics {
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
}

pub fn building_diagnostics(sim: &Simulation, id: BuildingID) -> Option<BuildingDiagnostics> {
    let map = sim.map();
    let building = map.get(id)?;
    let residences = sim.read::<Residences>();
    let house = building.kind == BuildingKind::House;
    let utilities = sim.read::<UtilityFlow>().status(&map, &residences, id);
    let power = if building.is_orphaned() {
        PowerStatus::MissingNetwork
    } else {
        residence::power_status(&map, &sim.read::<ElectricityFlow>(), id)
    };
    let mut blockers = Vec::new();
    if building.is_orphaned() {
        blockers.push(BuildingBlocker::RoadDisconnected);
    }
    if sim.read::<Fires>().by_building.contains_key(&id) {
        blockers.push(BuildingBlocker::OnFire);
    }
    if residences.is_abandoned(id) {
        blockers.push(BuildingBlocker::Abandoned);
    }

    let owner = sim.read::<BuildingInfos>().owner(id);
    let mut needs_power = house;
    match building.kind {
        BuildingKind::GoodsCompany(prototype) => {
            needs_power = try_prototype(prototype)
                .is_some_and(|proto| proto.power_consumption > Some(Power::ZERO));
            let company = owner
                .and_then(|soul| match soul {
                    SoulID::GoodsCompany(company) => {
                        sim.get(company).map(|company| (soul, company))
                    }
                    _ => None,
                })
                .filter(|(_, company)| company.comp.building == id);
            if let Some((owner, company)) = company {
                if let Some(proto) = try_prototype(company.comp.proto) {
                    if proto.n_workers > 0 && company.workers.0.is_empty() {
                        blockers.push(BuildingBlocker::NoWorkers);
                    }
                    if let Some(recipe) = &proto.recipe {
                        let market = sim.read::<Market>();
                        blockers.extend(
                            recipe_blockers(recipe, owner, &market).map(BuildingBlocker::Recipe),
                        );
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
    if needs_power && power != PowerStatus::Available {
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
        let stores = residence::retail_store_positions(&map);
        let has_retail = residence::has_retail_access(&stores, building.door_pos.xy());
        if !has_retail {
            blockers.push(BuildingBlocker::NoRetailAccess);
        }
        (
            Some(has_retail),
            residence::requirements_with_stores(sim, id, Some(&stores)),
        )
    } else {
        (None, Vec::new())
    };
    Some(BuildingDiagnostics {
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
        HumanDecisionKind::DeliverAtBuilding(building) => Some(Destination::Building(*building)),
        _ => None,
    };
    let destination = intent_destination.or(citizen.router.target_dest.filter(|destination| {
        !matches!(destination, Destination::Building(id) if citizen.location == Location::Building(*id))
    }));
    let intention = match decision {
        HumanDecisionKind::SetVehicle(_) => CitizenIntention::ChangeVehicle,
        HumanDecisionKind::DeliverAtBuilding(_) => CitizenIntention::Deliver,
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
            citizen
                .goods
                .excluded_seller()
                .map(|_| CitizenBlocker::PickupTimedOut)
        });
    Some(CitizenDiagnostics {
        intention,
        destination,
        blocker,
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
    fn p4g_retail_access_uses_doors_and_is_read_only() {
        let (test, house) = city();
        let home_door = test.g.map().get(house).unwrap().door_pos.xy();
        let store = GoodsCompanyID::new("commercial-high-l1");
        let footprint = OBB::new(home_door + Vec2::x(1540.0), Vec2::Y, 50.0, 50.0);
        let place_store = |offset: f32| {
            test.g
                .map_mut()
                .build_special_building(
                    &footprint,
                    BuildingKind::GoodsCompany(store),
                    BuildingGen::NoWalkway {
                        door_pos: Vec2::x(offset),
                    },
                    None,
                    None,
                )
                .unwrap()
        };
        for (offset, met) in [(-41.0, true), (-39.0, false)] {
            let store = place_store(offset);
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
            test.g.map_mut().remove_building(store);
            assert!(building_diagnostics(&test.g, store).is_none());
        }
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
        assert_eq!(
            building_diagnostics(&test.g, house).unwrap().power,
            PowerStatus::MissingNetwork
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
}
