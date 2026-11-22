use std::collections::BTreeMap;

use prototypes::ItemID;
use serde::{Deserialize, Serialize};

use super::Trade;
use crate::map::BuildingID;
use crate::souls::desire::Work;
use crate::transportation::Location;
use crate::utils::resources::Resources;
use crate::{CompanyID, HumanEnt, HumanID, ParCommandBuffer, VehicleID, World};

pub use prototypes::WarehousePolicy;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ShipmentId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LoadId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemQuantity {
    pub item: ItemID,
    pub units: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Shipment;

#[derive(Debug, Serialize, Deserialize)]
pub struct TruckLoad {
    pub vehicle: VehicleID,
    pub items: Vec<ItemQuantity>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct CargoBook {
    next_id: u64,
    shipments: BTreeMap<ShipmentId, Shipment>,
    loads: BTreeMap<LoadId, TruckLoad>,
    pub policies: BTreeMap<CompanyID, WarehousePolicy>,
}

impl CargoBook {
    pub fn manifest_for(&self, vehicle: VehicleID) -> Option<&TruckLoad> {
        self.loads.values().find(|load| load.vehicle == vehicle)
    }
}

/// Returns true when the cargo path owns settlement of this trade.
pub fn settle_physical_match(_world: &mut World, _resources: &Resources, _trade: Trade) -> bool {
    false
}

/// Returns true when the cargo path owns settlement of this arrival.
pub fn settle_delivery(
    _commands: &ParCommandBuffer<HumanEnt>,
    _driver: HumanID,
    _location: &Location,
    _destination: BuildingID,
    _work: Option<&mut Work>,
) -> bool {
    false
}

/// Returns true when the cargo system settles the load's arrival.
pub fn complete_delivery(_load: LoadId, _truck: VehicleID, _at: BuildingID) -> bool {
    false
}

pub fn cargo_update(_world: &mut World, _resources: &mut Resources) {}
