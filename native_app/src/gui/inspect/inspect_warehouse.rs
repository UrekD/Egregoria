use goryak::{mincolumn, on_primary_container, on_secondary_container, textc};
use prototypes::try_prototype;
use simulation::economy::{cargo::CargoBook, Market};
use simulation::map::{BuildingID, BuildingKind};
use simulation::map_dynamic::BuildingInfos;
use simulation::{Simulation, SoulID};

use crate::uiworld::UiWorld;

pub(crate) fn section(_uiw: &UiWorld, sim: &Simulation, building: BuildingID) {
    let map = sim.map();
    let Some(premises) = map.get(building) else {
        return;
    };
    let BuildingKind::GoodsCompany(proto) = premises.kind else {
        return;
    };
    let Some(owner @ SoulID::GoodsCompany(_)) = sim.read::<BuildingInfos>().owner(building) else {
        return;
    };
    let book = sim.read::<CargoBook>();
    let mut stock = book.stock_for(building);
    if let Some(recipe) = &proto.prototype().recipe {
        for item in recipe.consumption.iter().chain(&recipe.production) {
            stock.entry(item.id).or_default();
        }
    }
    if stock.is_empty() {
        return;
    }

    let market = sim.read::<Market>();
    mincolumn(4.0, || {
        textc(on_primary_container(), "Stock and freight");
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
        let loads = book.outgoing_loads(building);
        textc(
            on_primary_container(),
            format!("Outgoing loads ({})", loads.count()),
        );
        for load in book.outgoing_loads(building) {
            let destination = match map.get(load.destination()).map(|building| building.kind) {
                Some(BuildingKind::GoodsCompany(id)) => id.prototype().label.as_str(),
                Some(BuildingKind::RailFreightStation(id)) => id.prototype().label.as_str(),
                _ => "Unknown destination",
            };
            for line in &load.items {
                let name = try_prototype(line.item).map_or("Unknown item", |item| &item.label);
                textc(
                    on_secondary_container(),
                    format!("{name} × {} to {destination}", line.units),
                );
            }
        }
    });
}
