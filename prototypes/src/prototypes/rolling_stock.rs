use crate::{get_lua, get_lua_opt, Money, Prototype};
use mlua::Table;
use std::ops::Deref;

use super::*;

#[derive(Clone, Debug, serde::Serialize)]
pub struct RollingStockPrototype {
    pub base: VehiclePrototype,
    pub id: RollingStockID,
    /// meter
    pub length: f32,
    /// metric ton
    pub mass: u32,
    /// m/s
    pub max_speed: f32,
    /// kN
    pub acc_force: f32,
    /// kN
    pub dec_force: f32,
    // The registry appends passenger policy after the legacy catalogue encoding.
    #[serde(skip)]
    pub passenger_seats: u16,
    #[serde(skip)]
    pub passenger_upkeep_per_day: Money,
}

impl Prototype for RollingStockPrototype {
    type Parent = VehiclePrototype;
    type ID = RollingStockID;
    const NAME: &'static str = "rolling-stock";

    fn from_lua(table: &Table) -> mlua::Result<Self> {
        let base = VehiclePrototype::from_lua(table)?;
        let passenger_seats = table.get::<_, Option<u16>>("passenger-seats")?.unwrap_or(0);
        let passenger_upkeep_per_day = get_lua_opt::<Money>(table, "passenger-upkeep-per-day")?
            .unwrap_or_else(|| {
                if passenger_seats == 0 {
                    Money::ZERO
                } else {
                    Money::new_bucks(25)
                }
            });
        if passenger_seats > 500
            || !(Money::ZERO..=Money::new_bucks(10_000)).contains(&passenger_upkeep_per_day)
        {
            return Err(mlua::Error::external(
                "Invalid passenger stock seats or upkeep",
            ));
        }
        Ok(Self {
            id: Self::ID::new(&base.name),
            base,
            length: get_lua::<f32>(table, "length")?,
            mass: get_lua(table, "mass")?,
            max_speed: get_lua::<f32>(table, "max_speed")?,
            acc_force: get_lua::<f32>(table, "acc_force")?,
            dec_force: get_lua::<f32>(table, "dec_force")?,
            passenger_seats,
            passenger_upkeep_per_day,
        })
    }
    fn id(&self) -> Self::ID {
        self.id
    }
    fn parent(&self) -> &Self::Parent {
        &self.base
    }
}

impl Deref for RollingStockPrototype {
    type Target = VehiclePrototype;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}
