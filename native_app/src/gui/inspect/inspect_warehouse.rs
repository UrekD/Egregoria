use goryak::{
    mincolumn, minrow, on_primary_container, on_secondary_container, primary,
    selectable_label_primary, textc, ProgressBar,
};
use prototypes::try_prototype;
use simulation::economy::{
    cargo::{CargoBook, TruckLoad, WarehousePolicy},
    Market,
};
use simulation::map::{BuildingID, BuildingKind, Map};
use simulation::map_dynamic::BuildingInfos;
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SoulID};
use yakui::Vec2;

use crate::uiworld::UiWorld;

pub(crate) fn section(uiw: &UiWorld, sim: &Simulation, building: BuildingID) {
    let map = sim.map();
    let Some(premises) = map.get(building) else {
        return;
    };
    let BuildingKind::GoodsCompany(proto) = premises.kind else {
        return;
    };
    let Some(owner @ SoulID::GoodsCompany(company)) = sim.read::<BuildingInfos>().owner(building)
    else {
        return;
    };
    let book = sim.read::<CargoBook>();
    let mut stock = book.stock_for(building);
    if let Some(recipe) = &proto.prototype().recipe {
        for item in recipe.consumption.iter().chain(&recipe.production) {
            stock.entry(item.id).or_default();
        }
    }
    let storage = proto.prototype().storage.as_ref();
    if let Some(storage) = storage {
        for &item in &storage.accepted_items {
            stock.entry(item).or_default();
        }
    }
    if stock.is_empty() {
        return;
    }

    let market = sim.read::<Market>();
    mincolumn(4.0, || {
        textc(
            on_primary_container(),
            if storage.is_some() {
                "Warehouse"
            } else {
                "Stock and freight"
            },
        );
        if let Some(storage) = storage {
            let stored: u32 = stock
                .iter()
                .map(|(&item, status)| {
                    market.capital(owner, item).max(0) as u32 + status.incoming + status.reserved
                })
                .sum();
            let returnable: u32 = book
                .outgoing_loads(building)
                .flat_map(|load| &load.items)
                .map(|line| line.units)
                .sum();
            let capacity = storage.capacity_units;
            let occupied = stored + returnable;
            let size = Vec2::new(240.0, 22.0);
            ProgressBar {
                value: (occupied as f32 / capacity as f32).min(1.0),
                size,
                color: primary().adjust(0.7),
            }
            .show_children(|| {
                textc(
                    on_primary_container(),
                    format!("{occupied} / {capacity} units committed"),
                );
            });
            textc(
                on_secondary_container(),
                format!(
                    "Restock below {} · target {} units",
                    storage.low_water_units, storage.high_water_units
                ),
            );
            let policy = book.warehouse_policy(company, storage);
            minrow(4.0, || {
                for (choice, label) in [
                    (WarehousePolicy::Balanced, "Balanced"),
                    (WarehousePolicy::ReserveLocally, "Reserve locally"),
                    (WarehousePolicy::ExportSurplus, "Export surplus"),
                ] {
                    if selectable_label_primary(policy == choice, label).clicked && policy != choice
                    {
                        uiw.commands().push(WorldCommand::SetWarehousePolicy {
                            building,
                            policy: choice,
                        });
                    }
                }
            });
        }
        for (item, stock) in stock {
            let name = try_prototype(item).map_or("Unknown item", |item| &item.label);
            let on_hand = market.capital(owner, item);
            textc(
                on_secondary_container(),
                format!(
                    "{name} · {on_hand} on hand · {} reserved · {} incoming",
                    stock.reserved, stock.incoming
                ),
            );
        }
        if storage.is_some() {
            textc(
                on_primary_container(),
                format!("Incoming loads ({})", book.incoming_loads(building).count()),
            );
            for load in book.incoming_loads(building) {
                show_load(&map, load, "from", load.source());
            }
        }
        textc(
            on_primary_container(),
            format!("Outgoing loads ({})", book.outgoing_loads(building).count()),
        );
        for load in book.outgoing_loads(building) {
            show_load(&map, load, "to", load.destination());
        }
    });
}

fn show_load(map: &Map, load: &TruckLoad, direction: &str, endpoint: BuildingID) {
    let place = match map.get(endpoint).map(|building| building.kind) {
        Some(BuildingKind::GoodsCompany(id)) => id.prototype().label.as_str(),
        Some(BuildingKind::RailFreightStation(id)) => id.prototype().label.as_str(),
        _ => "Unknown building",
    };
    for line in &load.items {
        let item = try_prototype(line.item).map_or("Unknown item", |item| &item.label);
        textc(
            on_secondary_container(),
            format!("{item} × {} {direction} {place}", line.units),
        );
    }
}
