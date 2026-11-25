use prototypes::TICKS_PER_MINUTE;
use scenarios::traffic_repair::{
    score_matched_offers, Cohort, Observation, Offer, ScoreError, Terminal,
};

fn offers() -> Vec<Offer> {
    (0..40)
        .map(|tag| Offer {
            tag,
            cohort: if tag < 20 {
                Cohort::Person
            } else {
                Cohort::Freight
            },
            directed_group: format!("od-{}", tag / 10),
            destination: format!("receiver-{}", tag / 10),
            tick: 100,
            free_flow_ticks: 50,
            units: if tag < 20 { 1 } else { 3 },
        })
        .collect()
}

fn observations(offers: &[Offer], arrival: impl Fn(u32) -> u64) -> Vec<Observation> {
    offers
        .iter()
        .map(|offer| Observation {
            tag: offer.tag,
            admitted_at: Some(offer.tick),
            terminal: Some(Terminal::Completed {
                tick: arrival(offer.tag),
                destination: offer.destination.clone(),
                units: offer.units,
            }),
        })
        .collect()
}

#[test]
fn cutoff_and_manifest_credit() {
    let offers = offers();
    let control = observations(&offers, |tag| if tag % 20 >= 18 { 7_500 } else { 250 });
    let repaired = observations(&offers, |_| 240);

    let primary = score_matched_offers(&offers, &control, &repaired, 7_000).unwrap();
    assert_eq!(primary.control[&Cohort::Person].credited, 18);
    assert_eq!(primary.control[&Cohort::Freight].credited, 54);
    assert_eq!(
        primary.control[&Cohort::Person].p95_delay_ticks,
        35 * TICKS_PER_MINUTE + 1,
        "C2 freezes unfinished delay even when the horizon exceeds the OD limit",
    );
    assert_eq!(
        primary.control[&Cohort::Freight].p95_delay_ticks,
        35 * TICKS_PER_MINUTE + 1
    );
    assert_eq!(primary.treatment[&Cohort::Freight].p95_delay_ticks, 90);
    assert!(primary.passes_improvement);
    assert!(primary.preserves_credit);

    let tail = score_matched_offers(&offers, &control, &repaired, 7_500).unwrap();
    assert_eq!(tail.control[&Cohort::Person].credited, 20);
    assert_eq!(tail.control[&Cohort::Freight].credited, 60);
    assert!(!tail.passes_improvement);
    assert!(tail.preserves_credit);

    let mut partial = repaired.clone();
    for row in &mut partial[38..] {
        let Some(Terminal::Completed { units, .. }) = &mut row.terminal else {
            unreachable!()
        };
        *units = 2;
    }
    let partial = score_matched_offers(&offers, &control, &partial, 7_000).unwrap();
    let freight = &partial.treatment[&Cohort::Freight];
    assert_eq!(freight.credited, 58);
    assert_eq!(freight.p95_delay_ticks, 35 * TICKS_PER_MINUTE + 1);
    assert!(!partial.passes_improvement);

    let mut wrong_receiver = repaired.clone();
    let Some(Terminal::Completed { destination, .. }) = &mut wrong_receiver[39].terminal else {
        unreachable!()
    };
    *destination = "unrelated receiver".into();
    assert_eq!(
        score_matched_offers(&offers, &control, &wrong_receiver, 7_000),
        Err(ScoreError::InvalidObservation(39)),
    );
}

#[test]
fn repair_cohort_and_no_loss() {
    let offers = offers();
    let control = observations(&offers, |tag| if tag % 20 >= 18 { 7_500 } else { 250 });
    let person_only = observations(&offers, |tag| {
        if tag < 20 {
            240
        } else if tag >= 38 {
            7_500
        } else {
            250
        }
    });
    let score = score_matched_offers(&offers, &control, &person_only, 7_000).unwrap();
    assert!(!score.passes_improvement, "both cohorts must improve");

    let group_regression = observations(&offers, |tag| if tag / 10 == 0 { 253 } else { 240 });
    let score = score_matched_offers(&offers, &control, &group_regression, 7_000).unwrap();
    assert!(
        !score.passes_improvement,
        "the pooled p95 cannot hide a regressing OD"
    );

    let mut missing = observations(&offers, |_| 240);
    missing.pop();
    assert_eq!(
        score_matched_offers(&offers, &control, &missing, 7_000),
        Err(ScoreError::OfferSetMismatch),
    );

    let mut lost = observations(&offers, |_| 240);
    lost[0].terminal = Some(Terminal::Cancelled { tick: 220 });
    let score = score_matched_offers(&offers, &control, &lost, 7_000).unwrap();
    assert!(
        !score.passes_improvement,
        "new attrition outweighs net gains"
    );
    assert!(!score.preserves_credit, "an original completion was lost");
}

#[test]
fn snapshot_and_sidecar_parity() {
    use simulation::diagnostics::elapsed_tick;
    use simulation::map::{Density, LotLayout, TerraformKind, WaterBodyId, WaterEdit, ZoneKind};
    use simulation::utils::replay::{ReplayBudget, ReplayNext};
    use simulation::world_command::{CommandOutcomes, WorldCommand};
    use simulation::{MapPreset, NewGameOptions, SavePhase, Simulation, SlotId, SnapshotRevision};

    scenarios::ScenarioCtx::new(simulation::SimulationOptions {
        terrain_size: 1,
        ..simulation::SimulationOptions::LEGACY
    });
    let options = NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: 16,
        ..NewGameOptions::default()
    };
    let mut sim = Simulation::new_game_with_replay(&options).unwrap();
    let start = elapsed_tick(&sim);
    let mut schedule = Simulation::schedule();
    let rejected = WorldCommand::EditWater(WaterEdit::RemoveFreshWater {
        body: WaterBodyId(1),
    });
    let no_op = WorldCommand::EditWater(WaterEdit::SetSeaLevel { level_dm: None });
    let rejected_paint = WorldCommand::ZonePaint {
        area: geom::OBB::new(geom::vec2(-100.0, -100.0), geom::Vec2::X, 10.0, 10.0),
        kind: Some(ZoneKind::Residential),
        density: Density::Low,
    };
    sim.apply_commands([&rejected]);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0).len(), 1);
    sim.apply_commands([&no_op]);
    sim.apply_commands([&rejected_paint]);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0).len(), 1);

    let root = std::env::temp_dir().join(format!("p7-replay-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    struct TestSlotRoot(std::path::PathBuf);
    impl Drop for TestSlotRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let root = TestSlotRoot(root);
    let checkpoint = SlotId::new("checkpoint").unwrap();
    assert!(matches!(
        sim.save_to_slot(&root.0, &checkpoint, SnapshotRevision::new(10))
            .unwrap(),
        SavePhase::Complete
    ));
    let mut resumed = Simulation::load_from_slot(&root.0, &checkpoint).unwrap();
    assert_eq!(resumed.hashes(), sim.hashes());

    // A valid but rejected sea edit still affects the budget prepass for
    // the following terrain edit. Replay must retain both original inputs.
    let sea = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    });
    let raise = WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: geom::vec2(7072., 3072.),
        radius: 48.,
        amount: 100.,
        level: 0.,
        slope: None,
    };
    let layout = WorldCommand::SetLotLayout(LotLayout::Uniform);
    let later = [&rejected, &sea, &raise, &layout, &rejected];
    sim.tick(&mut schedule, later);
    resumed.tick(&mut Simulation::schedule(), later);
    assert_eq!(elapsed_tick(&sim).0, start.0 + 1);
    assert_eq!(sim.map().lot_layout(), LotLayout::Uniform);
    for index in [0, 2, 4] {
        assert_eq!(sim.read::<CommandOutcomes>().rejections_for(index).len(), 1);
    }
    assert_eq!(resumed.hashes(), sim.hashes());

    let final_slot = SlotId::new("final").unwrap();
    assert!(matches!(
        sim.save_to_slot(&root.0, &final_slot, SnapshotRevision::new(11))
            .unwrap(),
        SavePhase::Complete
    ));
    let replay = Simulation::load_replay_from_slot(&root.0, &final_slot).unwrap();
    let (mut restored, mut cursor) = Simulation::try_from_replay(replay).unwrap();
    let mut replay_schedule = Simulation::schedule();
    let mut budget = ReplayBudget::new(1);
    for (expected, advancing) in [
        (&[&rejected][..], false),
        (&[&no_op][..], false),
        (&[&rejected_paint][..], false),
        (&later[..], true),
    ] {
        let tick = prototypes::Tick(restored.get_tick());
        let ReplayNext::Run(op) = cursor.next_operation(tick, &budget) else {
            panic!("missing replay batch at {tick:?}");
        };
        assert_eq!(op.advances(), advancing);
        let recorded: Vec<_> = cursor
            .commands_for(op)
            .iter()
            .map(|(_, cmd)| serde_json::to_value(cmd).unwrap())
            .collect();
        let expected: Vec<_> = expected
            .iter()
            .map(|cmd| serde_json::to_value(cmd).unwrap())
            .collect();
        assert_eq!(recorded, expected);
        cursor.advance_one(&mut restored, &mut replay_schedule, &mut budget);
    }
    assert!(matches!(
        cursor.next_operation(prototypes::Tick(restored.get_tick()), &budget),
        ReplayNext::Settled { complete: true }
    ));
    assert_eq!(restored.hashes(), sim.hashes());
}

#[test]
fn manifest_admission() {
    use scenarios::traffic_repair::{
        verify_fixture, FixtureClass, FixtureFingerprint, FixtureRejection,
    };

    let manifest = wave0_manifest();
    let pin: FixtureFingerprint =
        serde_json::from_value(manifest["lesce_town_pin"].clone()).unwrap();
    assert_eq!(
        manifest["matched_cohort"]["od_limit_ticks"].as_u64(),
        Some(scenarios::traffic_repair::OD_DELAY_LIMIT_TICKS)
    );
    let release = FixtureClass::Release;
    let diagnostic = FixtureClass::Diagnostic;
    assert_eq!(verify_fixture(release, release, &pin, &pin), Ok(()));
    assert_eq!(
        verify_fixture(diagnostic, release, &pin, &pin),
        Err(FixtureRejection::WrongClass)
    );

    let mut staged = pin.clone();
    staged.snapshot_schema = "0.9.0".into();
    assert_eq!(
        verify_fixture(release, release, &pin, &staged),
        Err(FixtureRejection::FingerprintMismatch)
    );
    let mut staged = pin.clone();
    staged.run_source_commit.clone_from(&pin.source_commit);
    assert_eq!(
        verify_fixture(release, release, &pin, &staged),
        Err(FixtureRejection::FingerprintMismatch)
    );
    let mut staged = pin.clone();
    staged.opening = None;
    assert_eq!(
        verify_fixture(release, release, &staged, &staged),
        Err(FixtureRejection::MissingOpening)
    );
    let mut staged = pin.clone();
    staged.opening.as_mut().unwrap().passed = false;
    assert_eq!(
        verify_fixture(release, release, &staged, &staged),
        Err(FixtureRejection::MissingOpening)
    );
    let mut staged = pin.clone();
    staged.workers += 1;
    assert_eq!(
        verify_fixture(release, release, &pin, &staged),
        Err(FixtureRejection::FingerprintMismatch)
    );
    let mut malformed = pin;
    malformed.snapshot_sha256.clear();
    assert_eq!(
        verify_fixture(release, release, &malformed, &malformed),
        Err(FixtureRejection::MalformedFingerprint)
    );
}

fn wave0_manifest() -> serde_json::Value {
    serde_json::from_str(include_str!("fixtures/p7_wave0.json")).unwrap()
}

#[test]
#[ignore = "requires the SHA-256-verified CM0 schema v2 report in P7_CM0_MATRIX"]
fn cm0_stock_cases() {
    use serde_json::Value;
    use std::collections::BTreeSet;

    let manifest = wave0_manifest();
    let cm0 = &manifest["cm0"];
    let report: Value = serde_json::from_reader(
        std::fs::File::open(std::env::var_os("P7_CM0_MATRIX").expect("P7_CM0_MATRIX")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["version"], cm0["schema"]);
    assert_eq!(report["contract"], cm0["demand"]);
    let cases = report["cases"].as_array().unwrap();
    assert_eq!(cases.len(), cm0["case_count"].as_u64().unwrap() as usize);
    let unique: BTreeSet<_> = cases.iter().map(|case| case["case"].to_string()).collect();
    assert_eq!(unique.len(), cases.len());
    for field in ["offered", "admitted", "completed"] {
        let total: u64 = cases
            .iter()
            .map(|case| case["totals"][field].as_u64().unwrap())
            .sum();
        assert_eq!(total, cm0["day_totals"][field].as_u64().unwrap(), "{field}");
    }
    for workload in cm0["groups"].as_array().unwrap() {
        let group: Vec<_> = cases
            .iter()
            .filter(|case| {
                case["case"]["demand_pattern"] == workload["demand_pattern"]
                    && case["case"]["load"] == workload["load"]
            })
            .collect();
        assert_eq!(group.len(), workload["cases"].as_u64().unwrap() as usize);
        let eligible = group
            .iter()
            .filter(|case| case["gates"]["eligible"] == true)
            .count();
        assert_eq!(eligible, workload["eligible"].as_u64().unwrap() as usize);
    }
    for expected in cm0["controls"].as_array().unwrap() {
        let actual = cases
            .iter()
            .find(|case| case["case"] == expected["case"])
            .expect("missing stock control");
        let id = expected["id"].as_str().unwrap();
        let peaks = actual["peaks"].as_array().unwrap();
        assert_eq!(peaks.len(), 2, "{id}");
        let arrivals: Vec<_> = peaks
            .iter()
            .map(|peak| peak["completed_by_cutoff"].clone())
            .collect();
        assert_eq!(
            Value::Array(arrivals),
            expected["completed_by_cutoff"],
            "{id}"
        );
        let cutoffs: Vec<_> = peaks
            .iter()
            .map(|peak| peak["cutoff_tick"].clone())
            .collect();
        assert_eq!(Value::Array(cutoffs), expected["cutoff_ticks"], "{id}");
        assert_eq!(actual["gates"], expected["gates"], "{id}");
        assert_eq!(
            actual["occupancy"]["max_blocking_ticks"], expected["max_blocking_ticks"],
            "{id}"
        );
        let worst = actual["cohorts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|od| od["p95_delay_ticks"].as_u64().unwrap())
            .max()
            .unwrap();
        assert_eq!(
            worst,
            expected["worst_od_p95_ticks"].as_u64().unwrap(),
            "{id}"
        );
        for (peak, expected) in peaks
            .iter()
            .zip(expected["cutoff_queues"].as_array().unwrap())
        {
            assert_eq!(&peak["cutoff_queues"], expected, "{id}");
        }
    }
}

#[test]
#[ignore = "P7-G: stock scenario runner exposes only one simulation worker"]
fn worker_and_native_comparator_parity() {
    use scenarios::perf_scale::{benchmark_layout, PerfCase, PerfLayout};

    let run = |workers| {
        let report = benchmark_layout(
            PerfCase {
                agents: 16,
                warmup_ticks: 0,
                measured_ticks: 10,
                workers,
            },
            PerfLayout::Growing,
        )
        .unwrap_or_else(|err| panic!("{workers}-worker prerequisite: {err}"));
        serde_json::to_value(report).unwrap()
    };

    let serial = run(1);
    assert_eq!(serial["actual_agents"], 16);
    assert_eq!(serial["completed_steps"], 10);
    for workers in [4, 16] {
        let parallel = run(workers);
        for field in [
            "tick",
            "humans",
            "vehicles",
            "companies",
            "trains",
            "money_cents",
            "peak_active_vehicles",
        ] {
            assert_eq!(serial[field], parallel[field], "{workers} workers: {field}");
        }
    }
}
#[test]
fn repair_cohort_and_no_loss_fixtures_stage_the_same_offers() {
    use scenarios::traffic_repair::{prepare_repair_fixture, FixtureArm, FixtureOffer};
    use simulation::map::road_edit::RoadEditStatus;
    use simulation::world_command::CommandOutcomes;
    use std::collections::{BTreeMap, BTreeSet};

    let mut repair = prepare_repair_fixture(FixtureArm::Repair).unwrap();
    let mut no_loss = prepare_repair_fixture(FixtureArm::NoLoss).unwrap();
    assert_eq!(repair.checkpoint, no_loss.checkpoint);
    assert_eq!(
        serde_json::to_value(&repair.schedule).unwrap(),
        serde_json::to_value(&no_loss.schedule).unwrap()
    );
    assert_eq!(repair.repair_at, repair.checkpoint + 1);
    assert_eq!(repair.primary_cutoff, repair.checkpoint + 10_600);
    assert_eq!(repair.tail_cutoff, repair.checkpoint + 144_000);
    let manifest = wave0_manifest();
    let pinned = &manifest["matched_cohort"]["repair_command"];
    for (fixture, start, requested) in [
        (&repair, "opening_speed_m_per_s", "requested_speed_m_per_s"),
        (
            &no_loss,
            "no_loss_opening_speed_m_per_s",
            "no_loss_requested_speed_m_per_s",
        ),
    ] {
        use simulation::map::road_edit::RoadUpgrade;
        use simulation::world_command::WorldCommand;

        let map = fixture.simulation.map();
        let opening = map.roads()[fixture.road].pattern(map.lanes());
        assert!(opening
            .lanes()
            .all(|(_, _, speed)| f64::from(speed) == pinned[start].as_f64().unwrap()));
        let WorldCommand::MapUpgradeRoad {
            change: RoadUpgrade::Replace(pattern),
            ..
        } = &fixture.repair
        else {
            panic!("fixture requires a priced road replacement");
        };
        assert!(pattern
            .lanes()
            .all(|(_, _, speed)| f64::from(speed) == pinned[requested].as_f64().unwrap()));
    }
    let mut groups = BTreeMap::new();
    let mut people = BTreeSet::new();
    for entry in &repair.schedule {
        *groups.entry(entry.group).or_insert(0) += 1;
        match entry.offer {
            FixtureOffer::Person {
                human,
                home,
                destination,
            } => {
                assert!(people.insert(human));
                assert!(repair.simulation.world().humans.contains_key(human));
                assert!(repair.simulation.map().get(home).is_some());
                assert!(repair.simulation.map().get(destination).is_some());
                assert_eq!(
                    entry.tick - repair.checkpoint,
                    100 + u64::from(entry.tag - 1) * 50
                );
            }
            FixtureOffer::Freight {
                sender,
                receiver,
                units,
                ..
            } => {
                assert_ne!(sender, receiver);
                assert_eq!(units, 1);
                assert!(repair.simulation.map().get(sender).is_some());
                assert!(repair.simulation.map().get(receiver).is_some());
                assert_eq!(
                    entry.tick - repair.checkpoint,
                    100 + u64::from(entry.tag - 121) * 250
                );
            }
        }
    }
    assert_eq!(people.len(), 120);
    assert_eq!(groups.len(), 8);
    assert_eq!(groups.values().filter(|&&offers| offers == 20).count(), 6);
    assert_eq!(groups.values().filter(|&&offers| offers == 12).count(), 2);

    for (fixture, expected) in [
        (&mut repair, RoadEditStatus::Applied),
        (&mut no_loss, RoadEditStatus::Applied),
    ] {
        fixture
            .simulation
            .tick(&mut simulation::Simulation::schedule(), [&fixture.repair]);
        assert_eq!(
            simulation::diagnostics::elapsed_tick(&fixture.simulation).0,
            fixture.repair_at
        );
        let last = fixture.simulation.read::<CommandOutcomes>();
        assert_eq!(
            last.road_edit(0).unwrap().as_ref().unwrap().status,
            expected
        );
    }
}

#[test]
fn showcase_archive_is_historical() {
    let manifest = wave0_manifest();
    let showcase = &manifest["stock_slots"]["showcase-release"];
    assert_eq!(showcase["snapshot_schema"], "0.10.0");
    assert_eq!(showcase["release_eligible"], false);
}

#[test]
fn repair_cohort_and_no_loss_preserves_physical_arrivals_and_unloads() {
    use scenarios::traffic_repair::{prepare_repair_fixture, FixtureArm, FixtureOffer};

    let fixture = prepare_repair_fixture(FixtureArm::NoLoss).unwrap();
    let [stock, edit] = run_repair_checkpoint(&fixture, "no-loss");
    let mut persons = 0;
    let mut freight = 0;
    for ((offer, stock), edit) in fixture.schedule.iter().zip(&stock).zip(&edit) {
        assert_eq!(stock.tag, offer.tag);
        assert_eq!(edit.tag, offer.tag);
        assert_eq!(stock.admitted_at, Some(offer.tick));
        assert_eq!(edit.admitted_at, stock.admitted_at);
        let destination = match offer.offer {
            FixtureOffer::Person { destination, .. } => {
                persons += 1;
                destination
            }
            FixtureOffer::Freight { receiver, .. } => {
                freight += 1;
                receiver
            }
        };
        let accepted = |row: &Observation| {
            matches!(
                &row.terminal,
                Some(Terminal::Completed { tick, units: 1, destination: at })
                    if (offer.tick..=fixture.tail_cutoff).contains(tick) && at == &format!("{destination:?}")
            )
        };
        if !accepted(stock) || !accepted(edit) {
            eprintln!(
                "tag {} at {destination:?}, stock {:?}, edit {:?}",
                offer.tag, stock.terminal, edit.terminal
            );
            panic!("no-loss arm is missing a physical completion");
        }
    }
    assert_eq!((persons, freight), (120, 24));
}

#[test]
#[ignore = "runs both full matched repair arms"]
fn repair_cohort_matched_headless() {
    use scenarios::traffic_repair::{prepare_repair_fixture, FixtureArm, FixtureOffer};

    let fixture = prepare_repair_fixture(FixtureArm::Repair).unwrap();
    let [control, treatment] = run_repair_checkpoint(&fixture, "repair-cohort");
    let report = |treatment: bool, observations: &[Observation]| {
        assert_eq!(observations.len(), fixture.schedule.len());
        let mut primary = [0, 0];
        let mut tail = [0, 0];
        let mut durations = [Vec::new(), Vec::new()];
        for (offered, observed) in fixture.schedule.iter().zip(observations) {
            assert_eq!(observed.tag, offered.tag);
            assert_eq!(observed.admitted_at, Some(offered.tick));
            let (index, expected) = match offered.offer {
                FixtureOffer::Person { destination, .. } => (0, destination),
                FixtureOffer::Freight { receiver, .. } => (1, receiver),
            };
            if let Some(Terminal::Completed {
                tick,
                destination,
                units,
            }) = &observed.terminal
            {
                assert_eq!(destination, &format!("{expected:?}"));
                assert_eq!(*units, 1);
                assert!((offered.tick..=fixture.tail_cutoff).contains(tick));
                tail[index] += 1;
                if *tick <= fixture.primary_cutoff {
                    primary[index] += 1;
                    durations[index].push(*tick - offered.tick);
                }
            }
        }
        let observed_p95 = durations.map(|mut durations| {
            durations.sort_unstable();
            (durations.len() * 95)
                .div_ceil(100)
                .checked_sub(1)
                .map(|i| durations[i])
        });
        eprintln!(
            "treatment={treatment} primary={primary:?} tail={tail:?} completed-travel-p95-ticks={observed_p95:?}"
        );
        tail
    };
    let control = report(false, &control);
    let treatment = report(true, &treatment);
    assert!(treatment
        .into_iter()
        .zip(control)
        .all(|(edited, stock)| edited >= stock));
}

fn run_repair_checkpoint(
    fixture: &scenarios::traffic_repair::RepairFixture,
    name: &str,
) -> [Vec<Observation>; 2] {
    use scenarios::traffic_repair::run_no_loss_arm;
    use simulation::{SavePhase, Simulation, SlotId, SnapshotRevision};

    let root = std::env::temp_dir().join(format!("p7-{name}-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    struct PrivateSlot(std::path::PathBuf);
    impl Drop for PrivateSlot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let root = PrivateSlot(root);
    let slot = SlotId::new(name).unwrap();
    assert!(matches!(
        fixture
            .simulation
            .save_to_slot(&root.0, &slot, SnapshotRevision::new(1))
            .unwrap(),
        SavePhase::Complete,
    ));
    [false, true].map(|treatment| {
        let snapshot = Simulation::load_from_slot(&root.0, &slot).unwrap();
        run_no_loss_arm(snapshot, fixture, treatment)
            .unwrap_or_else(|error| panic!("{name} arm (edited={treatment}): {error}"))
    })
}
