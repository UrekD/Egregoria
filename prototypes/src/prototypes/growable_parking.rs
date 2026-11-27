use super::growable_massing::profile_integer;
use mlua::{FromLua, Lua, Table, Value};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum GrowableParking {
    Surface(u8),
    Garage {
        spaces: u16,
        fallback_surface_spaces: u8,
    },
}

impl<'lua> FromLua<'lua> for GrowableParking {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let table = Table::from_lua(value, lua)?;
        let kind: String = table.get("kind")?;
        let fallback: Value = table.get("fallback_surface_spaces")?;
        match kind.as_str() {
            "surface" if matches!(fallback, Value::Nil) => {
                Ok(Self::Surface(
                    profile_integer(table.get("spaces")?, 8, "parking.spaces")? as u8,
                ))
            }
            "garage" => Ok(Self::Garage {
                spaces: profile_integer(table.get("spaces")?, 256, "parking.spaces")? as u16,
                fallback_surface_spaces: match fallback {
                    Value::Nil => 0,
                    value => profile_integer(value, 8, "parking.fallback_surface_spaces")? as u8,
                },
            }),
            "surface" => Err(mlua::Error::external(
                "parking.fallback_surface_spaces requires a garage",
            )),
            _ => Err(mlua::Error::external("unknown parking.kind")),
        }
    }
}
