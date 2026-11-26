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
    let batches = [
        ScheduledBatch {
            tick: checkpoint + 21,
            advances: false,
            commands: &paused,
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
    assert_eq!(resumed.treatment.batches[0].tick, fixture.repair_at);
    assert_eq!(resumed.treatment.batches[0].accepted, [0]);

    cancelled.store(true, Ordering::Relaxed);
    assert_eq!(
        compare_repair(&snapshot, &inputs, request),
        Err(ComparisonError::Cancelled),
    );
}
