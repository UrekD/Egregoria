use std::collections::{BTreeMap, BTreeSet};

use prototypes::{try_prototype, Density, GoodsCompanyID};
use serde::{Deserialize, Serialize};

use crate::Simulation;

const MILESTONES: [(u32, &[&str]); 3] = [
    (100, &["Bus service", "Medium density"]),
    (250, &["High density"]),
    (500, &["Warehouses"]),
];

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProgressionMode {
    Guided,
    #[default]
    FreeBuild,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ObjectiveId {
    PositiveBalance,
    StockedShops,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BuildCapability {
    Prototype(GoodsCompanyID),
    ZoneDensity(Density),
    BusService,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LockedUntil {
    pub milestone: u8,
    pub population: u32,
}

#[derive(Default, Serialize, Deserialize)]
pub struct ProgressionState {
    pub mode: ProgressionMode,
    pub highest_milestone: u8,
    pub claimed: BTreeSet<ObjectiveId>,
    pub last_processed_day: Option<u64>,
    pub streaks: BTreeMap<ObjectiveId, u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ObjectiveProgress {
    #[default]
    Unavailable,
    InProgress {
        completed: u32,
        required: u32,
    },
    Claimed,
}

#[derive(Debug)]
pub struct ProgressionStatus {
    pub mode: ProgressionMode,
    pub population: u32,
    pub next: Option<(u8, u32, Vec<String>)>,
    pub objectives: Vec<(ObjectiveId, ObjectiveProgress)>,
}

pub fn progression_status(sim: &Simulation) -> ProgressionStatus {
    let state = sim.read::<ProgressionState>();
    let population = sim.world().humans.len() as u32;
    let next = if state.mode == ProgressionMode::Guided {
        MILESTONES
            .iter()
            .enumerate()
            .find(|&(index, &(required, _))| {
                index as u8 >= state.highest_milestone && population < required
            })
            .map(|(index, &(required, unlocks))| {
                (
                    index as u8 + 1,
                    required,
                    unlocks.iter().map(|&name| name.to_owned()).collect(),
                )
            })
    } else {
        None
    };
    ProgressionStatus {
        mode: state.mode,
        population,
        next,
        objectives: Vec::new(),
    }
}

pub fn can_build(sim: &Simulation, cap: BuildCapability) -> Result<(), LockedUntil> {
    let state = sim.read::<ProgressionState>();
    if state.mode == ProgressionMode::FreeBuild {
        return Ok(());
    }
    // Starter roads, low R/C/I/O zones, power, water, sewage, fire, clinic,
    // police, school, park, garbage and freight connections are available at tier zero.
    let milestone = match cap {
        BuildCapability::Prototype(id) => try_prototype(id).map_or(0, |proto| {
            if proto.storage.is_some() {
                3
            } else {
                proto.unlock_milestone.unwrap_or(0)
            }
        }),
        BuildCapability::BusService | BuildCapability::ZoneDensity(Density::Medium) => 1,
        BuildCapability::ZoneDensity(Density::High) => 2,
        BuildCapability::ZoneDensity(Density::Low) => 0,
    };
    if milestone <= state.highest_milestone {
        return Ok(());
    }

    let (population, _) = MILESTONES[usize::from(milestone - 1)];
    if sim.world().humans.len() >= population as usize {
        Ok(())
    } else {
        Err(LockedUntil {
            milestone,
            population,
        })
    }
}

pub fn progression_update(_sim: &mut Simulation) {}
