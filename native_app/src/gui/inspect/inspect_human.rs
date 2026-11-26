use goryak::{dragvalue, error, fixed_spacer, minrow, on_secondary_container, textc, Window};
use prototypes::ItemID;
use std::borrow::Cow;
use yakui::widgets::Pad;

use simulation::diagnostics::{citizen_diagnostics, CitizenBlocker, CitizenIntention};
use simulation::economy::Market;
use simulation::map_dynamic::{Destination, ParkingReserveError, RouterError};
use simulation::souls::desire::WorkKind;
use simulation::transportation::Location;
use simulation::{HumanID, Simulation};

use crate::gui::inspect::{building_link, follow_button};
use crate::gui::item_icon_yakui;
use crate::uiworld::UiWorld;

/// Inspect a specific building, showing useful information about it
pub fn inspect_human(uiworld: &UiWorld, sim: &Simulation, id: HumanID) -> bool {
    let Some(human) = sim.get(id) else {
        return false;
    };
    let Some(diagnostics) = citizen_diagnostics(sim, id) else {
        return false;
    };

    let pinfo = &human.personal_info;
    let title = format!("{}{:?} • {}", pinfo.age, pinfo.gender, pinfo.name);

    let mut is_open = true;

    fn label(x: impl Into<Cow<'static, str>>) {
        textc(on_secondary_container(), x);
    }

    Window {
        title: title.into(),
        pad: Pad::all(10.0),
        radius: 10.0,
        opened: &mut is_open,
        child_spacing: 5.0,
    }
    .show(|| {
        if cfg!(debug_assertions) {
            label(format!("{:?}", id));
        }
        let scale = sim.population_scale();
        if scale != simulation::PopulationScale::REALISTIC {
            label(format!("Representative agent · {scale}"));
        }

        match human.location {
            Location::Outside => {}
            Location::Vehicle(_) => {
                label("In a vehicle");
            }
            Location::Transit(_) => {
                label("On a bus");
            }
            Location::Building(x) => {
                minrow(5.0, || {
                    label("In a building:");
                    building_link(uiworld, sim, x);
                });
            }
        }

        fixed_spacer((0.0, 5.0));
        label("Why");
        label(match diagnostics.intention {
            CitizenIntention::Idle => "Idle",
            CitizenIntention::Travel => "Traveling",
            CitizenIntention::GoHome => "Going home",
            CitizenIntention::GoToWork => "Going to work",
            CitizenIntention::PickUpGoods => "Picking up retail goods",
            CitizenIntention::ChangeVehicle => "Changing vehicle",
            CitizenIntention::Deliver => "Making a delivery",
        });
        match diagnostics.destination {
            Some(Destination::Outside(pos)) => label(format!("Destination · {pos}")),
            Some(Destination::Building(building)) if sim.map().get(building).is_some() => {
                minrow(5.0, || {
                    label("Destination");
                    building_link(uiworld, sim, building);
                });
            }
            Some(Destination::Building(_)) => label("Destination removed"),
            None => {}
        }
        if let Some(blocker) = diagnostics.blocker {
            textc(error(), blocker_description(blocker));
        }
        fixed_spacer((0.0, 5.0));

        minrow(5.0, || {
            label("House is");
            building_link(uiworld, sim, human.home.house);
        });

        label(format!("Last ate: {}", human.food.last_ate));

        if let Some(ref x) = human.work {
            minrow(5.0, || {
                label("Working at");
                building_link(uiworld, sim, x.workplace);
                match x.kind {
                    WorkKind::Driver { .. } => {
                        label("as a driver");
                    }
                    WorkKind::Worker => {
                        label("as a worker");
                    }
                }
            });
        }

        fixed_spacer((0.0, 10.0));
        label("Desires");
        minrow(5.0, || {
            let mut score = human.food.last_score;
            dragvalue().show(&mut score);
            label("Food");
        });
        minrow(5.0, || {
            let mut score = human.home.last_score;
            dragvalue().show(&mut score);
            label("Home");
        });
        minrow(5.0, || {
            let mut score = human.work.as_ref().map(|x| x.last_score).unwrap_or(0.0);
            dragvalue().show(&mut score);
            label("Work");
        });

        let market = sim.read::<Market>();

        fixed_spacer((0.0, 10.0));

        let jobopening = ItemID::new("job-opening");
        for (&item_id, m) in market.iter() {
            let Some(v) = m.capital(id.into()) else {
                continue;
            };
            if item_id == jobopening {
                continue;
            }

            item_icon_yakui(uiworld, item_id, v);
        }

        follow_button(uiworld, id);
    });
    is_open
}

fn blocker_description(blocker: CitizenBlocker) -> &'static str {
    match blocker {
        CitizenBlocker::DestinationRemoved => "The route's destination was removed",
        CitizenBlocker::DestinationDisconnected => "The destination has no road connection",
        CitizenBlocker::PickupTimedOut => "The last retail pickup timed out",
        CitizenBlocker::Route(RouterError::PropertyParkingBlocked) => "The driveway entrance is blocked",
        CitizenBlocker::Route(RouterError::LocatingVehicle) => "Could not locate the vehicle",
        CitizenBlocker::Route(RouterError::TranslatingParkingSpotToDrivePos) => {
            "Could not route to the parking spot"
        }
        CitizenBlocker::Route(RouterError::ReservingParkingSpot(reason)) => match reason {
            ParkingReserveError::FindingNearestLane => "Could not find a lane near the destination",
            ParkingReserveError::FetchingLaneData => "The parking lane is unavailable",
            ParkingReserveError::NoSpotFoundAfterSearch => "Could not find a parking spot",
        },
    }
}
