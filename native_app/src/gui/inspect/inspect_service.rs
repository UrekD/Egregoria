use geom::Vec3;
use goryak::{error, mincolumn, minrow, on_secondary_container, textc, tooltip};
use prototypes::{ServiceKind, ServicePrototype};
use simulation::economy::TripPurpose;
use simulation::map::{water_provider_site, Building};
use simulation::map_dynamic::{
    active_business, effective_load, ActiveBusiness, BuildingInfos, CoverageGrid, Destination,
    ElectricityFlow, RoutingStep,
};
use simulation::transportation::{Location, VehicleKind};
use simulation::{HumanID, PopulationScale, Simulation};

use super::entity_link_named;
use crate::uiworld::UiWorld;

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

fn service_staff(
    sim: &Simulation,
    building: &Building,
    service: &ServicePrototype,
) -> ServiceStaff {
    let map = sim.map();
    let business = active_business(building, &sim.read::<BuildingInfos>(), sim.world());
    let slots = business.map_or(service.n_workers, |business| business.slots) as usize;
    let rows = business
        .and_then(|business| sim.get(business.company))
        .into_iter()
        .flat_map(|company| company.workers.0.iter().copied())
        .filter_map(|id| {
            let human = sim.get(id)?;
            let state = match human.location {
                Location::Building(at) if at == building.id => StaffState::AtWork,
                Location::Building(at) if at == human.home.house => StaffState::AtHome,
                _ if human.router.target_dest == Some(Destination::Building(building.id)) => {
                    StaffState::Commuting
                }
                Location::Outside | Location::Vehicle(_) | Location::Transit(_)
                    if human.router.trip_purpose == TripPurpose::Home
                        && human.router.target_dest
                            == Some(Destination::Building(human.home.house)) =>
                {
                    StaffState::Commuting
                }
                _ => StaffState::Other,
            };
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
                Location::Vehicle(vehicle) | Location::Transit(vehicle) => {
                    let label = sim.get(vehicle).map_or("In a vehicle", |vehicle| {
                        match vehicle.vehicle.kind {
                            VehicleKind::Car => "In a car",
                            VehicleKind::Truck => "In a truck",
                            VehicleKind::Bus => "On a bus",
                        }
                    });
                    position_label(label, sim.pos(vehicle).unwrap_or(human.trans.pos))
                }
                Location::Outside => {
                    let label = if matches!(
                        human.router.cur_step(),
                        Some(RoutingStep::WaitAtStop { .. })
                    ) {
                        "Waiting at a bus stop"
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

fn position_label(label: &str, pos: Vec3) -> String {
    format!("{label} ({:.0}, {:.0})", pos.x, pos.y)
}

pub(super) fn render(
    uiworld: &UiWorld,
    sim: &Simulation,
    building: &Building,
    service: &ServicePrototype,
) {
    let map = sim.map();
    let staff = service_staff(sim, building, service);
    let scale = sim.population_scale();
    let at_work = staff
        .rows
        .iter()
        .filter(|row| row.state == StaffState::AtWork)
        .count();
    let blackout = map
        .electricity
        .net_id(building.id)
        .is_some_and(|net| sim.read::<ElectricityFlow>().blackout(net));
    let kind = service.service_kind;
    let utility = matches!(kind, ServiceKind::WaterPump | ServiceKind::SewageOutlet);
    let blocker = if building
        .connected_road
        .is_none_or(|road| map.get(road).is_none())
    {
        Some("Disconnected from the road")
    } else if staff.business.is_none() {
        Some("No active operator")
    } else if utility && water_provider_site(&map, building.id).is_some_and(|site| site.is_err()) {
        Some("Invalid utility site")
    } else if blackout && kind != ServiceKind::SewageOutlet {
        Some("Power blackout")
    } else {
        None
    };

    let label = |text: String| textc(on_secondary_container(), text);
    mincolumn(4.0, || {
        textc(on_secondary_container(), "Service staff");
        tooltip("Assigned workers determine service capacity", || {
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
        if utility {
            let load = effective_load(building, None, staff.business);
            let (metric, output) = match kind {
                ServiceKind::WaterPump => ("water output", load.water_production),
                _ => ("sewage capacity", load.sewage_capacity),
            };
            let actual = if inactive { 0.0 } else { output };
            label(format!("Network-wide · {actual:.0} {metric}"));
        } else if inactive {
            textc(on_secondary_container(), "Coverage inactive");
        } else if let Some(allocation) = sim
            .read::<CoverageGrid>()
            .service_allocation(building.id, kind)
        {
            let people = allocation.people * f32::from(scale.persons_per_agent());
            label(format!(
                "Last coverage pass · {people:.1} people served · {:.0} m reach",
                allocation.distance_m,
            ));
        } else {
            textc(on_secondary_container(), "Coverage awaiting the next pass");
        }

        for row in staff.rows {
            minrow(8.0, || {
                entity_link_named(uiworld, sim, row.id.into(), row.name);
                label(format!("{} · {}", row.state.label(), row.location));
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::{vec2, Vec2, OBB};
    use prototypes::{BuildingGen, GameTime, GoodsCompanyID, ServiceID, Tick, TICKS_PER_MINUTE};
    use scenarios::builders::{fill_houses, grid_city};
    use scenarios::ScenarioCtx;
    use simulation::map::BuildingKind;
    use simulation::map_dynamic::coverage_update;
    use simulation::souls::goods_company::company_soul;
    use simulation::souls::human::spawn_human;
    use simulation::{SimulationOptions, SoulID};
    use slotmapd::Key;

    #[test]
    fn service_staff_lists_live_workers_with_states_locations_and_served_coverage() {
        let mut fixture = ScenarioCtx::new(SimulationOptions {
            terrain_size: 4,
            save_replay: false,
            ..SimulationOptions::LEGACY
        });
        let grid = grid_city(&mut fixture, vec2(700.0, 700.0), 4, 4, 160.0);
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
                Some(grid.roads[0]),
            )
            .unwrap();
        let homes = fill_houses(&mut fixture, 4);
        let sim = &mut fixture.sim;
        // Start from the same unsampled electrical state as a newly installed network.
        *sim.write::<ElectricityFlow>() = ElectricityFlow::default();
        let Some(SoulID::GoodsCompany(company)) = company_soul(sim, station, proto) else {
            panic!("fire station must have a company");
        };

        let workers = std::array::from_fn(|index| spawn_human(sim, homes[index]).unwrap());
        let [at_work, commuting, at_home, visiting] = workers;
        {
            let world = sim.world_mut_unchecked();
            world.companies[company].workers.0 = [workers.as_slice(), &[HumanID::null()]].concat();
            world.humans[at_work].location = Location::Building(station);
            world.humans[commuting].location = Location::Outside;
            world.humans[commuting].router.target_dest = Some(Destination::Building(station));
            world.humans[commuting].router.trip_purpose = TripPurpose::Work;
            world.humans[at_home].location = Location::Building(homes[2]);
            world.humans[visiting].location = Location::Building(homes[0]);
        }
        let service = ServiceID::from(proto).prototype();
        let staff = service_staff(sim, sim.map().get(station).unwrap(), service);
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
        assert!(allocation.people > 0.0);
        assert!(allocation.people < 400.0);
        assert!(allocation.distance_m > 0.0);
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
