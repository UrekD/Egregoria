#![allow(clippy::iter_over_hash_type)]

use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

use common::error::MultiError;

use crate::{
    BuildingGen, BuildingPrototype, CompanyKind, Density, GoodsCompanyID, GrowableChainKey,
    GrowableCompanyID, GrowableMassing, GrowableMassingStyle, GrowableZoneKind, ItemID, Money,
    Prototypes, RecipeItem, ServiceKind, ZoneKind, MAX_PROFILED_SLOTS,
};

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("{0}: only factories can have trucks")]
    WrongTrucks(String),
    #[error("{0}: factories must have trucks if it produces things")]
    ZeroTrucks(String),
    #[error("{0}.{1}: referenced prototype not found")]
    ReferencedProtoNotFound(String, &'static str),

    #[error("{0}.{1}: {2}")]
    InvalidField(String, &'static str, String),
}

pub(crate) fn validate(proto: &Prototypes) -> Result<(), MultiError<ValidationError>> {
    let mut errors = vec![];

    for building in proto.building.values() {
        if building.unlock_milestone.is_some_and(|milestone| milestone > 3) {
            errors.push(ValidationError::InvalidField(
                building.name.clone(),
                "unlock_milestone",
                "must be in [0, 3]".into(),
            ));
        }
    }

    for service in proto.service.values() {
        for (invalid, field, message) in [
            (
                !(service.radius_m > 0.0 && service.radius_m <= 3000.0),
                "radius_m",
                "must be finite and in (0, 3000]",
            ),
            (
                !(1..=10_000).contains(&service.capacity),
                "capacity",
                "must be in [1, 10000]",
            ),
            (
                !(1..=1000).contains(&service.n_workers),
                "n_workers",
                "must be in [1, 1000]",
            ),
            (
                service.upkeep_per_day < Money::ZERO,
                "upkeep_per_day",
                "must not be negative",
            ),
            (service.kind != CompanyKind::Store, "kind", "must be a store"),
            (service.n_trucks != 0, "n_trucks", "must be zero"),
            (service.recipe.is_some(), "recipe", "must be omitted"),
            (
                service.service_kind == ServiceKind::WaterPump
                    && service.water_production.unwrap_or_default() <= 0.0,
                "water_production",
                "must be positive for water pumps",
            ),
            (
                service.service_kind == ServiceKind::SewageOutlet
                    && service.sewage_capacity.unwrap_or_default() <= 0.0,
                "sewage_capacity",
                "must be positive for sewage outlets",
            ),
        ] {
            if invalid {
                errors.push(ValidationError::InvalidField(
                    service.name.clone(),
                    field,
                    message.to_string(),
                ));
            }
        }
    }

    for comp in proto.goods_company.values() {
        for (invalid, field, message) in [
            (
                comp.cargo_capacity_units == Some(0),
                "cargo_capacity_units",
                "must be positive",
            ),
            (
                comp.dispatch_min_units.is_some_and(|minimum| {
                    comp.cargo_capacity_units.is_none_or(|capacity| minimum > capacity)
                }),
                "dispatch_min_units",
                "requires cargo_capacity_units and must not exceed it",
            ),
            (
                comp.dispatch_max_wait_ticks == Some(0),
                "dispatch_max_wait_ticks",
                "must be positive",
            ),
        ] {
            if invalid {
                errors.push(ValidationError::InvalidField(comp.name.clone(), field, message.into()));
            }
        }

        if let Some(storage) = &comp.storage {
            for (invalid, field, message) in [
                (storage.capacity_units == 0, "storage.capacity_units", "must be positive"),
                (
                    storage.low_water_units >= storage.high_water_units,
                    "storage.low_water_units",
                    "must be below high_water_units",
                ),
                (
                    storage.high_water_units > storage.capacity_units,
                    "storage.high_water_units",
                    "must not exceed capacity_units",
                ),
            ] {
                if invalid {
                    errors.push(ValidationError::InvalidField(comp.name.clone(), field, message.into()));
                }
            }
            if storage.accepted_items.iter().any(|item| !proto.item.contains_key(item)) {
                errors.push(ValidationError::ReferencedProtoNotFound(
                    comp.name.clone(),
                    "storage.accepted_items",
                ));
            }
        }

        if comp.n_trucks > 0 && comp.kind != CompanyKind::Factory {
            errors.push(ValidationError::WrongTrucks(comp.name.clone()));
        }

        if comp.n_trucks == 0
            && comp.kind == CompanyKind::Factory
            && !proto
                .growable_company
                .get(&GrowableCompanyID::from(comp.id))
                .is_some_and(|growable| growable.growable)
            && comp
                .recipe
                .as_ref()
                .map(|r| !r.production.is_empty())
                .unwrap_or(false)
        {
            errors.push(ValidationError::ZeroTrucks(comp.name.clone()));
        }

        if let Some(ref r) = comp.recipe {
            for item in &r.consumption {
                if !proto.item.contains_key(&item.id) {
                    errors.push(ValidationError::ReferencedProtoNotFound(
                        comp.name.clone(),
                        "consumption",
                    ));
                }
            }

            for item in &r.production {
                if !proto.item.contains_key(&item.id) {
                    errors.push(ValidationError::ReferencedProtoNotFound(
                        comp.name.clone(),
                        "production",
                    ));
                }
            }
        }

        if comp.power_consumption.map_or(false, |v| v.0 < 0) {
            errors.push(ValidationError::InvalidField(
                comp.name.clone(),
                "power_consumption",
                "must not be negative".to_string(),
            ));
        }

        if comp.power_production.map_or(false, |v| v.0 < 0) {
            errors.push(ValidationError::InvalidField(
                comp.name.clone(),
                "power_production",
                "must not be negative".to_string(),
            ));
        }
    }

    validate_growables(proto, &mut errors);

    if !errors.is_empty() {
        return Err(MultiError(errors));
    }
    Ok(())
}

/// Power's Lua conversion can lose nonfinite or sub-watt negative inputs before registry validation.
pub(crate) fn validate_profile_power_fields(table: &mlua::Table) -> mlua::Result<()> {
    for field in ["power_consumption", "power_production"] {
        let rate = match table.get::<_, mlua::Value>(field)? {
            mlua::Value::Integer(number) => Some(number as f64),
            mlua::Value::Number(number) => Some(number),
            mlua::Value::String(text) => common::parse_f64(text.to_str()?.trim())
                .ok()
                .map(|(number, _)| number),
            _ => None,
        };
        if rate.is_some_and(|rate| !rate.is_finite() || rate < 0.0) {
            return Err(mlua::Error::external(format!(
                "field {field}: must be finite and nonnegative"
            )));
        }
    }
    Ok(())
}

fn validate_growables(proto: &Prototypes, errors: &mut Vec<ValidationError>) {
    use GrowableZoneKind::{MixedUse, Pure};

    let mut home_chains = BTreeMap::<_, Vec<_>>::new();
    let mut shops = BTreeMap::<_, Vec<_>>::new();

    for home in proto.residence.values() {
        if home.massing.is_some() {
            home_chains
                .entry((home.zone_kind, home.density))
                .or_default()
                .push(home);
        } else if home.zone_kind != Pure(ZoneKind::Residential) {
            profile_checker(errors, &home.name)(
                false,
                "massing",
                "a residence outside the Residential zone requires a supported mixed profile",
            );
        }
    }
    for ((zone_kind, density), chain) in &mut home_chains {
        chain.sort_unstable_by_key(|home| (home.level, home.id));
        let mixed = *zone_kind == MixedUse;
        let profile = chain[0].massing.unwrap();

        for (index, home) in chain.iter().enumerate() {
            let mut check = profile_checker(errors, &home.name);
            check(
                !proto.has_conflicting_declarations(&home.name),
                "type",
                "profiled name is declared with another prototype type",
            );
            check(
                (*zone_kind == Pure(ZoneKind::Residential) || mixed)
                    && zone_kind.supports_density(*density),
                "zone_kind",
                "requires Residential or Medium/High Mixed Use",
            );
            check(
                usize::from(home.level) == index + 1 && home.level <= 5,
                "level",
                "a profiled residence chain must be unique and contiguous from level 1 through at most level 5",
            );
            check(
                home.massing == Some(profile),
                "massing",
                "all levels of a profiled residence chain must share the same profile",
            );
            check(
                (1..=MAX_PROFILED_SLOTS).contains(&home.capacity),
                "capacity",
                "must be in [1, 1024]",
            );
            if let Some(massing) = home.massing {
                validate_profile(
                    &home.base,
                    home.lot_size_min,
                    massing,
                    *density,
                    if mixed { 2 } else { 1 },
                    &mut check,
                );
            }
        }
    }

    let low_retail = proto
        .growable_company
        .get(&GrowableCompanyID::new("commercial-l1"))
        .filter(|shop| {
            shop.growable
                && shop.chain_key()
                    == GrowableChainKey {
                        zone_kind: Pure(ZoneKind::Commercial),
                        density: Density::Low,
                        level: 1,
                    }
                && !proto.has_conflicting_declarations(&shop.name)
        })
        .and_then(|shop| proto.goods_company.get(&GoodsCompanyID::from(shop.id)))
        .filter(|shop| shop.kind == CompanyKind::Store && shop.storage.is_none())
        .and_then(|shop| shop.recipe.as_ref());

    for shop in proto.growable_company.values() {
        let key = shop.chain_key();
        let mixed = key.zone_kind == MixedUse;
        if shop.growable {
            shops.entry(key).or_default().push(shop);
        }
        if shop.massing.is_none() && !mixed {
            continue;
        }
        let mut check = profile_checker(errors, &shop.name);
        check(
            !proto.has_conflicting_declarations(&shop.name),
            "type",
            "profiled name is declared with another prototype type",
        );
        let Some(company) = proto.goods_company.get(&GoodsCompanyID::from(shop.id)) else {
            check(false, "type", "requires a canonical goods-company entry");
            continue;
        };
        check(
            matches!(
                key.zone_kind,
                Pure(ZoneKind::Commercial | ZoneKind::Industrial | ZoneKind::Office) | MixedUse
            ) && key.zone_kind.supports_density(key.density),
            "zone_kind",
            "requires Commercial, Industrial, Office or Medium/High Mixed Use",
        );
        check((1..=5).contains(&key.level), "level", "must be in [1, 5]");
        check(
            (1..=MAX_PROFILED_SLOTS).contains(&company.n_workers),
            "n_workers",
            "must be in [1, 1024]",
        );
        if let Some(profile) = shop.massing {
            validate_profile(
                &company.base,
                shop.lot_size_min,
                profile,
                key.density,
                1,
                &mut check,
            );
        }
        if mixed {
            check(
                shop.growable && key.level == 1,
                "level",
                "requires an active level-one shop",
            );
            check(
                company.kind == CompanyKind::Store,
                "kind",
                "must be a store",
            );
            check(company.n_workers == 12, "n_workers", "must have 12 workers");
            check(company.n_trucks == 0, "n_trucks", "must have no fleet");
            check(
                company.storage.is_none(),
                "storage",
                "warehouse storage disables retail production",
            );
            check(
                matches!(
                    company.bgen,
                    BuildingGen::CenteredDoor {
                        vertical_factor: 1.0
                    }
                ),
                "bgen",
                "requires a centered grade-level storefront",
            );
            check(
                shop.massing.is_some_and(|profile| profile.max_floors == 1),
                "massing",
                "the business role must have exactly one floor",
            );
        }

        let Some(recipe) = &company.recipe else {
            check(false, "recipe", "a profiled company requires a recipe");
            continue;
        };
        check(
            (1.0..=f64::from(u32::MAX)).contains(&recipe.batch_ticks()),
            "recipe.duration",
            "full-speed batch duration must be representable in positive u32 ticks",
        );
        check(
            recipe.storage_multiplier > 0,
            "recipe.storage_multiplier",
            "must be positive",
        );
        for (field, items) in [
            ("recipe.consumption", &recipe.consumption),
            ("recipe.production", &recipe.production),
        ] {
            let mut seen = BTreeSet::new();
            check(
                items
                    .iter()
                    .all(|item| item.amount > 0 && seen.insert(item.id)),
                field,
                "amounts must be positive and item IDs unique",
            );
        }
        let multiplier = i64::from(recipe.storage_multiplier);
        check(
            recipe.consumption.iter().all(|item| {
                let storage = i64::from(item.amount) * (multiplier + 1);
                (1..=i64::from(i32::MAX)).contains(&storage)
            }),
            "recipe.consumption",
            "quantity times (storage_multiplier + 1) must fit in positive i32",
        );
        // Production can begin at output_limit - 1. Account for the batch it adds.
        check(
            recipe.production.iter().all(|item| {
                let post_batch_stock = i64::from(item.amount) * (multiplier + 2) - 1;
                (1..=i64::from(i32::MAX)).contains(&post_batch_stock)
            }),
            "recipe.production",
            "maximum post-batch stock must fit in positive i32",
        );
        check(
            !recipe.production.is_empty(),
            "recipe.production",
            "requires an output",
        );
        if mixed {
            let Some(low) = low_retail else {
                check(
                    false,
                    "recipe",
                    "requires the canonical Low commercial recipe",
                );
                continue;
            };
            check(
                same_recipe_items(&recipe.consumption, &low.consumption),
                "recipe.consumption",
                "must match the Low commercial input quantities",
            );
            check(
                same_recipe_items(&recipe.production, &low.production),
                "recipe.production",
                "must match the Low commercial output quantities",
            );
            check(
                recipe.duration == low.duration,
                "recipe.duration",
                "must match the Low commercial labor duration",
            );
            check(
                recipe.storage_multiplier == low.storage_multiplier,
                "recipe.storage_multiplier",
                "must match the Low commercial storage policy",
            );
            check(
                recipe
                    .consumption
                    .iter()
                    .any(|item| item.id == ItemID::new("goods")),
                "recipe.consumption",
                "requires the goods input",
            );
            check(
                ["retail-goods", "shop-delivery"].into_iter().all(|name| {
                    recipe
                        .production
                        .iter()
                        .any(|item| item.id == ItemID::new(name))
                }),
                "recipe.production",
                "requires retail-goods and shop-delivery outputs",
            );
        }
    }

    for group in shops.values() {
        if group.len() > 1
            && group
                .iter()
                .any(|shop| shop.massing.is_some() || shop.chain_key().zone_kind == MixedUse)
        {
            let mut names: Vec<_> = group.iter().map(|shop| &shop.name).collect();
            names.sort_unstable();
            for name in names {
                profile_checker(errors, name)(
                    false,
                    "level",
                    "duplicate growable company chain key",
                );
            }
        }
    }
    if home_chains.keys().any(|(kind, _)| *kind == MixedUse)
        || shops.keys().any(|key| key.zone_kind == MixedUse)
    {
        for density in [Density::Medium, Density::High] {
            let key = GrowableChainKey {
                zone_kind: MixedUse,
                density,
                level: 1,
            };
            let has_home = home_chains.get(&(MixedUse, density)).is_some_and(|chain| {
                chain
                    .first()
                    .is_some_and(|home| home.level == 1 && home.massing.is_some())
            });
            let has_shop = shops.get(&key).is_some_and(|group| group.len() == 1);
            profile_checker(errors, &format!("Mixed Use {density:?}"))(
                has_home && has_shop,
                "level",
                "requires one profiled level-one home and one level-one shop",
            );
        }
    }
}

fn same_recipe_items(a: &[RecipeItem], b: &[RecipeItem]) -> bool {
    a.len() == b.len()
        && a.iter().all(|item| {
            b.iter()
                .any(|other| item.id == other.id && item.amount == other.amount)
        })
}

fn profile_checker<'a>(
    errors: &'a mut Vec<ValidationError>,
    name: &'a str,
) -> impl FnMut(bool, &'static str, &str) + 'a {
    move |valid, field, message| {
        if !valid {
            errors.push(ValidationError::InvalidField(
                name.into(),
                field,
                message.into(),
            ));
        }
    }
}

fn validate_profile(
    building: &BuildingPrototype,
    lot_size_min: f32,
    massing: GrowableMassing,
    density: Density,
    min_floors: u8,
    check: &mut impl FnMut(bool, &'static str, &str),
) {
    use GrowableMassingStyle::{Block, House, Industrial, Tower};
    const MAX_AXIS_M: f32 = u16::MAX as f32 / 10.0;
    // A site occupies a slotmap key with a u32 slot index. Reserve half of the world
    // budget for other loads after accounting for both roles and full slot scaling.
    const MAX_UNSCALED_POWER: i64 = i64::MAX / u32::MAX as i64 / 4 / MAX_PROFILED_SLOTS as i64;
    const MAX_UNSCALED_WATER: f64 =
        f32::MAX as f64 / u32::MAX as f64 / 4.0 / MAX_PROFILED_SLOTS as f64;

    check(
        lot_size_min > 0.0 && lot_size_min <= MAX_AXIS_M * MAX_AXIS_M,
        "lot_size_min",
        "must be finite, positive and fit within the supported parcel dimensions",
    );
    for (field, axis) in [
        ("massing.min_plot_width_m", massing.min_plot_width_m),
        ("massing.min_plot_depth_m", massing.min_plot_depth_m),
        ("size.w", building.size.w),
        ("size.h", building.size.h),
    ] {
        check(
            (0.1..=MAX_AXIS_M).contains(&axis),
            field,
            "must fit in positive u16 decimeters",
        );
    }
    let density_cap = match density {
        Density::Low => 2,
        Density::Medium => 10,
        Density::High => 28,
    };
    let style_cap = match massing.style {
        House => 2,
        Block => 10,
        Tower | Industrial => 28,
    };
    check(
        (min_floors..=style_cap.min(density_cap)).contains(&massing.max_floors),
        "massing.max_floors",
        "must be positive and within the style and density caps",
    );
    check(
        massing
            .floors_by_level
            .iter()
            .all(|floors| (min_floors..=massing.max_floors).contains(floors))
            && massing
                .floors_by_level
                .windows(2)
                .all(|pair| pair[0] <= pair[1]),
        "massing.floors_by_level",
        "floors must be nondecreasing, within the cap, and leave room above a mixed storefront",
    );
    check(
        match building.bgen {
            BuildingGen::CenteredDoor { vertical_factor } => {
                vertical_factor.is_finite() && vertical_factor > 0.0
            }
            BuildingGen::NoWalkway { door_pos } => door_pos.x.is_finite() && door_pos.y.is_finite(),
            BuildingGen::House | BuildingGen::Farm => true,
        },
        "bgen",
        "generator dimensions and door position must be finite",
    );
    for (field, power) in [
        ("power_consumption", building.power_consumption),
        ("power_production", building.power_production),
    ] {
        check(
            power.is_none_or(|power| (0..=MAX_UNSCALED_POWER).contains(&power.watts())),
            field,
            "must be nonnegative and safe to scale across the supported world",
        );
    }
    for (field, rate) in [
        ("water_consumption", building.water_consumption),
        ("water_production", building.water_production),
        ("sewage_capacity", building.sewage_capacity),
    ] {
        check(
            rate.is_none_or(|rate| (0.0..=MAX_UNSCALED_WATER).contains(&f64::from(rate))),
            field,
            "must be finite, nonnegative and safe to scale across the supported world",
        );
    }
}
