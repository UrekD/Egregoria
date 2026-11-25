use std::collections::BTreeMap;

use geom::{vec2, Vec2};
use ordered_float::OrderedFloat;
use prototypes::{try_prototype, ServiceKind, Tick};
use serde::{Deserialize, Serialize};

use crate::economy::balance::{periodic_tick, CITY_UPDATE_INTERVAL_TICKS};
use crate::economy::Budget;
use crate::map::{Building, BuildingID, BuildingKind, Lot, Map};
use crate::map_dynamic::{BuildingInfos, CoverageGrid, UtilityFlow};
use crate::souls::residence::Residences;
use crate::{Simulation, SoulID};

pub const POLLUTION_RADIUS: f32 = 160.0;
const POLLUTION_RETENTION: f32 = 0.9;
const MIN_POLLUTION: f32 = 1.0;
const BASE_LAND_VALUE: f32 = 20.0 + 10.0;
const FULL_COVERAGE_BONUS: f32 = 15.0;
const UTILITIES_BONUS: f32 = 10.0;

#[derive(Default, Serialize, Deserialize)]
pub struct EnvGrid {
    pub cells: BTreeMap<(i32, i32), EnvCell>,
    by_building: BTreeMap<BuildingID, u8>,
    #[serde(default)]
    service_access: BTreeMap<BuildingID, f32>,
}

impl EnvGrid {
    pub const CELL_SIZE: f32 = 32.0;
    pub(crate) const UPDATE_OFFSET: u64 = 2;

    pub(crate) const fn due(tick: u64) -> bool {
        tick >= CITY_UPDATE_INTERVAL_TICKS
            && tick % CITY_UPDATE_INTERVAL_TICKS == Self::UPDATE_OFFSET
    }

    /// Pollution in the cell containing `pos`, or zero for an empty cell.
    pub fn pollution_at(&self, pos: Vec2) -> f32 {
        self.cells
            .get(&Self::cell_at(pos))
            .map_or(0.0, |cell| cell.pollution)
    }

    /// Most recent land value for `building`, or zero if it has no entry.
    pub fn land_value(&self, building: BuildingID) -> u8 {
        self.land_value_sample(building).unwrap_or_default()
    }

    /// Most recent land value for a building with a recorded environment sample.
    pub fn land_value_sample(&self, building: BuildingID) -> Option<u8> {
        self.by_building.get(&building).copied()
    }

    /// Estimate an empty lot from its nearest measured frontage neighbour and local conditions.
    pub fn initial_lot_value(&self, map: &Map, lot: &Lot) -> u8 {
        const RADIUS_SQUARED: f32 = 96.0 * 96.0;
        let center = lot.shape.center();
        let service_bonus = map
            .get(lot.parent)
            .into_iter()
            .flat_map(|road| road.connected_buildings.iter().copied())
            .filter_map(|id| {
                let building = map.get(id)?;
                let bonus = *self.service_access.get(&id)?;
                let distance = building.obb.center().distance2(center);
                (distance <= RADIUS_SQUARED && building.connected_road == Some(lot.parent))
                    .then_some((OrderedFloat(distance), id, bonus))
            })
            .min_by_key(|&(distance, id, _)| (distance, id))
            .map_or(0.0, |(_, _, bonus)| bonus);
        self.value_at(map, center, service_bonus, 0.0)
    }

    fn value_at(&self, map: &Map, center: Vec2, coverage_bonus: f32, utilities_bonus: f32) -> u8 {
        let value =
            BASE_LAND_VALUE + coverage_bonus + utilities_bonus + shoreline_value_bonus(map, center)
                - self.pollution_at(center) / 2.0;
        value.clamp(0.0, 100.0) as u8
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
    let tick = periodic_tick(&sim.read::<Budget>(), Tick(sim.get_tick()));
    if !EnvGrid::due(tick) {
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
    env.service_access
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
        let value = env.value_at(&map, building.obb.center(), coverage_bonus, utilities_bonus);
        env.by_building.insert(id, value);
        env.service_access.insert(id, coverage_bonus);
    }
}

pub use crate::map::shoreline_value_bonus;

fn has_land_value(building: &Building) -> bool {
    match building.kind {
        BuildingKind::House => true,
        BuildingKind::GoodsCompany(company) => company.is_growable(),
        _ => false,
    }
}
