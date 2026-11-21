use std::collections::BTreeMap;

use prototypes::Tick;
use serde::{Deserialize, Serialize};

use crate::{map::BuildingID, Simulation};

#[derive(Default, Serialize, Deserialize)]
pub struct Fires {
    /// Tick at which each active fire started.
    pub by_building: BTreeMap<BuildingID, Tick>,
}

pub fn events_update(_sim: &mut Simulation) {}
