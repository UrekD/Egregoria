use crate::{get_lua, GameDuration, ItemID, TICKS_PER_SECOND_F64};
use egui_inspect::Inspect;
use mlua::{FromLua, Lua, Table, Value};

#[derive(Debug, Clone, Inspect)]
pub struct RecipeItem {
    pub id: ItemID,
    pub amount: i32,
}

impl<'lua> FromLua<'lua> for RecipeItem {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let table: Table = FromLua::from_lua(value, lua)?;

        if let Ok(v) = table.get(1) {
            let item_id = ItemID::from_lua(v, lua)?;
            let amount = table.get(2)?;
            return Ok(Self {
                id: item_id,
                amount,
            });
        }

        let name = get_lua::<String>(&table, "id")?;
        let item_id = ItemID::from(&name);
        let amount = get_lua(&table, "amount")?;

        Ok(Self {
            id: item_id,
            amount,
        })
    }
}

#[derive(Debug, Clone, Inspect)]
pub struct Recipe {
    pub consumption: Vec<RecipeItem>,
    pub production: Vec<RecipeItem>,

    /// Time to execute the recipe when the facility is at full capacity
    pub duration: GameDuration,

    /// Quantity to store per production in terms of quantity produced. So if it takes 1ton of flour to make
    /// 1 ton of bread. A storage multiplier of 3 means 3 tons of bread will be stored before stopping to
    /// produce it.
    pub storage_multiplier: i32,
}

/// Game seconds of production per declared recipe second. Durations were tuned when a batch
/// advanced one declared second per real second, at 10 game seconds per real second.
pub const RECIPE_GAME_SECONDS_PER_DECLARED_SECOND: f64 = 10.0;

impl Recipe {
    /// Ticks one batch takes at full productivity. Pinned to game time, so batches per game day
    /// do not depend on the clock speed. Declared durations are whole seconds; rounding undoes
    /// the tick rounding of the parsed duration.
    pub fn batch_ticks(&self) -> f64 {
        self.duration.seconds().round()
            * RECIPE_GAME_SECONDS_PER_DECLARED_SECOND
            * TICKS_PER_SECOND_F64
    }
}

impl<'lua> FromLua<'lua> for Recipe {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let table: Table = FromLua::from_lua(value, lua)?;
        Ok(Self {
            consumption: get_lua(&table, "consumption")?,
            production: get_lua(&table, "production")?,
            duration: get_lua(&table, "duration")?,
            storage_multiplier: get_lua(&table, "storage_multiplier")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tick, TICKS_PER_DAY};
    use std::str::FromStr;

    fn recipe(duration: &str) -> Recipe {
        Recipe {
            consumption: vec![],
            production: vec![],
            duration: GameDuration(Tick::from_str(duration).unwrap()),
            storage_multiplier: 1,
        }
    }

    #[test]
    fn recipe_output_per_game_day_survives_the_clock_change() {
        // Under the 10 game s/real s clock a recipe declared as N seconds finished one batch
        // every N * 50 ticks of a 432,000-tick day.
        for declared in [40.0, 100.0, 200.0, 360.0, 450.0, 480.0] {
            let old_batches_per_day = 432_000.0 / (declared * 50.0);
            let r = recipe(&format!("{declared}s"));
            let batches_per_day = TICKS_PER_DAY as f64 / r.batch_ticks();
            assert!(
                (batches_per_day - old_batches_per_day).abs() < 1e-9,
                "{declared}s: {batches_per_day} batches/day, was {old_batches_per_day}"
            );
        }
        assert!((recipe("100s").batch_ticks() - 5_000.0 / 3.0).abs() < 1e-9);
    }
}
