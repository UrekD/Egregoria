use crate::{
    get_lua, BuildingPrototype, GrowableChainKey, GrowableMassing, GrowableZoneKind, Prototype,
    ResidencePrototypeID,
};
use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::ops::Deref;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    #[default]
    Low,
    High,
    Medium,
}

impl Density {
    /// Density order for comparisons and indexing. Enum variants retain their serialized order.
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
        }
    }
}

impl Ord for Density {
    fn cmp(&self, other: &Self) -> Ordering {
        self.ordinal().cmp(&other.ordinal())
    }
}

impl PartialOrd for Density {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'lua> FromLua<'lua> for Density {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let density = String::from_lua(value, lua)?;
        match density.as_str() {
            "low" => Ok(Self::Low),
            "high" => Ok(Self::High),
            "medium" => Ok(Self::Medium),
            _ => Err(mlua::Error::external(format!("Unknown density: {density}"))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ResidencePrototype {
    pub base: BuildingPrototype,
    pub id: ResidencePrototypeID,
    pub zone_kind: GrowableZoneKind,
    pub density: Density,
    pub level: u8,
    pub capacity: u32,
    pub lot_size_min: f32,
    pub massing: Option<GrowableMassing>,
}

impl ResidencePrototype {
    pub fn chain_key(&self) -> GrowableChainKey {
        GrowableChainKey {
            zone_kind: self.zone_kind,
            density: self.density,
            level: self.level,
        }
    }
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
            zone_kind: get_lua::<Option<_>>(table, "zone_kind")?.unwrap_or_default(),
            density: get_lua(table, "density")?,
            level: get_lua(table, "level")?,
            capacity: get_lua(table, "capacity")?,
            lot_size_min: get_lua(table, "lot_size_min")?,
            massing: get_lua(table, "massing")?,
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
