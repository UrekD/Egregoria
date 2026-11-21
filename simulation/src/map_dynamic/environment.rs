use std::collections::BTreeMap;

use geom::{vec2, Vec2};
use prototypes::ServiceKind;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::map::{BuildingID, BuildingKind};
use crate::map_dynamic::{CoverageGrid, UtilityFlow};
use crate::souls::residence::Residences;
use crate::Simulation;

const UPDATE_INTERVAL: u64 = 3_000;
const POLLUTION_RADIUS: f32 = 160.0;
const POLLUTION_RETENTION: f32 = 0.9;
const MIN_POLLUTION: f32 = 1.0;
const BASE_LAND_VALUE: f32 = 20.0 + 10.0;
const FULL_COVERAGE_BONUS: f32 = 15.0;
const UTILITIES_BONUS: f32 = 10.0;

#[derive(Default)]
pub struct EnvGrid {
    pub cells: BTreeMap<(i32, i32), EnvCell>,
    by_building: BTreeMap<BuildingID, u8>,
}

impl EnvGrid {
    pub const CELL_SIZE: f32 = 32.0;

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

#[derive(Serialize, Deserialize)]
enum SavedEnvGrid<Cells, LandValues> {
    // The spine stub serialized an empty map. Its Bincode length of zero
    // is also the discriminant of this variant.
    EmptySpine,
    Snapshot {
        cells: Cells,
        by_building: LandValues,
    },
}

impl Serialize for EnvGrid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        SavedEnvGrid::Snapshot {
            cells: &self.cells,
            by_building: &self.by_building,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EnvGrid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match SavedEnvGrid::deserialize(deserializer)? {
            SavedEnvGrid::EmptySpine => Ok(Self::default()),
            SavedEnvGrid::Snapshot { cells, by_building } => Ok(Self { cells, by_building }),
        }
    }
}

pub fn env_update(sim: &mut Simulation) {
    if sim.get_tick() % UPDATE_INTERVAL != 0 {
        return;
    }
    profiling::scope!("map_dynamic::env_update");

    let map = sim.map();
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
                .and_then(|home| home.prototype.prototype().pollution),
            BuildingKind::GoodsCompany(company) => company.prototype().pollution,
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

    env.by_building.clear();
    for building in map.buildings().values() {
        let eligible = match building.kind {
            BuildingKind::House => true,
            BuildingKind::GoodsCompany(company) => company.is_growable(),
            _ => false,
        };
        if !eligible {
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
