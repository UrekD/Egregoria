use crate::map::{BuildingID, BuildingKind, Map};
use crate::souls::residence::Residences;
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
    #[serde(skip)]
    vacancies: Option<Vacancies>,
    #[serde(skip)]
    vacancy_changes: BTreeSet<BuildingID>,
}

#[derive(Clone, Debug, Default)]
struct Vacancies {
    homes: BTreeSet<BuildingID>,
    unowned: BTreeSet<BuildingID>,
}

impl BuildingInfos {
    pub fn insert(&mut self, building: BuildingID) {
        self.assignment.insert(building, BuildingInfo::default());
        self.mark_vacancy_changed(building);
    }

    pub fn remove(&mut self, building: BuildingID) -> Option<BuildingInfo> {
        let info = self.assignment.remove(building)?;
        self.mark_vacancy_changed(building);
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
            self.mark_vacancy_changed(building);
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
        self.mark_vacancy_changed(building);
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
        self.mark_vacancy_changed(building);
    }

    pub fn set_owner(&mut self, building: BuildingID, soul: SoulID) {
        let Some(info) = self.get_mut(building) else {
            return;
        };
        if let Some(previous) = info.owner.replace(soul) {
            self.owners.remove(&previous);
        }
        self.owners.insert(soul, building);
        self.mark_vacancy_changed(building);
    }

    pub(crate) fn mark_vacancy_changed(&mut self, building: BuildingID) {
        self.vacancy_changes.insert(building);
    }

    pub(crate) fn vacant_buildings(
        &mut self,
        map: &Map,
        residences: &Residences,
        immigrating: bool,
    ) -> Vec<(BuildingKind, BuildingID)> {
        let vacancies = self.vacancies.get_or_insert_with(|| {
            // Rebuild the derived index from loaded building and residence state.
            self.vacancy_changes.extend(self.assignment.keys());
            Vacancies::default()
        });
        for id in std::mem::take(&mut self.vacancy_changes) {
            vacancies.homes.remove(&id);
            vacancies.unowned.remove(&id);
            let Some((building, info)) = map.get(id).zip(self.assignment.get(id)) else {
                continue;
            };
            match building.kind {
                BuildingKind::House
                    if !residences.is_abandoned(id)
                        && residences
                            .by_building
                            .get(&id)
                            .is_some_and(|home| info.residents.len() < home.capacity as usize) =>
                {
                    vacancies.homes.insert(id);
                }
                BuildingKind::GoodsCompany(_) | BuildingKind::RailFreightStation(_)
                    if info.owner.is_none() =>
                {
                    vacancies.unowned.insert(id);
                }
                _ => {}
            }
        }

        let homes = immigrating
            .then_some(&vacancies.homes)
            .into_iter()
            .flatten();
        let mut buildings: Vec<_> = vacancies
            .unowned
            .iter()
            .chain(homes)
            .filter_map(|&id| {
                let building = map.get(id)?;
                // A house can lose or regain its road without changing its vacancy.
                (building.kind != BuildingKind::House || !building.is_orphaned())
                    .then_some((building.kind, id))
            })
            .collect();
        buildings.sort_unstable_by_key(|&(_, id)| id);
        buildings
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
