use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{map::BuildingID, Simulation};

#[derive(Default, Serialize, Deserialize)]
pub struct UtilityFlow {
    water_served: BTreeSet<BuildingID>,
    sewage_served: BTreeSet<BuildingID>,
}

impl UtilityFlow {
    pub fn has_water(&self, building: BuildingID) -> bool {
        self.water_served.contains(&building)
    }

    pub fn has_sewage(&self, building: BuildingID) -> bool {
        self.sewage_served.contains(&building)
    }
}

pub fn utility_flow_update(_sim: &mut Simulation) {}
