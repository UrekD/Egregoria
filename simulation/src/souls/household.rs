use std::collections::BTreeMap;

use prototypes::ItemID;
use serde::{Deserialize, Serialize};

use crate::economy::Trade;
use crate::utils::resources::Resources;
use crate::World;

#[derive(Default, Serialize, Deserialize)]
pub struct HouseholdStock {
    pub on_hand: BTreeMap<ItemID, u32>,
    pub carried: BTreeMap<ItemID, u32>,
    pub needs: Needs,
    pub basket: Option<BasketId>,
    pub last_shop: Option<u64>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Needs {
    pub food: f32,
    pub goods: f32,
    pub leisure: f32,
    pub rest: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BasketId(pub u64);

/// Returns true when the household path owns settlement of this pickup.
pub fn settle_local_pickup(_world: &mut World, _resources: &Resources, _trade: Trade) -> bool {
    false
}

pub fn household_update(_world: &mut World, _resources: &mut Resources) {}
