use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Simulation;

#[derive(Default, Serialize, Deserialize)]
pub struct EnvGrid {
    pub cells: BTreeMap<(i32, i32), EnvCell>,
}

impl EnvGrid {
    pub const CELL_SIZE: f32 = 32.0;
}

#[derive(Default, Serialize, Deserialize)]
pub struct EnvCell {
    pub pollution: f32,
    pub land_value: f32,
}

pub fn env_update(_sim: &mut Simulation) {}
