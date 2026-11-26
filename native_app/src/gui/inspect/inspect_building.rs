use goryak::{
    button_secondary, dragvalue, error, fixed_spacer, mincolumn, minrow, on_secondary_container,
    primary, selectable_label_primary, textc, ProgressBar, VertScrollSize, Window,
};
use prototypes::{
    try_prototype, GameDuration, GrowableCompanyID, ItemID, Recipe, ResidencePrototypeID, Tick,
};
use simulation::diagnostics::{
    building_diagnostics, elapsed_tick, BuildingBlocker, ImportStatus, RecipeBlocker,
};
use simulation::economy::cargo::{
    CargoBook, DemandKey, Endpoint, FreightCursor, FreightPage, FreightRecord, FreightStage,
    FreightTrace, ImportBlocker,
};
use simulation::economy::Market;
use simulation::map::{
    Building, BuildingID, BuildingKind, Density, GrowableBusinessState, Zone, MAX_ZONE_AREA,
};
use simulation::map_dynamic::{active_business, BuildingInfos, ElectricityFlow, UtilityStatus};
use simulation::souls::freight_station::FreightTrainState;
use simulation::souls::residence::Residences;
use simulation::transportation::Location;
use simulation::world_command::WorldCommand;
use simulation::{PopulationScale, Simulation, SoulID};
use std::borrow::Cow;
use std::collections::BTreeMap;
use yakui::widgets::{Pad, StateResponse};
use yakui::{use_state, Vec2};

use crate::gui::inspect::{
    building_link, entity_link, inspect_needs, inspect_utility, inspect_warehouse,
};
use crate::gui::item_icon_yakui;
use crate::gui::zoning::zone_name;
use crate::uiworld::UiWorld;

fn label(x: impl Into<Cow<'static, str>>) {
    textc(on_secondary_container(), x);
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FreightHistory {
    Demand(DemandKey),
    Terminal(Endpoint),
    Corridor(BuildingID),
}

type FreightPageSelection = StateResponse<(BuildingID, BTreeMap<FreightHistory, FreightCursor>)>;

/// Inspect a specific building, showing useful information about it
pub fn inspect_building(uiworld: &UiWorld, sim: &Simulation, id: BuildingID) -> bool {
    let map = sim.map();
    let Some(building) = map.buildings().get(id) else {
        return false;
    };
    let Some(diagnostics) = building_diagnostics(sim, id) else {
        return false;
    };
    let scale = sim.population_scale();

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
        VertScrollSize::Fixed(610.0).show(|| {
            mincolumn(5.0, || {
                let freight_page = use_state(move || (id, BTreeMap::new()));
                if freight_page.borrow().0 != id {
                    freight_page.set((id, BTreeMap::new()));
                }
                if cfg!(debug_assertions) {
                    label(format!("{:?}", building.id));
                }

                render_why(sim, &diagnostics.blockers);
                match building.kind {
                    BuildingKind::House => render_house(uiworld, sim, building),
                    BuildingKind::GoodsCompany(id) => {
                        if let Some(site) = sim.growable_summary(building.id) {
                            label(format!(
                                "{} · {} density · Level {} · {} floors",
                                zone_name(site.zone.kind),
                                density_name(site.zone.density),
                                site.level,
                                site.floors,
                            ));
                            label(format!(
                                "Jobs  {} / {}",
                                scale.persons(site.filled_jobs),
                                scale.persons(site.planned_jobs as usize)
                            ));
                            match site.business {
                                Some(GrowableBusinessState::AwaitingSpawn) => {
                                    label("Awaiting business")
                                }
                                Some(GrowableBusinessState::ClosedUntil { .. }) => {
                                    label("Business closed")
                                }
                                Some(GrowableBusinessState::Open) if building.is_mixed_use() => {
                                    label("Business open")
                                }
                                _ => {}
                            }
                            if building.has_residential_use() {
                                render_house(uiworld, sim, building);
                            }
                        } else if let Some(proto) = try_prototype(GrowableCompanyID::from(id))
                            .filter(|proto| proto.growable)
                        {
                            label(format!(
                                "{} · {} density · Level {}",
                                zone_name(proto.zone_kind),
                                density_name(proto.density),
                                proto.level
                            ));
                            label(format!(
                                "Job capacity  {}",
                                scale.persons(proto.n_workers as usize)
                            ));
                        }
                        render_goodscompany(uiworld, sim, building, &freight_page);
                    }
                    BuildingKind::RailFreightStation(_) => {
                        render_freightstation(uiworld, sim, building, &freight_page);
                    }
                    BuildingKind::TrainStation => {}
                    BuildingKind::ExternalTrading => {
                        let trace = sim.read::<FreightTrace>();
                        render_freight_page(
                            uiworld,
                            sim,
                            &trace,
                            &freight_page,
                            FreightHistory::Corridor(building.id),
                            building.id,
                            trace.corridor_report(building.id),
                        );
                    }
                };

                inspect_needs::render(sim, id, &diagnostics);
                inspect_utility::section(sim, id);
                inspect_warehouse::section(uiworld, sim, id);

                if let Some(ref zone) = building.zone {
                    let mut cpy = zone.filldir;
                    minrow(5.0, || {
                        let mut ang = cpy.angle_cossin().to_degrees();

                        if uiworld.can_author() && dragvalue().min(-180.0).max(180.0).show(&mut ang.0) {
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
        });
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
    let binfos = sim.read::<BuildingInfos>();
    let info = binfos.get(b.id);
    let residents: Vec<_> = info
        .into_iter()
        .flat_map(|info| &info.residents)
        .filter_map(|&soul| {
            let SoulID::Human(id) = soul else { return None };
            Some((id, sim.get(id)?))
        })
        .collect();
    let employed = residents
        .iter()
        .filter(|(_, human)| human.work.is_some())
        .count();
    let scale = sim.population_scale();
    let population = scale.persons(residents.len());
    if !b.is_mixed_use() {
        label(format!(
            "Residential · {} density · Level {level}",
            density_name(density)
        ));
    }
    if let Some(capacity) = capacity {
        let capacity = scale.persons(capacity as usize);
        label(format!("Residents  {population} / {capacity}"));
    }
    if !residents.is_empty() {
        label(format!(
            "Employment  {} / {population}",
            scale.persons(employed)
        ));
    }

    if !residents.is_empty() {
        label("Households");
    }
    for &(id, human) in &residents {
        minrow(5.0, || {
            entity_link(uiworld, sim, id);
            if info.is_some_and(|info| info.owner == Some(SoulID::Human(id))) {
                label("Owner");
            }
            if human.location == Location::Building(b.id) {
                label("At home");
            }
        });
    }
    if let Some(info) = info {
        for &soul in &info.inside {
            if let SoulID::Human(id) = soul {
                if !info.residents.contains(&soul)
                    && sim
                        .get(id)
                        .is_some_and(|human| human.location == Location::Building(b.id))
                {
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
        Density::Medium => "Medium",
    }
}

fn render_why(sim: &Simulation, blockers: &[BuildingBlocker]) {
    fixed_spacer((0.0, 5.0));
    label("Why");
    if blockers.is_empty() {
        label("No current blockers");
    }
    for &blocker in blockers {
        let description = match blocker {
            BuildingBlocker::RoadDisconnected => "Disconnected from the road".into(),
            BuildingBlocker::MissingOwner => "No active owner".into(),
            BuildingBlocker::Power(status) => inspect_needs::power_detail(status).into(),
            BuildingBlocker::Water(status) => format!("Water · {}", utility_blocker(status)),
            BuildingBlocker::Sewage(status) => format!("Sewage · {}", utility_blocker(status)),
            BuildingBlocker::NoRetailAccess => "No nearby retailer".into(),
            BuildingBlocker::Abandoned => "Residence abandoned".into(),
            BuildingBlocker::OnFire => "Building on fire".into(),
            BuildingBlocker::NoWorkers => "No workers assigned".into(),
            BuildingBlocker::Recipe(RecipeBlocker::Empty) => {
                "Recipe has no inputs or outputs".into()
            }
            BuildingBlocker::Recipe(RecipeBlocker::MissingInput {
                item,
                stock,
                required,
            }) => {
                format!(
                    "Needs {} · {stock} / {required} in stock",
                    item.prototype().label
                )
            }
            BuildingBlocker::Recipe(RecipeBlocker::OutputFull { item, stock, limit }) => {
                format!(
                    "{} storage full · {stock} / {limit}",
                    item.prototype().label
                )
            }
            BuildingBlocker::Import(status) => import_label(status, elapsed_tick(sim).0),
        };
        textc(error(), description);
    }
}

fn import_label(status: ImportStatus, now: u64) -> String {
    match status {
        ImportStatus::Waiting { since } => {
            let duration = GameDuration(Tick(now.saturating_sub(since)));
            format!("Waiting for imported goods by train and truck · {duration} game time")
        }
        ImportStatus::NoRoute => {
            "No import route · no freight station with trains reaches it by road".into()
        }
        ImportStatus::Held => "New imports are paused at the terminal".into(),
    }
}

fn utility_blocker(status: UtilityStatus) -> &'static str {
    match status {
        UtilityStatus::Served => "Available",
        UtilityStatus::Shortage => "Network shortage",
        UtilityStatus::Disconnected => "Disconnected",
        UtilityStatus::Unknown => "Awaiting network refresh",
    }
}

fn render_freightstation(
    uiworld: &UiWorld,
    sim: &Simulation,
    b: &Building,
    selection: &FreightPageSelection,
) {
    let Some(SoulID::FreightStation(owner)) = sim.read::<BuildingInfos>().owner(b.id) else {
        return;
    };
    let Some(freight) = sim.world().get(owner) else {
        return;
    };

    label(format!(
        "Exports waiting for a train: {}",
        freight.f.waiting_cargo
    ));
    label(format!(
        "Imports on order by rail: {}",
        freight.f.wanted_cargo
    ));
    let stock = sim.read::<CargoBook>().import_stock(b.id);
    if !stock.is_empty() {
        let lines: Vec<_> = stock
            .iter()
            .map(|(item, units)| format!("{units} {}", item.prototype().label))
            .collect();
        label(format!("Imports for delivery: {}", lines.join(", ")));
    }

    let terminal = Endpoint {
        soul: SoulID::FreightStation(owner),
        building: b.id,
    };
    let trace = sim.read::<FreightTrace>();
    let setting = trace.terminal_policy(terminal);
    let runs = sim.read::<CargoBook>().station_truck_runs(terminal);
    label(format!(
        "{runs} active truck runs · {} active train calls",
        freight.f.trains.len()
    ));
    let mut policy = setting.policy;
    if uiworld.can_author() {
        minrow(6.0, || {
            if selectable_label_primary(policy.admit_imports, "Accept imports").clicked {
                policy.admit_imports = !policy.admit_imports;
            }
            if selectable_label_primary(policy.admit_exports, "Accept exports").clicked {
                policy.admit_exports = !policy.admit_exports;
            }
        });
        minrow(6.0, || {
            label("Concurrent calls");
            for limit in 0..=2 {
                if selectable_label_primary(policy.max_calls == limit, &limit.to_string()).clicked {
                    policy.max_calls = limit;
                }
            }
        });
        minrow(6.0, || {
            label("Truck runs");
            for limit in 0..=6 {
                if selectable_label_primary(policy.max_truck_runs == limit, &limit.to_string())
                    .clicked
                {
                    policy.max_truck_runs = limit;
                }
            }
        });
        if policy != setting.policy {
            uiworld
                .commands()
                .push(WorldCommand::SetFreightTerminalPolicy {
                    terminal,
                    expected_policy_revision: setting.revision,
                    policy,
                });
        }
    } else {
        label(format!(
            "Up to {} concurrent calls and {} truck runs",
            policy.max_calls, policy.max_truck_runs
        ));
    }
    if !policy.admit_imports
        || !policy.admit_exports
        || policy.max_calls == 0
        || policy.max_truck_runs == 0
    {
        label("Accepted cargo remains queued when a limit is reached.");
    }
    let recent = trace.terminal_view(terminal);
    if !recent.rows.is_empty() {
        label("Recent freight");
    }
    render_freight_page(
        uiworld,
        sim,
        &trace,
        selection,
        FreightHistory::Terminal(terminal),
        b.id,
        recent,
    );

    fixed_spacer((0.0, 10.0));
    label("Trains:");
    for (tid, state) in &freight.f.trains {
        minrow(5.0, || {
            entity_link(uiworld, sim, *tid);
            match state {
                FreightTrainState::Arriving => {
                    label(if trace.train_dispatch_held(*tid) {
                        "Dispatch held"
                    } else {
                        "Arriving"
                    });
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

fn render_freight_page(
    uiworld: &UiWorld,
    sim: &Simulation,
    trace: &FreightTrace,
    selection: &FreightPageSelection,
    key: FreightHistory,
    site: BuildingID,
    first: FreightPage,
) {
    let older = selection.borrow().1.get(&key).copied();
    let page = if let Some(cursor) = older {
        let Ok(page) = trace.continue_trace(cursor) else {
            label("This freight page is no longer available.");
            if button_secondary("Latest").show().clicked {
                selection.borrow_mut().1.remove(&key);
            }
            return;
        };
        page
    } else {
        first
    };
    if page.rows.is_empty() {
        return;
    }
    for record in &page.rows {
        render_freight_record(uiworld, sim, record, site);
    }
    minrow(5.0, || {
        if older.is_some() && button_secondary("Latest").show().clicked {
            selection.borrow_mut().1.remove(&key);
        }
        if let Some(cursor) = page.next {
            if button_secondary("Older freight").show().clicked {
                selection.borrow_mut().1.insert(key, cursor);
            }
        }
    });
}

fn render_freight_record(
    uiworld: &UiWorld,
    sim: &Simulation,
    record: &FreightRecord,
    site: BuildingID,
) {
    let item = record
        .fragment
        .item
        .and_then(try_prototype)
        .map_or("Legacy cargo", |item| item.label.as_str());
    let stage = match record.stage {
        FreightStage::Reserved => "Reserved",
        FreightStage::InboundRail => "Inbound by rail",
        FreightStage::TerminalStock => "Received by terminal",
        FreightStage::OutboundTruck => "Loaded onto truck",
        FreightStage::Delivered => "Delivered",
        FreightStage::Returned => "Returned",
        FreightStage::ExportStaged => "Ready for export",
        FreightStage::ExportLoaded => "Loaded onto train",
        FreightStage::OutboundRail => "Outbound by rail",
        FreightStage::Exited => "Exported by rail",
        FreightStage::WrittenOff => "Written off",
    };
    label(format!("{} {item} · {stage}", record.fragment.units));
    minrow(5.0, || {
        if record.terminal.building != site {
            building_link(uiworld, sim, record.terminal.building);
        }
        if let Some(call) = record.fragment.call {
            if call.outside_gate != site {
                building_link(uiworld, sim, call.outside_gate);
            }
            entity_link(uiworld, sim, call.train);
        }
        if let Some(truck) = record.truck {
            entity_link(uiworld, sim, truck);
        }
        if let Some(receiver) = record.receiver.filter(|receiver| receiver.building != site) {
            building_link(uiworld, sim, receiver.building);
        }
    });
}

fn render_goodscompany(
    uiworld: &UiWorld,
    sim: &Simulation,
    b: &Building,
    selection: &FreightPageSelection,
) {
    let Some(business) = active_business(b, &sim.read::<BuildingInfos>(), sim.world()) else {
        return;
    };
    let c_id = business.company;
    let c = &sim.world().companies[c_id];
    let goods = &c.comp;
    let workers = &c.workers;
    let proto = c.comp.proto.prototype();

    let market = &*sim.read::<Market>();
    let map = &*sim.map();
    let elec_flow = &*sim.read::<ElectricityFlow>();

    let max_workers = goods.max_workers;
    let scale = sim.population_scale();
    ProgressBar {
        value: workers.0.len() as f32 / max_workers.max(1) as f32,
        size: Vec2::new(200.0, 25.0),
        color: primary().adjust(0.7),
    }
    .show_children(|| {
        label(format!(
            "Workers  {} / {}",
            scale.persons(workers.0.len()),
            scale.persons(max_workers as usize)
        ));
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
        let trace = sim.read::<FreightTrace>();
        let cargo = sim.read::<CargoBook>();
        for input in &r.consumption {
            let key = DemandKey {
                owner: c_id,
                building: b.id,
                item: input.id,
            };
            let Some(demand) = cargo.freight_demand(sim, key) else {
                continue;
            };
            let page = trace.trace_demand(key);
            let history = FreightHistory::Demand(key);
            if !selection.borrow().1.contains_key(&history)
                && demand.on_hand >= demand.batch_need
                && demand.ordered == 0
                && demand.on_trucks == 0
                && page.rows.is_empty()
            {
                continue;
            }
            label(format!(
                "{} · {} on hand · {} needed per batch · restock target {}",
                input.id.prototype().label,
                demand.on_hand,
                demand.batch_need,
                demand.target,
            ));
            label(format!(
                "{} on order · {} on trucks · {} at serving terminals",
                demand.ordered, demand.on_trucks, demand.terminal_stock,
            ));
            if let Some(blocker) = demand.blocker {
                let (message, terminal) = match blocker {
                    ImportBlocker::NoRoute => ("No import route", None),
                    ImportBlocker::AdmissionHeld(terminal) => {
                        ("New imports paused", Some(terminal))
                    }
                    ImportBlocker::RailDispatchHeld(terminal) => {
                        ("Train dispatch held", Some(terminal))
                    }
                    ImportBlocker::TruckDispatchHeld(terminal) => {
                        ("Truck dispatch held", Some(terminal))
                    }
                };
                minrow(5.0, || {
                    label(message);
                    if let Some(terminal) = terminal {
                        building_link(uiworld, sim, terminal.building);
                    }
                });
            }
            render_freight_page(uiworld, sim, &trace, selection, history, b.id, page);
        }
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
    render_company_storage(uiworld, market, c_id.into(), scale);
}

fn render_company_storage(
    uiworld: &UiWorld,
    market: &Market,
    company: SoulID,
    scale: PopulationScale,
) {
    let jobs = ItemID::new("job-opening");
    for (&id, item_market) in market.iter() {
        if id == jobs {
            if let Some(order) = item_market
                .sell_order(company)
                .filter(|order| order.qty > 0)
            {
                label(format!("Vacant jobs {}", scale.persons(order.qty as usize)));
            }
            continue;
        }
        if let Some(quantity) = item_market.capital(company) {
            item_icon_yakui(uiworld, id, quantity);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use prototypes::{TICKS_PER_HOUR, TICKS_PER_MINUTE};
    use yakui::widgets::TextWidget;

    #[test]
    fn population_scale_company_storage_displays_job_vacancies_as_people() {
        fn collect_labels(ui: &yakui::Yakui, id: yakui::WidgetId, labels: &mut Vec<String>) {
            let node = ui.dom().get(id).unwrap();
            if let Some(text) = node.widget.downcast_ref::<TextWidget>() {
                labels.push(format!("{text:?}"));
            }
            for &child in &node.children {
                collect_labels(ui, child, labels);
            }
        }

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new(false);
        let company = SoulID::GoodsCompany(Default::default());
        let jobs = ItemID::new("job-opening");
        let groceries = ItemID::new("groceries");
        let mut market = sim.write::<Market>();
        market.produce(company, jobs, 7);
        market.sell(company, geom::Vec2::ZERO, jobs, 7, 0);
        market.produce(company, groceries, 3);
        let mut uiworld = UiWorld::with_defaults();
        uiworld.insert(crate::gui::textures::UiTextures::default());
        let mut ui = yakui::Yakui::new();
        ui.set_surface_size(Vec2::splat(500.0));
        ui.start();
        render_company_storage(
            &uiworld,
            &market,
            company,
            PopulationScale::try_new(8).unwrap(),
        );
        ui.finish();
        let mut labels = Vec::new();
        collect_labels(&ui, ui.dom().root(), &mut labels);
        assert!(
            labels
                .iter()
                .any(|text| text.contains("\"Vacant jobs 56\"")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|text| text.contains("\"x3\"")),
            "{labels:?}"
        );
        assert!(
            !labels.iter().any(|text| text.contains("\"x7\"")),
            "{labels:?}"
        );
    }

    #[test]
    fn qw1_import_wait_uses_elapsed_game_time() {
        let since = 500;
        let status = ImportStatus::Waiting { since };
        assert_eq!(
            import_label(status, since + TICKS_PER_HOUR + 30 * TICKS_PER_MINUTE),
            "Waiting for imported goods by train and truck · 1.50h game time"
        );
        assert_eq!(
            import_label(status, since - 1),
            "Waiting for imported goods by train and truck · 0s game time"
        );
        assert_eq!(
            import_label(ImportStatus::NoRoute, since),
            "No import route · no freight station with trains reaches it by road"
        );
    }
}
