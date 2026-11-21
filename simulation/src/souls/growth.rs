use crate::map::{BuildingID, BuildingKind};
use crate::souls::residence;
use crate::Simulation;

pub fn growth_update(_: &mut Simulation) {}

/// Called after a growable has been placed successfully.
pub fn on_growable_placed(sim: &Simulation, building: BuildingID, kind: BuildingKind) {
    if kind == BuildingKind::House {
        residence::on_house_built(sim, building);
    }
}

/// Called after a growable has been removed successfully.
pub fn on_growable_removed(sim: &mut Simulation, building: BuildingID) {
    residence::on_building_removed(sim, building);
}

#[test]
#[ignore = "P1b growth and residence behaviour"]
fn p1b_growth_and_residence_lifecycle() {
    // Grow zoned houses and fill capacity over passes, level up after six checks,
    // clean references on abandonment, restore lots on collapse/player removal,
    // and verify walking both without a car and when destination parking fails.
    todo!();
}
