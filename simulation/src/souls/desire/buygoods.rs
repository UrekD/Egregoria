use crate::economy::Bought;
use crate::map_dynamic::BuildingInfos;
use crate::souls::human::HumanDecisionKind;
use crate::transportation::Location;
use crate::{HumanEnt, HumanID, ParCommandBuffer};
use geom::Transform;
use prototypes::GameTime;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuyGoods;

debug_inspect_impl!(BuyGoods);

impl BuyGoods {
    pub fn score(&self, _: &GameTime, _: HumanID, _: &Location, _: &Bought) -> f32 {
        f32::NEG_INFINITY
    }

    pub fn apply(
        &mut self,
        _: &ParCommandBuffer<HumanEnt>,
        _: &BuildingInfos,
        _: &GameTime,
        _: HumanID,
        _: &Transform,
        _: &Location,
        _: &mut Bought,
    ) -> HumanDecisionKind {
        HumanDecisionKind::Yield
    }
}

#[test]
#[ignore = "P1c growable companies and retail behaviour"]
fn p1c_growable_companies_and_retail() {
    // Produce goods, buy them at commercial companies, buy and collect retail goods,
    // export office services, guard the price graph, and keep growables out of the menu.
    todo!();
}
