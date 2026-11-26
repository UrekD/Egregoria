use serde::{Deserialize, Serialize};

use super::{Endpoint, FreightTrace};
use crate::map::BuildingKind;
use crate::map_dynamic::BuildingInfos;
use crate::world_command::Rejection;
use crate::{CompanyID, Simulation, SoulID};
use prototypes::WarehouseSpec;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreightTerminalPolicy {
    pub admit_imports: bool,
    pub admit_exports: bool,
    pub max_calls: u8,
    pub max_truck_runs: u8,
}

impl Default for FreightTerminalPolicy {
    fn default() -> Self {
        Self {
            admit_imports: true,
            admit_exports: true,
            max_calls: 2,
            max_truck_runs: 6,
        }
    }
}

impl FreightTerminalPolicy {
    pub(crate) fn valid(self) -> bool {
        self.max_calls <= 2 && self.max_truck_runs <= 6
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct TerminalPolicyView {
    pub revision: u64,
    pub policy: FreightTerminalPolicy,
}

pub(crate) fn validate_policy(
    sim: &Simulation,
    terminal: Endpoint,
    expected_revision: u64,
    policy: FreightTerminalPolicy,
) -> Result<(), Rejection> {
    let map = sim.map();
    if !matches!(terminal.soul, SoulID::FreightStation(_))
        || !terminal.live(sim.world(), &map, &sim.read::<BuildingInfos>())
        || !map
            .get(terminal.building)
            .is_some_and(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))
    {
        return Err(Rejection::InvalidFreightTerminal);
    }
    let current = sim.read::<FreightTrace>().terminal_policy(terminal);
    if expected_revision != current.revision {
        return Err(Rejection::StaleFreightPolicy);
    }
    if !policy.valid() {
        return Err(Rejection::InvalidFreightPolicy);
    }
    Ok(())
}

pub(crate) fn set_policy(
    sim: &Simulation,
    terminal: Endpoint,
    expected_revision: u64,
    policy: FreightTerminalPolicy,
) -> Result<(), Rejection> {
    validate_policy(sim, terminal, expected_revision, policy)?;
    let mut freight = sim.write::<FreightTrace>();
    let current = freight.terminal_policy(terminal);
    if policy != current.policy {
        let revision = expected_revision
            .checked_add(1)
            .ok_or(Rejection::InvalidFreightPolicy)?;
        freight
            .policies
            .insert(terminal, TerminalPolicyView { revision, policy });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DepotRestock {
    pub low: u32,
    pub high: u32,
}

impl DepotRestock {
    fn valid(self, capacity: u32) -> bool {
        self.low < self.high && self.high <= capacity
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DepotRestockView {
    pub revision: u64,
    pub limits: DepotRestock,
}

impl FreightTrace {
    pub fn depot_restock(&self, company: CompanyID, storage: &WarehouseSpec) -> DepotRestockView {
        self.depots
            .get(&company)
            .copied()
            .unwrap_or(DepotRestockView {
                revision: 0,
                limits: DepotRestock {
                    low: storage.low_water_units,
                    high: storage.high_water_units,
                },
            })
    }
}

pub(crate) fn validate_restock(
    sim: &Simulation,
    depot: Endpoint,
    revision: u64,
    limits: DepotRestock,
) -> Result<CompanyID, Rejection> {
    let SoulID::GoodsCompany(company) = depot.soul else {
        return Err(Rejection::InvalidWarehouse);
    };
    if !depot.live(sim.world(), &sim.map(), &sim.read::<BuildingInfos>()) {
        return Err(Rejection::InvalidWarehouse);
    }
    let storage = sim.world().companies[company]
        .comp
        .proto
        .prototype()
        .storage
        .as_ref()
        .ok_or(Rejection::InvalidWarehouse)?;
    if !limits.valid(storage.capacity_units) {
        return Err(Rejection::InvalidFreightPolicy);
    }
    if sim
        .read::<FreightTrace>()
        .depot_restock(company, storage)
        .revision
        != revision
    {
        return Err(Rejection::StaleFreightPolicy);
    }
    Ok(company)
}

pub(crate) fn set_restock(
    sim: &Simulation,
    depot: Endpoint,
    revision: u64,
    limits: DepotRestock,
) -> Result<(), Rejection> {
    let company = validate_restock(sim, depot, revision, limits)?;
    let spec = sim.world().companies[company]
        .comp
        .proto
        .prototype()
        .storage
        .as_ref()
        .unwrap();
    let mut trace = sim.write::<FreightTrace>();
    if trace.depot_restock(company, spec).limits != limits {
        let revision = revision
            .checked_add(1)
            .ok_or(Rejection::InvalidFreightPolicy)?;
        trace
            .depots
            .insert(company, DepotRestockView { revision, limits });
    }
    Ok(())
}
