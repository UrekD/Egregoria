use engine::GfxContext;
use simulation::Simulation;

use crate::rendering::MapRenderer;
use crate::uiworld::UiWorld;

pub(crate) fn prepare_ground(
    _uiw: &UiWorld,
    _sim: &Simulation,
    _renderer: &mut MapRenderer,
    _gfx: &mut GfxContext,
    _world_generation: u64,
) {
}
