use goryak::{
    mincolumn, minrow, on_primary_container, on_secondary_container, primary,
    selectable_label_primary, textc, ProgressBar,
};
use prototypes::try_prototype;
use simulation::economy::{
    cargo::{stored_items, CargoBook, TruckLoad, WarehouseActivity, WarehousePolicy},
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
        for item in stored_items(storage) {
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
            let capacity = storage.capacity_units;
            let occupied = book.warehouse_committed(&market, owner, building, storage);
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
            // The watermarks apply to the space all items share.
            textc(
                on_secondary_container(),
                format!(
                    "Shared space · restock below {} · fill to {} units · {}",
                    storage.low_water_units,
                    storage.high_water_units,
                    if book.is_restocking(company) {
                        "restocking"
                    } else {
                        "holding"
                    }
                ),
            );
            if let Some(activity) = book.warehouse_activity(company) {
                textc(
                    on_secondary_container(),
                    match activity {
                        WarehouseActivity::Moving => "Trucks on the road",
                        WarehouseActivity::Queued => "Cargo waits for a driver at the dock",
                        WarehouseActivity::NoWorkers => "Idle · no workers to drive the trucks",
                        WarehouseActivity::NoSupplierInReach => {
                            "Idle · no supplier of its items in reach by road"
                        }
                        WarehouseActivity::WaitingForSupply => {
                            "Restocking · suppliers in reach have nothing to spare yet"
                        }
                        WarehouseActivity::NothingBelowWatermark => {
                            "Idle · stock is above the restock level and nothing nearby is short"
                        }
                    },
                );
            }
            let policy = book.warehouse_policy(company, storage);
            minrow(4.0, || {
                for (choice, label) in [
                    (WarehousePolicy::Balanced, "Balanced"),
                    (WarehousePolicy::ReserveLocally, "Reserve locally"),
                    (WarehousePolicy::ExportSurplus, "Export surplus"),
                ] {
                    if selectable_label_primary(policy == choice, label).clicked
                        && policy != choice
                        && uiw.can_author()
                    {
                        uiw.commands().push(WorldCommand::SetWarehousePolicy {
                            building,
                            policy: choice,
                        });
                    }
                }
            });
            textc(
                on_secondary_container(),
                match policy {
                    WarehousePolicy::Balanced => {
                        "Exports unused items above the restock level and others above the fill level"
                    }
                    WarehousePolicy::ReserveLocally => "Keeps all stock for local recipes",
                    WarehousePolicy::ExportSurplus => {
                        "Exports like Balanced and stocks up for a closer freight station"
                    }
                },
            );
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
