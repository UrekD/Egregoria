use crate::{
    get_lua, try_prototype, Density, GoodsCompanyID, GoodsCompanyPrototype, GrowableChainKey,
    GrowableCompanyID, GrowableMassing, GrowableZoneKind, Prototype,
};
use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ZoneKind {
    Residential,
    Commercial,
    Industrial,
    Office,
}

impl<'lua> FromLua<'lua> for ZoneKind {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let kind = String::from_lua(value, lua)?;
        match kind.as_str() {
            "residential" => Ok(Self::Residential),
            "commercial" => Ok(Self::Commercial),
            "industrial" => Ok(Self::Industrial),
            "office" => Ok(Self::Office),
            _ => Err(mlua::Error::external(format!("Unknown zone kind: {kind}"))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GrowableCompanyPrototype {
    pub base: GoodsCompanyPrototype,
    pub id: GrowableCompanyID,
    /// The pure sector used by existing consumers. Use `chain_key` for staged Mixed Use.
    pub zone_kind: ZoneKind,
    pub density: Density,
    pub level: u8,
    pub lot_size_min: f32,
    pub growable: bool,
    pub massing: Option<GrowableMassing>,
    mixed_use: bool,
}

impl GrowableCompanyPrototype {
    pub fn chain_key(&self) -> GrowableChainKey {
        GrowableChainKey {
            zone_kind: if self.mixed_use {
                GrowableZoneKind::MixedUse
            } else {
                GrowableZoneKind::Pure(self.zone_kind)
            },
            density: self.density,
            level: self.level,
        }
    }
}

impl GoodsCompanyID {
    pub fn is_growable(self) -> bool {
        try_prototype(GrowableCompanyID::from(self)).is_some_and(|proto| proto.growable)
    }
}

impl Prototype for GrowableCompanyPrototype {
    type Parent = GoodsCompanyPrototype;
    type ID = GrowableCompanyID;
    const NAME: &'static str = "growable-company";

    fn from_lua(table: &Table) -> mlua::Result<Self> {
        let base = GoodsCompanyPrototype::from_lua(table)?;
        let kind = get_lua::<GrowableZoneKind>(table, "zone_kind")?;
        Ok(Self {
            id: Self::ID::new(&base.name),
            base,
            zone_kind: match kind {
                GrowableZoneKind::Pure(kind) => kind,
                GrowableZoneKind::MixedUse => ZoneKind::Commercial,
            },
            density: get_lua(table, "density")?,
            level: get_lua(table, "level")?,
            lot_size_min: get_lua(table, "lot_size_min")?,
            growable: get_lua(table, "growable")?,
            massing: get_lua(table, "massing")?,
            mixed_use: kind == GrowableZoneKind::MixedUse,
        })
    }

    fn id(&self) -> Self::ID {
        self.id
    }

    fn parent(&self) -> &Self::Parent {
        &self.base
    }
}

impl Deref for GrowableCompanyPrototype {
    type Target = GoodsCompanyPrototype;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}
