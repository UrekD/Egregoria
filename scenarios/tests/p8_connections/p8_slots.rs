use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use prototypes::{try_prototype, ItemID, WarehousePolicy};
use serde::Serialize;
use serde_json::{json, Value};
use simulation::economy::cargo::{CargoBook, ImportStatus};
use simulation::economy::{
    Budget, BudgetCategory, Market, TransitStats, TripStats, TICKS_PER_BUDGET_DAY,
};
use simulation::map::{BuildingID, BuildingKind, LaneKind};
use simulation::map_dynamic::transit::elapsed_ticks;
use simulation::map_dynamic::Transit;
use simulation::souls::freight_station::train_stop;
use simulation::transportation::train::RailWagonKind;
use simulation::transportation::Location;
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand};
use simulation::{Simulation, SlotId, SoulID};

const WINDOW: u64 = 1_800;
const SLOTS: [&str; 2] = ["showcase_city", "lesce_osm"];

fn input_root() -> PathBuf {
    let root = PathBuf::from(std::env::var_os("P8_W0_SLOT_ROOT").expect("set P8_W0_SLOT_ROOT"));
    assert!(root.is_absolute(), "slot root must be absolute");
    root
}

fn load(root: &Path, slot: &str) -> Simulation {
    // Initializes the registry before validating the snapshot.
    scenarios::ensure_repo_root_cwd();
    simulation::init::init();
    Simulation::load_from_slot(root, &SlotId::new(slot).unwrap()).unwrap()
}

#[derive(Clone, Copy, Serialize)]
struct Receiver {
    building: BuildingID,
    soul: SoulID,
    item: ItemID,
    item_name: &'static str,
    company_prototype: &'static str,
    recipe_units: i32,
}

fn receiver(sim: &Simulation) -> Option<Receiver> {
    let cargo = sim.read::<CargoBook>();
    let market = sim.read::<Market>();
    sim.world()
        .companies
        .iter()
        .filter_map(|(id, company)| {
            let prototype = try_prototype(company.comp.proto)?;
            let recipe = prototype.recipe.as_ref()?;
            let soul = SoulID::GoodsCompany(id);
            let item = recipe.consumption.iter().min_by_key(|item| {
                let stock = market.capital(soul, item.id);
                (stock >= item.amount, stock != 0, item.id)
            })?;
            let waiting = matches!(
                cargo.import_status(soul),
                Some(ImportStatus::Waiting { .. })
            );
            let stock = market.capital(soul, item.id);
            Some((
                (
                    stock >= item.amount,
                    !waiting,
                    stock != 0,
                    company.comp.building,
                ),
                Receiver {
                    building: company.comp.building,
                    soul,
                    item: item.id,
                    item_name: &try_prototype(item.id)?.base.name,
                    company_prototype: &prototype.base.base.name,
                    recipe_units: item.amount,
                },
            ))
        })
        .min_by_key(|(order, _)| *order)
        .map(|(_, receiver)| receiver)
}

#[derive(Serialize)]
struct Trace {
    schema: u32,
    slot: &'static str,
    arm: &'static str,
    start_elapsed: u64,
    end_elapsed: u64,
    fixture: Value,
    fork_hashes: BTreeMap<String, u64>,
    receiver: Option<Receiver>,
    control_receipt: Option<Value>,
    events: Vec<Value>,
    counters: Value,
    final_hashes: BTreeMap<String, u64>,
}

fn elapsed(sim: &mut Simulation) -> u64 {
    elapsed_ticks(sim.unrestricted_world_res().unwrap().1).0
}

fn fixture(sim: &Simulation) -> Value {
    let transit = sim.read::<Transit>();
    let map = sim.map();
    let world = sim.world();
    json!({
        "simulation_tick": sim.get_tick(),
        "seed": sim.read::<simulation::SimulationOptions>().seed,
        "persons_per_agent": sim.population_scale().persons_per_agent(),
        "humans": world.humans.len(),
        "employed": world.humans.values().filter(|human| human.work.is_some()).count(),
        "companies": world.companies.len(),
        "roads": map.roads().len(),
        "rail_lanes": map.lanes().values().filter(|lane| lane.kind == LaneKind::Rail).count(),
        "bus_lines": transit.lines.len(),
        "bus_stops": transit.stops.len(),
        "target_buses": transit.lines.values().map(|line| u64::from(line.target_vehicles)).sum::<u64>(),
        "active_buses": transit.runs.len(),
        "trains": world.trains.len(),
        "passenger_wagons": world.wagons.values().filter(|wagon| matches!(wagon.wagon.kind, RailWagonKind::Passenger)).count(),
        "passenger_station_markers": map.buildings().values().filter(|building| building.kind == BuildingKind::TrainStation).count(),
        "freight_terminals": world.freight_stations.len(),
        "external_markets": map.buildings().values().filter(|building| building.kind == BuildingKind::ExternalTrading).count(),
        "warehouses": world.companies.values().filter(|c| try_prototype(c.comp.proto).is_some_and(|p| p.storage.is_some())).count(),
    })
}

fn diagnostic_trace(root: &Path, slot: &'static str, try_policy: bool) -> Trace {
    let mut sim = load(root, slot);
    let start = elapsed(&mut sim);
    let mut trace = Trace {
        schema: 1,
        slot,
        arm: if try_policy {
            "terminal-policy"
        } else {
            "control"
        },
        start_elapsed: start,
        end_elapsed: start + WINDOW,
        fixture: fixture(&sim),
        fork_hashes: sim.hashes(),
        receiver: receiver(&sim),
        control_receipt: None,
        events: Vec::new(),
        counters: Value::Null,
        final_hashes: BTreeMap::new(),
    };
    if try_policy {
        let terminal = sim
            .world()
            .freight_stations
            .values()
            .next()
            .map(|terminal| terminal.f.building);
        trace.control_receipt = terminal.map(|building| {
            // Probe the existing UI command at a rail terminal. Wave 2 will
            // switch this adapter to the actual terminal release command.
            sim.apply_commands([&WorldCommand::SetWarehousePolicy {
                building,
                policy: WarehousePolicy::ReserveLocally,
            }]);
            let result = sim.read::<CommandOutcomes>();
            assert!(result.accepted.is_empty());
            assert_eq!(result.last, [("SetWarehousePolicy", Rejection::InvalidWarehouse)]);
            json!({"building": building, "command": "SetWarehousePolicy", "accepted": result.accepted, "rejected": format!("{:?}", result.last)})
        });
    }
    let trip_count = |sim: &Simulation| {
        let stats = sim.read::<TripStats>();
        [
            stats.walk.completed,
            stats.car.completed,
            stats.transit.completed,
            stats.give_ups,
        ]
    };
    let fiscal = |sim: &Simulation| {
        let budget = sim.read::<Budget>();
        [
            BudgetCategory::TradeImport,
            BudgetCategory::TradeExport,
            BudgetCategory::TransitFares,
        ]
        .map(|category| budget.total(category))
    };
    let initial_trips = trip_count(&sim);
    let initial_fiscal = fiscal(&sim);
    let initial_boards: u64 = sim.read::<TransitStats>().boardings.values().sum();
    let mut schedule = Simulation::schedule();
    let mut rail_frames = BTreeMap::new();
    let mut shipments = BTreeSet::new();
    let mut boarded_lines = BTreeMap::new();
    let mut previous_buses = BTreeMap::new();
    let mut previous_receiver = None;
    let mut stockout_ticks = 0;
    let mut rail_transitions = 0;
    let mut bus_transfers = 0;
    let mut shift_progress = 0;

    for now in start..=trace.end_elapsed {
        assert_eq!(elapsed(&mut sim), now);
        let world = sim.world();
        let transit = sim.read::<Transit>();
        let cargo = sim.read::<CargoBook>();
        let buses: BTreeMap<_, _> = world
            .humans
            .iter()
            .filter_map(|(id, human)| match human.location {
                Location::Transit(vehicle) => Some((id, vehicle)),
                _ => None,
            })
            .collect();
        for (&human, &vehicle) in &buses {
            if previous_buses.get(&human) != Some(&vehicle) {
                let Some(run) = transit.runs.get(&vehicle) else {
                    continue;
                };
                let target = world.humans[human].router.target_dest;
                let prior = boarded_lines.insert(human, (run.line, target));
                // Stock lacks a stable intent/occurrence key. Retain this as
                // a diagnostic observation, independent of verified transfers.
                bus_transfers += u64::from(prior.is_some_and(|(line, door)| {
                    line != run.line && door.is_some() && door == target
                }));
                trace.events.push(json!({"tick": now, "kind": "bus_aboard", "person": human, "vehicle": vehicle, "line": run.line, "target": target, "initial": now == start}));
            }
        }
        for &human in previous_buses.keys() {
            if !buses.contains_key(&human) {
                trace
                    .events
                    .push(json!({"tick": now, "kind": "bus_exit", "person": human}));
            }
        }
        previous_buses = buses;

        for (id, terminal) in world.freight_stations.iter() {
            let terminal_pos = train_stop(&terminal.trans);
            let status = &terminal.f;
            let assignments: Vec<_> = status
                .trains
                .iter()
                .map(|&(train, stage)| (train, format!("{stage:?}")))
                .collect();
            if rail_frames.get(&id)
                != Some(&(
                    status.waiting_cargo,
                    status.wanted_cargo,
                    assignments.clone(),
                ))
            {
                rail_transitions += u64::from(now > start);
                trace.events.push(json!({
                    "tick": now, "kind": "terminal_state", "terminal": id, "berth": terminal_pos,
                    "waiting": status.waiting_cargo, "wanted": status.wanted_cargo,
                    "calls": assignments.iter().map(|(id, stage)| {
                        let train = &world.trains[*id];
                        json!({
                            "train": id, "stage": stage, "position": train.trans.pos,
                            "direction": train.trans.dir, "speed": train.speed.0,
                            "traversal": format!("{:?}", train.it.get_travers()),
                            "distance_to_berth_m": train.trans.pos.distance(terminal_pos)
                        })
                    }).collect::<Vec<_>>(),
                    "import_pool": cargo.import_stock(status.building).into_iter().collect::<Vec<_>>(),
                }));
                rail_frames.insert(id, (status.waiting_cargo, status.wanted_cargo, assignments));
            }
        }

        // Company road manifests and terminal receipts are independent stock
        // views. Neither supplies train-to-truck lineage.
        for load in world
            .vehicles
            .keys()
            .filter_map(|id| cargo.manifest_for(id))
        {
            if shipments.insert((
                load.vehicle,
                load.source(),
                load.destination(),
                load.items
                    .iter()
                    .map(|line| (line.item, line.units))
                    .collect::<Vec<_>>(),
            )) {
                trace.events.push(json!({"tick": now, "kind": "road_manifest", "truck": load.vehicle, "from": load.source(), "to": load.destination(), "items": load.items}));
            }
        }

        if let Some(receiver) = trace.receiver {
            let stock = sim.read::<Market>().capital(receiver.soul, receiver.item);
            stockout_ticks += u64::from(now > start && stock == 0);
            let status = cargo
                .import_status(receiver.soul)
                .map(|status| format!("{status:?}"));
            let receipt = cargo
                .freight_day(now / TICKS_PER_BUDGET_DAY)
                .map(|day| day.receipt(receiver.soul, receiver.item))
                .unwrap_or_default();
            let frame = (stock, status, receipt.road_units, receipt.station_units);
            if previous_receiver.as_ref() != Some(&frame) {
                trace.events.push(json!({"tick": now, "kind": "receiver_state", "on_hand": stock, "import_status": frame.1, "road_units": frame.2, "station_units": frame.3}));
                previous_receiver = Some(frame);
            }
        }
        if now < trace.end_elapsed {
            shift_progress += world
                .trains
                .values()
                .filter(|train| train.speed.0 > 0.01)
                .count();
            drop(cargo);
            drop(transit);
            sim.tick(&mut schedule, &[]);
        }
    }
    let final_trips = trip_count(&sim);
    let final_fiscal = fiscal(&sim);
    let final_boards: u64 = sim.read::<TransitStats>().boardings.values().sum();
    trace.counters = json!({
        "boards": final_boards - initial_boards,
        "walk_car_bus_giveup_deltas": std::array::from_fn::<_, 4, _>(|i| final_trips[i] - initial_trips[i]),
        "import_export_fare_deltas": std::array::from_fn::<_, 3, _>(|i| (final_fiscal[i] - initial_fiscal[i]).to_string()),
        "observed_cross_line_same_target": bus_transfers,
        "train_moving_ticks": shift_progress,
        "terminal_state_changes": rail_transitions,
        "distinct_road_manifests": shipments.len(),
        "receiver_stockout_ticks": stockout_ticks,
    });
    trace.final_hashes = sim.hashes();
    trace
}

#[test]
#[ignore = "requires the two externally produced Wave 0 diagnostic slots"]
fn p8_slot_stock_traces() {
    let root = input_root();
    for slot in SLOTS {
        let control = diagnostic_trace(&root, slot, false);
        // A rejected paused command cannot affect the following gameplay prefix.
        let attempted = diagnostic_trace(&root, slot, true);
        println!("slot_trace {}", serde_json::to_string(&control).unwrap());
        println!("slot_trace {}", serde_json::to_string(&attempted).unwrap());
        assert_eq!(control.fixture, attempted.fixture);
        assert_eq!(control.fork_hashes, attempted.fork_hashes);
        assert_eq!(control.counters, attempted.counters);
        assert_eq!(control.final_hashes, attempted.final_hashes);
        assert_eq!(control.events, attempted.events);
    }
}
