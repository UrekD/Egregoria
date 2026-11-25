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

#[cfg(test)]
mod tests {
    use super::*;
    use prototypes::{GameTime, Tick};

    #[test]
    fn econ_food_orders_before_the_last_two_meals() {
        let [groceries, _] = household_items();
        let mut stock = HouseholdStock::default();
        let mut food = BuyFood::new(GameTime::new(Tick(0)).instant());
        stock.on_hand.insert(groceries, 2);
        assert!(food.score(&stock) > 0.0);
        stock.on_hand.insert(groceries, 3);
        assert_eq!(food.score(&stock), 0.0);
    }
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

    pub(crate) fn forget_if(&mut self, removed: impl FnOnce(BuildingID) -> bool) {
        if matches!(self.state, BuyFoodState::BoughtAt(id) if removed(id)) {
            self.state = BuyFoodState::Empty;
        }
    }

    pub(crate) fn cancel_purchase(&mut self) {
        self.state = BuyFoodState::Empty;
    }

    pub(crate) fn score(&mut self, stock: &HouseholdStock) -> f32 {
        let [groceries, retail] = household_items();
        let staples = stock.units(groceries);
        let supply = if staples > 0 {
            staples
        } else {
            stock.units(retail)
        };
        self.last_score = if supply <= 2 {
            0.55 + (1.0 - stock.needs.food) * 0.20
        } else {
            0.0
        };
        self.last_score
    }
}
