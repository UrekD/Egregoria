use crate::map::BuildingID;
use crate::SoulID;
use serde::{Deserialize, Serialize};
use slotmapd::SecondaryMap;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct BuildingInfo {
    pub owner: Option<SoulID>,
    pub inside: Vec<SoulID>,
    pub residents: BTreeSet<SoulID>,
}

#[derive(Clone, Default, Serialize, Deserialize, Debug)]
pub struct BuildingInfos {
    assignment: SecondaryMap<BuildingID, BuildingInfo>,
    owners: BTreeMap<SoulID, BuildingID>,
    residences: BTreeMap<SoulID, BuildingID>,
}

impl BuildingInfos {
    pub fn insert(&mut self, building: BuildingID) {
        self.assignment.insert(building, BuildingInfo::default());
    }

    pub fn remove(&mut self, building: BuildingID) -> Option<BuildingInfo> {
        let info = self.assignment.remove(building)?;
        if let Some(owner) = info.owner {
            if self.owners.get(&owner) == Some(&building) {
                self.owners.remove(&owner);
            }
        }
        for resident in &info.residents {
            self.residences.remove(resident);
        }
        Some(info)
    }

    pub(crate) fn remove_soul(&mut self, soul: SoulID) {
        self.remove_resident(soul);
        if let Some(building) = self.owners.remove(&soul) {
            if let Some(info) = self.get_mut(building) {
                if info.owner == Some(soul) {
                    info.owner = None;
                }
            }
        }
        for (_, info) in self.assignment.iter_mut() {
            info.inside.retain(|&occupant| occupant != soul);
        }
    }

    pub fn get(&self, building: BuildingID) -> Option<&BuildingInfo> {
        self.assignment.get(building)
    }

    pub fn get_mut(&mut self, building: BuildingID) -> Option<&mut BuildingInfo> {
        self.assignment.get_mut(building)
    }

    pub fn building_owned_by(&self, soul: SoulID) -> Option<BuildingID> {
        match soul {
            SoulID::Human(_) => self.residence_of(soul),
            _ => self.owners.get(&soul).copied(),
        }
    }

    pub fn residence_of(&self, soul: SoulID) -> Option<BuildingID> {
        self.residences.get(&soul).copied()
    }

    pub(crate) fn add_resident(&mut self, building: BuildingID, soul: SoulID) {
        if self.get(building).is_none() || self.residence_of(soul) == Some(building) {
            return;
        }
        self.remove_resident(soul);
        self.residences.insert(soul, building);
        let info = self.get_mut(building).expect("registered residence");
        info.residents.insert(soul);
        if info.owner.is_none() {
            self.set_owner(building, soul);
        }
    }

    pub(crate) fn remove_resident(&mut self, soul: SoulID) {
        let Some(building) = self.residences.remove(&soul) else {
            return;
        };
        let Some(info) = self.get_mut(building) else {
            return;
        };
        info.residents.remove(&soul);
        if info.owner == Some(soul) {
            info.owner = None;
            let successor = info.residents.first().copied();
            self.owners.remove(&soul);
            if let Some(successor) = successor {
                self.set_owner(building, successor);
            }
        }
    }

    pub fn set_owner(&mut self, building: BuildingID, soul: SoulID) {
        let Some(info) = self.get_mut(building) else {
            return;
        };
        if let Some(previous) = info.owner.replace(soul) {
            self.owners.remove(&previous);
        }
        self.owners.insert(soul, building);
    }

    pub fn owner(&self, building: BuildingID) -> Option<SoulID> {
        self.assignment.get(building).and_then(|x| x.owner)
    }

    pub fn get_in(&mut self, building: BuildingID, e: SoulID) {
        let b = unwrap_ret!(self.get_mut(building));
        if cfg!(debug_assertions) && b.inside.contains(&e) {
            log::warn!(
                "called get_in({:?}, {:?}) but it was already inside",
                building,
                e
            );
        }
        b.inside.push(e);
    }

    pub fn get_out(&mut self, building: BuildingID, e: SoulID) {
        let b = unwrap_ret!(self.get_mut(building));
        let inside = &mut b.inside;
        if let Some(i) = inside.iter().position(|v| *v == e) {
            inside.swap_remove(i);
        } else {
            log::warn!(
                "called get_out({:?}, {:?}) but it was not inside",
                building,
                e
            );
        }
    }
}
