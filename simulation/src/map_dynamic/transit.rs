use std::collections::BTreeMap;

use geom::{Color, Vec2, Vec3};
use prototypes::{Money, Tick};
use serde::{Deserialize, Serialize};
use slotmapd::{new_key_type, HopSlotMap};

use crate::utils::resources::Resources;
use crate::world_command::{Rejection, WorldCommand};
use crate::{Simulation, VehicleID, World};

new_key_type! {
    pub struct StopID;
    pub struct LineID;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitStop {
    pub id: StopID,
    pub pos: Vec3,
    pub dir: Vec2,
    pub walk_pos: Vec3,
    pub berth: Vec3,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TransitLine {
    pub id: LineID,
    pub stops: Vec<StopID>,
    pub target_vehicles: u8,
    pub fare: Money,
    pub color: Color,
    pub suspended: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BusRun {
    pub line: LineID,
    pub next_stop_idx: usize,
    pub dwell_until: Option<Tick>,
    pub leg_started: Tick,
    pub failures: u8,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Transit {
    pub stops: HopSlotMap<StopID, TransitStop>,
    pub lines: HopSlotMap<LineID, TransitLine>,
    pub runs: BTreeMap<VehicleID, BusRun>,
    /// Increment when stop or line data changes, including while paused.
    pub revision: u64,
}

impl Transit {
    /// Estimated interval between buses, in elapsed ticks.
    pub fn headway(&self, _line: LineID) -> Option<u64> {
        None
    }
}

pub fn validate(command: &WorldCommand) -> Result<(), Rejection> {
    use WorldCommand::*;

    match command {
        TransitAddStop { .. }
        | TransitRemoveStop { .. }
        | TransitCreateLine { .. }
        | TransitEditLine { .. }
        | TransitRemoveLine { .. }
        | TransitSetVehicles { .. }
        | TransitSetFare { .. } => Err(Rejection::TransitNotImplemented),
        _ => Ok(()),
    }
}

pub fn on_vehicle_dropped(_sim: &mut Simulation, _vehicle: VehicleID, _position: Vec3) {}

pub fn on_topology_changed(_sim: &mut Simulation) {}

pub fn transit_stop_update(_world: &mut World, _resources: &mut Resources) {}

pub fn transit_dispatch_update(_world: &mut World, _resources: &mut Resources) {}

pub fn transit_board_update(_world: &mut World, _resources: &mut Resources) {}
