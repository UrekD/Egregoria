use crate::map::{BuildingID, Density};
use crate::map_dynamic::BuildingInfos;
use crate::souls::lifecycle;
use crate::{Simulation, SoulID};
use geom::Vec3;
use ordered_float::OrderedFloat;
use prototypes::{CompanyKind, ResidencePrototype, ResidencePrototypeID, TICKS_PER_HOUR};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct Residence {
    pub prototype: ResidencePrototypeID,
    pub density: Density,
    pub level: u8,
    pub capacity: u32,
}

impl From<&ResidencePrototype> for Residence {
    fn from(proto: &ResidencePrototype) -> Self {
        Self {
            prototype: proto.id,
            density: proto.density,
            level: proto.level,
            capacity: proto.capacity,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct ResidenceChecks {
    good: u8,
    poor: u8,
    abandoned_hours: Option<u8>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Residences {
    pub by_building: BTreeMap<BuildingID, Residence>,
    checks: BTreeMap<BuildingID, ResidenceChecks>,
}

impl Residences {
    pub fn is_abandoned(&self, building: BuildingID) -> bool {
        self.checks
            .get(&building)
            .is_some_and(|checks| checks.abandoned_hours.is_some())
    }
}

pub(crate) fn prototype(density: Density, level: u8) -> Option<&'static ResidencePrototype> {
    ResidencePrototype::iter().find(|proto| proto.density == density && proto.level == level)
}

pub fn on_house_built(sim: &Simulation, building: BuildingID) {
    if let Some(proto) = prototype(Density::Low, 1) {
        register(sim, building, proto);
    }
}

pub(crate) fn register(sim: &Simulation, building: BuildingID, proto: &ResidencePrototype) {
    sim.write::<Residences>()
        .by_building
        .insert(building, proto.into());
}

pub fn on_building_removed(sim: &mut Simulation, building: BuildingID) {
    let mut residences = sim.write::<Residences>();
    residences.by_building.remove(&building);
    residences.checks.remove(&building);
}

pub(crate) fn evict_residents(sim: &mut Simulation, building: BuildingID, door: Vec3) {
    let evictees = {
        let mut infos = sim.write::<BuildingInfos>();
        let Some(info) = infos.get(building) else {
            return;
        };
        let evictees = info.residents.clone();
        for &soul in &evictees {
            infos.remove_resident(soul);
        }
        evictees
    };
    if evictees.is_empty() {
        return;
    }

    let vacancies = {
        let residences = sim.read::<Residences>();
        let map = sim.map();
        let infos = sim.read::<BuildingInfos>();
        let mut vacancies = Vec::new();
        for (&id, residence) in &residences.by_building {
            if id == building || residences.is_abandoned(id) {
                continue;
            }
            let Some(home) = map.get(id).filter(|home| !home.is_orphaned()) else {
                continue;
            };
            let Some(info) = infos.get(id) else {
                continue;
            };
            let free = (residence.capacity as usize).saturating_sub(info.residents.len());
            if free > 0 {
                vacancies.push((OrderedFloat(home.door_pos.distance2(door)), id, free));
            }
        }
        vacancies.sort_unstable_by_key(|&(distance, id, _)| (distance, id));
        vacancies
    };

    let mut destinations = vacancies
        .into_iter()
        .flat_map(|(_, id, free)| std::iter::repeat_n(id, free));
    for soul in evictees {
        let SoulID::Human(id) = soul else { continue };
        if let Some(home) = destinations.next() {
            if let Some(human) = sim.world.humans.get_mut(id) {
                human.home.house = home;
                sim.write::<BuildingInfos>().add_resident(home, soul);
            }
        } else {
            lifecycle::despawn_human(sim, id);
        }
    }
}

pub fn residence_update(sim: &mut Simulation) {
    if sim.get_tick() % TICKS_PER_HOUR != 0 {
        return;
    }

    let mut abandon = Vec::new();
    let mut collapse = Vec::new();
    let mut upgrades = Vec::new();
    {
        let map = sim.map();
        let infos = sim.read::<BuildingInfos>();
        let mut residences = sim.write::<Residences>();
        let stores: Vec<_> = map
            .buildings()
            .values()
            .filter_map(|building| {
                let company = building.kind.as_goods_company()?.prototype();
                (company.kind == CompanyKind::Store).then_some(building.door_pos.xy())
            })
            .collect();
        let Residences {
            by_building,
            checks,
        } = &mut *residences;

        for (&id, residence) in by_building.iter_mut() {
            let Some(building) = map.get(id) else {
                continue;
            };
            let Some(info) = infos.get(id) else { continue };
            let checks = checks.entry(id).or_default();
            if let Some(hours) = &mut checks.abandoned_hours {
                *hours += 1;
                if *hours >= 12 {
                    collapse.push(id);
                }
                continue;
            }

            let occupied = info.residents.len();
            let employed = info.residents.iter().filter(|&&soul| {
                matches!(soul, SoulID::Human(human) if sim.get(human).is_some_and(|h| h.work.is_some()))
            }).count();
            let capacity = residence.capacity as usize;
            let poor = occupied * 5 < capacity && (occupied == 0 || employed * 10 < occupied * 3);
            checks.poor = if poor { checks.poor + 1 } else { 0 };
            if building.is_orphaned() || checks.poor >= 24 {
                checks.abandoned_hours = Some(0);
                abandon.push((id, building.door_pos));
                continue;
            }

            let good = occupied > 0
                && occupied * 5 >= capacity * 4
                && employed * 10 >= occupied * 7
                && stores
                    .iter()
                    .any(|store| store.distance2(building.door_pos.xy()) <= 1_500.0_f32.powi(2));
            checks.good = if good { (checks.good + 1).min(6) } else { 0 };
            if checks.good == 6 {
                if let Some(next) = residence
                    .level
                    .checked_add(1)
                    .and_then(|level| prototype(residence.density, level))
                {
                    *residence = next.into();
                    checks.good = 0;
                    upgrades.push((id, next.level));
                }
            }
        }
    }

    // All newly abandoned residences are marked before looking for vacancies.
    for (building, door) in abandon {
        evict_residents(sim, building, door);
        lifecycle::clear_building_references(sim, building, Some(door));
    }
    for building in collapse {
        super::growth::remove_growable(sim, building);
    }
    if !upgrades.is_empty() {
        let mut map = sim.map_mut();
        for (id, level) in upgrades {
            map.regenerate_house_mesh(id, level);
        }
    }
}
