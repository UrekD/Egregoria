use super::plan::PlanTransform;
use super::*;
use simulation::map::{TileExtent, TilePurchased};

#[test]
fn land_tiles_hit_test_rectangular_plan_and_edges() {
    let extent = TileExtent {
        width: 8,
        height: 6,
    };
    let plan = PlanTransform::new(extent, Vec2::new(400.0, 300.0));
    assert_eq!(
        plan.tile_at(Vec2::new(0.0, 0.0)),
        Some(LandTileId { x: 0, y: 5 })
    );
    assert_eq!(
        plan.tile_at(Vec2::new(399.9, 299.9)),
        Some(LandTileId { x: 7, y: 0 })
    );
    assert_eq!(
        plan.tile_at(Vec2::new(50.0, 50.0)),
        Some(LandTileId { x: 1, y: 4 })
    );
    assert_eq!(plan.tile_at(Vec2::new(400.0, 20.0)), None);
    assert_eq!(plan.tile_at(Vec2::new(20.0, 300.0)), None);
    assert_eq!(plan.tile_at(Vec2::new(-0.1, 0.0)), None);
}

#[test]
fn land_tiles_receipt_must_match_pending_purchase() {
    let tile = LandTileId { x: 9, y: 8 };
    let pending = PendingPurchase {
        generation: 3,
        after_sequence: 7,
        command_index: 2,
        tile,
        batch: Some(BatchId {
            generation: 3,
            sequence: 12,
        }),
    };
    let receipt = TilePurchaseReceipt {
        generation: 3,
        frame: 10,
        batch_sequence: 8,
        command_index: 2,
        tile,
        result: Ok(TilePurchased {
            tile,
            paid: Money::new_bucks(10_500),
        }),
    };
    assert!(!pending.matches(&receipt, None));
    assert!(!pending.matches(
        &receipt,
        Some(BatchId {
            generation: 3,
            sequence: 11
        })
    ));
    let batch = pending.batch;
    assert!(!pending.matches(
        &TilePurchaseReceipt {
            generation: 2,
            ..receipt
        },
        batch
    ));
    assert!(!pending.matches(
        &TilePurchaseReceipt {
            batch_sequence: 7,
            ..receipt
        },
        batch
    ));
    assert!(!pending.matches(
        &TilePurchaseReceipt {
            command_index: 0,
            ..receipt
        },
        batch
    ));
    assert!(pending.matches(&receipt, batch));

    let network = PendingPurchase {
        batch: None,
        ..pending
    };
    assert!(network.matches(
        &TilePurchaseReceipt {
            command_index: 11,
            ..receipt
        },
        None
    ));
    assert!(!network.matches(
        &TilePurchaseReceipt {
            tile: LandTileId { x: 8, y: 9 },
            ..receipt
        },
        None
    ));
}

#[test]
fn land_tiles_confirmation_respects_backpressure_and_waits_for_authority() {
    use crate::session::SessionScreen;
    use crate::uiworld::SaveLoadState;

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let uiw = UiWorld::with_defaults();
    *uiw.write::<SessionScreen>() = SessionScreen::Playing;
    let sim = Simulation::new(false);
    let tile = LandTileId { x: 8, y: 9 };
    let price = Money::new_bucks(10_000);
    let mut state = LandTilesUi::default();
    state.select(tile);

    state.submit(&uiw, &sim, tile, price);
    assert!(uiw.commands().is_empty());
    assert_eq!(state.confirmation, Some(price));
    uiw.write::<SaveLoadState>().authoring_blocked = true;
    state.submit(&uiw, &sim, tile, price);
    assert!(uiw.commands().is_empty());
    uiw.write::<SaveLoadState>().authoring_blocked = false;
    state.submit(&uiw, &sim, tile, price);
    state.submit(&uiw, &sim, tile, price);
    assert_eq!(uiw.commands().as_ref().len(), 1);
    assert!(state.pending.is_some());
    assert!(state.receipt.is_none());

    let receipt = TilePurchaseReceipt {
        generation: 0,
        frame: 1,
        batch_sequence: sim.read::<CommandOutcomes>().batch_seq() + 1,
        command_index: 0,
        tile,
        result: Err(Rejection::InsufficientFunds),
    };
    state.receive(&[receipt], None);
    assert!(state.pending.is_none());
    assert_eq!(state.receipt, Some(receipt));
    assert_eq!(
        purchase_error(Rejection::InsufficientFunds, PopulationScale::REALISTIC),
        "Not enough money"
    );
    assert!(purchase_error(
        Rejection::Locked { milestone: 1 },
        PopulationScale::try_new(8).unwrap()
    )
    .contains("800 residents"));
}

#[test]
fn land_tiles_hit_test_last_representable_point() {
    for (width, height) in [(19, 19), (18, 17), (17, 18)] {
        let plan = PlanTransform::new(TileExtent { width, height }, Vec2::splat(440.0));
        let inside = Vec2::new(
            f32::from_bits(plan.size.x.to_bits() - 1),
            f32::from_bits(plan.size.y.to_bits() - 1),
        );
        assert_eq!(
            plan.tile_at(inside),
            Some(LandTileId { x: width - 1, y: 0 })
        );
        assert_eq!(plan.tile_at(Vec2::new(plan.size.x, inside.y)), None);
        assert_eq!(plan.tile_at(Vec2::new(inside.x, plan.size.y)), None);
    }
}

#[test]
fn land_tiles_open_preserves_the_held_shortcut() {
    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let uiw = UiWorld::with_defaults();
    let mut physical = engine::InputContext::default();
    physical.keyboard.pressed.insert(engine::Key::c("L"));
    let sim = native_fixture();
    uiw.write::<InputMap>()
        .build_input_tree(&mut uiw.write::<Bindings>());
    let prepare = |input: &engine::InputContext| {
        let pressed = uiw
            .write::<InputMap>()
            .land_tiles_pressed(input, true, true);
        latch_shortcut(&uiw, true, pressed);
    };
    // A busy frame sees the press and a second frame sees the release.
    prepare(&physical);
    prepare(&engine::InputContext::default());
    assert!(shortcut(&uiw, &sim));
    assert!(uiw.read::<GuiState>().windows.land_tiles_open);
    assert!(!shortcut(&uiw, &sim));
    prepare(&physical);
    assert!(shortcut(&uiw, &sim));
    prepare(&physical);
    assert!(!shortcut(&uiw, &sim));
    assert!(!uiw.read::<GuiState>().windows.land_tiles_open);
}

fn native_fixture() -> Simulation {
    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    Simulation::new_game(&simulation::NewGameOptions {
        preset: simulation::MapPreset::Flat,
        land_mode: simulation::map::LandMode::Tiled,
        terrain_chunks: 17,
        progression: ProgressionMode::FreeBuild,
        ..simulation::NewGameOptions::default()
    })
    .unwrap()
}

#[test]
fn land_tiles_point_hint_preserves_specific_tool_error() {
    let sim = native_fixture();
    let uiw = UiWorld::with_defaults();
    *uiw.write::<Tool>() = Tool::Terraforming;
    uiw.write::<InputMap>().unprojected = Some(vec2(4700.0, 4200.0).z(0.0));
    *uiw.write::<ErrorTooltip>() = ErrorTooltip::new("Reserved footprint");
    show_unowned_hover(&sim, &uiw);
    assert_eq!(
        uiw.read::<ErrorTooltip>().msg.as_deref(),
        Some("Reserved footprint")
    );
}

#[test]
fn land_tiles_generation_change_clears_stale_pending_purchase() {
    let tile = LandTileId { x: 9, y: 8 };
    let mut state = LandTilesUi {
        generation: 3,
        selected: Some(tile),
        pending: Some(PendingPurchase {
            generation: 3,
            after_sequence: 7,
            command_index: 0,
            tile,
            batch: None,
        }),
        ..Default::default()
    };
    state.sync_generation(4);
    assert_eq!(state.generation, 4);
    assert!(state.selected.is_none() && state.pending.is_none() && state.receipt.is_none());
}
#[test]
fn land_tiles_unowned_frontier_has_approaches_from_owned_road() {
    let sim = native_fixture();
    let map = sim.map();
    let land = map.land_ownership();
    let approaches = map
        .preview_land_approaches(LandTileId { x: 9, y: 7 })
        .unwrap();
    assert!(approaches
        .iter()
        .any(|&point| land.tile_at(point).is_some_and(|tile| land.owns(tile))));
    assert!(map
        .preview_land_approaches(LandTileId { x: 7, y: 7 })
        .unwrap()
        .is_empty());
}

#[test]
fn land_tiles_terraform_checks_packed_support_beyond_owned_cursor() {
    use crate::gui::terraforming::{terraforming, TerraformingResource};
    use crate::gui::{ErrorTooltip, Tool};
    use simulation::map::{LandDenial, TerraformKind};

    let mut sim = native_fixture();
    let uiw = UiWorld::with_defaults();
    *uiw.write::<Tool>() = Tool::Terraforming;
    let point = vec2(4600.0, 4200.0).z(0.0);
    uiw.write::<InputMap>().unprojected = Some(point);
    uiw.write::<InputMap>().act.insert(InputAction::Select);
    let command = {
        let resource = uiw.read::<TerraformingResource>();
        WorldCommand::Terraform {
            kind: TerraformKind::Elevation,
            center: point.xy(),
            radius: resource.radius,
            amount: resource.amount,
            level: 0.0,
            slope: None,
        }
    };
    let reason = command.preview(&sim).unwrap_err();
    assert!(matches!(
        reason,
        Rejection::LandDenied(LandDenial::Unowned(_))
    ));
    command.apply(&mut sim);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0)[0].1, reason);
    terraforming(&sim, &uiw);
    assert_eq!(
        uiw.read::<ErrorTooltip>().msg.as_deref(),
        Some(crate::gui::notifications::command_message("Terraform", reason).as_ref())
    );
    assert!(uiw.commands().is_empty());
}

#[test]
fn land_tiles_train_checks_reserved_rail_at_owned_cursor() {
    use crate::gui::addtrain::{addtrain, TrainSpawnResource};
    use crate::gui::{ErrorTooltip, Tool};
    use simulation::map::{LandDenial, LaneKind, ProjectKind};

    let mut sim = native_fixture();
    let uiw = UiWorld::with_defaults();
    *uiw.write::<Tool>() = Tool::Train;
    let wagons = vec![prototypes::RollingStockID::new("locomotive")];
    {
        let mut train = uiw.write::<TrainSpawnResource>();
        train.wagons.clone_from(&wagons);
        train.calculate();
    }
    let (point, command) = {
        let map = sim.map();
        let land = map.land_ownership();
        let road = map
            .roads()
            .values()
            .find(|road| {
                land.is_protected(ProjectKind::Road(road.id))
                    && road.lanes_iter().any(|(_, kind)| kind == LaneKind::Rail)
            })
            .unwrap();
        let center = vec2(4200.0, 4424.0).z(0.0);
        let lane = map
            .get(
                map.nearest_lane(center, LaneKind::Rail, Some(20.0))
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(lane.parent, road.id);
        let point = lane.points.project(center);
        assert!(land.owns(land.tile_at(point.xy()).unwrap()));
        (
            point,
            WorldCommand::SpawnTrain {
                wagons,
                lane: lane.id,
                dist: lane.points.length_at_proj(point),
            },
        )
    };
    let reason = command.preview(&sim).unwrap_err();
    assert!(matches!(
        reason,
        Rejection::LandDenied(LandDenial::ProtectedGateway)
    ));
    command.apply(&mut sim);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0)[0].1, reason);
    uiw.write::<InputMap>().unprojected = Some(point);
    uiw.write::<InputMap>().just_act.insert(InputAction::Select);
    addtrain(&sim, &uiw);
    assert_eq!(
        uiw.read::<ErrorTooltip>().msg.as_deref(),
        Some(crate::gui::notifications::command_message("SpawnTrain", reason).as_ref())
    );
    assert!(uiw.commands().is_empty());
}

#[test]
fn land_tiles_overview_statuses_match_authoritative_quote_under_balance_changes() {
    let sim = native_fixture();
    let extent = sim.map().land_ownership().extent().unwrap();
    for balance in [Money::MAX, Money::ZERO, Money::new_inner(i64::MIN)] {
        sim.write::<Government>().money = balance;
        let expected: Vec<_> = extent
            .tiles()
            .map(|tile| {
                quote_tile(&sim, tile).map_or(TileStatus::Unavailable, |quote| quote.status)
            })
            .collect();
        assert_eq!(quote_tile_statuses(&sim), expected);
    }
}

#[test]
fn land_tiles_bulldozer_checks_protected_road_at_owned_cursor() {
    let mut sim = native_fixture();
    let uiw = UiWorld::with_defaults();
    *uiw.write::<Tool>() = Tool::Bulldozer;
    let point = vec2(4200.0, 4424.0).z(0.0);
    let command = {
        use simulation::map::{ProjectFilter, ProjectKind};
        let map = sim.map();
        let hit = map.project(point, 0.0, ProjectFilter::ROAD);
        let ProjectKind::Road(road) = hit.kind else {
            panic!("expected the rail corridor");
        };
        WorldCommand::MapRemoveRoad(road)
    };
    let reason = command.preview(&sim).unwrap_err();
    command.apply(&mut sim);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0)[0].1, reason);
    uiw.write::<InputMap>().unprojected = Some(point);
    uiw.write::<InputMap>().just_act.insert(InputAction::Select);
    crate::gui::bulldozer::bulldozer(&sim, &uiw);
    assert_eq!(
        uiw.read::<ErrorTooltip>().msg.as_deref(),
        Some(crate::gui::notifications::command_message("MapRemoveRoad", reason).as_ref())
    );
    assert!(uiw.commands().is_empty());
}

#[test]
fn land_tiles_zoning_stroke_exposes_the_denial_for_an_independent_stamp() {
    use geom::{Vec2, OBB};
    use simulation::map::{Density, ZoneGrid, ZoneKind, ZonePaintError};

    let sim = native_fixture();
    let map = sim.map();
    let legal = OBB::new(vec2(4500.0, 4000.0), Vec2::X, 24.0, 24.0);
    let seam = OBB::new(vec2(4606.0, 4000.0), Vec2::X, 24.0, 24.0);
    let kind = Some(ZoneKind::Residential);
    let (preview, denial) =
        ZoneGrid::preview_paint_stroke(&map, &[legal, seam], kind, Density::Low).unwrap();
    let single = ZoneGrid::eligible_paint_cells(&map, &legal, kind, Density::Low).unwrap();
    assert_eq!(preview.cells(), single.cells());
    assert!(matches!(denial, Some(ZonePaintError::TileLocked(_))));
    drop(map);
    let result = WorldCommand::ZonePaint {
        area: seam,
        kind,
        density: Density::Low,
    }
    .preview(&sim);
    assert!(matches!(result, Err(Rejection::TileLocked)));
}

#[test]
fn land_tiles_transit_preview_uses_the_committed_walk_and_berth() {
    use simulation::map::{LaneKind, ProjectKind};
    use simulation::map_dynamic::{transit::preview_stop, Transit};

    let mut sim = native_fixture();
    let (pos, dir) = {
        let map = sim.map();
        let road = map
            .roads()
            .values()
            .find(|road| {
                !map.land_ownership()
                    .is_protected(ProjectKind::Road(road.id))
                    && road.has_sidewalks()
            })
            .unwrap();
        let &(id, _) = road
            .outgoing_lanes_from(road.src)
            .iter()
            .rfind(|(_, kind)| *kind == LaneKind::Driving)
            .unwrap();
        let lane = map.get(id).unwrap();
        lane.points.point_dir_along(lane.points.length() * 0.5)
    };
    let (walk, berth, _) = preview_stop(&sim, pos, dir.xy()).unwrap();
    WorldCommand::TransitAddStop { pos, dir: dir.xy() }.apply(&mut sim);
    let transit = sim.read::<Transit>();
    let stop = transit.stops.values().next().unwrap();
    assert_eq!(stop.walk_pos, walk);
    assert_eq!(stop.berth, berth);
}

#[test]
fn land_tiles_lot_brush_reports_the_prepared_house_denial() {
    use crate::gui::lotbrush::{lotbrush, LotBrushResource};

    let mut sim = native_fixture();
    let uiw = UiWorld::with_defaults();
    *uiw.write::<Tool>() = Tool::LotBrush;
    uiw.write::<LotBrushResource>().radius = 1.0;
    sim.write::<Government>().money = Money::ZERO;
    let (point, command) = {
        let map = sim.map();
        let lot = map.lots().values().next().unwrap();
        (
            lot.shape.center().z(0.0),
            WorldCommand::MapBuildHouse(lot.id),
        )
    };
    let reason = command.preview(&sim).unwrap_err();
    assert_eq!(reason, Rejection::InsufficientFunds);
    command.apply(&mut sim);
    assert_eq!(sim.read::<CommandOutcomes>().rejections_for(0)[0].1, reason);
    uiw.write::<InputMap>().unprojected = Some(point);
    uiw.write::<InputMap>().act.insert(InputAction::Select);
    lotbrush(&sim, &uiw);
    assert_eq!(
        uiw.read::<ErrorTooltip>().msg.as_deref(),
        Some(crate::gui::notifications::command_message("MapBuildHouse", reason).as_ref())
    );
    assert!(uiw.commands().is_empty());
}
