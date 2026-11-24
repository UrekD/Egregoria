use crate::{get_lua, Density, GrowableCompanyPrototype, Prototypes, ResidencePrototype, ZoneKind};
use mlua::{FromLua, Lua, Table, Value};

pub const GROWABLE_FLOOR_HEIGHT_M: f32 = 3.2;
pub const MIXED_USE_SHOP_FLOOR_HEIGHT_M: f32 = 4.5;
pub const MAX_PROFILED_SLOTS: u32 = 1_024;

/// Land use in the prototype registry. The command enum stays four-kind until the
/// spine can wire Mixed Use through paint, placement and serialization together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GrowableZoneKind {
    Pure(ZoneKind),
    MixedUse,
}

impl GrowableZoneKind {
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::Pure(ZoneKind::Residential) => 0,
            Self::Pure(ZoneKind::Commercial) => 1,
            Self::Pure(ZoneKind::Industrial) => 2,
            Self::Pure(ZoneKind::Office) => 3,
            Self::MixedUse => 4,
        }
    }

    pub const fn supports_density(self, density: Density) -> bool {
        !matches!((self, density), (Self::MixedUse, Density::Low))
    }
}

impl Default for GrowableZoneKind {
    fn default() -> Self {
        Self::Pure(ZoneKind::Residential)
    }
}

impl<'lua> FromLua<'lua> for GrowableZoneKind {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        if matches!(&value, Value::String(kind) if kind.as_bytes() == b"mixed-use") {
            return Ok(Self::MixedUse);
        }
        ZoneKind::from_lua(value, lua).map(Self::Pure)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrowableChainKey {
    pub zone_kind: GrowableZoneKind,
    pub density: Density,
    pub level: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowableMassingStyle {
    House,
    Block,
    Tower,
    Industrial,
}

impl<'lua> FromLua<'lua> for GrowableMassingStyle {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        match String::from_lua(value, lua)?.as_str() {
            "house" => Ok(Self::House),
            "block" => Ok(Self::Block),
            "tower" => Ok(Self::Tower),
            "industrial" => Ok(Self::Industrial),
            style => Err(mlua::Error::external(format!(
                "Unknown massing style: {style}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrowableMassing {
    pub style: GrowableMassingStyle,
    pub floors_by_level: [u8; 5],
    pub max_floors: u8,
    /// Minimum parcel width along the road-facing edge, in metres.
    pub min_plot_width_m: f32,
    pub min_plot_depth_m: f32,
}

impl GrowableMassing {
    pub fn nominal_floors(&self, level: u8) -> Option<u8> {
        self.floors_by_level
            .get(usize::from(level.checked_sub(1)?))
            .copied()
    }

    /// Nominal storey height, before roof and foundation geometry.
    pub fn nominal_height_m(&self, level: u8, zone_kind: GrowableZoneKind) -> Option<f32> {
        let floors = self.nominal_floors(level)?;
        if zone_kind == GrowableZoneKind::MixedUse {
            Some(
                f32::from(floors.checked_sub(1)?) * GROWABLE_FLOOR_HEIGHT_M
                    + MIXED_USE_SHOP_FLOOR_HEIGHT_M,
            )
        } else {
            Some(f32::from(floors) * GROWABLE_FLOOR_HEIGHT_M)
        }
    }
}

impl<'lua> FromLua<'lua> for GrowableMassing {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> mlua::Result<Self> {
        let table = Table::from_lua(value, lua)?;
        Ok(Self {
            style: get_lua(&table, "style")?,
            floors_by_level: get_lua(&table, "floors_by_level")?,
            max_floors: get_lua(&table, "max_floors")?,
            min_plot_width_m: get_lua(&table, "min_plot_width_m")?,
            min_plot_depth_m: get_lua(&table, "min_plot_depth_m")?,
        })
    }
}

/// The level-one home supplies the shared shell profile. The shop supplies the retail role.
#[derive(Clone, Copy, Debug)]
pub struct MixedUsePair<'a> {
    pub home: &'a ResidencePrototype,
    pub shop: &'a GrowableCompanyPrototype,
}

impl Prototypes {
    pub fn residence_for_chain(&self, key: GrowableChainKey) -> Option<&ResidencePrototype> {
        unique_match(
            self.orderings
                .residence
                .iter()
                .map(|id| &self.residence[id])
                .filter(|home| home.chain_key() == key),
        )
    }

    pub fn growable_company_for_chain(
        &self,
        key: GrowableChainKey,
    ) -> Option<&GrowableCompanyPrototype> {
        unique_match(
            self.orderings
                .growable_company
                .iter()
                .map(|id| &self.growable_company[id])
                .filter(|shop| shop.growable && shop.chain_key() == key),
        )
    }

    pub fn mixed_use_pair(&self, density: Density) -> Option<MixedUsePair<'_>> {
        let zone_kind = GrowableZoneKind::MixedUse;
        if !zone_kind.supports_density(density) {
            return None;
        }
        let key = GrowableChainKey {
            zone_kind,
            density,
            level: 1,
        };
        let home = self.residence_for_chain(key)?;
        let shop = self.growable_company_for_chain(key)?;
        home.massing?;
        Some(MixedUsePair { home, shop })
    }
}

fn unique_match<T>(mut candidates: impl Iterator<Item = T>) -> Option<T> {
    let first = candidates.next()?;
    candidates.next().is_none().then_some(first)
}
