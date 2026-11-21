use crate::economy::ZoneDemand;
use crate::map::{BuildingKind, ZoneKind};
use crate::map_dynamic::BuildingInfos;
use crate::souls::freight_station::freight_station_soul;
use crate::souls::goods_company::company_soul;
use crate::souls::human::spawn_human;
use crate::souls::residence::Residences;
use crate::Simulation;

#[macro_use]
pub mod desire;

pub mod events;
pub mod freight_station;
pub mod goods_company;
pub mod growth;
pub mod happiness;
pub mod human;
pub mod lifecycle;
pub mod residence;

/// Adds souls to empty buildings
pub(crate) fn add_souls_to_empty_buildings(sim: &mut Simulation) {
    profiling::scope!("souls::add_souls_to_empty_buildings");
    let map = sim.map();
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let immigrating = sim
        .read::<ZoneDemand>()
        .values
        .get(&ZoneKind::Residential)
        .is_some_and(|&demand| demand > 0.0);
    let mut empty_buildings = Vec::with_capacity(16);

    for (id, building) in map.buildings() {
        let info = unwrap_cont!(infos.get(id));
        let available = match building.kind {
            BuildingKind::House => {
                immigrating
                    && !building.is_orphaned()
                    && !residences.is_abandoned(id)
                    && residences
                        .by_building
                        .get(&id)
                        .is_some_and(|home| info.residents.len() < home.capacity as usize)
            }
            _ => info.owner.is_none(),
        };
        if available {
            empty_buildings.push((building.kind, id));
        }
    }
    drop(infos);
    drop(map);
    drop(residences);
    empty_buildings.sort_unstable_by_key(|&(_, id)| id);

    let mut n_souls_added = 0;

    for (bkind, build_id) in empty_buildings {
        match bkind {
            BuildingKind::House => {
                n_souls_added += usize::from(spawn_human(sim, build_id).is_some());
            }
            BuildingKind::GoodsCompany(id) => {
                company_soul(sim, build_id, id);

                n_souls_added += 1;
            }
            BuildingKind::RailFreightStation(id) => {
                freight_station_soul(sim, build_id, id);
                n_souls_added += 1;
            }
            _ => {}
        }
    }

    if n_souls_added > 0 {
        log::info!("{} souls added", n_souls_added);
    }
}
