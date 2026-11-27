use geom::Vec3;
use goryak::{error, mincolumn, minrow, on_secondary_container, textc, tooltip};
use prototypes::{try_prototype, GoodsCompanyPrototype, Power, ServiceID, ServiceKind};
use simulation::economy::TripPurpose;
use simulation::map::{water_provider_site, Building, BuildingID, Map};
use simulation::map_dynamic::{
    active_business, connected_site_network, effective_load, ActiveBusiness, BuildingInfos,
    CoverageGrid, Destination, ElectricityFlow, Itinerary, RoutingStep, ServiceAllocation,
};
use simulation::souls::human::HumanDecisionKind;
use simulation::transportation::{Location, VehicleKind};
use simulation::{CompanyID, HumanEnt, HumanID, PopulationScale, Simulation};

use super::entity_link_named;
use crate::uiworld::UiWorld;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StaffActivity {
    Service(ServiceKind),
    Power,
}

impl StaffActivity {
    pub(super) fn for_company(company: &GoodsCompanyPrototype) -> Option<Self> {
        if let Some(service) = try_prototype(ServiceID::from(company.id)) {
            Some(Self::Service(service.service_kind))
        } else {
            (company.n_workers > 0 && company.power_production > Some(Power::ZERO))
                .then_some(Self::Power)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaffState {
    AtWork,
    Commuting,
    AtHome,
    Other,
}

impl StaffState {
    fn label(self) -> &'static str {
        match self {
            Self::AtWork => "At work",
            Self::Commuting => "Commuting",
            Self::AtHome => "At home",
            Self::Other => "Other",
        }
    }
}

struct StaffRow {
    id: HumanID,
    name: String,
    state: StaffState,
    location: String,
}

struct ServiceStaff {
    business: Option<ActiveBusiness>,
    slots: usize,
    rows: Vec<StaffRow>,
}

impl ServiceStaff {
    fn summary(&self, scale: PopulationScale, blocker: Option<&str>) -> String {
        match (blocker, self.slots.saturating_sub(self.rows.len())) {
            (Some(reason), _) => format!("Inactive · {reason}"),
            (None, 0) if self.slots == 0 => "Working · No staff required".into(),
            (None, 0) => "Working · Fully staffed".into(),
            (None, _) if self.rows.is_empty() => "Understaffed · No workers assigned".into(),
            (None, vacancies) => {
                let count = scale.persons(vacancies);
                let unit = if count == 1 { "position" } else { "positions" };
                format!("Understaffed · {count} open {unit}")
            }
        }
    }
}

fn service_staff(sim: &Simulation, building: &Building, slots: u32) -> ServiceStaff {
    let map = sim.map();
    let business = active_business(building, &sim.read::<BuildingInfos>(), sim.world());
    let slots = business.map_or(slots, |business| business.slots) as usize;
    let rows = business
        .and_then(|business| sim.get(business.company))
        .into_iter()
        .flat_map(|company| company.workers.0.iter().copied())
        .filter_map(|id| {
            let human = sim.get(id)?;
            let vehicle = match human.location {
                Location::Vehicle(id) | Location::Transit(id) => sim.get(id),
                _ => None,
            };
            let state = staff_state(
                human,
                building.id,
                business.map(|operator| operator.company),
                vehicle.map(|vehicle| &vehicle.it),
            );
            let location = match human.location {
                Location::Building(at) => map.get(at).map_or_else(
                    || "Building unavailable".to_owned(),
                    |current| {
                        let name = if at == human.home.house {
                            "Home"
                        } else {
                            super::inspect_building::building_title(current)
                        };
                        if at == building.id {
                            name.to_owned()
                        } else {
                            position_label(name, current.door_pos)
                        }
                    },
                ),
                Location::Vehicle(_) | Location::Transit(_) => {
                    let label =
                        vehicle.map_or("In a vehicle", |vehicle| match vehicle.vehicle.kind {
                            VehicleKind::Car => "In a car",
                            VehicleKind::Truck => "In a truck",
                            VehicleKind::Bus => "On a bus",
                        });
                    position_label(label, vehicle.map_or(human.trans.pos, |v| v.trans.pos))
                }
                Location::RailTransit(train) => position_label(
                    "On a passenger train",
                    sim.pos(train).unwrap_or(human.trans.pos),
                ),
                Location::TransitHeld => {
                    position_label("Waiting for safe rail egress", human.trans.pos)
                }
                Location::Outside => {
                    let label = if matches!(
                        human.router.cur_step(),
                        Some(RoutingStep::WaitAtStop { .. })
                    ) {
                        "Waiting for transit"
                    } else {
                        "Outdoors"
                    };
                    position_label(label, human.trans.pos)
                }
            };
            Some(StaffRow {
                id,
                name: human.personal_info.name.to_string(),
                state,
                location,
            })
        })
        .collect();
    ServiceStaff {
        business,
        slots,
        rows,
    }
}

fn staff_state(
    human: &HumanEnt,
    workplace: BuildingID,
    employer: Option<CompanyID>,
    vehicle_itinerary: Option<&Itinerary>,
) -> StaffState {
    let route_stalled = match human.location {
        Location::Outside => human.it.is_wait_for_reroute().is_some(),
        Location::Vehicle(_) | Location::Transit(_) => {
            vehicle_itinerary.is_none_or(|itinerary| itinerary.is_wait_for_reroute().is_some())
        }
        Location::Building(_) | Location::RailTransit(_) => false,
        Location::TransitHeld => true,
    };
    match human.location {
        Location::Building(at) if at == workplace => StaffState::AtWork,
        Location::Building(at) if at == human.home.house => StaffState::AtHome,
        _ if route_stalled => StaffState::Other,
        _ if human.router.trip_purpose == TripPurpose::Work
            && human.router.target_dest == Some(Destination::Building(workplace))
            && human.router.trip_start.is_some()
            && human.router.last_error.is_none() =>
        {
            StaffState::Commuting
        }
        _ if employer.is_some_and(|employer| {
            matches!(human.decision.kind, HumanDecisionKind::GoHome(_))
                && human
                    .router
                    .is_returning_home_from_work(employer, human.home.house)
        }) =>
        {
            StaffState::Commuting
        }
        _ => StaffState::Other,
    }
}

fn position_label(label: &str, pos: Vec3) -> String {
    format!("{label} ({:.0}, {:.0})", pos.x, pos.y)
}

pub(super) fn render(
    uiworld: &UiWorld,
    sim: &Simulation,
    building: &Building,
    company: &GoodsCompanyPrototype,
    activity: StaffActivity,
) {
    let map = sim.map();
    let staff = service_staff(sim, building, company.n_workers);
    let scale = sim.population_scale();
    let at_work = staff
        .rows
        .iter()
        .filter(|row| row.state == StaffState::AtWork)
        .count();
    let network = map.electricity.net_id(building.id);
    let road_connected = building
        .connected_road
        .is_some_and(|road| map.get(road).is_some());
    let generation = match activity {
        StaffActivity::Power => power_output(&map, building, staff.business),
        StaffActivity::Service(_) => None,
    };
    let blocker = if matches!(activity, StaffActivity::Service(_)) && !road_connected {
        Some("Disconnected from the road")
    } else if staff.business.is_none() {
        Some("No active operator")
    } else {
        match activity {
            StaffActivity::Power if generation.is_none() => Some("No electricity network"),
            StaffActivity::Service(ServiceKind::WaterPump | ServiceKind::SewageOutlet)
                if water_provider_site(&map, building.id).is_some_and(|site| site.is_err()) =>
            {
                Some("Invalid utility site")
            }
            StaffActivity::Service(kind)
                if kind != ServiceKind::SewageOutlet
                    && network.is_some_and(|net| sim.read::<ElectricityFlow>().blackout(net)) =>
            {
                Some("Power blackout")
            }
            _ => None,
        }
    };

    let label = |text: String| textc(on_secondary_container(), text);
    mincolumn(4.0, || {
        let title = match activity {
            StaffActivity::Service(_) => "Service staff",
            StaffActivity::Power => "Power plant staff",
        };
        textc(on_secondary_container(), title);
        tooltip("Assigned workers determine operating capacity", || {
            label(format!(
                "Staffed {} / {} · {} at work",
                scale.persons(staff.rows.len()),
                scale.persons(staff.slots),
                scale.persons(at_work),
            ));
        });
        let color = if blocker.is_some() || staff.rows.len() < staff.slots {
            error()
        } else {
            on_secondary_container()
        };
        textc(color, staff.summary(scale, blocker));

        let inactive = blocker.is_some() || (staff.slots > 0 && staff.rows.is_empty());
        match activity {
            StaffActivity::Power => {
                let output = generation.unwrap_or(Power::ZERO);
                label(format!("Electricity output · {output}"));
                if !road_connected {
                    let connection = if generation.is_some() {
                        "Road disconnected · Isolated electricity network"
                    } else {
                        "Disconnected from the road"
                    };
                    textc(on_secondary_container(), connection);
                }
            }
            StaffActivity::Service(kind @ (ServiceKind::WaterPump | ServiceKind::SewageOutlet)) => {
                let load = effective_load(building, None, staff.business);
                let (metric, output) = match kind {
                    ServiceKind::WaterPump => ("water output", load.water_production),
                    _ => ("sewage capacity", load.sewage_capacity),
                };
                let actual = if inactive { 0.0 } else { output };
                label(format!("Network-wide · {actual:.0} {metric}"));
            }
            StaffActivity::Service(_) if inactive => {
                textc(on_secondary_container(), "Coverage inactive");
            }
            StaffActivity::Service(kind) => {
                let coverage = sim.read::<CoverageGrid>();
                if let Some(allocation) = coverage.service_allocation(building.id, kind) {
                    tooltip(
                        "Demand includes residents and workers at each address. A person can count at home and at a separate workplace.",
                        || label(coverage_summary(allocation, scale)),
                    );
                } else {
                    textc(on_secondary_container(), "Coverage awaiting the next pass");
                }
            }
        }

        for row in staff.rows {
            minrow(8.0, || {
                entity_link_named(uiworld, sim, row.id.into(), row.name);
                label(format!("{} · {}", row.state.label(), row.location));
            });
        }
    });
}

fn power_output(map: &Map, building: &Building, business: Option<ActiveBusiness>) -> Option<Power> {
    let network = if building.development().is_some() {
        connected_site_network(map, building)
    } else {
        map.electricity.net_id(building.id)
    };
    network.map(|_| effective_load(building, None, business).power_production)
}

fn coverage_summary(allocation: ServiceAllocation, scale: PopulationScale) -> String {
    let demand = allocation.people * f32::from(scale.persons_per_agent());
    format!(
        "Last coverage pass · {demand:.1} demand units served · {:.0} m reach",
        allocation.distance_m,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec2, Transform, Vec2, OBB};
    use prototypes::{
        BuildingGen, GameTime, GoodsCompanyID, Tick, TICKS_PER_HOUR, TICKS_PER_MINUTE,
    };
    use scenarios::builders::{fill_houses, grid_city};
    use scenarios::ScenarioCtx;
    use simulation::map::{BuildingKind, PathKind, ProjectFilter, ProjectKind};
    use simulation::map_dynamic::{coverage_update, Router, RouterError};
    use simulation::souls::desire::{Work, WorkKind};
    use simulation::souls::goods_company::company_soul;
    use simulation::souls::human::spawn_human;
    use simulation::transportation::spawn_driving_vehicle;
    use simulation::world_command::WorldCommand;
    use simulation::{SimulationOptions, SoulID};
    use slotmapd::Key;

    #[test]
    fn service_staff_lists_live_workers_with_states_locations_and_served_coverage() {
        let mut fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut fixture, vec2(700.0, 700.0), 4, 4, 160.0);
        let ProjectKind::Road(road) = fixture
            .sim
            .map()
            .project(geom::vec3(780.0, 700.0, 0.0), 0.0, ProjectFilter::ROAD)
            .kind
        else {
            panic!("expected a road next to both sites");
        };
        let proto = GoodsCompanyID::new("fire-station");
        let station = fixture
            .sim
            .write::<simulation::map::Map>()
            .build_special_building(
                &OBB::new(vec2(760.0, 679.5), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto),
                BuildingGen::CenteredDoor {
                    vertical_factor: 1.0,
                },
                None,
                Some(road),
            )
            .unwrap();
        let shop_proto = GoodsCompanyID::new("commercial-l1");
        let shop = fixture
            .sim
            .write::<simulation::map::Map>()
            .build_special_building(
                &OBB::new(vec2(800.0, 679.5), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(shop_proto),
                shop_proto.prototype().bgen,
                None,
                Some(road),
            )
            .unwrap();
        let homes = fill_houses(&mut fixture, 4);
        let sim = &mut fixture.sim;
        // Start from the same unsampled electrical state as a newly installed network.
        *sim.write::<ElectricityFlow>() = ElectricityFlow::default();
        let Some(SoulID::GoodsCompany(company)) = company_soul(sim, station, proto) else {
            panic!("fire station must have a company");
        };

        let Some(SoulID::GoodsCompany(shop_company)) = company_soul(sim, shop, shop_proto) else {
            panic!("shop must have a company");
        };
        let workers = std::array::from_fn(|index| spawn_human(sim, homes[index]).unwrap());
        let [at_work, commuting, at_home, visiting] = workers;
        {
            let world = sim.world_mut_unchecked();
            world.companies[shop_company].workers.0 = vec![at_home];
            world.companies[company].workers.0 = [workers.as_slice(), &[HumanID::null()]].concat();
            world.humans[at_work].location = Location::Building(station);
            world.humans[commuting].location = Location::Outside;
            world.humans[commuting].router.target_dest = Some(Destination::Building(station));
            world.humans[commuting].router.trip_purpose = TripPurpose::Work;
            world.humans[commuting].router.trip_start = Some(Tick(1));
            world.humans[at_home].location = Location::Building(homes[2]);
            world.humans[visiting].location = Location::Building(homes[0]);
        }
        let staff = service_staff(
            sim,
            sim.map().get(station).unwrap(),
            proto.prototype().n_workers,
        );
        assert_eq!(staff.slots, 8);
        assert_eq!(
            staff.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            workers
        );
        assert_eq!(
            staff.rows.iter().map(|row| row.state).collect::<Vec<_>>(),
            [
                StaffState::AtWork,
                StaffState::Commuting,
                StaffState::AtHome,
                StaffState::Other
            ],
        );
        assert_eq!(staff.rows[0].location, "Fire Station");
        assert!(
            staff.rows[1].location.contains("Outdoors"),
            "{}",
            staff.rows[1].location
        );
        assert!(staff.rows[2].location.starts_with("Home ("));
        assert!(staff.rows[3].location.starts_with("House ("));
        assert!(workers
            .into_iter()
            .zip(&staff.rows)
            .all(|(id, row)| { row.name == sim.get(id).unwrap().personal_info.name.to_string() }));
        assert_eq!(
            staff.summary(PopulationScale::REALISTIC, None),
            "Understaffed · 4 open positions"
        );

        assert!(sim
            .read::<CoverageGrid>()
            .service_allocation(station, ServiceKind::Fire)
            .is_none());
        sim.write::<GameTime>().tick = Tick(3 * TICKS_PER_MINUTE);
        coverage_update(sim);
        let allocation = sim
            .read::<CoverageGrid>()
            .service_allocation(station, ServiceKind::Fire)
            .unwrap();
        // Four residents plus the same citizen's separate workplace demand.
        assert_eq!(allocation.people, 5.0);
        assert!(sim.read::<CoverageGrid>().coverage(shop, ServiceKind::Fire) > 0.0);
        assert_eq!(
            coverage_summary(allocation, PopulationScale::REALISTIC),
            format!(
                "Last coverage pass · 5.0 demand units served · {:.0} m reach",
                allocation.distance_m
            ),
        );
        assert!(allocation.distance_m > 0.0);
    }

    #[test]
    fn service_staff_classifies_trips_by_purpose_and_route_status() {
        let mut fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut fixture, vec2(700.0, 700.0), 4, 4, 160.0);
        let homes = fill_houses(&mut fixture, 2);
        let worker = spawn_human(&mut fixture.sim, homes[0]).unwrap();
        let human = &mut fixture.sim.world_mut_unchecked().humans[worker];
        human.location = Location::Outside;
        human.router.trip_start = Some(Tick(1));
        human.router.target_dest = Some(Destination::Building(homes[1]));

        human.router.trip_purpose = TripPurpose::Leisure;
        human.decision.kind = HumanDecisionKind::VisitLeisure(homes[1]);
        let leisure = staff_state(human, homes[1], None, None);

        human.router.trip_purpose = TripPurpose::Work;
        let work = staff_state(human, homes[1], None, None);
        human.router.last_error = Some(RouterError::LocatingVehicle);
        let failed = staff_state(human, homes[1], None, None);

        human.router.last_error = None;
        human.router.trip_start = None;
        let pending = staff_state(human, homes[1], None, None);

        human.router.trip_start = Some(Tick(2));
        human.router.target_dest = Some(Destination::Building(homes[0]));
        human.router.trip_purpose = TripPurpose::Home;
        human.decision.kind = HumanDecisionKind::DepositAtHome;
        let groceries = staff_state(human, homes[1], None, None);

        assert_eq!(
            [leisure, work, failed, pending, groceries],
            [
                StaffState::Other,
                StaffState::Commuting,
                StaffState::Other,
                StaffState::Other,
                StaffState::Other
            ],
        );
    }

    #[test]
    fn service_staff_counts_homebound_commutes_only_from_work() {
        let mut fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let grid = grid_city(&mut fixture, vec2(700.0, 700.0), 4, 4, 160.0);
        let proto = GoodsCompanyID::new("park").prototype();
        let park = fixture
            .sim
            .write::<simulation::map::Map>()
            .build_special_building(
                &OBB::new(vec2(760.0, 679.5), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(proto.id),
                proto.bgen,
                None,
                Some(grid.roads[0]),
            )
            .unwrap();
        let [home, visited, other_home] = fill_houses(&mut fixture, 3).try_into().unwrap();
        let sim = &mut fixture.sim;
        let Some(SoulID::GoodsCompany(company)) = company_soul(sim, park, proto.id) else {
            panic!("park must have a company");
        };

        let workers =
            [(park, home), (home, home), (visited, other_home)].map(|(departure, home)| {
                let worker = spawn_human(sim, home).unwrap();
                let door = sim.map().get(departure).unwrap().door_pos;
                {
                    let mut infos = sim.write::<BuildingInfos>();
                    infos.get_out(home, worker.into());
                    infos.get_in(departure, worker.into());
                }
                let human = &mut sim.world_mut_unchecked().humans[worker];
                human.location = Location::Building(departure);
                human.trans.pos = door;
                human.router = Router::new(None);
                human.work = Some(Work::new(park, WorkKind::Worker, 0.0));
                if departure != park {
                    human.decision.wait = u8::MAX;
                }
                worker
            });
        sim.world_mut_unchecked().companies[company].workers.0 = workers.to_vec();
        let [on_shift, visitor, _] = workers;
        *sim.write::<GameTime>() = GameTime::new(Tick(2 * TICKS_PER_HOUR));
        fixture.run_ticks(1);

        let sim = &mut fixture.sim;
        *sim.write::<GameTime>() = GameTime::new(Tick(11 * TICKS_PER_HOUR));
        sim.world_mut_unchecked().edit_human(visitor, |human| {
            human.decision.wait = u8::MAX;
            human.router.go_to(Destination::Building(park));
            human.router.trip_purpose = TripPurpose::Leisure;
        });
        for _ in 0..20 {
            fixture
                .sim
                .world_mut_unchecked()
                .edit_human(visitor, |human| {
                    // Let the real route enter the park without running unrelated errands.
                    human.decision.wait = u8::MAX;
                    if let Some(RoutingStep::WalkTo(door)) = human.router.cur_step() {
                        human.trans.pos = *door;
                        human.it = Itinerary::NONE;
                    }
                });
            fixture.run_ticks(1);
            if fixture.sim.get(visitor).unwrap().location == Location::Building(park) {
                break;
            }
        }
        assert_eq!(
            fixture.sim.get(visitor).unwrap().location,
            Location::Building(park)
        );
        assert_eq!(
            fixture.sim.get(on_shift).unwrap().location,
            Location::Building(park)
        );
        for worker in workers {
            fixture
                .sim
                .world_mut_unchecked()
                .edit_human(worker, |human| {
                    human.decision.wait = 0;
                    human.decision.kind = HumanDecisionKind::GoHome(human.home.house);
                });
        }
        fixture.run_ticks(4);

        let sim = &fixture.sim;
        assert!(workers.iter().all(|&worker| {
            let human = sim.get(worker).unwrap();
            human.location == Location::Outside
                && human.router.trip_purpose == TripPurpose::Home
                && human.router.last_error.is_none()
        }));
        let states = {
            let staff = service_staff(sim, sim.map().get(park).unwrap(), proto.n_workers);
            workers.map(|worker| {
                staff
                    .rows
                    .iter()
                    .find(|row| row.id == worker)
                    .unwrap()
                    .state
            })
        };
        let sim = &mut fixture.sim;
        let destination = sim.map().get(home).unwrap().door_pos;
        let car =
            spawn_driving_vehicle(sim, VehicleKind::Car, Transform::new(destination)).unwrap();
        let world = sim.world_mut_unchecked();
        world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Vehicle, destination);
        let human = &mut world.humans[on_shift];
        human.it = Itinerary::wait_for_reroute(PathKind::Pedestrian, destination);
        let walking = staff_state(human, park, Some(company), None);
        human.it = Itinerary::NONE;
        human.location = Location::Vehicle(car);
        let driving = staff_state(human, park, Some(company), Some(&world.vehicles[car].it));
        assert_eq!(
            (states, [walking, driving]),
            (
                [StaffState::Commuting, StaffState::Other, StaffState::Other],
                [StaffState::Other; 2]
            ),
        );
    }

    #[test]
    fn service_staff_excludes_stalled_pedestrian_and_vehicle_commutes() {
        let mut fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        grid_city(&mut fixture, vec2(700.0, 700.0), 4, 4, 160.0);
        let home = fill_houses(&mut fixture, 1)[0];
        let grid = grid_city(&mut fixture, vec2(1400.0, 700.0), 2, 1, 160.0);
        let park = GoodsCompanyID::new("park").prototype();
        let workplace = fixture
            .sim
            .write::<Map>()
            .build_special_building(
                &OBB::new(vec2(1460.0, 679.5), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(park.id),
                park.bgen,
                None,
                Some(grid.roads[0]),
            )
            .unwrap();
        let worker = spawn_human(&mut fixture.sim, home).unwrap();
        fixture
            .sim
            .world_mut_unchecked()
            .edit_human(worker, |human| {
                human.router = Router::new(None);
                human.router.go_to(Destination::Building(workplace));
                human.router.trip_purpose = TripPurpose::Work;
                human.decision.wait = u8::MAX;
            });
        fixture.run_ticks(8);
        let sim = &mut fixture.sim;
        let destination = sim.map().get(workplace).unwrap().door_pos;
        let car =
            spawn_driving_vehicle(sim, VehicleKind::Car, Transform::new(destination)).unwrap();
        let world = sim.world_mut_unchecked();
        world.vehicles[car].it = Itinerary::wait_for_reroute(PathKind::Vehicle, destination);
        let human = &mut world.humans[worker];
        assert_eq!(human.location, Location::Outside);
        assert!(human.router.last_error.is_none());
        assert!(human
            .it
            .is_wait_for_reroute()
            .is_some_and(|ticks| ticks > 0));
        let pedestrian = staff_state(human, workplace, None, None);
        human.it = Itinerary::NONE;
        human.location = Location::Vehicle(car);
        let driver = staff_state(human, workplace, None, Some(&world.vehicles[car].it));
        assert_eq!([pedestrian, driver], [StaffState::Other; 2]);
    }

    #[test]
    fn service_staff_power_output_matches_the_isolated_network() {
        let mut fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let grid = grid_city(&mut fixture, vec2(700.0, 700.0), 4, 4, 160.0);
        let proto = GoodsCompanyID::new("coal-power-plant").prototype();
        let road = grid.roads[0];
        let plant = fixture
            .sim
            .write::<Map>()
            .build_special_building(
                &OBB::new(vec2(780.0, 607.0), Vec2::X, proto.size.w, proto.size.h),
                BuildingKind::GoodsCompany(proto.id),
                proto.bgen,
                None,
                Some(road),
            )
            .unwrap();
        let home = fill_houses(&mut fixture, 1)[0];
        let sim = &mut fixture.sim;
        let Some(SoulID::GoodsCompany(company)) = company_soul(sim, plant, proto.id) else {
            panic!("coal plant must have a company");
        };
        let worker = spawn_human(sim, home).unwrap();
        sim.world_mut_unchecked().companies[company].workers.0 = vec![worker];
        fixture.submit(&[WorldCommand::MapRemoveRoad(road)]);
        fixture.run_ticks(1);

        let sim = &fixture.sim;
        let map = sim.map();
        let building = map.get(plant).unwrap();
        assert!(building.connected_road.is_none());
        let network = map
            .electricity
            .net_id(plant)
            .expect("isolated plant network");
        let production = sim
            .read::<ElectricityFlow>()
            .network_stats(network)
            .produced_power;
        assert!(production > Power::ZERO);
        let staff = service_staff(sim, building, proto.n_workers);
        assert_eq!(
            power_output(&map, building, staff.business),
            Some(production)
        );
    }

    #[test]
    fn service_staff_panel_includes_staffed_electricity_producers() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let coal = GoodsCompanyID::new("coal-power-plant").prototype();
        let solar = GoodsCompanyID::new("solar-panel").prototype();
        assert_eq!(StaffActivity::for_company(coal), Some(StaffActivity::Power));
        assert_eq!(StaffActivity::for_company(solar), None);
    }

    #[test]
    fn service_staff_summary_explains_empty_rosters_and_operating_blockers() {
        let staff = ServiceStaff {
            business: None,
            slots: 2,
            rows: Vec::new(),
        };
        let scale = PopulationScale::REALISTIC;
        assert_eq!(
            staff.summary(scale, None),
            "Understaffed · No workers assigned"
        );
        assert_eq!(
            staff.summary(scale, Some("Power blackout")),
            "Inactive · Power blackout",
        );
        let staff = ServiceStaff { slots: 0, ..staff };
        assert_eq!(staff.summary(scale, None), "Working · No staff required");
    }
}
