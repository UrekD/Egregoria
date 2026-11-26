use crate::debug_gui::debug_window::DebugState;
use crate::gui::follow::FollowEntity;
use crate::gui::{InspectedBuilding, InspectedEntity, InspectedRoad};
use crate::uiworld::UiWorld;
use geom::Vec3;
use goryak::{button_primary, primary_link};
use inspect_building::inspect_building;
use inspect_human::inspect_human;
use inspect_train::inspect_train;
use inspect_vehicle::inspect_vehicle;
use simulation::map::BuildingID;
use simulation::{AnyEntity, Simulation};
use slotmapd::Key;

mod inspect_building;
mod inspect_human;
mod inspect_needs;
mod inspect_road;
mod inspect_service;
mod inspect_utility;
mod inspect_warehouse;
mod inspect_train;
mod inspect_vehicle;

pub fn new_inspector(uiworld: &UiWorld, sim: &Simulation) {
    profiling::scope!("hud::inspector");
    if *uiworld.read::<crate::gui::Tool>() == crate::gui::Tool::RoadEditor {
        let selected = uiworld.read::<crate::gui::roadeditor::RoadEditorResource>()
            .inspect.as_ref().map(|selection| selection.id);
        if let Some(id) = selected {
            if !crate::gui::traffic_repair::render_junction(uiworld, sim, id) {
                uiworld.write::<crate::gui::roadeditor::RoadEditorResource>().inspect = None;
            }
        }
    }
    let inspected_building = *uiworld.read::<InspectedBuilding>();
    if let Some(b) = inspected_building.e {
        let is_open = inspect_building(uiworld, sim, b);
        if !is_open {
            uiworld.write::<InspectedBuilding>().e = None;
        }
    }

    let inspected_entity = uiworld.read::<InspectedEntity>().e;
    let Some(e) = inspected_entity else {
        let road = uiworld.read::<InspectedRoad>().0;
        if inspected_building.e.is_none() {
            if let Some(road) = road {
                if !inspect_road::inspect_road(uiworld, sim, road) {
                    uiworld.write::<InspectedRoad>().0 = None;
                }
            }
        }
        return;
    };

    let force_debug_inspect = uiworld.read::<DebugState>().debug_inspector;
    if force_debug_inspect {
        return;
    }

    let mut is_open = true;
    match e {
        AnyEntity::HumanID(id) if !force_debug_inspect => {
            is_open = inspect_human(uiworld, sim, id);
        }
        AnyEntity::VehicleID(id) if !force_debug_inspect => {
            is_open = inspect_vehicle(uiworld, sim, id);
        }
        AnyEntity::WagonID(id) if !force_debug_inspect => {
            let Some(w) = sim.world().get(id) else {
                return;
            };
            let train_id = w.itfollower.leader;
            uiworld.write::<InspectedEntity>().e = Some(AnyEntity::TrainID(train_id));
        }
        AnyEntity::TrainID(id) if !force_debug_inspect => {
            is_open = inspect_train(uiworld, sim, id);
        }
        _ => {}
    }

    if !is_open {
        uiworld.write::<InspectedEntity>().e = None;
    }
}

pub fn building_link(uiworld: &UiWorld, sim: &Simulation, b: BuildingID) {
    if primary_link(format!("{:?}", b.data())) {
        if let Some(b) = sim.map().buildings().get(b) {
            focus_building(uiworld, b.id, b.door_pos);
        }
    }
}

pub(crate) fn focus_building(uiworld: &UiWorld, building: BuildingID, door: Vec3) {
    uiworld.write::<InspectedBuilding>().e = Some(building);
    uiworld.write::<FollowEntity>().0 = None;
    uiworld.camera_mut().targetpos = door;
}

pub fn entity_link(uiworld: &UiWorld, sim: &Simulation, e: impl Into<AnyEntity>) {
    entity_link_inner(uiworld, sim, e.into())
}

fn entity_link_inner(uiworld: &UiWorld, sim: &Simulation, e: AnyEntity) {
    let linkname = match e {
        AnyEntity::HumanID(id) => {
            if let Some(human) = sim.world().humans.get(id) {
                human.personal_info.name.to_string()
            } else {
                "???".to_string()
            }
        }
        _ => format!("{}", e),
    };

    entity_link_named(uiworld, sim, e, linkname);
}

fn entity_link_named(uiworld: &UiWorld, sim: &Simulation, e: AnyEntity, name: String) {
    if primary_link(name) {
        uiworld.write::<InspectedEntity>().e = Some(e);
        if sim.pos_any(e).is_some() {
            uiworld.write::<FollowEntity>().0 = Some(e);
        }
    }
}

pub fn follow_button(uiworld: &UiWorld, id: impl Into<AnyEntity>) {
    follow_button_inner(uiworld, id.into())
}

fn follow_button_inner(uiworld: &UiWorld, id: AnyEntity) {
    let mut follow = uiworld.write::<FollowEntity>();
    if follow.0 != Some(id) && button_primary("follow").show().clicked {
        follow.0 = Some(id);
    }
}
