use std::collections::BTreeMap;

use geom::{vec2, Vec2};
use prototypes::{try_prototype, ServiceKind};
use serde::{Deserialize, Serialize};

use crate::map::{Building, BuildingID, BuildingKind};
use crate::map_dynamic::{BuildingInfos, CoverageGrid, UtilityFlow};
use crate::souls::residence::Residences;
use crate::{Simulation, SoulID};

const UPDATE_INTERVAL: u64 = 3_000;
const POLLUTION_RADIUS: f32 = 160.0;
const POLLUTION_RETENTION: f32 = 0.9;
const MIN_POLLUTION: f32 = 1.0;
const BASE_LAND_VALUE: f32 = 20.0 + 10.0;
const FULL_COVERAGE_BONUS: f32 = 15.0;
const UTILITIES_BONUS: f32 = 10.0;

#[derive(Default, Serialize, Deserialize)]
pub struct EnvGrid {
    pub cells: BTreeMap<(i32, i32), EnvCell>,
    by_building: BTreeMap<BuildingID, u8>,
}

impl EnvGrid {
    pub const CELL_SIZE: f32 = 32.0;
    pub(crate) const UPDATE_OFFSET: u64 = 2;

    pub(crate) const fn due(tick: u64) -> bool {
        tick >= UPDATE_INTERVAL && tick % UPDATE_INTERVAL == Self::UPDATE_OFFSET
    }

    /// Pollution in the cell containing `pos`, or zero for an empty cell.
    pub fn pollution_at(&self, pos: Vec2) -> f32 {
        self.cells
            .get(&Self::cell_at(pos))
            .map_or(0.0, |cell| cell.pollution)
    }

    /// Most recent land value for `building`, or zero if it has no entry.
    pub fn land_value(&self, building: BuildingID) -> u8 {
        self.by_building.get(&building).copied().unwrap_or_default()
    }

    fn cell_at(pos: Vec2) -> (i32, i32) {
        let cell = (pos / Self::CELL_SIZE).floor();
        (cell.x as i32, cell.y as i32)
    }

    fn add_pollution(&mut self, pos: Vec2, strength: f32) {
        let radius = vec2(POLLUTION_RADIUS, POLLUTION_RADIUS);
        let (min_x, min_y) = Self::cell_at(pos - radius);
        let (max_x, max_y) = Self::cell_at(pos + radius);

        for x in min_x..=max_x {
            for y in min_y..=max_y {
                let center = vec2(x as f32 + 0.5, y as f32 + 0.5) * Self::CELL_SIZE;
                let distance = center.distance(pos);
                if distance < POLLUTION_RADIUS {
                    let amount = strength * (1.0 - distance / POLLUTION_RADIUS);
                    let cell = self.cells.entry((x, y)).or_default();
                    cell.pollution = (cell.pollution + amount).min(f32::MAX);
                }
            }
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct EnvCell {
    pub pollution: f32,
}

pub fn env_update(sim: &mut Simulation) {
    if !EnvGrid::due(sim.get_tick()) {
        return;
    }
    profiling::scope!("map_dynamic::env_update");

    let map = sim.map();
    let infos = sim.read::<BuildingInfos>();
    let residences = sim.read::<Residences>();
    let coverage = sim.read::<CoverageGrid>();
    let utilities = sim.read::<UtilityFlow>();
    let mut env = sim.write::<EnvGrid>();

    // The building slot map iterates in slot order, including after reload.
    for building in map.buildings().values() {
        let pollution = match building.kind {
            BuildingKind::House => residences
                .by_building
                .get(&building.id)
                .and_then(|home| try_prototype(home.prototype))
                .and_then(|proto| proto.pollution),
            BuildingKind::GoodsCompany(company) => {
                let Some(proto) = try_prototype(company) else {
                    continue;
                };
                let Some(strength) = proto.pollution else {
                    continue;
                };
                let Some(SoulID::GoodsCompany(owner)) = infos.owner(building.id) else {
                    continue;
                };
                sim.world.companies.get(owner).map(|company| {
                    strength * company.raw_productivity(proto, building.zone.as_ref())
                })
            }
            _ => None,
        };
        if let Some(strength) = pollution.filter(|strength| strength.is_finite() && *strength > 0.0)
        {
            env.add_pollution(building.obb.center(), strength);
        }
    }
    env.cells.retain(|_, cell| {
        cell.pollution *= POLLUTION_RETENTION;
        cell.pollution >= MIN_POLLUTION
    });

    env.by_building
        .retain(|&id, _| map.get(id).is_some_and(has_land_value));
    for building in map.buildings().values() {
        if !has_land_value(building) {
            continue;
        }

        let id = building.id;
        let coverage_bonus = [
            ServiceKind::Park,
            ServiceKind::School,
            ServiceKind::Health,
            ServiceKind::Police,
        ]
        .into_iter()
        .map(|kind| coverage.coverage(id, kind) / 100.0 * FULL_COVERAGE_BONUS)
        .sum::<f32>();
        let utilities_bonus = if utilities.has_water(id) && utilities.has_sewage(id) {
            UTILITIES_BONUS
        } else {
            0.0
        };
        let pollution = env.pollution_at(building.obb.center());
        let land_value = BASE_LAND_VALUE + coverage_bonus + utilities_bonus - pollution / 2.0;
        env.by_building
            .insert(id, land_value.clamp(0.0, 100.0) as u8);
    }
}

fn has_land_value(building: &Building) -> bool {
    match building.kind {
        BuildingKind::House => true,
        BuildingKind::GoodsCompany(company) => company.is_growable(),
        _ => false,
    }
}
