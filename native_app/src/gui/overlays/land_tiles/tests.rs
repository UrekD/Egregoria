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
        purchase_error(Rejection::InsufficientFunds),
        "Not enough money"
    );
    assert!(purchase_error(Rejection::Locked { milestone: 1 }).contains("100 residents"));
}
