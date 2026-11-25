use crate::map::{
    BuildingKind, Density, LanePatternBuilder, MapProject, TerraformKind, TerrainChunkID,
    UpdateType, WaterBasin, WaterBodyId, WaterDirtyChunks, WaterEdit, WaterEditError, WaterStroke,
    ZoneGrid, ZoneKind, ZonePaintError,
};
use crate::map_dynamic::{EnvGrid, UtilityFlow};
use crate::world_command::{CommandOutcomes, Rejection};
use crate::{
    MapPreset, NewGameOptions, Replay, SavePhase, Simulation, SlotId, SnapshotRevision,
    WorldCommand,
};
use common::saveload::{Encoder, JSON};
use geom::{vec2, vec3, Vec2, OBB};
use prototypes::{BuildingGen, FreightStationPrototypeID};
use std::path::PathBuf;

fn lake(x: f32, y: f32) -> WaterEdit {
    WaterEdit::FillLake {
        basin: WaterBasin::new(&[
            vec2(x, y),
            vec2(x + 64.0, y),
            vec2(x + 64.0, y + 64.0),
            vec2(x, y + 64.0),
        ])
        .unwrap(),
        level_dm: 0,
        depth_m: 5.0,
    }
}

fn authored_lake() -> Simulation {
    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 0).unwrap();
    sim.edit_water(lake(512.0, 256.0)).unwrap();
    sim
}

#[test]
fn p6_water_mixed_stamp_previews_and_paints_only_dry_cells_and_erases_both() {
    let mut sim = authored_lake();
    // Columns 62–65 straddle the x=512 m terrain chunk seam and lake edge.
    let stamp = OBB::new(vec2(512.0, 264.0), Vec2::X, 32.0, 16.0);
    let all: Vec<_> = (62..66).flat_map(|x| [(x, 32), (x, 33)]).collect();
    let dry = &all[..4];
    let paint = Some(ZoneKind::Residential);

    let preview =
        ZoneGrid::eligible_paint_cells_with_land(&sim.map(), &stamp, paint, Density::Low, |_| true)
            .unwrap();
    assert_eq!(preview.cells(), dry);
    assert_eq!(preview.replacement().map(|cell| cell.kind), paint);
    let (touched, projected) = sim
        .read::<ZoneGrid>()
        .lot_zone_after_prepared_paint(&stamp, &preview);
    assert!(touched && projected.is_some());
    let generation = sim.read::<ZoneGrid>().generation();
    sim.apply_commands([&WorldCommand::ZonePaint {
        area: stamp,
        kind: paint,
        density: Density::Low,
    }]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    assert_eq!(sim.read::<ZoneGrid>().generation(), generation + 1);
    assert_eq!(
        sim.read::<ZoneGrid>()
            .cells()
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        preview.cells()
    );

    assert_eq!(sim.read::<ZoneGrid>().lot_zone(&stamp), projected);

    let wet_only = WorldCommand::ZonePaint {
        area: OBB::new(vec2(528.0, 264.0), Vec2::X, 16.0, 16.0),
        kind: paint,
        density: Density::Low,
    };
    sim.apply_commands([&wet_only]);
    assert_eq!(
        sim.read::<CommandOutcomes>().rejections_for(0),
        [("ZonePaint", Rejection::WetFootprint)]
    );
    assert_eq!(sim.read::<ZoneGrid>().generation(), generation + 1);

    let preview =
        ZoneGrid::eligible_paint_cells_with_land(&sim.map(), &stamp, None, Density::Low, |_| true)
            .unwrap();
    assert_eq!(preview.cells(), all);
    // Seed legacy wet cells so the erase test observes deletion on both sides.
    sim.write::<ZoneGrid>().paint(stamp, paint, Density::Low);
    assert_eq!(sim.read::<ZoneGrid>().cells().len(), all.len());
    let generation = sim.read::<ZoneGrid>().generation();
    sim.apply_commands([&WorldCommand::ZonePaint {
        area: stamp,
        kind: None,
        density: Density::Low,
    }]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    assert_eq!(sim.read::<ZoneGrid>().generation(), generation + 1);
    assert!(sim.read::<ZoneGrid>().cells().is_empty());
}

#[test]
fn p6_water_locked_wet_seam_precedes_filter_for_paint_and_erase() {
    let sim = authored_lake();
    let stamp = OBB::new(vec2(512.0, 264.0), Vec2::X, 32.0, 16.0);
    let owns = |(x, _)| x < 64;
    sim.write::<ZoneGrid>()
        .paint(stamp, Some(ZoneKind::Commercial), Density::Low);
    let before = sim.hashes();
    for kind in [Some(ZoneKind::Residential), None] {
        assert_eq!(
            ZoneGrid::eligible_paint_cells_with_land(&sim.map(), &stamp, kind, Density::Low, owns),
            Err(ZonePaintError::TileLocked((64, 32)))
        );
        // TODO(TILES): send this stamp through ZonePaint with actual map ownership.
        assert_eq!(
            WorldCommand::apply_zone_paint(&sim, &stamp, kind, Density::Low, owns),
            Err(Rejection::TileLocked)
        );
        assert_eq!(sim.hashes(), before);
    }
}

#[test]
fn p6_water_rejects_special_building_on_water() {
    let mut sim = authored_lake();
    let station = FreightStationPrototypeID::new("freight-station");
    let place = |center| WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(center, Vec2::X, 16.0, 16.0),
        kind: BuildingKind::RailFreightStation(station),
        gen: BuildingGen::NoWalkway {
            door_pos: Vec2::ZERO,
        },
        zone: None,
        connected_road: None,
    };
    let wet = place(vec2(544.0, 288.0));
    let before = sim.hashes();
    sim.apply_commands([&wet]);
    assert_eq!(
        sim.read::<CommandOutcomes>().rejections_for(0),
        [("MapBuildSpecialBuilding", Rejection::WetFootprint)]
    );
    assert_eq!(sim.hashes(), before);
    sim.apply_commands([&place(vec2(448.0, 288.0))]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
}

#[test]
#[ignore = "W2a implements elevated crossings over water"]
fn p6_water_elevated_crossing_connects_opposite_banks() {
    use crate::map::{PathKind, ROAD_Z_OFFSET};
    use crate::map_dynamic::Itinerary;
    use prototypes::Tick;

    let mut sim = authored_lake();
    let from = vec3(384.0, 288.0, 12.0);
    let to = vec3(704.0, 288.0, 12.0);
    let command = WorldCommand::MapMakeConnection {
        from: MapProject::ground(from),
        to: MapProject::ground(to),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    };
    sim.apply_commands([&command]);
    assert!(
        sim.read::<CommandOutcomes>().last.is_empty(),
        "{:?}",
        sim.read::<CommandOutcomes>().last
    );
    let map = sim.map();
    assert_eq!(map.roads().len(), 1);
    let road = map.roads().values().next().unwrap();
    let point = vec2(544.0, 288.0);
    let deck = road.points().project_2d(point);
    let surface = map.environment().water_at(point).unwrap().surface_z;
    assert!(deck.z - ROAD_Z_OFFSET - surface >= 2.5);
    for kind in [PathKind::Vehicle, PathKind::Pedestrian] {
        assert!(Itinerary::route(Tick(sim.get_tick()), from, to, &map, kind).is_some());
    }
}

struct SlotRoot(PathBuf);

impl SlotRoot {
    fn new() -> Self {
        let output = std::process::Command::new("mktemp")
            .arg("-d")
            .arg(std::env::temp_dir().join("egregoria-w5.XXXXXXXXXX"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        Self(
            std::str::from_utf8(&output.stdout)
                .unwrap()
                .strip_suffix('\n')
                .unwrap()
                .into(),
        )
    }
}

impl Drop for SlotRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn replay_bytes(sim: &Simulation) -> Vec<u8> {
    JSON::encode(&*sim.read::<Replay>()).unwrap()
}

fn gameplay_hashes(sim: &Simulation) -> std::collections::BTreeMap<String, u64> {
    let mut hashes = sim.hashes();
    assert!(hashes.remove("replay").is_some());
    hashes
}

fn publish_and_check(sim: &Simulation, root: &SlotRoot, label: &str, steps: u64) -> Simulation {
    let id = SlotId::new(label).unwrap();
    let result = sim
        .encode_slot(SnapshotRevision::new(1))
        .unwrap()
        .publish(&root.0, &id)
        .unwrap();
    assert!(matches!(result, SavePhase::Complete), "{label}: {result:?}");
    let restored = Simulation::load_from_slot(&root.0, &id).unwrap();
    assert_eq!(restored.hashes(), sim.hashes(), "{label}: snapshot");
    assert_eq!(
        replay_bytes(&restored),
        replay_bytes(sim),
        "{label}: snapshot replay"
    );

    let replay = Simulation::load_replay_from_slot(&root.0, &id).unwrap();
    assert_eq!(
        JSON::encode(&replay).unwrap(),
        replay_bytes(sim),
        "{label}: sidecar"
    );
    let (mut replayed, mut loader) = Simulation::try_from_replay(replay).unwrap();
    let mut schedule = Simulation::schedule();
    for _ in 0..=steps {
        if loader.advance_tick(&mut replayed, &mut schedule) {
            assert_eq!(loader.step_progress(), Some((steps, steps)), "{label}");
            assert_eq!(loader.idx, sim.read::<Replay>().command_count(), "{label}");
            assert_eq!(replayed.get_tick(), sim.get_tick(), "{label}");
            assert_eq!(replayed.hashes(), sim.hashes(), "{label}: replay");
            return restored;
        }
    }
    panic!("{label}: replay did not finish after {steps} steps");
}

#[test]
fn p6_water_edit_replays_across_a_published_slot_boundary() {
    super::init_test_simulation();
    let root = SlotRoot::new();
    let options = NewGameOptions {
        preset: MapPreset::Flat,
        terrain_chunks: 16,
        ..NewGameOptions::default()
    };
    let mut sim = Simulation::new_game_with_replay(&options).unwrap();
    publish_and_check(&sim, &root, "water_before", 0);
    let start_tick = sim.get_tick();
    let initial_commands = sim.read::<Replay>().command_count();
    let sea = WorldCommand::EditWater(WaterEdit::SetSeaLevel {
        level_dm: Some(-100),
    });
    let absent = WorldCommand::EditWater(WaterEdit::RemoveFreshWater {
        body: WaterBodyId(99),
    });
    let river = WorldCommand::EditWater(WaterEdit::CarveRiver {
        polyline: WaterStroke::new(&[vec2(6480.0, 1120.0), vec2(6880.0, 1120.0)]).unwrap(),
        width_m: 48.0,
        depth_m: 5.0,
        level_dm: None,
    });
    let basin = WorldCommand::EditWater(lake(6656.0, 1280.0));
    let out_of_bounds = WorldCommand::EditWater(lake(-64.0, 1280.0));

    let WorldCommand::EditWater(edit) = &sea else {
        unreachable!()
    };
    let preview = sim.preview_water_edit(edit).unwrap();
    assert!(preview.changed);
    assert!(!preview.strict_before && !preview.strict_after);
    sim.apply_commands([&sea]);
    let initial_revision = sim.map().environment().water_revision();
    {
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(outcomes.accepted, [0]);
        let outcome = outcomes.water_edit(0).unwrap().as_ref().unwrap();
        assert!(outcome.changed);
        assert!(outcome.dirty.terrain().is_empty());
        assert_eq!(outcome.dirty, preview.dirty);
    }
    assert_eq!(sim.read::<Replay>().command_count(), initial_commands + 1);
    let before = sim.hashes();
    let recorded = replay_bytes(&sim);
    let sequence = sim.read::<CommandOutcomes>().batch_seq();
    let mut dirty = sim
        .map()
        .subscribe(UpdateType::Water | UpdateType::Terrain | UpdateType::RoadSupport);
    sim.apply_commands([&absent]);
    assert!(sim.read::<CommandOutcomes>().batch_seq() > sequence);
    assert_eq!(
        sim.read::<CommandOutcomes>().water_edit(0),
        Some(&Err(WaterEditError::BodyUnavailable(WaterBodyId(99))))
    );
    assert_eq!(sim.hashes(), before);
    assert_eq!(replay_bytes(&sim), recorded);
    assert_eq!(dirty.take_updated_chunks().count(), 0);

    sim.apply_commands([&sea]);
    let retry = sim
        .read::<CommandOutcomes>()
        .water_edit(0)
        .unwrap()
        .as_ref()
        .unwrap()
        .clone();
    assert!(!retry.changed);
    assert_eq!(retry.dirty, WaterDirtyChunks::default());
    assert_eq!(sim.map().environment().water_revision(), initial_revision);
    assert_eq!(replay_bytes(&sim), recorded);
    // Clearing the empty sea keeps both freshwater commands in a local batch.
    sim.edit_water(WaterEdit::SetSeaLevel { level_dm: None })
        .unwrap();
    sim.apply_commands([&river, &out_of_bounds, &basin]);
    {
        let outcomes = sim.read::<CommandOutcomes>();
        assert_eq!(outcomes.accepted, [0, 2]);
        for (index, id) in [(0, 1), (2, 2)] {
            assert_eq!(
                outcomes.water_edit(index).unwrap().as_ref().unwrap().body,
                Some(WaterBodyId(id))
            );
        }
        assert_eq!(
            outcomes.water_edit(1),
            Some(&Err(WaterEditError::OutOfBounds))
        );
    }
    let mut helper = Simulation::new_game_with_replay(&options).unwrap();
    helper.apply_commands([&sea]);
    helper
        .edit_water(WaterEdit::SetSeaLevel { level_dm: None })
        .unwrap();
    assert_eq!(
        helper
            .carve_river(&[vec2(6480.0, 1120.0), vec2(6880.0, 1120.0)], 48.0, 5.0)
            .unwrap(),
        WaterBodyId(1)
    );
    helper.apply_commands([&basin]);
    let commands = |sim: &Simulation| {
        let replay = sim.read::<Replay>();
        replay
            .commands(initial_commands..replay.command_count())
            .map(|command| JSON::encode(command).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(commands(&sim), commands(&helper));
    assert_eq!(commands(&sim).len(), 4);
    assert_eq!(gameplay_hashes(&sim), gameplay_hashes(&helper));
    assert_eq!(sim.get_tick(), start_tick);

    let bank_road = WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(6432.0, 1216.0, 10.0)),
        to: MapProject::ground(vec3(7040.0, 1216.0, 10.0)),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    };
    sim.apply_commands([&bank_road]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    let lots = [vec2(6688.0, 1260.0), vec2(6950.0, 1260.0)].map(|target| {
        sim.map()
            .lots()
            .values()
            .min_by(|a, b| {
                a.shape
                    .center()
                    .distance2(target)
                    .total_cmp(&b.shape.center().distance2(target))
            })
            .unwrap()
            .id
    });
    sim.apply_commands(
        lots.each_ref()
            .map(|lot| WorldCommand::MapBuildHouse(*lot))
            .iter(),
    );
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0, 1]);
    let [bank, inland] = [vec2(6688.0, 1260.0), vec2(6950.0, 1260.0)].map(|target| {
        sim.map()
            .buildings()
            .values()
            .filter(|building| building.kind == BuildingKind::House)
            .min_by(|a, b| {
                a.obb
                    .center()
                    .distance2(target)
                    .total_cmp(&b.obb.center().distance2(target))
            })
            .unwrap()
            .id
    });
    let mut restored = publish_and_check(&sim, &root, "water_painted", 0);
    let mut schedule = Simulation::schedule();
    assert!(restored
        .read::<UtilityFlow>()
        .water_pending(&restored.map()));
    let samples = |sim: &Simulation| {
        let env = sim.read::<EnvGrid>();
        [bank, inland].map(|home| env.land_value_sample(home))
    };
    assert_eq!(samples(&restored), [None, None]);
    let refresh_tick = crate::economy::balance::CITY_UPDATE_INTERVAL_TICKS + EnvGrid::UPDATE_OFFSET;
    let steps_to_refresh = refresh_tick - start_tick;
    for _ in 0..steps_to_refresh {
        restored.tick(&mut schedule, &[]);
    }
    assert!(!restored
        .read::<UtilityFlow>()
        .water_pending(&restored.map()));
    let [Some(bank_value), Some(inland_value)] = samples(&restored) else {
        panic!("homes did not receive land value at the first scheduled refresh");
    };
    assert!(
        bank_value > inland_value,
        "bank={bank_value}, inland={inland_value}"
    );
    let totals = restored
        .read::<UtilityFlow>()
        .status(
            &restored.map(),
            &restored.read::<crate::souls::residence::Residences>(),
            bank,
        )
        .unwrap()
        .totals
        .unwrap();
    assert!(totals.water_demand > 0.0);
    publish_and_check(&restored, &root, "water_refreshed", steps_to_refresh);
    let remove = WorldCommand::EditWater(WaterEdit::RemoveFreshWater {
        body: WaterBodyId(2),
    });
    let lake_bed = |sim: &Simulation| {
        let map = sim.map();
        let env = map.environment();
        std::array::from_fn::<_, 25, _>(|i| {
            env.base_height(vec2(
                6656.0 + 16.0 * (i % 5) as f32,
                1280.0 + 16.0 * (i / 5) as f32,
            ))
        })
    };
    let before_refill = lake_bed(&restored);
    restored.apply_commands([&remove]);
    assert_eq!(samples(&restored), [Some(bank_value), Some(inland_value)]);
    assert!(restored
        .read::<UtilityFlow>()
        .water_pending(&restored.map()));
    restored.apply_commands([&basin]);
    let outcome = restored
        .read::<CommandOutcomes>()
        .water_edit(0)
        .unwrap()
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(outcome.body, Some(WaterBodyId(3)));
    assert!(outcome.changed);
    assert_eq!(lake_bed(&restored), before_refill);
    assert!(outcome
        .dirty
        .water()
        .contains(&TerrainChunkID::new_i16(13, 2)));
    publish_and_check(&restored, &root, "water_refilled", steps_to_refresh);

    let new_lake = WorldCommand::EditWater(lake(7264.0, 1472.0));
    let terraform = WorldCommand::Terraform {
        kind: TerraformKind::Elevation,
        center: vec2(7200.0, 1200.0),
        radius: 32.0,
        amount: -2.0,
        level: 0.0,
        slope: None,
    };
    let roads_before: std::collections::BTreeSet<_> = restored.map().roads().keys().collect();
    let dry_link = [vec3(7216.0, 1280.0, 10.0), vec3(7504.0, 1280.0, 10.0)];
    let indexed = WorldCommand::MapMakeMultipleConnections(
        [dry_link[0], dry_link[1], vec3(6688.0, 1312.0, 10.0)]
            .map(MapProject::ground)
            .into(),
        vec![
            (0, 1, None, LanePatternBuilder::default().build()),
            (0, 2, None, LanePatternBuilder::default().build()),
        ],
    );
    restored.tick(&mut schedule, [&new_lake, &terraform, &indexed]);
    {
        let outcomes = restored.read::<CommandOutcomes>();
        assert_eq!(outcomes.accepted, [0, 1]);
        assert_eq!(
            outcomes.link_outcomes(2),
            [
                (0, Ok(())),
                (
                    1,
                    Err(("MapMakeMultipleConnections", Rejection::WetFootprint))
                )
            ]
        );
    }
    assert_eq!(
        restored
            .read::<CommandOutcomes>()
            .water_edit(0)
            .unwrap()
            .as_ref()
            .unwrap()
            .body,
        Some(WaterBodyId(4))
    );
    let recorded = commands(&restored);
    assert_eq!(recorded.len(), 12);
    assert_eq!(recorded.last(), Some(&JSON::encode(&indexed).unwrap()));
    let loaded = publish_and_check(&restored, &root, "water_final", steps_to_refresh + 1);
    let new_road = |sim: &Simulation| {
        let map = sim.map();
        let roads: Vec<_> = map
            .roads()
            .keys()
            .filter(|id| !roads_before.contains(id))
            .collect();
        assert_eq!(roads.len(), 1);
        let road = map.get(roads[0]).unwrap();
        let ends = [road.src, road.dst].map(|id| map.get(id).unwrap().pos.xy());
        assert_eq!(ends, dry_link.map(|point| point.xy()));
        roads[0]
    };
    assert_eq!(new_road(&restored), new_road(&loaded));
}

#[test]
fn p6_water_waters_starters_are_dry_routable_and_affordable() {
    use crate::economy::{Budget, BudgetCategory, Government};
    use crate::map::{Building, PathKind, WaterKind};
    use crate::map_dynamic::Itinerary;
    use crate::souls::freight_station::train_stop;
    use geom::Transform;
    use prototypes::{Money, Tick};

    super::init_test_simulation();
    for size in [16, 50, 80] {
        for seed in [0, 1, 77] {
            let options = NewGameOptions {
                preset: MapPreset::Waters,
                terrain_chunks: size,
                seed,
                starting_money_bucks: NewGameOptions::MIN_STARTING_MONEY_BUCKS,
                ..NewGameOptions::default()
            };
            let mut city = Simulation::new_game_with_replay(&options)
                .unwrap_or_else(|error| panic!("{options:?}: {error}"));
            assert!(city.read::<Replay>().enabled);
            assert!(city.read::<CommandOutcomes>().last.is_empty());
            if (size, seed) == (16, 0) {
                let mut missing_station = Replay::default();
                missing_station.enabled = true;
                missing_station.push(
                    Tick(city.get_tick()),
                    WorldCommand::Init(Box::new(*city.read::<crate::SimulationOptions>())),
                );
                let error = Simulation::try_from_replay(missing_station).err().unwrap();
                assert!(error.to_string().contains("Replay starters"), "{error}");
            }
            let map = city.map();
            let env = map.environment();
            let width = f32::from(size) * TerrainChunkID::SIZE_F32;
            assert_eq!(env.size(), (size, size));
            assert_eq!(
                env.water_body_kind(vec2(width - 8.0, width * 0.4)),
                Some(WaterKind::Sea)
            );
            let market = map.get(map.external_train_stations[0]).unwrap();
            let station = map
                .buildings()
                .values()
                .find(|building| matches!(building.kind, BuildingKind::RailFreightStation(_)))
                .unwrap();
            for building in [market, station] {
                assert!(Building::dry_footprint(
                    env,
                    &building.obb,
                    building.zone.as_ref()
                ));
            }
            let platform = train_stop(&Transform::new_dir(
                station.obb.center().z(station.height),
                station.obb.axis()[1].z(0.0).normalize(),
            ));
            let depot = station.id;
            let market = market.obb.center().z(market.height);
            for (start, end) in [(platform, market), (market, platform)] {
                assert!(
                    Itinerary::route(Tick(city.get_tick()), start, end, &map, PathKind::Rail)
                        .is_some(),
                    "{options:?}: no starter freight route from {start:?} to {end:?}"
                );
            }
            let money = city.read::<Government>().money;
            let spent = city
                .read::<Budget>()
                .total(BudgetCategory::StarterInfrastructure);
            assert!(money > Money::ZERO && spent < 0, "{options:?}");
            assert_eq!(
                i128::from(money.inner()),
                i128::from(Money::new_bucks(i64::from(options.starting_money_bucks)).inner())
                    + spent,
                "{options:?}"
            );
            drop(map);
            check_starter_freight(&mut city, depot, market, &options);
        }
    }
}

fn check_starter_freight(
    city: &mut Simulation,
    depot: crate::map::BuildingID,
    market: geom::Vec3,
    options: &NewGameOptions,
) {
    use crate::map::LaneKind;
    use crate::souls::freight_station::FreightTrainState::{Arriving, Loading, Moving};
    use prototypes::RollingStockID;

    let command = {
        let map = city.map();
        let lane = map.nearest_lane(market, LaneKind::Rail, None).unwrap();
        WorldCommand::SpawnTrain {
            wagons: vec![
                RollingStockID::new("locomotive"),
                RollingStockID::new("freight-wagon"),
            ],
            lane,
            dist: map.get(lane).unwrap().points.length() * 0.5,
        }
    };
    let mut schedule = Simulation::schedule();
    city.tick(&mut schedule, [&command]);
    assert_eq!(city.read::<CommandOutcomes>().accepted, [0], "{options:?}");
    let train = city.world().trains.keys().next().unwrap();
    let station = city
        .world()
        .freight_stations
        .iter()
        .find_map(|(id, station)| (station.f.building == depot).then_some(id))
        .unwrap();
    city.world_mut_unchecked().freight_stations[station]
        .f
        .accept_export(10);
    let mut loaded = false;
    let mut outbound = false;
    for _ in 0..12_000 {
        city.tick(&mut schedule, &[]);
        let terminal = &city.world().freight_stations[station].f;
        let state = terminal
            .trains
            .iter()
            .find_map(|&(id, state)| (id == train).then_some(state));
        assert!(city.world().trains.contains_key(train));
        if outbound && matches!(state, None | Some(Arriving)) {
            assert_eq!(terminal.waiting_cargo, 0);
            return;
        }
        loaded |= matches!(state, Some(Loading));
        outbound |= loaded && matches!(state, Some(Moving));
    }
    panic!("{options:?}: starter train did not complete its freight round trip (loaded={loaded}, outbound={outbound})");
}

#[test]
#[ignore = "W3 implements live intake and outlet siting"]
fn p6_water_river_accepts_downstream_outlet_and_rejects_upstream_outlet() {
    use prototypes::GoodsCompanyID;

    super::init_test_simulation();
    let mut sim = Simulation::new_authored_flat(2, 0).unwrap();
    sim.edit_water(WaterEdit::CarveRiver {
        polyline: WaterStroke::new(&[vec2(128.0, 480.0), vec2(896.0, 480.0)]).unwrap(),
        width_m: 64.0,
        depth_m: 5.0,
        level_dm: Some(0),
    })
    .unwrap();
    let road = WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(192.0, 384.0, 10.0)),
        to: MapProject::ground(vec3(832.0, 384.0, 10.0)),
        inter: None,
        pat: LanePatternBuilder::default().build(),
    };
    sim.apply_commands([&road]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    let road = sim.map().roads().keys().next().unwrap();
    let site = |name, x| WorldCommand::MapBuildSpecialBuilding {
        pos: OBB::new(vec2(x, 408.0), Vec2::Y, 20.0, 20.0),
        kind: BuildingKind::GoodsCompany(GoodsCompanyID::new(name)),
        gen: BuildingGen::CenteredDoor {
            vertical_factor: 1.0,
        },
        zone: None,
        connected_road: Some(road),
    };

    sim.apply_commands([&site("water-pump", 512.0)]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
    let before = sim.hashes();
    sim.apply_commands([&site("sewage-outlet", 320.0)]);
    let outcomes = sim.read::<CommandOutcomes>();
    let [(command, reason)] = outcomes.rejections_for(0) else {
        panic!(
            "expected one typed outlet order rejection, got {:?}",
            outcomes.last
        );
    };
    assert_eq!(*command, "MapBuildSpecialBuilding");
    // The W3/W0 handoff adds this typed variant to Rejection.
    assert!(format!("{reason:?}").starts_with("WaterServiceSite(OrderConflict { role: Outlet,"));
    drop(outcomes);
    assert_eq!(sim.hashes(), before);
    sim.apply_commands([&site("sewage-outlet", 704.0)]);
    assert_eq!(sim.read::<CommandOutcomes>().accepted, [0]);
}
