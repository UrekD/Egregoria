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
    mod tiled_start;
    pub(crate) use tiled_start::commission_native_land;
    mod water;

    pub use building::*;
    pub use presets::*;
    pub use water::generate_waters;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PresetImportError {
        WaterConfigured,
    }

    pub fn load_parismap(map: &mut super::Map) -> Result<(), PresetImportError> {
        if map.needs_topology_preparation() {
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
        if map.needs_topology_preparation() {
            return Err(PresetImportError::WaterConfigured);
        }
        presets::load_testfield(map, pos, size, spacing);
        Ok(())
    }
}

mod authored;
mod change_detection;
mod electricity_cache;
mod height_override;
pub mod import;
pub(crate) mod land_frontage;
mod land_ownership;
mod land_site;
mod light_policy;
#[allow(clippy::module_inception)]
mod map;
mod pathfinding;
mod reachability;
pub mod road_edit;
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
pub use authored::*;
pub use change_detection::*;
pub use electricity_cache::*;
pub use land_ownership::*;
pub(crate) use land_site::PreparedEffects;
pub use light_policy::*;
pub use map::*;
pub use spatial_map::*;
pub use terrain::*;
pub use traffic_control::*;
pub use traversable::*;
pub use turn_policy::*;
pub use water::water_service::*;
pub use water::*;
pub use zonegrid::*;

pub use ::pathfinding as pathfinding_crate;

pub const CROSSWALK_WIDTH: f32 = 2.0;
pub const ROAD_Z_OFFSET: f32 = 0.3;
pub const MAX_SLOPE: f32 = 0.25; // 25% grade

/// Checks the scalar fields that the wire decoder validates for water edits.
/// WaterStroke and WaterBasin can only be constructed with checked geometry.
pub(crate) fn validate_water_edit_wire(edit: &WaterEdit) -> Result<(), WaterEditError> {
    let (level, depth) = match *edit {
        WaterEdit::SetSeaLevel { level_dm } => (level_dm, None),
        WaterEdit::CarveRiver {
            width_m,
            depth_m,
            level_dm,
            ..
        } => {
            if !water::valid_river_width(width_m) {
                return Err(WaterEditError::InvalidInput);
            }
            (level_dm, Some(depth_m))
        }
        WaterEdit::FillLake {
            level_dm, depth_m, ..
        } => (Some(level_dm), Some(depth_m)),
        WaterEdit::RemoveFreshWater { .. } => return Ok(()),
    };
    if depth.is_some_and(|depth| !water::valid_water_depth(f64::from(depth))) {
        return Err(WaterEditError::InvalidInput);
    }
    if let Some(level) = level {
        water::packed_bed_limit(level, water::MIN_DEPTH)?;
    }
    Ok(())
}

pub(crate) fn validate_loaded_land(sim: &crate::Simulation) -> Result<(), String> {
    land_ownership::validate_loaded_land(sim)?;
    sim.map()
        .validate_commissioned_reserves(sim.read::<crate::SimulationOptions>().land_recipe)
        .map_err(|error| format!("Invalid commissioned land: {error:?}"))
}
