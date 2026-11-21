use goryak::{
    dragvalue, fixed_spacer, minrow, on_secondary_container, primary, textc, ProgressBar, Window,
};
use prototypes::{
    try_prototype, CompanyKind, GrowableCompanyID, ItemID, Recipe, ResidencePrototypeID,
};
use simulation::economy::Market;
use simulation::map::{
    Building, BuildingID, BuildingKind, Density, ProjectFilter, Zone, MAX_ZONE_AREA,
};
use simulation::map_dynamic::{BuildingInfos, ElectricityFlow};
use simulation::souls::freight_station::FreightTrainState;
use simulation::souls::residence::Residences;
use simulation::world_command::WorldCommand;
use simulation::{Simulation, SoulID};
use std::borrow::Cow;
use yakui::widgets::Pad;
use yakui::Vec2;

use crate::gui::inspect::{entity_link, inspect_needs};
use crate::gui::item_icon_yakui;
use crate::gui::zoning::zone_name;
use crate::uiworld::UiWorld;

fn label(x: impl Into<Cow<'static, str>>) {
    textc(on_secondary_container(), x);
}

/// Inspect a specific building, showing useful information about it
pub fn inspect_building(uiworld: &UiWorld, sim: &Simulation, id: BuildingID) -> bool {
    let map = sim.map();
    let Some(building) = map.buildings().get(id) else {
        return false;
    };

    let title: &str = match building.kind {
        BuildingKind::House => "House",
        BuildingKind::GoodsCompany(id) => &id.prototype().label,
        BuildingKind::RailFreightStation(id) => &id.prototype().name,
        BuildingKind::TrainStation => "Train Station",
        BuildingKind::ExternalTrading => "External Trading",
    };

    let mut is_open = true;
    Window {
        title: title.into(),
        pad: Pad::all(10.0),
        radius: 10.0,
        opened: &mut is_open,
        child_spacing: 5.0,
    }
    .show(|| {
        if cfg!(debug_assertions) {
            label(format!("{:?}", building.id));
        }

        match building.kind {
            BuildingKind::House => render_house(uiworld, sim, building),
            BuildingKind::GoodsCompany(id) => {
                if let Some(proto) =
                    try_prototype(GrowableCompanyID::from(id)).filter(|proto| proto.growable)
                {
                    label(format!(
                        "{} · {} density · Level {}",
                        zone_name(proto.zone_kind),
                        density_name(proto.density),
                        proto.level
                    ));
                    label(format!("Job capacity  {}", proto.n_workers));
                    if proto.power_consumption.is_some() {
                        need("Power", has_power(sim, building.id));
                    }
                }
                render_goodscompany(uiworld, sim, building);
            }
            BuildingKind::RailFreightStation(_) => {
                render_freightstation(uiworld, sim, building);
            }
            BuildingKind::TrainStation => {}
            BuildingKind::ExternalTrading => {}
        };

        inspect_needs::render(sim, id);

        if let Some(ref zone) = building.zone {
            let mut cpy = zone.filldir;
            minrow(5.0, || {
                let mut ang = cpy.angle_cossin().to_degrees();

                if dragvalue().min(-180.0).max(180.0).show(&mut ang.0) {
                    cpy = ang.to_radians().vec2();
                    uiworld.commands().push(WorldCommand::UpdateZone {
                        building: id,
                        zone: Zone {
                            filldir: cpy,
                            ..zone.clone()
                        },
                    })
                }

                label("Fill angle");
            });

            ProgressBar {
                value: zone.area / MAX_ZONE_AREA,
                size: Vec2::new(200.0, 25.0),
                color: primary().adjust(0.7),
            }
            .show_children(|| {
                label(format!("area: {}/{}", zone.area, MAX_ZONE_AREA));
            });
        }
    });

    is_open
}

fn render_house(uiworld: &UiWorld, sim: &Simulation, b: &Building) {
    let residences = sim.read::<Residences>();
    let residence = residences.by_building.get(&b.id);
    let prototype = residence.map_or_else(
        || ResidencePrototypeID::new("residence-low-l1"),
        |residence| residence.prototype,
    );
    let capacity = residence
        .map(|home| home.capacity)
        .or_else(|| try_prototype(prototype).map(|p| p.capacity));
    let density = residence.map_or(Density::Low, |home| home.density);
    let level = residence.map_or(1, |home| home.level);
    let residents: Vec<_> = sim
        .world()
        .humans
        .iter()
        .filter(|(_, human)| human.home.house == b.id)
        .collect();
    let employed = residents
        .iter()
        .filter(|(_, human)| human.work.is_some())
        .count();
    label(format!(
        "Residential · {} density · Level {level}",
        density_name(density)
    ));
    if let Some(capacity) = capacity {
        label(format!("Residents  {} / {capacity}", residents.len()));
    }
    if !residents.is_empty() {
        label(format!("Employment  {employed} / {}", residents.len()));
    }

    let map = sim.map();
    let retail = map.spatial_map().query_around(b.obb.center(), 1500.0, ProjectFilter::BUILDING).any(|project| {
        let Some(store) = project.as_building().and_then(|id| map.get(id)) else { return false };
        matches!(store.kind, BuildingKind::GoodsCompany(id) if !id.is_service() && id.prototype().kind == CompanyKind::Store)
            && store.obb.center().distance2(b.obb.center()) <= 1500.0 * 1500.0
    });
    need("Store within 1.5 km", retail);
    if try_prototype(prototype).is_some_and(|p| p.power_consumption.is_some()) {
        need("Power", has_power(sim, b.id));
    }

    let binfos = sim.read::<BuildingInfos>();
    let info = binfos.get(b.id);
    if !residents.is_empty() {
        label("Households");
    }
    for &(id, _) in &residents {
        minrow(5.0, || {
            entity_link(uiworld, sim, id);
            if info.is_some_and(|info| info.owner == Some(SoulID::Human(id))) {
                label("Owner");
            }
            if info.is_some_and(|info| info.inside.contains(&SoulID::Human(id))) {
                label("At home");
            }
        });
    }
    if let Some(info) = info {
        for &soul in &info.inside {
            if let SoulID::Human(id) = soul {
                if residents.iter().all(|&(resident, _)| resident != id) {
                    minrow(5.0, || {
                        entity_link(uiworld, sim, id);
                        label("Visiting");
                    });
                }
            }
        }
    }
}

fn density_name(density: Density) -> &'static str {
    match density {
        Density::Low => "Low",
        Density::High => "High",
    }
}

fn need(name: &str, met: bool) {
    let status = if met { "Met" } else { "Unmet" };
    let color = if met {
        on_secondary_container()
    } else {
        goryak::error()
    };
    textc(color, format!("{name}  {status}"));
}

fn has_power(sim: &Simulation, id: BuildingID) -> bool {
    sim.map()
        .electricity
        .net_id(id)
        .is_some_and(|net| !sim.read::<ElectricityFlow>().blackout(net))
}

fn render_freightstation(uiworld: &UiWorld, sim: &Simulation, b: &Building) {
    let Some(SoulID::FreightStation(owner)) = sim.read::<BuildingInfos>().owner(b.id) else {
        return;
    };
    let Some(freight) = sim.world().get(owner) else {
        return;
    };

    label(format!("Waiting cargo: {}", freight.f.waiting_cargo));
    label(format!("Wanted cargo: {}", freight.f.wanted_cargo));

    fixed_spacer((0.0, 10.0));
    label("Trains:");
    for (tid, state) in &freight.f.trains {
        minrow(5.0, || {
            entity_link(uiworld, sim, *tid);
            match state {
                FreightTrainState::Arriving => {
                    label("Arriving");
                }
                FreightTrainState::Loading => {
                    label("Loading");
                }
                FreightTrainState::Moving => {
                    label("Moving");
                }
            }
        });
    }
}

fn render_goodscompany(uiworld: &UiWorld, sim: &Simulation, b: &Building) {
    let owner = sim.read::<BuildingInfos>().owner(b.id);

    let Some(SoulID::GoodsCompany(c_id)) = owner else {
        return;
    };
    let Some(c) = sim.world().companies.get(c_id) else {
        return;
    };
    let goods = &c.comp;
    let workers = &c.workers;
    let proto = c.comp.proto.prototype();

    let market = &*sim.read::<Market>();
    let map = &*sim.map();
    let elec_flow = &*sim.read::<ElectricityFlow>();

    let max_workers = goods.max_workers;
    ProgressBar {
        value: workers.0.len() as f32 / max_workers as f32,
        size: Vec2::new(200.0, 25.0),
        color: primary().adjust(0.7),
    }
    .show_children(|| {
        label(format!("workers: {}/{}", workers.0.len(), max_workers));
    });

    if let Some(driver) = goods.driver {
        minrow(5.0, || {
            label("Driver is");
            entity_link(uiworld, sim, driver);
        });
    }
    let productivity = c.productivity(proto, b.zone.as_ref(), map, elec_flow);
    if productivity < 1.0 {
        ProgressBar {
            value: productivity,
            size: Vec2::new(200.0, 25.0),
            color: primary().adjust(0.7),
        }
        .show_children(|| {
            label(format!(
                "productivity: {:.0}%",
                (productivity * 100.0).round()
            ));
        });
    }

    if let Some(ref r) = proto.recipe {
        render_recipe(uiworld, r);
    }

    if let Some(net_id) = map.electricity.net_id(b.id) {
        let blackout = elec_flow.blackout(net_id);

        if let Some(power_c) = proto.power_consumption {
            ProgressBar {
                value: productivity,
                size: Vec2::new(200.0, 25.0),
                color: primary().adjust(0.7),
            }
            .show_children(|| {
                label(format!(
                    "power: {}/{}",
                    productivity as f64 * power_c,
                    power_c
                ));
            });
        }

        if let Some(power_prod) = proto.power_production {
            label(format!(
                "producing power: {}",
                power_prod * productivity as f64
            ));

            let stats = elec_flow.network_stats(net_id);

            ProgressBar {
                value: if blackout { 0.0 } else { 1.0 },
                size: Vec2::new(200.0, 25.0),
                color: primary().adjust(0.7),
            }
            .show_children(|| {
                label(format!(
                    "Network health: {}/{}={:.0}%",
                    stats.produced_power,
                    stats.consumed_power,
                    (100 * stats.produced_power.0) / stats.consumed_power.0.max(1)
                ));
            });
        }
    }

    ProgressBar {
        value: goods.progress,
        size: Vec2::new(200.0, 25.0),
        color: primary().adjust(0.7),
    }
    .show_children(|| {
        label(format!("{:.0}%", goods.progress * 100.0));
    });

    fixed_spacer((0.0, 10.0));
    label("Storage");

    let jobopening = ItemID::new("job-opening");
    for (&id, m) in market.iter() {
        let Some(v) = m.capital(c_id.into()) else {
            continue;
        };
        if id == jobopening && v == 0 {
            continue;
        }

        item_icon_yakui(uiworld, id, v);
    }
}

fn render_recipe(uiworld: &UiWorld, recipe: &Recipe) {
    if recipe.consumption.is_empty() {
        label("No Inputs");
    } else {
        label(if recipe.consumption.len() == 1 {
            "Input"
        } else {
            "Inputs"
        });
        minrow(5.0, || {
            for item in recipe.consumption.iter() {
                item_icon_yakui(uiworld, item.id, item.amount);
            }
        });
    }

    if recipe.production.is_empty() {
        label("No Outputs");
    } else {
        label(if recipe.production.len() == 1 {
            "Output"
        } else {
            "Outputs"
        });
        minrow(5.0, || {
            for item in recipe.production.iter() {
                item_icon_yakui(uiworld, item.id, item.amount);
            }
        });
    }
}
