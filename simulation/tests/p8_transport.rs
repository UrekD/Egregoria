//! Stock-seam reductions for P8. Run explicit reds with --ignored --nocapture.

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
#[ignore = "P8 R1: conflicting spawn must reject before publishing or charging"]
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

    sim.apply_commands([&spawn(100.0), &spawn(250.0)]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0, 1]);
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
    sim.apply_commands([&WorldCommand::SpawnTrain {
        wagons: consist(),
        lane,
        dist,
    }]);
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
#[ignore = "P8 R1/F: physical berth proof must survive an itinerary ending early"]
fn p8_freight_physical_boundary_short_of_berth() {
    freight_call(Some(Interruption::BeforeBerth));
}

#[test]
#[ignore = "P8 R1/F: an apparent arrival on the wrong deck must leave cargo untouched"]
fn p8_freight_physical_boundary_wrong_deck() {
    freight_call(Some(Interruption::WrongDeck));
}

#[test]
#[ignore = "P8 R1/F: a train may only be freed after crossing the outside exit"]
fn p8_freight_physical_boundary_short_of_exit() {
    freight_call(Some(Interruption::BeforeExit));
}

#[test]
#[ignore = "P8 R1: retain the junction claim while the rear is stopped"]
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
    lead.it = Itinerary::NONE;
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
        "stock expired a claim without measured rear clearance"
    );
}
