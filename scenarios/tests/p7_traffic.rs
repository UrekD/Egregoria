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
    use simulation::map::{LotLayout, TerraformKind, WaterBodyId, WaterEdit};
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
    sim.apply_commands([&rejected]);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0).len(), 1);
    sim.apply_commands([&no_op]);

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
