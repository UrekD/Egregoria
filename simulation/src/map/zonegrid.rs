use geom::OBB;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use prototypes::{Density, ZoneKind};

pub const ZONE_CELL_SIZE: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneCell {
    pub kind: ZoneKind,
    pub density: Density,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ZoneGrid {
    pub cells: BTreeMap<(i32, i32), ZoneCell>,
}

impl ZoneGrid {
    pub fn paint(&mut self, _: OBB, _: Option<ZoneKind>, _: Density) {}

    pub fn lot_zone(&self, _: &OBB) -> Option<ZoneCell> {
        None
    }
}

#[test]
#[ignore = "P1a zoning and demand behaviour"]
fn p1a_zoning_and_demand() {
    // Reject NaN and huge paint bounds, keep zones through road and lot regeneration,
    // resolve majorities and ties, and verify positive and deterministic bootstrap demand.
    todo!();
}
