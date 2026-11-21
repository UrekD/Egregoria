use crate::map::ZoneKind;
use crate::{utils::resources::Resources, World};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Default, Serialize, Deserialize)]
pub struct ZoneDemand {
    pub values: BTreeMap<ZoneKind, f32>,
}

pub fn demand_update(_: &mut World, _: &mut Resources) {}
