use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{map::BuildingID, Simulation};

#[derive(Default, Serialize, Deserialize)]
pub struct Happiness {
    pub by_building: BTreeMap<BuildingID, u8>,
}

pub fn happiness_update(_sim: &mut Simulation) {}
