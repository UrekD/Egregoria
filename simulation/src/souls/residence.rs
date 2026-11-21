use crate::map::{BuildingID, Density};
use crate::Simulation;
use prototypes::ResidencePrototypeID;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct Residence {
    pub prototype: ResidencePrototypeID,
    pub density: Density,
    pub level: u8,
    pub capacity: u32,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Residences {
    pub by_building: BTreeMap<BuildingID, Residence>,
}

pub fn on_house_built(_: &Simulation, _: BuildingID) {}

pub fn on_building_removed(_: &mut Simulation, _: BuildingID) {}

pub fn residence_update(_: &mut Simulation) {}
