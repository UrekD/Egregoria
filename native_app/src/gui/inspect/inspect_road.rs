use goryak::Window;
use simulation::map::RoadID;
use simulation::Simulation;
use yakui::widgets::Pad;

use super::inspect_utility;
use crate::uiworld::UiWorld;

pub(crate) fn inspect_road(_uiw: &UiWorld, sim: &Simulation, road: RoadID) -> bool {
    if !sim.map().roads().contains_key(road) {
        return false;
    }

    let mut opened = true;
    Window {
        title: "Road".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 8.0,
    }
    .show(|| inspect_utility::render(sim, road));
    opened
}
