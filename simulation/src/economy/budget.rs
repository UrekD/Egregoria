use crate::economy::Government;
use crate::map::ZoneKind;
use crate::{utils::resources::Resources, World};
use prototypes::Money;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum BudgetCategory {
    Construction,
    CitizenUpkeepLegacy,
    TradeImport,
    TradeExport,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Budget {
    // Cumulative turnover can exceed the range of the treasury balance.
    totals: BTreeMap<BudgetCategory, i128>,
}

impl Budget {
    /// Record a signed change in the treasury, with income positive.
    pub fn record(&mut self, category: BudgetCategory, delta: Money) {
        if delta != Money::ZERO {
            *self.totals.entry(category).or_default() += i128::from(delta.inner());
        }
    }

    /// Cumulative amount in the same inner units as `Money`.
    pub fn total(&self, category: BudgetCategory) -> i128 {
        self.totals.get(&category).copied().unwrap_or_default()
    }

    pub fn set_tax_rate(&mut self, _: ZoneKind, _: u16) {}

    pub(crate) fn apply(resources: &Resources, category: BudgetCategory, delta: Money) {
        if delta != Money::ZERO {
            let mut government = resources.write::<Government>();
            let mut budget = resources.write::<Self>();
            government.money += delta;
            budget.record(category, delta);
        }
    }
}

pub fn budget_update(_: &mut World, _: &mut Resources) {}

#[test]
#[ignore = "P1d budget behaviour"]
fn p1d_budget_and_tax_rates() {
    // Reconcile the ledger, verify hourly accrual and tax bounds, and run budget_day.
    todo!();
}
