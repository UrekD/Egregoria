use crate::{
    get_lua, BuildingPrototype, GrowableChainKey, GrowableMassing, GrowableZoneKind, Prototype,
    ResidencePrototypeID, MAX_PROFILED_SLOTS,
};
use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::ops::Deref;

use super::growable_massing::profile_integer;

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

#[derive(Clone, Debug, serde::Serialize)]
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
        let zone_kind =
            get_lua::<Option<GrowableZoneKind>>(table, "zone_kind")?.unwrap_or_default();
        let massing = get_lua::<Option<GrowableMassing>>(table, "massing")?;
        let (level, capacity) = if massing.is_some() || zone_kind == GrowableZoneKind::MixedUse {
            crate::validation::validate_profile_power_fields(table)?;
            (
                profile_integer(table.get("level")?, 5, "level")? as u8,
                profile_integer(table.get("capacity")?, MAX_PROFILED_SLOTS, "capacity")?,
            )
        } else {
            (get_lua(table, "level")?, get_lua(table, "capacity")?)
        };
        let base = BuildingPrototype::from_lua(table)?;
        Ok(Self {
            id: Self::ID::new(&base.name),
            base,
            zone_kind,
            density: get_lua(table, "density")?,
            level,
            capacity,
            lot_size_min: get_lua(table, "lot_size_min")?,
            massing,
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
