use crate::economy::ZoneDemand;
use crate::map::ZoneKind;
use crate::map_dynamic::{transit::elapsed_ticks, BuildingInfos, VacancyTarget};
use crate::souls::freight_station::freight_station_soul;
use crate::souls::goods_company::company_soul;
use crate::souls::human::spawn_human;
use crate::souls::residence::Residences;
use crate::Simulation;

#[macro_use]
pub mod desire;

pub mod development;
pub mod events;
pub mod freight_station;
pub mod goods_company;
pub mod growth;
pub mod happiness;
pub mod household;
pub mod human;
pub mod lifecycle;
pub mod residence;

/// Adds souls to empty buildings
pub(crate) fn add_souls_to_empty_buildings(sim: &mut Simulation) {
    profiling::scope!("souls::add_souls_to_empty_buildings");
    let now = elapsed_ticks(&sim.resources).0;
    let map = sim.map();
    let mut infos = sim.write::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let immigrating = sim
        .read::<ZoneDemand>()
        .values
        .get(&ZoneKind::Residential)
        .is_some_and(|&demand| demand > 0.0);
    let empty_buildings = infos.vacant_buildings(&map, &residences, immigrating, now);
    drop(infos);
    drop(map);
    drop(residences);

    let mut n_souls_added = 0;

    for target in empty_buildings {
        let spawned = match target {
            VacancyTarget::Business(id, proto) => company_soul(sim, id, proto).is_some(),
            VacancyTarget::Residence(id) => spawn_human(sim, id).is_some(),
            VacancyTarget::Freight(id, proto) => {
                let unowned = sim.read::<BuildingInfos>().owner(id).is_none();
                unowned && freight_station_soul(sim, id, proto).is_some()
            }
        };
        n_souls_added += usize::from(spawned);
    }

    if n_souls_added > 0 {
        log::info!("{} souls added", n_souls_added);
    }
}
