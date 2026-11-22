use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use simulation::map::Map;
use simulation::Simulation;

pub(super) const AVAILABLE: bool = false;

pub(super) fn draw(
    _sim: &Simulation,
    _map: &Map,
    _uiw: &UiWorld,
    _draw: &mut ImmediateDraw,
    _rebuild: bool,
) {
}

pub(super) fn legend(_uiw: &UiWorld) {}
