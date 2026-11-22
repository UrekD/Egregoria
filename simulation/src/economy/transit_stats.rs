use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::map_dynamic::LineID;

#[derive(Default, Serialize, Deserialize)]
pub struct TransitStats {
    pub boardings: BTreeMap<LineID, u64>,
}

pub fn on_board(_stats: &mut TransitStats, _line: LineID) {}
