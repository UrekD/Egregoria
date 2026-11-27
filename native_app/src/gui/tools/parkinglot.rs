use crate::gui::{ErrorTooltip, PotentialCommands, Tool};
use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use goryak::{mincolumn, minrow, on_secondary_container, padxy, selectable_label_primary, textc};
use simulation::map::building_parking::{garage_footprint, parking_entrance_near, GarageKind};
use simulation::map::CurbAnchor;
use simulation::world_command::WorldCommand;
use simulation::Simulation;

pub struct ParkingGarageTool {
    kind: GarageKind,
    entrance: Option<CurbAnchor>,
}

impl Default for ParkingGarageTool {
    fn default() -> Self {
        Self {
            kind: GarageKind::MultiStorey,
            entrance: None,
        }
    }
}

impl ParkingGarageTool {
    pub(crate) fn clear(&mut self) {
        self.entrance = None;
    }
}

pub(crate) fn properties(uiw: &UiWorld) {
    let mut state = uiw.write::<ParkingGarageTool>();
    padxy(12.0, 8.0, || {
        mincolumn(5.0, || {
            minrow(6.0, || {
                for kind in [GarageKind::MultiStorey, GarageKind::Underground] {
                    let label = format!("{} · {}", kind.label(), kind.cost());
                    if selectable_label_primary(state.kind == kind, &label).clicked {
                        state.kind = kind;
                        state.clear();
                    }
                }
            });
            textc(
                on_secondary_container(),
                if state.entrance.is_some() {
                    "Click to build at the selected entrance"
                } else {
                    "Click a curb to select the garage entrance"
                },
            );
        });
    });
}

pub(crate) fn update(sim: &Simulation, uiw: &UiWorld) {
    if *uiw.read::<Tool>() != Tool::ParkingGarage {
        return;
    }
    uiw.write::<PotentialCommands>().0.clear();
    let mut state = uiw.write::<ParkingGarageTool>();
    let input = uiw.read::<InputMap>();
    let over_ui = goryak::pointer_over_ui(input.screen.x, input.screen.y)
        || engine::pointer_over_egui(input.screen);
    let click = !over_ui && input.just_act.contains(&InputAction::Select) && uiw.can_author();
    let map = sim.map();
    let picked = (!over_ui)
        .then(|| {
            input
                .unprojected
                .and_then(|pos| parking_entrance_near(&map, pos))
        })
        .flatten();
    let Some(entrance) = state.entrance.or(picked) else {
        return;
    };
    let Some((footprint, mouth)) = garage_footprint(&map, state.kind, entrance) else {
        state.clear();
        *uiw.write::<ErrorTooltip>() =
            ErrorTooltip::new("Choose a curb with a driving lane and sidewalk");
        return;
    };
    drop(map);
    let command = WorldCommand::MapPlaceParkingGarage {
        kind: state.kind,
        entrance,
    };
    let preview = command.validate_and_price(sim);
    let color = if preview.is_ok() {
        simulation::colors().gui_success
    } else {
        simulation::colors().gui_danger
    };
    let mut draw = uiw.write::<ImmediateDraw>();
    draw.obb(footprint, mouth.z + 0.08).color(color.a(0.45));
    draw.circle(
        mouth.up(0.12),
        if state.entrance.is_some() { 1.75 } else { 1.0 },
    )
    .color(color);
    draw.line(mouth.up(0.1), footprint.center().z(mouth.z + 0.1), 0.65)
        .color(color);
    match preview {
        Ok(_) if click && state.entrance.is_some() => {
            uiw.commands().push(command);
            state.clear();
        }
        Ok(_) => {
            if click {
                state.entrance = Some(entrance);
            }
            uiw.write::<PotentialCommands>().set(command);
        }
        Err(reason) => {
            if click {
                state.clear();
            }
            *uiw.write::<ErrorTooltip>() = ErrorTooltip::new(
                crate::gui::notifications::command_message("MapPlaceParkingGarage", reason),
            );
        }
    }
}
