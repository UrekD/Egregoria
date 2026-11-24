use std::ops::Deref;

use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};

use crate::{
    get_lua, get_lua_opt, try_prototype, GoodsCompanyID, GoodsCompanyPrototype, Money, Prototype,
    ServiceID,
};

pub const DEFAULT_WATER_SERVICE_DISTANCE_M: f32 = 48.0;
pub const MAX_WATER_SERVICE_DISTANCE_M: f32 = 128.0;

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
    pub max_water_distance_m: Option<f32>,
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
        let service_kind = get_lua(table, "service_kind")?;
        let max_water_distance_m = get_lua_opt::<f64>(table, "max_water_distance_m")?
            .map(|distance| {
                if !matches!(service_kind, ServiceKind::WaterPump | ServiceKind::SewageOutlet) {
                    return Err(mlua::Error::external(
                        "field max_water_distance_m: only water pumps and sewage outlets can set this field",
                    ));
                }
                let reach = distance as f32;
                if reach <= 0.0 || !distance.is_finite()
                    || distance > f64::from(MAX_WATER_SERVICE_DISTANCE_M)
                {
                    return Err(mlua::Error::external(
                        "field max_water_distance_m: must be finite and in (0, 128]",
                    ));
                }
                Ok(reach)
            })
            .transpose()?;
        Ok(Self {
            id: Self::ID::new(&base.name),
            base,
            service_kind,
            radius_m: get_lua(table, "radius_m")?,
            capacity: capacity as u32,
            upkeep_per_day: get_lua(table, "upkeep_per_day")?,
            max_water_distance_m,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w3_water_reach_rejects_invalid_lua_overrides() {
        let lua = Lua::new();
        let table: Table = lua
            .load(
                r#"return {
                    name = "test-pump", label = "Test Pump", kind = "store",
                    service_kind = "water_pump", radius_m = 1000, capacity = 1,
                    upkeep_per_day = 1, bgen = "house", size = 20,
                    asset = "assets/sprites/water_pump.png", price = 1,
                }"#,
            )
            .eval()
            .unwrap();

        assert_eq!(
            ServicePrototype::from_lua(&table)
                .unwrap()
                .max_water_distance_m,
            None
        );
        for distance in [0.0, -1.0, 128.000001, f64::INFINITY, f64::NAN] {
            table.set("max_water_distance_m", distance).unwrap();
            let error = ServicePrototype::from_lua(&table).unwrap_err();
            assert!(
                error.to_string().contains("max_water_distance_m"),
                "{error}"
            );
        }
        table.set("max_water_distance_m", 128.0).unwrap();
        assert_eq!(
            ServicePrototype::from_lua(&table)
                .unwrap()
                .max_water_distance_m,
            Some(128.0)
        );
        table.set("service_kind", "park").unwrap();
        assert!(ServicePrototype::from_lua(&table).is_err());
    }
}
