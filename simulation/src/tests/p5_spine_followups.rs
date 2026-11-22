use super::{p3_spine::loop_fixture, TestCtx};
use crate::economy::progression::{ProgressionMode, ProgressionState};
use crate::economy::{DailyEconomyMetrics, Government, TripPurpose, TICKS_PER_BUDGET_DAY};
use crate::map::{BuildingKind, ZoneKind};
use crate::map_dynamic::Transit;
use crate::world_command::{CommandOutcomes, Rejection, WorldCommand};
use crate::{Replay, SaveLoadError, Simulation, INITIAL_TICK};
use common::saveload::{
    list_slots_with_revisions, Bincode, CompressedBincode, Encoder, SlotId, SnapshotRevision,
};
use geom::{vec2, Vec2, OBB};
use prototypes::{Density, GameTime, GoodsCompanyID, Tick};

#[test]
fn daily_metrics_keep_fourteen_closed_days_and_the_open_day() {
    let mut ctx = TestCtx::new_fast();
    let mut saved_size = None;
    for day in 0..=30 {
        ctx.g
            .write::<DailyEconomyMetrics>()
            .record_arrival(day, TripPurpose::Shopping, day + 1);
        *ctx.g.write::<GameTime>() = GameTime::new(Tick(
            INITIAL_TICK + day * TICKS_PER_BUDGET_DAY - u64::from(day > 0),
        ));
        ctx.tick_fast();

        let metrics = ctx.g.read::<DailyEconomyMetrics>();
        let expected_days = (day.saturating_sub(14)..=day).collect::<Vec<_>>();
        assert_eq!(
            metrics.days.keys().copied().collect::<Vec<_>>(),
            expected_days
        );
        assert!(metrics
            .closed_day(day, day * TICKS_PER_BUDGET_DAY)
            .is_none());
        if day > 0 {
            let latest_closed = metrics
                .closed_day(day - 1, day * TICKS_PER_BUDGET_DAY)
                .unwrap();
            assert_eq!(
                latest_closed.trips_by_purpose[&TripPurpose::Shopping].duration_ticks,
                day
            );
        }
        if day >= 14 {
            let bytes = Bincode::encode(&*metrics).unwrap();
            let restored: DailyEconomyMetrics = Bincode::decode(&bytes).unwrap();
            assert_eq!(
                restored.days.keys().copied().collect::<Vec<_>>(),
                expected_days
            );
            assert_eq!(*saved_size.get_or_insert(bytes.len()), bytes.len());
        }
    }
}

#[test]
fn guided_build_locks_leave_treasury_and_replay_unchanged() {
    let (mut ctx, line) = loop_fixture();
    let sim = &mut ctx.g;
    let mut stops = sim.read::<Transit>().lines[line].stops.clone();
    let stop = stops[0];
    let (pos, dir) = {
        let transit = sim.read::<Transit>();
        (transit.stops[stop].pos, transit.stops[stop].dir)
    };
    stops.reverse();

    sim.write::<ProgressionState>().mode = ProgressionMode::Guided;
    sim.write::<Replay>().enabled = true;
    let area = OBB::new(vec2(256.0, 256.0), Vec2::X, 16.0, 16.0);
    let paint = |density| WorldCommand::ZonePaint {
        area,
        kind: Some(ZoneKind::Residential),
        density,
    };
    let place = |name| {
        let company = GoodsCompanyID::new(name);
        let prototype = company.prototype();
        WorldCommand::MapBuildSpecialBuilding {
            kind: BuildingKind::GoodsCompany(company),
            pos: OBB::new(
                vec2(380.0, 380.0),
                Vec2::X,
                prototype.size.w,
                prototype.size.h,
            ),
            gen: prototype.bgen,
            zone: None,
            connected_road: None,
        }
    };
    for (command, name, milestone) in [
        (place("small-depot"), "MapBuildSpecialBuilding", 2),
        (place("large-warehouse"), "MapBuildSpecialBuilding", 3),
        (paint(Density::Medium), "ZonePaint", 1),
        (paint(Density::High), "ZonePaint", 2),
        (
            WorldCommand::TransitCreateLine { stops },
            "TransitCreateLine",
            1,
        ),
        (
            WorldCommand::TransitSetVehicles { line, n: 1 },
            "TransitSetVehicles",
            1,
        ),
    ] {
        let before = sim.hashes();
        let money = sim.read::<Government>().money;
        let recorded = sim.read::<Replay>().command_count();
        sim.apply_commands([&command]);
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(outcomes.last, [(name, Rejection::Locked { milestone })]);
        assert!(outcomes.accepted.is_empty());
        assert_eq!(sim.read::<Government>().money, money);
        assert_eq!(sim.read::<Replay>().command_count(), recorded);
        assert_eq!(sim.hashes(), before);
    }

    let exemptions = [
        WorldCommand::ZonePaint {
            area,
            kind: None,
            density: Density::High,
        },
        WorldCommand::TransitSetVehicles { line, n: 0 },
        WorldCommand::TransitRemoveLine { line },
        WorldCommand::TransitRemoveStop { stop },
        WorldCommand::TransitAddStop { pos, dir },
    ];
    let recorded = sim.read::<Replay>().command_count();
    sim.apply_commands(&exemptions);
    let outcomes = sim.read::<CommandOutcomes>();
    assert!(outcomes.last.is_empty());
    assert_eq!(outcomes.accepted, [0, 1, 2, 3, 4]);
    assert_eq!(
        sim.read::<Replay>().command_count(),
        recorded + exemptions.len()
    );
}

/// A 0.8.7 slot has the same zip, header and metadata layout as a current one. Only the
/// header version differs, and the version gate runs before the world body is decoded.
#[test]
fn a_complete_0_8_7_slot_is_listed_as_incompatible() {
    let root = std::env::temp_dir().join(format!(
        "egregoria-s5c-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir(&root).unwrap();
    let id = SlotId::new("old_town").unwrap();
    let revision = SnapshotRevision::new(3);
    let sim = TestCtx::new().g;
    sim.write::<Replay>().enabled = true;
    let mut slot = sim
        .encode_slot(revision)
        .unwrap()
        .with_display_name("Old Town".into());
    let header = crate::SlotSnapshotHeader {
        magic: crate::SLOT_SNAPSHOT_MAGIC,
        version: "0.8.7".into(),
        revision,
    };
    slot.snapshot = CompressedBincode::encode(&(header, &sim)).unwrap();
    slot.publish(&root, &id).unwrap();

    // The same classification the Load list applies to each discovered slot.
    let mut versions = Vec::new();
    let listed = list_slots_with_revisions(&root, |path, kind| {
        let header = Simulation::read_slot_snapshot_header(path, kind).unwrap();
        versions.push(header.version);
        header.revision
    })
    .unwrap();
    assert_eq!(versions, ["0.8.7"]);
    assert_ne!(crate::VERSION, "0.8.7");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].meta.as_ref().unwrap().display_name, "Old Town");
    assert_eq!(listed[0].revision, Some(revision));
    assert!(matches!(
        Simulation::load_from_slot(&root, &id),
        Err(SaveLoadError::Incompatible { version }) if version == "0.8.7"
    ));
    std::fs::remove_dir_all(&root).unwrap();
}
