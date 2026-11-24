use crate::{
    get_lua, try_prototype, Density, GoodsCompanyID, GoodsCompanyPrototype, GrowableChainKey,
    GrowableCompanyID, GrowableMassing, GrowableZoneKind, Prototype, MAX_PROFILED_SLOTS,
};
use mlua::{FromLua, Lua, Table, Value};
use serde::{Deserialize, Serialize};
use std::ops::Deref;

use super::growable_massing::{lua_number, profile_integer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ZoneKind {
    Residential,
    Commercial,
    Industrial,
    Office,
    MixedUse,
}

impl ZoneKind {
    pub const ALL: [Self; 5] = [
        Self::Residential,
        Self::Commercial,
        Self::Industrial,
        Self::Office,
        Self::MixedUse,
    ];
    pub const TAXABLE: [Self; 4] = [
        Self::Residential,
        Self::Commercial,
        Self::Industrial,
        Self::Office,
    ];

    pub const fn ordinal(self) -> u8 {
        self as u8
    }

    pub const fn supports_density(self, density: Density) -> bool {
        !matches!((self, density), (Self::MixedUse, Density::Low))
    }

    pub const fn is_taxable(self) -> bool {
        !matches!(self, Self::MixedUse)
    }

    pub const fn abbreviation(self) -> &'static str {
        match self {
            Self::Residential => "R",
            Self::Commercial => "C",
            Self::Industrial => "I",
            Self::Office => "O",
            Self::MixedUse => "MX",
        }
    }

    pub const fn lua_token(self) -> &'static str {
        match self {
            Self::Residential => "residential",
            Self::Commercial => "commercial",
            Self::Industrial => "industrial",
            Self::Office => "office",
            Self::MixedUse => "mixed-use",
        }
    }
}

impl<'lua> FromLua<'lua> for ZoneKind {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let kind = String::from_lua(value, lua)?;
        Self::ALL
            .into_iter()
            .find(|zone| zone.lua_token() == kind)
            .ok_or_else(|| mlua::Error::external(format!("Unknown zone kind: {kind}")))
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
        let kind = get_lua::<GrowableZoneKind>(table, "zone_kind")?;
        let massing = get_lua::<Option<GrowableMassing>>(table, "massing")?;
        let profiled = massing.is_some() || kind == GrowableZoneKind::MixedUse;
        let level = if profiled {
            crate::validation::validate_profile_power_fields(table)?;
            profile_integer(table.get("level")?, 5, "level")? as u8
        } else {
            get_lua(table, "level")?
        };
        if profiled {
            profile_integer(table.get("n_workers")?, MAX_PROFILED_SLOTS, "n_workers")?;
            if let Some(recipe) = get_lua::<Option<Table>>(table, "recipe")? {
                validate_profile_recipe_lua(&recipe)?;
            }
        }
        let base = GoodsCompanyPrototype::from_lua(table)?;
        Ok(Self {
            id: Self::ID::new(&base.name),
            base,
            zone_kind: match kind {
                GrowableZoneKind::Pure(kind) => kind,
                GrowableZoneKind::MixedUse => ZoneKind::Commercial,
            },
            density: get_lua(table, "density")?,
            level,
            lot_size_min: get_lua(table, "lot_size_min")?,
            growable: get_lua(table, "growable")?,
            massing,
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

fn validate_profile_recipe_lua(recipe: &Table) -> mlua::Result<()> {
    profile_integer(
        recipe.get("storage_multiplier")?,
        i32::MAX as u32,
        "recipe.storage_multiplier",
    )?;
    for field in ["consumption", "production"] {
        for item in get_lua::<Vec<Table>>(recipe, field)? {
            let amount = if matches!(item.get::<_, Value>(1)?, Value::Nil) {
                item.get("amount")?
            } else {
                item.get(2)?
            };
            profile_integer(amount, i32::MAX as u32, &format!("recipe.{field}"))?;
        }
    }

    // Each supported unit fits safely in the Tick parser at this bound. Registry
    // validation checks the resulting full-speed batch duration separately.
    let valid = |part| (0.0..=f64::from(u32::MAX)).contains(&part);
    let bad_duration =
        || mlua::Error::external("field recipe.duration: invalid duration component");
    match recipe.get::<_, Value>("duration")? {
        Value::Table(parts) => {
            for pair in parts.pairs::<Value, Value>() {
                let (_, part) = pair?;
                if !lua_number(&part).is_some_and(valid) {
                    return Err(bad_duration());
                }
            }
        }
        Value::String(text) => {
            if !common::parse_f64(text.to_str()?.trim()).is_ok_and(|(part, _)| valid(part)) {
                return Err(bad_duration());
            }
        }
        part if lua_number(&part).is_some_and(valid) => {}
        _ => return Err(bad_duration()),
    }
    Ok(())
}

impl Deref for GrowableCompanyPrototype {
    type Target = GoodsCompanyPrototype;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}
