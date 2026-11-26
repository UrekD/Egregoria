use super::*;
use simulation::map::{LotLayout, WaterBodyId, WaterEdit};
use std::sync::atomic::AtomicBool;

#[test]
fn p7_comparison_resume_retains_later_indexed_inputs() {
    crate::ScenarioCtx::initialize();
    let fixture = prepare_repair_fixture_initialized(FixtureArm::NoLoss).unwrap();
    let checkpoint = fixture.checkpoint;
    // Keep the first two offers, one in each cohort.
    let scheduled = &fixture.schedule[..2];
    let scoring = scoring_offers(scheduled).unwrap();
    let paused = [
        WorldCommand::EditWater(WaterEdit::RemoveFreshWater {
            body: WaterBodyId(1),
        }),
        WorldCommand::SetLotLayout(LotLayout::Uniform),
    ];
    let advancing = [WorldCommand::SetLotLayout(LotLayout::Varied)];
    let second_paused = [WorldCommand::SetLotLayout(LotLayout::Uniform)];
    let batches = [
        ScheduledBatch {
            tick: checkpoint + 21,
            advances: false,
            commands: &paused,
        },
        ScheduledBatch {
            tick: checkpoint + 21,
            advances: false,
            commands: &second_paused,
        },
        ScheduledBatch {
            tick: checkpoint + 21,
            advances: true,
            commands: &advancing,
        },
    ];
    let snapshot = VerifiedFixtureSnapshot::capture(&fixture.simulation).unwrap();
    let inputs = AbsoluteSchedule {
        scheduled_offers: scheduled,
        scoring_offers: &scoring,
        batches: &batches,
        primary_cutoff: checkpoint + 120,
        tail_cutoff: checkpoint + 125,
    };
    let cancelled = AtomicBool::new(false);
    let request = ComparisonRequest {
        repair_at: fixture.repair_at,
        repair: &fixture.repair,
        resume_at: None,
        cancelled: &cancelled,
    };
    let repeated = compare_repair(&snapshot, &inputs, request).unwrap();
    let background = std::thread::scope(|scope| {
        scope
            .spawn(|| compare_repair(&snapshot, &inputs, request))
            .join()
            .unwrap()
            .unwrap()
    });
    assert_eq!(background, repeated);
    let resumed = compare_repair(
        &snapshot,
        &inputs,
        ComparisonRequest {
            resume_at: Some(checkpoint + 20),
            ..request
        },
    )
    .unwrap();
    assert_eq!(repeated, resumed);
    assert_eq!(resumed.control.batches[0].accepted, [1]);
    assert_eq!(resumed.control.batches[0].rejections[&0].len(), 1);
    assert_eq!(resumed.control.batches[1].accepted, [0]);
    assert_eq!(resumed.control.batches[2].accepted, [0]);
    assert_eq!(resumed.treatment.batches[0].tick, fixture.repair_at);
    assert_eq!(resumed.treatment.batches[0].accepted, [0]);
    assert_eq!(
        repeated,
        compare_scheduled(&snapshot, &inputs, request, false, true).unwrap()
    );

    cancelled.store(true, Ordering::Relaxed);
    assert_eq!(
        compare_repair(&snapshot, &inputs, request),
        Err(ComparisonError::Cancelled),
    );
}

#[test]
fn p7_comparison_overlapping_freight_uses_its_own_reservations() {
    use simulation::economy::cargo::{reserve_road_freight, ItemQuantity};
    use simulation::economy::Market;
    use simulation::map_dynamic::BuildingInfos;

    crate::ScenarioCtx::initialize();
    let mut fixture = prepare_repair_fixture_initialized(FixtureArm::NoLoss).unwrap();
    let mut offers: Vec<_> = fixture.schedule.iter().copied()
        .filter(|offer| matches!(offer.offer, FixtureOffer::Freight { .. }))
        .take(2).collect();
    let FixtureOffer::Freight { sender, receiver, item, .. } = offers[0].offer else {
        unreachable!()
    };
    let FixtureOffer::Freight { sender: other, item: other_item, units, .. } = offers[1].offer else {
        unreachable!()
    };
    assert_ne!(sender, other);
    assert_eq!(item, other_item);
    offers[1].offer = FixtureOffer::Freight { sender: other, receiver, item, units };

    let owner = fixture.simulation.read::<BuildingInfos>().owner(sender).unwrap();
    fixture.simulation.write::<Market>().produce(owner, item, 3);
    assert!(reserve_road_freight(
        &mut fixture.simulation, sender, receiver, ItemQuantity { item, units: 3 }
    ).is_some());
    // The city's existing reservation persists. The comparison's observer
    // starts cold and watches only the two offers released by this schedule.
    let snapshot = VerifiedFixtureSnapshot::capture(&fixture.simulation).unwrap();
    let inputs = AbsoluteSchedule {
        scheduled_offers: &offers,
        scoring_offers: &[],
        batches: &[],
        primary_cutoff: fixture.primary_cutoff,
        tail_cutoff: fixture.tail_cutoff,
    };
    let (result, _) = run_repair_arm(
        snapshot.load().unwrap(), &inputs,
        ComparisonRequest {
            repair_at: fixture.repair_at,
            repair: &fixture.repair,
            resume_at: Some(fixture.checkpoint + 1000),
            cancelled: &AtomicBool::new(false),
        },
        false, true, false,
    ).unwrap();
    for (observation, offer) in result.observations.iter().zip(offers) {
        let FixtureOffer::Freight { units, .. } = offer.offer else { unreachable!() };
        assert!(matches!(&observation.terminal,
            Some(crate::traffic_repair::Terminal::Completed { units: actual, .. }) if *actual == units
        ), "{observation:?}");
    }
}
