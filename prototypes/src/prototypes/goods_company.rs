use std::ops::Deref;

use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};

use egui_inspect::Inspect;

use crate::{get_lua, get_lua_opt, BuildingPrototype, GoodsCompanyID, ItemID, Prototype, Recipe, Zone};

#[derive(Copy, Clone, Serialize, Deserialize, Debug, PartialEq, Eq, Inspect)]
pub enum CompanyKind {
    /// Buyers come to get their goods
    Store,
    /// Buyers get their goods delivered to them
    Factory,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WarehousePolicy {
    #[default]
    Balanced,
    ReserveLocally,
    ExportSurplus,
}

impl<'lua> FromLua<'lua> for WarehousePolicy {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        match String::from_lua(value, lua)?.as_str() {
            "balanced" => Ok(Self::Balanced),
            "reserve_locally" => Ok(Self::ReserveLocally),
            "export_surplus" => Ok(Self::ExportSurplus),
            policy => Err(mlua::Error::external(format!("Unknown warehouse policy: {policy}"))),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct WarehouseSpec {
    pub capacity_units: u32,
    pub accepted_items: Vec<ItemID>,
    pub low_water_units: u32,
    pub high_water_units: u32,
    pub default_policy: WarehousePolicy,
}

impl<'lua> FromLua<'lua> for WarehouseSpec {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let table = Table::from_lua(value, lua)?;
        let accepted_items = get_lua::<Vec<String>>(&table, "accepted_items")?
            .iter()
            .map(|name| ItemID::new(name))
            .collect();
        Ok(Self {
            capacity_units: get_lua(&table, "capacity_units")?,
            accepted_items,
            low_water_units: get_lua(&table, "low_water_units")?,
            high_water_units: get_lua(&table, "high_water_units")?,
            default_policy: get_lua_opt(&table, "default_policy")?.unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GoodsCompanyPrototype {
    pub base: BuildingPrototype,
    pub id: GoodsCompanyID,
    pub kind: CompanyKind,
    pub recipe: Option<Recipe>,
    pub n_trucks: u32,
    pub n_workers: u32,
    pub zone: Option<Zone>,
    pub cargo_capacity_units: Option<u32>,
    pub dispatch_min_units: Option<u32>,
    pub dispatch_max_wait_ticks: Option<u32>,
    pub storage: Option<WarehouseSpec>,
}

impl Prototype for GoodsCompanyPrototype {
    type Parent = BuildingPrototype;
    type ID = GoodsCompanyID;
    const NAME: &'static str = "goods-company";

    fn from_lua(table: &Table) -> mlua::Result<Self> {
        let base = BuildingPrototype::from_lua(table)?;
        Ok(Self {
            id: Self::ID::from(&base.name),
            base,
            kind: get_lua(table, "kind")?,
            recipe: get_lua(table, "recipe")?,
            n_trucks: get_lua_opt(table, "n_trucks")?.unwrap_or(0),
            n_workers: get_lua_opt(table, "n_workers")?.unwrap_or(0),
            zone: get_lua(table, "zone").ok(),
            cargo_capacity_units: get_lua_opt(table, "cargo_capacity_units")?,
            dispatch_min_units: get_lua_opt(table, "dispatch_min_units")?,
            dispatch_max_wait_ticks: get_lua_opt(table, "dispatch_max_wait_ticks")?,
            storage: get_lua_opt(table, "storage")?,
        })
    }

    fn id(&self) -> Self::ID {
        self.id
    }

    fn parent(&self) -> &Self::Parent {
        &self.base
    }
}

impl Deref for GoodsCompanyPrototype {
    type Target = BuildingPrototype;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl<'a> FromLua<'a> for CompanyKind {
    fn from_lua(value: Value<'a>, lua: &'a Lua) -> mlua::Result<Self> {
        let s: String = FromLua::from_lua(value, lua)?;
        match &*s {
            "store" => Ok(Self::Store),
            "factory" => Ok(Self::Factory),
            _ => Err(mlua::Error::external(format!(
                "Unknown company kind: {}",
                s
            ))),
        }
    }
}
