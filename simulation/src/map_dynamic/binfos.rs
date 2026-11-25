use crate::map::{BuildingID, BuildingKind, Map};
use crate::souls::residence::Residences;
use crate::SoulID;
use prototypes::{FreightStationPrototypeID, GoodsCompanyID};
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
    business_addresses: BTreeSet<BuildingID>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VacancyTarget {
    Business(BuildingID, GoodsCompanyID),
    Residence(BuildingID),
    Freight(BuildingID, FreightStationPrototypeID),
}

impl VacancyTarget {
    fn order(self) -> (BuildingID, u8) {
        match self {
            Self::Business(id, _) => (id, 0),
            Self::Residence(id) => (id, 1),
            Self::Freight(id, _) => (id, 2),
        }
    }
}

impl BuildingInfos {
    pub fn insert(&mut self, building: BuildingID) {
        if self.assignment.get(building).is_none() {
            self.assignment.insert(building, BuildingInfo::default());
            self.mark_vacancy_changed(building);
        }
    }

    /// Register a shared address before the first resident can claim legacy ownership.
    pub(crate) fn mark_business_address(&mut self, building: BuildingID) {
        if self.business_addresses.insert(building)
            && matches!(self.owner(building), Some(SoulID::Human(_)))
        {
            self.clear_owner(building);
        }
    }

    pub fn remove(&mut self, building: BuildingID) -> Option<BuildingInfo> {
        let info = self.assignment.remove(building)?;
        self.business_addresses.remove(&building);
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
        self.remove_soul_affiliations(soul);
        for (_, info) in self.assignment.iter_mut() {
            info.inside.retain(|&occupant| occupant != soul);
        }
    }

    pub(crate) fn remove_soul_occupancy(&mut self, souls: &BTreeSet<SoulID>) {
        for (_, info) in self.assignment.iter_mut() {
            info.inside.retain(|occupant| !souls.contains(occupant));
        }
    }

    pub(crate) fn remove_soul_affiliations(&mut self, soul: SoulID) {
        self.remove_resident(soul);
        if let Some(building) = self.owners.remove(&soul) {
            if let Some(info) = self.get_mut(building) {
                if info.owner == Some(soul) {
                    info.owner = None;
                }
            }
            self.mark_vacancy_changed(building);
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
        if info.owner.is_none() && !self.business_addresses.contains(&building) {
            self.set_owner(building, soul);
        }
        self.mark_vacancy_changed(building);
    }

    pub(crate) fn take_residents(&mut self, building: BuildingID) -> BTreeSet<SoulID> {
        let Some(info) = self.assignment.get_mut(building) else {
            return BTreeSet::new();
        };
        let residents = std::mem::take(&mut info.residents);
        if info.owner.is_some_and(|owner| residents.contains(&owner)) {
            self.clear_owner(building);
        }
        for &resident in &residents {
            self.residences.remove(&resident);
        }
        self.mark_vacancy_changed(building);
        residents
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

    pub(crate) fn clear_owner(&mut self, building: BuildingID) {
        if let Some(owner) = self.get_mut(building).and_then(|info| info.owner.take()) {
            self.owners.remove(&owner);
            self.mark_vacancy_changed(building);
        }
    }

    pub fn set_owner(&mut self, building: BuildingID, soul: SoulID) {
        if matches!(soul, SoulID::Human(_)) && self.business_addresses.contains(&building) {
            return;
        }
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
        now: u64,
    ) -> Vec<VacancyTarget> {
        if self.vacancies.is_none() {
            self.vacancy_changes.extend(self.assignment.keys());
        }
        for id in std::mem::take(&mut self.vacancy_changes) {
            if map.get(id).is_some_and(|building| building.is_mixed_use())
                && self.assignment.get(id).is_some()
            {
                self.mark_business_address(id);
            }
            let vacancies = self.vacancies.get_or_insert_with(Vacancies::default);
            vacancies.homes.remove(&id);
            vacancies.unowned.remove(&id);
            let Some((building, info)) = map.get(id).zip(self.assignment.get(id)) else {
                continue;
            };
            if building.has_residential_use()
                && residences
                    .by_building
                    .get(&id)
                    .is_some_and(|home| info.residents.len() < home.capacity as usize)
            {
                vacancies.homes.insert(id);
            }
            if matches!(
                building.kind,
                BuildingKind::GoodsCompany(_) | BuildingKind::RailFreightStation(_)
            ) && info.owner.is_none()
            {
                vacancies.unowned.insert(id);
            }
        }
        let Some(vacancies) = &self.vacancies else {
            return Vec::new();
        };
        let mut targets: Vec<_> = vacancies
            .unowned
            .iter()
            .filter_map(|&id| {
                let building = map.get(id)?;
                match building.kind {
                    BuildingKind::GoodsCompany(proto)
                        if building.development().is_none_or(|site| {
                            !building.is_abandoned_growable()
                                && building
                                    .connected_road
                                    .is_some_and(|road| map.get(road).is_some())
                                && site.business.is_some_and(|role| role.can_open(now))
                        }) =>
                    {
                        Some(VacancyTarget::Business(id, proto))
                    }
                    BuildingKind::RailFreightStation(proto) => {
                        Some(VacancyTarget::Freight(id, proto))
                    }
                    _ => None,
                }
            })
            .collect();
        if immigrating {
            targets.extend(vacancies.homes.iter().filter_map(|&id| {
                let building = map.get(id)?;
                (!building.is_orphaned()
                    && !building.is_abandoned_growable()
                    && !residences.is_abandoned(id))
                .then_some(VacancyTarget::Residence(id))
            }));
        }
        targets.sort_unstable_by_key(|target| target.order());
        targets
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
