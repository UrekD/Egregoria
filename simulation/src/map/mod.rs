mod objects {
    mod building;
    mod intersection;
    mod lane;
    mod lot;
    mod parking;
    mod road;
    mod turn;

    pub use building::*;
    pub use intersection::*;
    pub use lane::*;
    pub use lot::*;
    pub use parking::*;
    pub use road::*;
    pub use turn::*;
}

pub use objects::*;

pub mod procgen {
    mod building;
    pub mod heightmap;
    mod presets;
    mod water;

    pub use building::*;
    pub use presets::*;
    pub use water::generate_waters;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PresetImportError {
        WaterConfigured,
    }

    pub fn load_parismap(map: &mut super::Map) -> Result<(), PresetImportError> {
        if map.environment().has_water_configuration() {
            return Err(PresetImportError::WaterConfigured);
        }
        presets::load_parismap(map);
        Ok(())
    }

    pub fn load_testfield(
        map: &mut super::Map,
        pos: geom::Vec2,
        size: u32,
        spacing: f32,
    ) -> Result<(), PresetImportError> {
        if map.environment().has_water_configuration() {
            return Err(PresetImportError::WaterConfigured);
        }
        presets::load_testfield(map, pos, size, spacing);
        Ok(())
    }
}

mod change_detection;
mod electricity_cache;
pub mod import;
mod light_policy;
#[allow(clippy::module_inception)]
mod map;
mod pathfinding;
mod reachability;
mod serializing;
mod spatial_map;
pub mod special_placement;
pub mod terrain;
mod traffic_control;
mod traversable;
mod turn_policy;
mod water;
mod water_serde;
pub mod zonegrid;

// Use self or else it would be ambiguous with "pathfinding" crate
pub use self::pathfinding::*;
pub use change_detection::*;
pub use electricity_cache::*;
pub use light_policy::*;
pub use map::*;
pub use spatial_map::*;
pub use terrain::*;
pub use traffic_control::*;
pub use traversable::*;
pub use turn_policy::*;
pub use water::*;
pub use zonegrid::*;

pub use ::pathfinding as pathfinding_crate;

pub const CROSSWALK_WIDTH: f32 = 2.0;
pub const ROAD_Z_OFFSET: f32 = 0.3;
pub const MAX_SLOPE: f32 = 0.25; // 25% grade
