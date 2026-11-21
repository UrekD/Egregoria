use std::ops::Deref;

use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};

use crate::{
    get_lua, try_prototype, GoodsCompanyID, GoodsCompanyPrototype, Money, Prototype, ServiceID,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ServiceKind {
    Health,
    Fire,
    Police,
    School,
    Garbage,
    Park,
    WaterPump,
    SewageOutlet,
}

impl<'lua> FromLua<'lua> for ServiceKind {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let kind = String::from_lua(value, lua)?;
        match kind.as_str() {
            "health" => Ok(Self::Health),
            "fire" => Ok(Self::Fire),
            "police" => Ok(Self::Police),
            "school" => Ok(Self::School),
            "garbage" => Ok(Self::Garbage),
            "park" => Ok(Self::Park),
            "water_pump" => Ok(Self::WaterPump),
            "sewage_outlet" => Ok(Self::SewageOutlet),
            _ => Err(mlua::Error::external(format!(
                "Unknown service kind: {kind}"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServicePrototype {
    pub base: GoodsCompanyPrototype,
    pub id: ServiceID,
    pub service_kind: ServiceKind,
    pub radius_m: f32,
    pub capacity: u32,
    pub upkeep_per_day: Money,
}

impl GoodsCompanyID {
    pub fn is_service(self) -> bool {
        try_prototype(ServiceID::from(self)).is_some()
    }
}

impl Prototype for ServicePrototype {
    type Parent = GoodsCompanyPrototype;
    type ID = ServiceID;
    const NAME: &'static str = "service";

    fn from_lua(table: &Table) -> mlua::Result<Self> {
        let base = GoodsCompanyPrototype::from_lua(table)?;
        let capacity = get_lua::<f64>(table, "capacity")?;
        if capacity.fract() != 0.0 {
            return Err(mlua::Error::external("field capacity: must be an integer"));
        }
        Ok(Self {
            id: Self::ID::new(&base.name),
            base,
            service_kind: get_lua(table, "service_kind")?,
            radius_m: get_lua(table, "radius_m")?,
            capacity: capacity as u32,
            upkeep_per_day: get_lua(table, "upkeep_per_day")?,
        })
    }

    fn id(&self) -> Self::ID {
        self.id
    }

    fn parent(&self) -> &Self::Parent {
        &self.base
    }
}

impl Deref for ServicePrototype {
    type Target = GoodsCompanyPrototype;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}
