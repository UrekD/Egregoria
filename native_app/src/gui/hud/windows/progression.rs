use super::stub_window;
use crate::uiworld::UiWorld;
use simulation::Simulation;

pub(crate) fn render(_uiw: &UiWorld, _sim: &Simulation, opened: &mut bool) {
    stub_window("Progression", opened);
}
