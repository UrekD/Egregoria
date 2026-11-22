use crate::gui::inspect::{entity_link, follow_button};
use crate::uiworld::UiWorld;
use goryak::{mincolumn, minrow, on_secondary_container, textc, VertScrollSize, Window};
use simulation::transportation::{Location, VehicleKind, VehicleState};
use simulation::{Simulation, VehicleID};
use yakui::widgets::Pad;

pub fn inspect_vehicle(uiworld: &UiWorld, sim: &Simulation, id: VehicleID) -> bool {
    let Some(v) = sim.get(id) else {
        return false;
    };

    let name = format!("{:?}", v.vehicle.kind);

    let mut is_open = true;
    Window {
        title: name.into(),
        pad: Pad::all(10.0),
        radius: 10.0,
        opened: &mut is_open,
        child_spacing: 5.0,
    }
    .show(|| {
        if cfg!(debug_assertions) {
            textc(on_secondary_container(), format!("{:?}", id));
        }

        match v.vehicle.state {
            VehicleState::Parked(_) | VehicleState::ParkedAtDepot => {
                textc(on_secondary_container(), "Parked");
            }
            VehicleState::Driving => {
                textc(
                    on_secondary_container(),
                    format!("Driving at {:.0}km/h", v.speed.0 * 3.6),
                );
            }
            VehicleState::Panicking(_) => {
                textc(on_secondary_container(), "Panicking");
            }
            VehicleState::ReturningToDepot { .. } => {
                textc(on_secondary_container(), "Returning to depot");
            }
            VehicleState::RoadToPark(_, _, _) => {
                textc(on_secondary_container(), "Parking");
            }
        }

        if matches!(v.vehicle.kind, VehicleKind::Bus) {
            let passengers: Vec<_> = sim
                .world()
                .humans
                .iter()
                .filter_map(|(human_id, human)| {
                    matches!(human.location, Location::Transit(bus) if bus == id)
                        .then_some(human_id)
                })
                .collect();
            textc(
                on_secondary_container(),
                format!("Passengers  {}", passengers.len()),
            );
            if !passengers.is_empty() {
                VertScrollSize::Fixed(210.0).show(|| {
                    mincolumn(4.0, || {
                        for human_id in passengers {
                            entity_link(uiworld, sim, human_id);
                        }
                    });
                });
            }
        } else {
            for (human_id, human) in &sim.world().humans {
                if human.router.personal_car == Some(id) {
                    minrow(5.0, || {
                        textc(on_secondary_container(), "Owned by");
                        entity_link(uiworld, sim, human_id);
                    });
                }
            }
        }

        follow_button(uiworld, id);
    });

    is_open
}
