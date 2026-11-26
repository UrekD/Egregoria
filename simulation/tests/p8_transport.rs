//! Physical boundary regressions for P8 freight and rail.

use std::sync::Once;

use geom::{vec3, Vec3};
use prototypes::RollingStockID;
use simulation::economy::{Budget, BudgetCategory, Government};
use simulation::map::{BuildingKind, LaneKind, LanePatternBuilder, MapProject, TraverseKind};
use simulation::map_dynamic::Itinerary;
use simulation::souls::freight_station::{freight_station_soul, train_stop, FreightTrainState};
use simulation::transportation::train::{
    locomotive_system, train_reservations_update, TrainReservations,
};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{FreightStationID, Simulation, SimulationOptions};

fn initialize() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        std::env::set_current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).unwrap();
        simulation::init::init();
    });
}

fn consist() -> Vec<RollingStockID> {
    ["locomotive", "freight-wagon"]
        .into_iter()
        .map(RollingStockID::new)
        .collect()
}

#[test]
fn p8_rail_shared_footprint() {
    initialize();
    let mut sim = Simulation::new_authored_flat(8, 0).unwrap();
    sim.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(200.0, 200.0, 0.0)),
        to: MapProject::ground(vec3(600.0, 200.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::new().rail(true).build(),
    }]);
    let lane = sim
        .map()
        .lanes()
        .values()
        .find(|lane| lane.kind == LaneKind::Rail)
        .unwrap()
        .id;
    let spawn = |dist| WorldCommand::SpawnTrain {
        wagons: consist(),
        lane,
        dist,
    };

    sim.apply_commands([&spawn(100.0), &spawn(100.0), &spawn(250.0)]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0, 2]);
    assert_eq!(sim.world().trains.len(), 2, "independent starts must fit");
    assert_eq!(sim.world().wagons.len(), 4);

    let cash = sim.read::<Government>().money;
    sim.apply_commands([&spawn(100.0)]);
    let outcomes = sim.read::<CommandOutcomes>();
    let wagons: Vec<_> = sim
        .world()
        .wagons
        .values()
        .map(|wagon| (wagon.itfollower.leader, wagon.trans.pos))
        .collect();
    eprintln!(
        "rail footprint: accepted={:?} rejected={:?} wagons={wagons:?} cash_delta={}",
        outcomes.accepted,
        outcomes.last,
        (sim.read::<Government>().money - cash).inner(),
    );
    assert!(
        outcomes.accepted.is_empty() && !outcomes.last.is_empty(),
        "an occupied lane interval admitted a second consist"
    );
    assert_eq!(sim.world().trains.len(), 2);
    assert_eq!(sim.world().wagons.len(), 4);
    assert_eq!(sim.read::<Government>().money, cash);
}

#[derive(Clone, Copy, Debug)]
enum Interruption {
    BeforeBerth,
    WrongDeck,
    BeforeExit,
}

fn freight_fixture() -> (Simulation, FreightStationID, Vec3, Vec3) {
    freight_fixture_with_wagons(consist())
}

fn freight_fixture_with_wagons(
    wagons: Vec<RollingStockID>,
) -> (Simulation, FreightStationID, Vec3, Vec3) {
    initialize();
    let mut sim = Simulation::new_with_options(SimulationOptions {
        save_replay: false,
        ..SimulationOptions::LEGACY
    });
    let (building, proto) = sim
        .map()
        .buildings()
        .values()
        .find_map(|building| {
            if let BuildingKind::RailFreightStation(proto) = building.kind {
                Some((building.id, proto))
            } else {
                None
            }
        })
        .expect("startup freight station");
    let station = freight_station_soul(&mut sim, building, proto).unwrap();
    let berth = train_stop(&sim.world().freight_stations[station].trans);
    let (outside, lane, dist) = {
        let map = sim.map();
        let market = map
            .buildings()
            .values()
            .find(|building| building.kind == BuildingKind::ExternalTrading)
            .unwrap();
        let outside = market.obb.center().z0();
        let lane = map
            .get(map.nearest_lane(outside, LaneKind::Rail, None).unwrap())
            .unwrap();
        (outside, lane.id, lane.points.length() / 2.0)
    };
    sim.apply_commands([&WorldCommand::SpawnTrain { wagons, lane, dist }]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);

    // Stage a fixed accepted export backlog and let the stock dispatcher assign
    // and drive the train. Order matching and receiver receipts are slot probes.
    sim.world_mut_unchecked().freight_stations[station]
        .f
        .waiting_cargo = 20;
    (sim, station, berth, outside)
}

fn freight_call(interruption: Option<Interruption>) {
    let (mut sim, station, berth, outside) = freight_fixture();
    let mut schedule = Simulation::schedule();
    let deadline = sim.get_tick() + 24_000;
    let interrupted_at = loop {
        assert!(
            sim.get_tick() < deadline,
            "freight call never exercised {interruption:?}"
        );
        sim.tick(&mut schedule, &[]);
        let terminal = &sim.world().freight_stations[station].f;
        let Some(&(id, state)) = terminal.trains.first() else {
            if terminal.waiting_cargo == 0 {
                let train = sim.world().trains.values().next().unwrap();
                let distance = train.trans.pos.xy().distance(outside.xy());
                eprintln!("freight control: export queue=0, outside distance={distance:.2} m");
                assert!(
                    distance < 80.0,
                    "control return did not reach outside track"
                );
                assert!(interruption.is_none());
                return;
            }
            continue;
        };
        let train = &sim.world().trains[id];
        if matches!(state, FreightTrainState::Loading) && interruption.is_none() {
            assert!(train.trans.pos.distance(berth) < 15.0);
        }
        let condition = match interruption {
            Some(Interruption::BeforeBerth | Interruption::WrongDeck) => {
                matches!(state, FreightTrainState::Arriving)
                    && train.trans.pos.distance(berth) > 100.0
            }
            Some(Interruption::BeforeExit) => {
                matches!(state, FreightTrainState::Moving)
                    && train.trans.pos.xy().distance(outside.xy()) > 100.0
            }
            None => false,
        };
        if condition {
            break (id, terminal.waiting_cargo);
        }
    };

    let (id, waiting) = interrupted_at;
    let train = &mut sim.world_mut_unchecked().trains[id];
    if matches!(interruption, Some(Interruption::WrongDeck)) {
        train.trans.pos = berth + vec3(0.0, 0.0, 20.0);
    }
    train.it = Itinerary::NONE;
    train.speed.0 = 0.0;
    let before = train.trans.pos;
    let trade = || [BudgetCategory::TradeImport, BudgetCategory::TradeExport];
    let totals = |sim: &Simulation| trade().map(|kind| sim.read::<Budget>().total(kind));
    let before_postings = totals(&sim);
    sim.tick(&mut schedule, &[]);
    let terminal = &sim.world().freight_stations[station].f;
    let after = sim.world().trains[id].trans.pos;
    let postings = totals(&sim);
    eprintln!(
        "freight boundary {interruption:?}: position={before:?}->{after:?}, berth={berth:?}, outside={outside:?}, queue={waiting}->{}, assignments={:?}, ledger={before_postings:?}->{postings:?}",
        terminal.waiting_cargo, terminal.trains
    );
    assert_eq!(after, before, "the injected train must remain stopped");
    assert_eq!(
        postings, before_postings,
        "interruption changed trade postings"
    );
    assert_eq!(
        terminal.waiting_cargo, waiting,
        "unverified arrival consumed export stock"
    );
    assert!(
        terminal.trains.iter().any(|&(train, _)| train == id),
        "unverified outside exit released the train"
    );
}

#[test]
fn p8_freight_physical_boundary_control() {
    freight_call(None);
}

#[test]
fn p8_freight_physical_boundary_short_of_berth() {
    freight_call(Some(Interruption::BeforeBerth));
}

#[test]
fn p8_freight_physical_boundary_wrong_deck() {
    freight_call(Some(Interruption::WrongDeck));
}

#[test]
fn p8_freight_physical_boundary_short_of_exit() {
    freight_call(Some(Interruption::BeforeExit));
}

#[test]
fn p8_rail_stopped_tail_retains_junction() {
    let (mut sim, _, _, _) = freight_fixture();
    let train = sim.world().trains.keys().next().unwrap();
    let mut schedule = Simulation::schedule();
    let deadline = sim.get_tick() + 24_000;
    let (turn, before_distance) = loop {
        assert!(
            sim.get_tick() < deadline,
            "the freight route did not straddle a junction"
        );
        sim.tick(&mut schedule, &[]);
        let lead = &sim.world().trains[train];
        let reservations = sim.read::<TrainReservations>();
        if let Some(sample) =
            reservations
                .localisations
                .iter()
                .find_map(|(&traversal, occupants)| {
                    let TraverseKind::Turn(turn) = traversal else {
                        return None;
                    };
                    let distance = *occupants.get(&train)?;
                    (lead.it.get_travers()?.kind != traversal
                        && (lead.locomotive.length - 3.0..lead.locomotive.length)
                            .contains(&distance)
                        && reservations.reservations.get(&turn.parent) == Some(&train))
                    .then_some((turn, distance))
                })
        {
            break sample;
        }
    };
    let poses = |sim: &Simulation| {
        sim.world()
            .wagons
            .values()
            .map(|wagon| wagon.trans.pos)
            .collect::<Vec<_>>()
    };
    let before_poses = poses(&sim);
    let lead = &mut sim.world_mut_unchecked().trains[train];
    let route = std::mem::take(&mut lead.it);
    lead.speed.0 = 0.0;
    let lead_position = lead.trans.pos;

    // Use the same motion and claim phases while holding the train stopped for
    // 100 realtime seconds, past the stock 60-second synthetic aging threshold.
    let steps = 100 * prototypes::TICKS_PER_REALTIME_SECOND;
    let (world, resources) = sim.unrestricted_world_res().unwrap();
    for _ in 0..steps {
        locomotive_system(world, resources);
        train_reservations_update(world, resources);
    }
    assert_eq!(poses(&sim), before_poses);
    assert_eq!(sim.world().trains[train].trans.pos, lead_position);
    let reservations = sim.read::<TrainReservations>();
    let after = reservations.reservations.get(&turn.parent);
    eprintln!("stopped rail tail: {turn:?}, distance={before_distance:.3}, poses={before_poses:?}, iterations={steps}, owner={after:?}");
    assert_eq!(
        after,
        Some(&train),
        "a held rear still occupies the junction"
    );
    drop(reservations);

    sim.world_mut_unchecked().trains[train].it = route;
    for _ in 0..500 {
        sim.tick(&mut schedule, &[]);
        let claim = sim.read::<TrainReservations>();
        if !claim
            .localisations
            .get(&TraverseKind::Turn(turn))
            .is_some_and(|owners| owners.contains_key(&train))
        {
            assert_ne!(sim.world().trains[train].trans.pos, lead_position);
            return;
        }
    }
    panic!("the junction claim was not released after the rear moved clear");
}

#[test]
fn p8_freight_terminal_policy_holds_accepted_exports_across_reload() {
    use common::saveload::{Bincode, Encoder};
    use simulation::economy::cargo::{Endpoint, FreightStage, FreightTerminalPolicy, FreightTrace};
    use simulation::world_command::Rejection;
    use simulation::SoulID;

    let (mut sim, station, _, _) = freight_fixture();
    let terminal = Endpoint {
        soul: SoulID::FreightStation(station),
        building: sim.world().freight_stations[station].f.building,
    };
    let command = |revision, policy| WorldCommand::SetFreightTerminalPolicy {
        terminal,
        expected_policy_revision: revision,
        policy,
    };
    let open = FreightTerminalPolicy::default();
    let held = FreightTerminalPolicy {
        admit_imports: false,
        admit_exports: false,
        max_calls: 0,
        max_truck_runs: 0,
    };
    let invalid = FreightTerminalPolicy {
        max_calls: 3,
        ..open
    };
    sim.apply_commands([&command(0, invalid), &command(0, held), &command(0, open)]);
    let outcomes = sim.read::<CommandOutcomes>();
    assert_eq!(outcomes.accepted, [1]);
    assert_eq!(
        outcomes.rejections_for(0),
        [("SetFreightTerminalPolicy", Rejection::InvalidFreightPolicy)]
    );
    assert_eq!(
        outcomes.rejections_for(2),
        [("SetFreightTerminalPolicy", Rejection::StaleFreightPolicy)]
    );
    drop(outcomes);

    let mut schedule = Simulation::schedule();
    for _ in 0..60 {
        sim.tick(&mut schedule, &[]);
    }
    assert!(sim.world().freight_stations[station].f.trains.is_empty());
    assert_eq!(sim.world().freight_stations[station].f.waiting_cargo, 20);
    let mut resumed: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    assert_eq!(sim.hashes(), resumed.hashes());
    assert_eq!(
        resumed
            .read::<FreightTrace>()
            .terminal_policy(terminal)
            .policy,
        held
    );

    // Reopening dispatch allows accepted stock to move even while new export
    // admissions stay paused.
    resumed.apply_commands([&command(
        1,
        FreightTerminalPolicy {
            max_calls: 1,
            ..held
        },
    )]);
    let mut checked_active_checkpoint = false;
    for _ in 0..24_000 {
        resumed.tick(&mut schedule, &[]);
        let trains = &resumed.world().freight_stations[station].f.trains;
        if !checked_active_checkpoint
            && trains
                .iter()
                .any(|(_, state)| matches!(state, FreightTrainState::Moving))
        {
            let from_snapshot: Simulation =
                Bincode::decode(&Bincode::encode(&resumed).unwrap()).unwrap();
            assert_eq!(resumed.hashes(), from_snapshot.hashes());
            resumed = from_snapshot;
            checked_active_checkpoint = true;
        }
        let trace = resumed.read::<FreightTrace>();
        let page = trace.terminal_view(terminal);
        if let Some(record) = page
            .rows
            .iter()
            .find(|record| record.stage == FreightStage::Exited)
        {
            assert!(checked_active_checkpoint);
            assert_eq!(record.fragment.units, 20);
            assert!(
                record.fragment.item.is_none(),
                "legacy stock has unknown SKU"
            );
            assert_eq!(resumed.world().freight_stations[station].f.waiting_cargo, 0);
            assert_eq!(
                trace
                    .corridor_report(record.fragment.call.unwrap().outside_gate)
                    .rows[0]
                    .stage,
                FreightStage::Exited
            );
            return;
        }
    }
    panic!("reopened terminal did not move the accepted shipment");
}

#[test]
fn p8_paused_train_footprint_is_restored_before_spawn() {
    use common::saveload::{Bincode, Encoder};

    let (sim, _, _, _) = freight_fixture();
    let train = sim.world().trains.values().next().unwrap();
    let pos = train.trans.pos;
    let (lane, offset) = {
        let map = sim.map();
        let lane = map.nearest_lane(pos, LaneKind::Rail, None).unwrap();
        let points = &map.get(lane).unwrap().points;
        (lane, points.length_at_proj(points.project(pos)))
    };
    // P6 did not publish a lane claim until the first movement update.
    sim.write::<TrainReservations>().localisations.clear();
    let mut restored: Simulation = Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
    restored.apply_commands([&WorldCommand::SpawnTrain {
        wagons: consist(),
        lane,
        dist: offset,
    }]);
    assert_eq!(restored.world().trains.len(), 1);
    assert!(restored.read::<CommandOutcomes>().accepted.is_empty());
}

#[test]
fn p8_snapshot_rejects_export_reservation_above_station_stock() {
    use common::saveload::{Bincode, Encoder};

    let (mut sim, station, _, _) = freight_fixture();
    sim.tick(&mut Simulation::schedule(), &[]);
    let terminal = &mut sim.world_mut_unchecked().freight_stations[station].f;
    assert_eq!(terminal.trains.len(), 1);
    terminal.waiting_cargo = 19;
    let error = Bincode::decode::<Simulation>(&Bincode::encode(&sim).unwrap()).err();
    assert!(
        error.is_some(),
        "accepted a reservation exceeding the station balance"
    );
}

#[test]
fn p8_lowering_dispatch_cap_keeps_a_moving_approach_admitted() {
    use simulation::economy::cargo::{Endpoint, FreightStage, FreightTerminalPolicy, FreightTrace};
    use simulation::SoulID;

    let (mut sim, station, berth, _) = freight_fixture();
    let terminal = Endpoint {
        soul: SoulID::FreightStation(station),
        building: sim.world().freight_stations[station].f.building,
    };
    let id = sim.world().trains.keys().next().unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..12_000 {
        sim.tick(&mut schedule, &[]);
        let train = &sim.world().trains[id];
        if train.speed.0 > 0.0
            && train
                .it
                .get_route()
                .is_some_and(|route| route.end_pos.distance(berth) < 15.0)
            && !sim
                .read::<FreightTrace>()
                .terminal_view(terminal)
                .rows
                .iter()
                .any(|row| row.stage == FreightStage::InboundRail)
        {
            let before = train.trans.pos;
            sim.apply_commands([&WorldCommand::SetFreightTerminalPolicy {
                terminal,
                expected_policy_revision: 0,
                policy: FreightTerminalPolicy {
                    max_calls: 0,
                    ..FreightTerminalPolicy::default()
                },
            }]);
            sim.tick(&mut schedule, &[]);
            assert_ne!(
                sim.world().trains[id].trans.pos,
                before,
                "cap froze a moving train"
            );
            return;
        }
    }
    panic!("did not find a moving inbound approach");
}

#[test]
fn p8_held_assignment_does_not_age_while_waiting_to_move() {
    use prototypes::{GameTime, TICKS_PER_HOUR};
    use simulation::economy::cargo::{Endpoint, FreightStage, FreightTerminalPolicy, FreightTrace};
    use simulation::souls::freight_station::freight_station_system;
    use simulation::SoulID;

    let (mut sim, station, berth, _) = freight_fixture();
    let terminal = Endpoint {
        soul: SoulID::FreightStation(station),
        building: sim.world().freight_stations[station].f.building,
    };
    let id = sim.world().trains.keys().next().unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..12_000 {
        sim.tick(&mut schedule, &[]);
        if sim.world().trains[id]
            .it
            .get_route()
            .is_some_and(|route| route.end_pos.distance(berth) < 15.0)
        {
            let open = FreightTerminalPolicy::default();
            let set_cap = |sim: &mut Simulation, expected_policy_revision, max_calls| {
                sim.apply_commands([&WorldCommand::SetFreightTerminalPolicy {
                    terminal,
                    expected_policy_revision,
                    policy: FreightTerminalPolicy { max_calls, ..open },
                }]);
            };
            let before = sim.world().trains[id].trans.pos;
            set_cap(&mut sim, 0, 0);
            sim.tick(&mut schedule, &[]);
            assert_eq!(sim.world().trains[id].trans.pos, before);
            let later = prototypes::Tick(sim.read::<GameTime>().tick.0 + TICKS_PER_HOUR + 5);
            *sim.write::<GameTime>() = GameTime::new(later);
            let (world, resources) = sim.unrestricted_world_res().unwrap();
            train_reservations_update(world, resources);
            {
                let claims = resources.read::<TrainReservations>();
                assert!(
                    claims.reservations.iter().all(|(&junction, &owner)| {
                        owner != id
                            || claims.localisations.iter().any(|(kind, occupants)| {
                                matches!(kind, TraverseKind::Turn(turn) if turn.parent == junction)
                                    && occupants.contains_key(&id)
                            })
                    }),
                    "a held train reserved a junction it did not occupy"
                );
            }
            freight_station_system(world, resources);
            assert_eq!(world.trains[id].trans.pos, before);
            // Keep the dispatcher from immediately creating a replacement
            // call if the accepted call is incorrectly expired on reopening.
            sim.write::<simulation::map_dynamic::Dispatcher>()
                .unregister(simulation::map_dynamic::DispatchID::FreightTrain(id));
            set_cap(&mut sim, 1, 1);
            let (world, resources) = sim.unrestricted_world_res().unwrap();
            // No movement between opening and the station phase, as when
            // competing traffic still owns the next junction.
            freight_station_system(world, resources);
            assert!(world.freight_stations[station]
                .f
                .trains
                .iter()
                .any(|&(train, _)| train == id));
            assert!(
                world.trains[id]
                    .it
                    .get_route()
                    .is_some_and(|route| route.end_pos.distance(berth) < 15.0),
                "reopening lost the accepted inbound route"
            );
            assert!(resources
                .read::<FreightTrace>()
                .terminal_view(terminal)
                .rows
                .iter()
                .all(|row| row.stage != FreightStage::WrittenOff));
            return;
        }
    }
    panic!("the positioning leg did not reach the gate");
}

#[test]
fn p8_freight_long_consist_clears_the_gate_turnout() {
    use simulation::economy::cargo::{Endpoint, FreightStage, FreightTrace};
    use simulation::SoulID;

    let wagons = std::iter::once(RollingStockID::new("locomotive"))
        .chain(std::iter::repeat_n(RollingStockID::new("freight-wagon"), 5))
        .collect();
    let (mut sim, station, _, outside) = freight_fixture_with_wagons(wagons);
    let id = sim.world().trains.keys().next().unwrap();
    let terminal = Endpoint {
        soul: SoulID::FreightStation(station),
        building: sim.world().freight_stations[station].f.building,
    };
    let (lane, gate_offset) = {
        let map = sim.map();
        let id = map.nearest_lane(outside, LaneKind::Rail, None).unwrap();
        let track = &map.get(id).unwrap().points;
        (id, track.length_at_proj(track.project(outside)))
    };
    let length = sim.world().trains[id].locomotive.length;
    assert!(
        gate_offset < length,
        "test gate must be too close to the approach turnout"
    );

    let mut schedule = Simulation::schedule();
    for _ in 0..24_000 {
        sim.tick(&mut schedule, &[]);
        let trace = sim.read::<FreightTrace>();
        let page = trace.terminal_view(terminal);
        assert!(
            page.rows
                .iter()
                .all(|row| row.stage != FreightStage::WrittenOff),
            "the accepted export was written off"
        );
        if let Some(exit) = page
            .rows
            .iter()
            .find(|row| row.stage == FreightStage::Exited)
        {
            assert_eq!(exit.fragment.units, 20);
            let pos = sim.world().trains[id].trans.pos;
            let map = sim.map();
            let track = &map.get(lane).unwrap().points;
            assert!(track.length_at_proj(track.project(pos)) >= length);
            assert!(sim
                .read::<TrainReservations>()
                .localisations
                .iter()
                .all(|(kind, owners)| kind.is_lane() || !owners.contains_key(&id)));
            return;
        }
    }
    panic!("the loaded train never cleared the outside gate");
}

#[test]
fn p8_default_market_export_fiscal_fence() {
    use geom::{vec2, OBB};
    use prototypes::{BuildingGen, GoodsCompanyID, ItemID};
    use simulation::economy::Market;
    use simulation::map::Map;
    use simulation::map_dynamic::BuildingInfos;
    use simulation::souls::goods_company::company_soul;

    let (mut sim, station, _, _) = freight_fixture();
    sim.world_mut_unchecked().freight_stations[station]
        .f
        .waiting_cargo = 0;
    let door = vec2(1000.0, 1000.0);
    let proto = GoodsCompanyID::new("small-depot");
    let building = sim
        .write::<Map>()
        .build_special_building(
            &OBB::new(door, geom::Vec2::X, 10.0, 10.0),
            BuildingKind::GoodsCompany(proto),
            BuildingGen::NoWalkway { door_pos: door },
            None,
            None,
        )
        .unwrap();
    sim.write::<BuildingInfos>().insert(building);
    let seller = company_soul(&mut sim, building, proto).unwrap();
    let coal = ItemID::new("coal");
    let mut schedule = Simulation::schedule();
    let start = sim.get_tick();
    let initial_budget = sim.read::<Budget>().total(BudgetCategory::TradeExport);
    let mut evidence = Vec::new();
    for quantity in [3, 4, 2] {
        {
            let mut market = sim.write::<Market>();
            market.produce(seller, coal, quantity as i32);
            market.sell(seller, door, coal, quantity, 0);
        }
        assert_eq!(sim.read::<Market>().capital(seller, coal), quantity as i32);
        sim.tick(&mut schedule, &[]);
        let stock = sim.world().freight_stations[station].f.waiting_cargo;
        let posted = sim.read::<Budget>().total(BudgetCategory::TradeExport) - initial_budget;
        evidence.push((sim.get_tick() - start, stock, posted));
        assert_eq!(sim.read::<Market>().capital(seller, coal), 0);
        sim.tick(&mut schedule, &[]);
        assert_eq!(
            sim.read::<Budget>().total(BudgetCategory::TradeExport) - initial_budget,
            posted
        );
    }
    eprintln!("default export fence {evidence:?}");
    assert_eq!(evidence, [(1, 3, 31_311), (3, 7, 73_059), (5, 9, 93_933)]);
}

#[test]
fn p8_snapshot_rejects_duplicate_train_assignment() {
    use common::saveload::{Bincode, Encoder};

    let (mut sim, station, _, _) = freight_fixture();
    sim.tick(&mut Simulation::schedule(), &[]);
    let assigned = &mut sim.world_mut_unchecked().freight_stations[station].f.trains;
    assigned.push(assigned[0]);
    let error = Bincode::decode::<Simulation>(&Bincode::encode(&sim).unwrap())
        .err()
        .unwrap();
    assert!(error
        .to_string()
        .contains("duplicate freight train assignment"));
}

#[test]
fn p8_market_batch_preserves_export_capacity_and_reload() {
    use common::saveload::{Bincode, Encoder};
    use geom::{vec2, Vec2, OBB};
    use prototypes::{BuildingGen, FreightStationPrototypeID, ItemID};
    use simulation::economy::cargo::{Endpoint, FreightTerminalPolicy};
    use simulation::economy::{market_update, Market};
    use simulation::map::Map;
    use simulation::map_dynamic::BuildingInfos;
    use simulation::SoulID;

    const BATCH: u32 = 65_537;
    initialize();
    let mut sim = Simulation::new_authored_flat(8, 0).unwrap();
    let proto = FreightStationPrototypeID::new("freight-station");
    let pos = vec2(250.0, 250.0);
    let building = sim
        .write::<Map>()
        .build_special_building(
            &OBB::new(pos, Vec2::X, 10.0, 10.0),
            BuildingKind::RailFreightStation(proto),
            BuildingGen::NoWalkway { door_pos: pos },
            None,
            None,
        )
        .unwrap();
    sim.write::<BuildingInfos>().insert(building);
    let station = freight_station_soul(&mut sim, building, proto).unwrap();
    sim.apply_commands([&WorldCommand::SetFreightTerminalPolicy {
        terminal: Endpoint {
            soul: SoulID::FreightStation(station),
            building,
        },
        expected_policy_revision: 0,
        policy: FreightTerminalPolicy {
            max_calls: 0,
            ..FreightTerminalPolicy::default()
        },
    }]);

    // Synthetic sellers isolate one market batch from production and transport.
    let mut sellers = slotmapd::SlotMap::<simulation::HumanID, ()>::with_key();
    let item = ItemID::new("coal");
    {
        let mut market = sim.write::<Market>();
        for _ in 0..BATCH {
            let seller = SoulID::Human(sellers.insert(()));
            market.produce(seller, item, 1);
            market.sell(seller, pos, item, 1, 0);
        }
    }
    let (world, resources) = sim.unrestricted_world_res().unwrap();
    market_update(world, resources);

    let staged = sim.world().freight_stations[station].f.waiting_cargo;
    assert!(
        (0..BATCH).contains(&staged) && staged > 0,
        "accepted {staged} of {BATCH}"
    );
    let market = sim.read::<Market>();
    let unsold: i32 = sellers
        .keys()
        .map(|id| market.capital(SoulID::Human(id), item))
        .sum();
    assert_eq!(staged + unsold as u32, BATCH);
    drop(market);
    let saved = Bincode::encode(&sim).unwrap();
    let restored: Simulation = Bincode::decode(&saved).unwrap();
    assert_eq!(
        restored.world().freight_stations[station].f.waiting_cargo,
        staged
    );
}
