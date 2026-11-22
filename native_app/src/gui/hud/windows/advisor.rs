use crate::uiworld::UiWorld;
use simulation::Simulation;

pub(super) const AVAILABLE: bool = false;

pub(super) fn render(_uiw: &UiWorld, _sim: &Simulation, opened: &mut bool) {
    super::stub_window("Advisor", opened);
}
