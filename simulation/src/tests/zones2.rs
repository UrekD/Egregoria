use super::init_test_simulation;
use crate::economy::{progression::ProgressionMode, Market};
use crate::map::{
    BuildingID, Density, GrowableBusinessState, GrowableSiteState, LanePatternBuilder, LotLayout,
    MapProject, ZoneCell, ZoneGrid, ZoneKind,
};
use crate::map_dynamic::{active_business, effective_load, BuildingInfos, EnvCell, EnvGrid};
use crate::souls::growth::check_growable_lot;
use crate::souls::{goods_company, human, lifecycle, residence::Residences};
use crate::world_command::WorldCommand;
use crate::{MapPreset, NewGameOptions, Replay, SaveLoadError, Simulation, SoulID};
use common::saveload::{slot_paths, Bincode, Encoder, SlotId, SnapshotKind, SnapshotRevision};
use geom::{vec2, vec3, Ray3, Vec2, Vec3, OBB};
use prototypes::{GameTime, ItemID, TICKS_PER_HOUR};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

struct SlotRoot(PathBuf);

impl SlotRoot {
    fn new() -> Self {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        loop {
            let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("egr-zones2-{}-{serial}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => panic!("could not create {}: {err}", path.display()),
            }
        }
    }
}

impl Drop for SlotRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn flat_city() -> Simulation {
    init_test_simulation();
    Simulation::new_game_with_replay(&NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: NewGameOptions::MIN_TERRAIN_CHUNKS,
        lot_layout: LotLayout::Uniform,
        progression: ProgressionMode::FreeBuild,
        ..NewGameOptions::default()
    })
    .unwrap()
}

fn road(from: Vec3, to: Vec3) -> WorldCommand {
    WorldCommand::MapMakeConnection {
        from: MapProject::ground(from),
        to: MapProject::ground(to),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    }
}

fn zone(kind: ZoneKind, density: Density) -> ZoneCell {
    ZoneCell { kind, density }
}

#[test]
fn zones2_acceptance_high_residential_placement_has_a_tower_shell() {
    let mut sim = flat_city();
    sim.apply_commands([&road(vec3(100.0, 180.0, 0.0), vec3(820.0, 180.0, 0.0))]);
    let kinds = [
        ZoneKind::Residential,
        ZoneKind::Commercial,
        ZoneKind::Industrial,
        ZoneKind::Office,
    ];

    for kind in kinds {
        let cell = zone(kind, Density::High);
        let sites = {
            let map = sim.map();
            let mut forms = BTreeSet::new();
            let mut lots = Vec::new();
            for lot in map.lots().values() {
                let Ok(site) = check_growable_lot(&map, lot, cell) else {
                    continue;
                };
                let fit = site.initial_form().expect("high-density profile");
                if forms.insert(fit.form() as u8) {
                    lots.push((lot.clone(), fit));
                }
            }
            assert_eq!(forms.len(), 3, "{kind:?}: missing a uniform-plot form");
            lots
        };

        for (lot, fit) in sites {
            let placed = sim.place_zoned_growable(lot.id, cell).unwrap();
            let summary = sim.growable_summary(placed.building).unwrap();
            assert_eq!(summary.zone, cell);
            assert_eq!(summary.level, 1);
            assert!(summary.floors >= 12);
            assert!(summary.height_above_base >= 12.0 * 3.2);
            assert_eq!(summary.housing_slots, placed.housing_slots);
            assert_eq!(summary.planned_jobs, placed.job_slots);
            assert_eq!(summary.active_job_slots, 0);
            assert_eq!(summary.housing_slots > 0, kind == ZoneKind::Residential);
            assert_eq!(summary.planned_jobs > 0, kind != ZoneKind::Residential);

            let map = sim.map();
            assert!(map.get(lot.id).is_none());
            assert_eq!(
                map.lot_origin(placed.building).unwrap().parent_road,
                lot.parent
            );
            let building = map.get(placed.building).unwrap();
            assert_eq!(building.obb.corners, lot.shape.corners);
            let shell = building.growable_shell().unwrap();
            assert_eq!(shell.door(), fit.door());
            assert_eq!(shell.base_z(), fit.base_z());
            assert_eq!(shell.roof_z(), summary.roof_z);
            assert!(fit
                .footprint()
                .corners
                .iter()
                .all(|&p| lot.shape.contains(p)));
            let triangles: Vec<_> = shell.triangles().collect();
            assert!(triangles
                .iter()
                .all(|(points, _)| points.iter().all(|p| p.is_finite())));
            assert!(shell.faces().map(|(face, _)| face.len()).sum::<usize>() <= 1536);
            assert!((1..=2304 / 3).contains(&triangles.len()));
            let ray = Ray3 {
                from: fit.shaft().center().z(summary.roof_z + 4.0),
                dir: -Vec3::Z,
            };
            assert!((shell.ray_depth(ray, 8.0).unwrap() - 4.0).abs() < 0.01);
        }
    }
}

#[test]
fn zones2_acceptance_same_parcel_value_changes_the_tower_capacity() {
    let mut low_value = flat_city();
    low_value.apply_commands([&road(vec3(100.0, 180.0, 0.0), vec3(680.0, 180.0, 0.0))]);
    let home = zone(ZoneKind::Residential, Density::High);
    let neighbor_lot = {
        let map = low_value.map();
        map.lots()
            .values()
            .find(|lot| check_growable_lot(&map, lot, home).is_ok())
            .unwrap()
            .clone()
    };
    let neighbor = low_value
        .place_zoned_growable(neighbor_lot.id, home)
        .unwrap()
        .building;
    let parcel = {
        let map = low_value.map();
        map.lots()
            .values()
            .filter(|lot| {
                lot.parent == neighbor_lot.parent
                    && lot.shape.center().distance2(neighbor_lot.shape.center()) <= 96.0_f32.powi(2)
            })
            .find(|lot| check_growable_lot(&map, lot, home).is_ok())
            .unwrap()
            .clone()
    };
    let root = SlotRoot::new();
    let id = SlotId::new("zones2_matched_value").unwrap();
    low_value
        .save_to_slot(&root.0, &id, SnapshotRevision::new(1))
        .unwrap();
    let mut high_value = Simulation::load_from_slot(&root.0, &id).unwrap();
    let env = |measured, access| {
        Bincode::decode::<EnvGrid>(
            &Bincode::encode(&(BTreeMap::<(i32, i32), EnvCell>::new(), measured, access)).unwrap(),
        )
        .unwrap()
    };
    // Set only the measured neighbor's service access. The empty parcel uses
    // its own pollution and utility inputs, even when its neighbor measured zero.
    *high_value.write::<EnvGrid>() = env(
        BTreeMap::from([(neighbor, 0_u8)]),
        BTreeMap::from([(neighbor, 42.0_f32)]),
    );
    assert_eq!(
        low_value.read::<EnvGrid>().land_value_sample(neighbor),
        None
    );
    assert_eq!(
        high_value.read::<EnvGrid>().land_value_sample(neighbor),
        Some(0)
    );
    let initial_value =
        |sim: &Simulation| sim.read::<EnvGrid>().initial_lot_value(&sim.map(), &parcel);
    assert_eq!(
        (initial_value(&low_value), initial_value(&high_value)),
        (30, 72)
    );
    let low = low_value.place_zoned_growable(parcel.id, home).unwrap();
    let high = high_value.place_zoned_growable(parcel.id, home).unwrap();
    let low_tower = low_value.growable_summary(low.building).unwrap();
    let high_tower = high_value.growable_summary(high.building).unwrap();
    assert_eq!(
        low_value
            .growable_land_value(low.building)
            .unwrap()
            .approved_band,
        0
    );
    assert_eq!(
        high_value
            .growable_land_value(high.building)
            .unwrap()
            .approved_band,
        2
    );
    assert_eq!(high_tower.floors, low_tower.floors + 2);
    assert!(high_tower.housing_slots > low_tower.housing_slots);
    assert!(high_tower.roof_z > low_tower.roof_z);
    let footprint = |sim: &Simulation, id: BuildingID| {
        let map = sim.map();
        map.get(id)
            .unwrap()
            .development()
            .unwrap()
            .frozen_shell
            .as_ref()
            .unwrap()
            .fit()
            .footprint()
    };
    assert_ne!(
        footprint(&low_value, low.building).corners,
        footprint(&high_value, high.building).corners
    );
}

#[test]
fn zones2_acceptance_profiled_home_and_business_loads_match_their_slots() {
    let mut sim = flat_city();
    sim.apply_commands([&road(vec3(100.0, 260.0, 0.0), vec3(680.0, 260.0, 0.0))]);
    let work_zone = zone(ZoneKind::Commercial, Density::High);
    let home_zone = zone(ZoneKind::Residential, Density::High);
    let parcel = |sim: &Simulation, cell| {
        let map = sim.map();
        map.lots()
            .values()
            .find(|lot| check_growable_lot(&map, lot, cell).is_ok())
            .unwrap()
            .id
    };
    let home = sim
        .place_zoned_growable(parcel(&sim, home_zone), home_zone)
        .unwrap();
    let shop = sim
        .place_zoned_growable(parcel(&sim, work_zone), work_zone)
        .unwrap();
    let proto = sim
        .map()
        .get(shop.building)
        .unwrap()
        .kind
        .as_goods_company()
        .unwrap();
    let Some(SoulID::GoodsCompany(company)) =
        goods_company::company_soul(&mut sim, shop.building, proto)
    else {
        panic!("the committed business must open exactly once");
    };
    assert!(goods_company::company_soul(&mut sim, shop.building, proto).is_none());
    assert_eq!(sim.get(company).unwrap().comp.max_workers, shop.job_slots);
    let job_opening = ItemID::new("job-opening");
    assert_eq!(
        sim.read::<Market>()
            .capital(SoulID::GoodsCompany(company), job_opening),
        shop.job_slots as i32
    );

    let load = |sim: &Simulation, id: BuildingID| {
        let map = sim.map();
        let building = map.get(id).unwrap();
        let infos = sim.read::<BuildingInfos>();
        let homes = sim.read::<Residences>();
        effective_load(
            building,
            homes.by_building.get(&id),
            active_business(building, &infos, sim.world()),
        )
    };
    let home_load = load(&sim, home.building);
    assert_eq!(home_load.residential_water, f64::from(home.housing_slots));
    assert_eq!(home_load.business_water, 0.0);
    let shop_load = load(&sim, shop.building);
    assert_eq!(shop_load.residential_water, 0.0);
    assert!(shop_load.business_water > 0.0);

    assert!(lifecycle::close_business(&mut sim, company));
    sim.apply_commands([]);
    let closed = sim.growable_summary(shop.building).unwrap();
    assert_eq!(closed.planned_jobs, shop.job_slots);
    assert_eq!(closed.active_job_slots, 0);
    assert_eq!(load(&sim, shop.building).water_demand(), 0.0);
    assert_eq!(
        load(&sim, home.building).water_demand(),
        home_load.water_demand()
    );
    assert_eq!(
        sim.read::<Market>()
            .capital(SoulID::GoodsCompany(company), job_opening),
        0
    );

    let root = SlotRoot::new();
    let slot = SlotId::new("zones2_controlled").unwrap();
    sim.save_to_slot(&root.0, &slot, SnapshotRevision::new(1))
        .unwrap();
    let mut restored = Simulation::load_from_slot(&root.0, &slot).unwrap();
    assert_eq!(sim.hashes(), restored.hashes());
    let Some(GrowableBusinessState::ClosedUntil { retry_elapsed_tick }) = closed.business else {
        panic!("expected a reopen deadline");
    };
    for sim in [&mut sim, &mut restored] {
        assert!(goods_company::company_soul(sim, shop.building, proto).is_none());
        sim.write::<GameTime>().tick.0 += retry_elapsed_tick;
        let Some(SoulID::GoodsCompany(reopened)) =
            goods_company::company_soul(sim, shop.building, proto)
        else {
            panic!("seller did not reopen after the deadline");
        };
        assert_ne!(reopened, company);
        assert_eq!(
            sim.growable_summary(shop.building)
                .unwrap()
                .active_job_slots,
            shop.job_slots
        );
    }
    assert_eq!(sim.hashes(), restored.hashes());
}

#[test]
fn zones2_acceptance_mixed_shop_closure_keeps_occupied_apartments() {
    let mut sim = flat_city();
    sim.apply_commands([&road(vec3(100.0, 260.0, 0.0), vec3(680.0, 260.0, 0.0))]);
    for density in [Density::Medium, Density::High] {
        let cell = zone(ZoneKind::MixedUse, density);
        let lot = {
            let map = sim.map();
            map.lots()
                .values()
                .find(|lot| check_growable_lot(&map, lot, cell).is_ok())
                .unwrap()
                .id
        };
        let placed = sim.place_zoned_growable(lot, cell).unwrap();
        let id = placed.building;
        let tenant = human::spawn_human(&mut sim, id).unwrap();
        let proto = sim.map().get(id).unwrap().kind.as_goods_company().unwrap();
        let Some(SoulID::GoodsCompany(company)) = goods_company::company_soul(&mut sim, id, proto)
        else {
            panic!("checked MX site did not open");
        };
        let open = sim.growable_summary(id).unwrap();
        assert_eq!(
            (open.occupants, open.active_job_slots, open.planned_jobs),
            (1, 12, 12)
        );
        assert!(lifecycle::close_business(&mut sim, company));
        sim.apply_commands([]);
        let closed = sim.growable_summary(id).unwrap();
        assert_eq!(
            (
                closed.occupants,
                closed.active_job_slots,
                closed.planned_jobs
            ),
            (1, 0, 12)
        );
        assert_eq!(closed.housing_slots, open.housing_slots);
        assert_eq!(
            sim.read::<BuildingInfos>()
                .residence_of(SoulID::Human(tenant)),
            Some(id)
        );
    }
}

#[test]
fn zones2_acceptance_replay_reconstructs_growth_and_bulldozing() {
    let mut sim = flat_city();
    let mut sched = Simulation::schedule();
    let root = SlotRoot::new();
    sim.apply_commands([&road(vec3(100.0, 380.0, 0.0), vec3(640.0, 380.0, 0.0))]);
    let area = OBB::new(vec2(360.0, 410.0), Vec2::X, 320.0, 80.0);
    let valid = WorldCommand::ZonePaint {
        area,
        kind: Some(ZoneKind::Residential),
        density: Density::High,
    };
    let invalid = WorldCommand::ZonePaint {
        area,
        kind: Some(ZoneKind::MixedUse),
        density: Density::Low,
    };
    let before = sim.read::<Replay>().command_count();
    sim.apply_commands([&invalid, &valid]);
    assert_eq!(sim.read::<Replay>().command_count(), before + 1);
    for _ in 0..TICKS_PER_HOUR {
        sim.tick(&mut sched, []);
    }
    let building = sim
        .map()
        .buildings()
        .keys()
        .find(|&id| {
            sim.growable_summary(id)
                .is_some_and(|summary| summary.floors >= 12)
        })
        .expect("residential demand must grow at least one tower");
    let origin = *sim.map().lot_origin(building).unwrap();
    let mut resumed = round_trip_and_play(&sim, &root.0, "zones2_before_bulldoze");
    let mut resumed_sched = Simulation::schedule();
    let erase = WorldCommand::ZonePaint {
        area,
        kind: None,
        density: Density::Low,
    };
    let demolish = WorldCommand::MapRemoveBuilding(building);
    sim.tick(&mut sched, [&erase, &demolish]);
    resumed.tick(&mut resumed_sched, [&erase, &demolish]);
    assert_eq!(resumed.hashes(), sim.hashes(), "first step after resume");
    assert!(sim.map().get(building).is_none());
    assert!(sim.read::<ZoneGrid>().cells().is_empty());
    {
        let map = sim.map();
        assert!(map
            .lots()
            .values()
            .any(|lot| lot.parent == origin.parent_road
                && lot.shape.center().distance2(origin.shape.center()) < 0.01));
    }
    let mut resumed = round_trip_and_play(&sim, &root.0, "zones2_after_bulldoze");
    sim.apply_commands([&valid]);
    resumed.apply_commands([&valid]);
    for _ in 0..TICKS_PER_HOUR {
        sim.tick(&mut sched, []);
        resumed.tick(&mut resumed_sched, []);
    }
    assert!(sim
        .map()
        .buildings()
        .keys()
        .any(|id| sim.growable_summary(id).is_some()));
    assert_eq!(
        resumed.hashes(),
        sim.hashes(),
        "first regrowth after resume"
    );
    let mut resumed = round_trip_and_play(&sim, &root.0, "zones2_after_regrowth");
    let (regrown, origin) = {
        let map = sim.map();
        let id = map
            .buildings()
            .keys()
            .find(|&id| {
                sim.growable_summary(id)
                    .is_some_and(|site| site.occupants > 0)
            })
            .expect("the repainted frontage must grow occupied housing");
        (id, *map.lot_origin(id).unwrap())
    };
    let cut = WorldCommand::MapRemoveRoad(origin.parent_road);
    sim.apply_commands([&cut]);
    resumed.apply_commands([&cut]);
    assert_eq!(sim.hashes(), resumed.hashes());
    for _ in 0..TICKS_PER_HOUR {
        sim.tick(&mut sched, []);
        resumed.tick(&mut resumed_sched, []);
    }
    assert_eq!(sim.hashes(), resumed.hashes());
    assert!(matches!(
        sim.growable_summary(regrown).unwrap().site,
        GrowableSiteState::Abandoned { .. }
    ));
    assert_eq!(sim.growable_summary(regrown).unwrap().occupants, 0);
    let mut resumed = round_trip_and_play(&sim, &root.0, "zones2_after_frontage_loss");
    for _ in 0..(13 * TICKS_PER_HOUR) {
        sim.tick(&mut sched, []);
        resumed.tick(&mut resumed_sched, []);
    }
    assert_eq!(sim.hashes(), resumed.hashes());
    {
        let map = sim.map();
        assert!(map.get(regrown).is_none());
        assert!(
            map.lots()
                .values()
                .all(|lot| { lot.shape.center().distance2(origin.shape.center()) >= 0.01 }),
            "collapse must not restore a parcel without frontage"
        );
    }
    round_trip_and_play(&sim, &root.0, "zones2_after_timed_collapse");
}

fn round_trip_and_play(sim: &Simulation, root: &Path, label: &str) -> Simulation {
    let id = SlotId::new(label).unwrap();
    sim.save_to_slot(root, &id, SnapshotRevision::new(1))
        .unwrap();
    let loaded = Simulation::load_from_slot(root, &id).unwrap();
    assert_eq!(loaded.hashes(), sim.hashes(), "{label} snapshot");
    let replay = Simulation::load_replay_from_slot(root, &id).unwrap();
    let (mut playback, mut loader) = Simulation::try_from_replay(replay).unwrap();
    let mut schedule = Simulation::schedule();
    let (_, steps) = loader.step_progress().expect("versioned stepped replay");
    assert!((0..=steps).any(|_| loader.advance_tick(&mut playback, &mut schedule)));
    assert_eq!(loader.step_progress(), Some((steps, steps)));
    assert_eq!(playback.hashes(), sim.hashes(), "{label} replay");
    loaded
}

#[test]
fn zones2_acceptance_save_cut_checks_versions_before_payloads() {
    assert_eq!(crate::VERSION, "0.11.0");
    let root = SlotRoot::new();
    let id = SlotId::new("zones2_p6").unwrap();
    let paths = slot_paths(&root.0, &id).unwrap();
    let obsolete = crate::SlotSnapshotHeader {
        magic: crate::SLOT_SNAPSHOT_MAGIC,
        version: "0.10.0".into(),
        revision: SnapshotRevision::new(5),
    };
    // Both payloads are deliberately undecodable as current simulation data.
    fs::write(&paths.bc, Bincode::encode(&obsolete).unwrap()).unwrap();
    let replay = br#"{"commands":[{"UnknownP6Command":[]}],"version":"0.10.0","revision":5}"#;
    fs::write(&paths.replay, replay).unwrap();
    assert_incompatible(&root.0, &id, "0.10.0");
    let error = crate::ReplayJson::decode::<Replay>(replay).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("Replay has version"));
    assert!(error.to_string().contains("0.10.0"));
    let mut replay = Replay::default();
    replay.version = "0.10.0".into();
    let error = Simulation::try_from_replay(replay).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("Replay has version"));
}

#[test]
#[ignore = "requires a production P5 snapshot and replay pair in EGR_ZONES2_P5_ROOT"]
fn zones2_acceptance_real_p5_save_and_replay_are_refused() {
    let root = PathBuf::from(std::env::var_os("EGR_ZONES2_P5_ROOT").expect("EGR_ZONES2_P5_ROOT"));
    assert_incompatible(&root, &SlotId::new("zones2_p5").unwrap(), "0.9.0");
}

fn assert_incompatible(root: &Path, id: &SlotId, expected_version: &str) {
    let listed = crate::list_slots_with_revisions(root, |path, kind| {
        Simulation::read_slot_snapshot_header(path, kind)
            .unwrap()
            .revision
    })
    .unwrap();
    let entry = listed.iter().find(|entry| entry.id == *id).unwrap();
    let is_old = |error| matches!(error, SaveLoadError::Incompatible { version } if version == expected_version);
    assert!(is_old(Simulation::load_from_slot(root, id).err().unwrap()));
    let header =
        Simulation::read_slot_snapshot_header(entry.snapshot_path(), entry.snapshot).unwrap();
    assert_eq!(header.version, expected_version);
    assert!(matches!(
        Simulation::validate_listed_snapshot(root, entry),
        Err(SaveLoadError::SnapshotChanged)
    ));
    assert!(matches!(
        Simulation::load_listed_snapshot(root, entry),
        Err(SaveLoadError::SnapshotChanged)
    ));
    assert!(is_old(
        Simulation::load_replay_from_slot(root, id).unwrap_err()
    ));
}

#[test]
fn zones2_save_and_replay_reject_a_different_prototype_registry() {
    if !super::in_prototype_fixture_process(
        module_path!(),
        "zones2_save_and_replay_reject_a_different_prototype_registry",
    ) {
        return;
    }

    let mut sim = flat_city();
    sim.apply_commands([&road(vec3(100.0, 180.0, 0.0), vec3(820.0, 180.0, 0.0))]);
    let lot = sim.map().lots().keys().next().unwrap();
    sim.place_zoned_growable(lot, zone(ZoneKind::MixedUse, Density::Medium))
        .unwrap();
    let root = SlotRoot::new();
    let id = SlotId::new("staged_city").unwrap();
    sim.save_to_slot(&root.0, &id, SnapshotRevision::new(1))
        .unwrap();
    let replay = Simulation::load_replay_from_slot(&root.0, &id).unwrap();
    Simulation::load_from_slot(&root.0, &id).unwrap();
    let zip = slot_paths(&root.0, &id).unwrap().zip;
    let header = Simulation::read_slot_snapshot_header(&zip, SnapshotKind::Zip).unwrap();
    assert!(header.check_compatibility().is_ok());

    // The same game version ships with a different registry.
    unsafe { crate::init::load_prototypes() };
    assert!(matches!(
        header.check_compatibility(),
        Err(SaveLoadError::PrototypeRegistry)
    ));
    assert!(matches!(
        Simulation::load_from_slot(&root.0, &id),
        Err(SaveLoadError::PrototypeRegistry)
    ));
    assert!(matches!(
        Simulation::load_replay_from_slot(&root.0, &id),
        Err(SaveLoadError::PrototypeRegistry)
    ));
    assert!(Simulation::try_from_replay(replay).is_err());
}
