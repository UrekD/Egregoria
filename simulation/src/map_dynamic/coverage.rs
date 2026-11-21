use std::collections::BTreeMap;

use prototypes::ServiceKind;
use serde::{Deserialize, Serialize};

use crate::{map::BuildingID, Simulation};

#[derive(Default, Serialize, Deserialize)]
pub struct CoverageGrid {
    pub by_kind: BTreeMap<ServiceKind, BTreeMap<BuildingID, f32>>,
}

impl CoverageGrid {
    pub fn coverage(&self, building: BuildingID, kind: ServiceKind) -> f32 {
        self.by_kind
            .get(&kind)
            .and_then(|receivers| receivers.get(&building))
            .copied()
            .unwrap_or_default()
    }
}

pub fn coverage_update(_sim: &mut Simulation) {}
