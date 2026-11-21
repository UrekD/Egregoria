use crate::{get_lua, BuildingPrototype, Prototype, ResidencePrototypeID};
use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Density {
    #[default]
    Low,
    High,
}

impl<'lua> FromLua<'lua> for Density {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let density = String::from_lua(value, lua)?;
        match density.as_str() {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            _ => Err(mlua::Error::external(format!("Unknown density: {density}"))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResidencePrototype {
    pub base: BuildingPrototype,
    pub id: ResidencePrototypeID,
    pub density: Density,
    pub level: u8,
    pub capacity: u32,
    pub lot_size_min: f32,
}

impl Prototype for ResidencePrototype {
    type Parent = BuildingPrototype;
    type ID = ResidencePrototypeID;
    const NAME: &'static str = "residence";

    fn from_lua(table: &Table) -> mlua::Result<Self> {
        let base = BuildingPrototype::from_lua(table)?;
        Ok(Self {
            id: Self::ID::new(&base.name),
            base,
            density: get_lua(table, "density")?,
            level: get_lua(table, "level")?,
            capacity: get_lua(table, "capacity")?,
            lot_size_min: get_lua(table, "lot_size_min")?,
        })
    }

    fn id(&self) -> Self::ID {
        self.id
    }

    fn parent(&self) -> &Self::Parent {
        &self.base
    }
}

impl Deref for ResidencePrototype {
    type Target = BuildingPrototype;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}
