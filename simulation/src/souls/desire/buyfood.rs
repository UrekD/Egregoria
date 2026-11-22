use egui_inspect::Inspect;
use prototypes::GameInstant;
use serde::{Deserialize, Serialize};

use crate::map::BuildingID;
use crate::souls::household::{household_items, HouseholdStock};

#[derive(Clone, Serialize, Deserialize, Debug)]
enum BuyFoodState {
    Empty,
    WaitingForTrade,
    BoughtAt(BuildingID),
}

debug_inspect_impl!(BuyFoodState);

#[derive(Inspect, Clone, Serialize, Deserialize, Debug)]
pub struct BuyFood {
    pub last_ate: GameInstant,
    state: BuyFoodState,
    pub last_score: f32,
}

impl BuyFood {
    pub fn new(start: GameInstant) -> Self {
        Self {
            last_ate: start,
            state: BuyFoodState::Empty,
            last_score: 0.0,
        }
    }

    pub(crate) fn forget_building(&mut self, building: BuildingID) {
        if matches!(self.state, BuyFoodState::BoughtAt(id) if id == building) {
            self.state = BuyFoodState::Empty;
        }
    }

    pub(crate) fn cancel_purchase(&mut self) {
        self.state = BuyFoodState::Empty;
    }

    pub(crate) fn score(&mut self, stock: &HouseholdStock) -> f32 {
        let [groceries, retail] = household_items();
        let supply = stock.units(groceries) + stock.units(retail);
        self.last_score = if supply <= 1 && stock.needs.food < 0.85 {
            0.55 + (1.0 - stock.needs.food) * 0.20
        } else {
            0.0
        };
        self.last_score
    }
}
