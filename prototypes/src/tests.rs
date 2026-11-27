#![cfg(test)]

use crate::load::load_prototypes;
use crate::{
    try_prototype, BuildingGen, CompanyKind, Density, GoodsCompanyID, GoodsCompanyPrototype,
    GrowableChainKey, GrowableCompanyID, GrowableMassing, GrowableMassingStyle, GrowableZoneKind,
    ItemID, ResidencePrototype, ResidencePrototypeID, ServiceID, ServiceKind, ServicePrototype,
    SolarPanelID, ZoneKind,
};

#[test]
fn test_base() {
    unsafe {
        match load_prototypes("../") {
            Ok(_) => {}
            Err(e) => {
                println!("failed to load prototypes: {}", e);
                assert!(false);
            }
        }

        println!(
            "{:?}",
            try_prototype(ItemID::new("job-opening"))
                .unwrap()
                .optout_exttrade
        );
        println!("{:?}", try_prototype(ItemID::new("cereal")));
        println!("{:#?}", try_prototype(GoodsCompanyID::new("bakery")));
        println!("{:?}", ItemID::new("unknown"));
        println!("{:?}", try_prototype(GoodsCompanyID::new("solar-panel")));
        println!("{:?}", try_prototype(SolarPanelID::new("solar-panel")));

        for density in [Density::Medium, Density::High] {
            assert!(
                crate::prototypes().mixed_use_pair(density).is_some(),
                "stock registry is missing the {density:?} Mixed Use pair"
            );
        }
        assert!(crate::prototypes().mixed_use_pair(Density::Low).is_none());
        for (name, zone) in [
            ("industrial-l1", ZoneKind::Industrial),
            ("commercial-l1", ZoneKind::Commercial),
            ("office-l1", ZoneKind::Office),
        ] {
            assert!(try_prototype(GoodsCompanyID::new(name)).is_some(), "{name}");
            let growable = try_prototype(GrowableCompanyID::new(name)).unwrap();
            assert!(growable.growable);
            assert_eq!(growable.zone_kind, zone);
            assert_eq!(growable.level, 1);
            assert!(growable.massing.is_some(), "{name}");
        }

        let residences: Vec<_> = ResidencePrototype::iter().collect();
        assert_eq!(residences.len(), 34);
        for density in [Density::Low, Density::Medium, Density::High] {
            for level in 1..=3 {
                let residence = crate::prototypes().legacy_residence(density, level).unwrap();
                assert!(matches!(residence.bgen, BuildingGen::House));
                assert_eq!(
                    residence.zone_kind,
                    GrowableZoneKind::Pure(ZoneKind::Residential)
                );
                assert!(residence.massing.is_none());
                assert!(residence.capacity > 0 && residence.power_consumption.is_some());
                assert!(residence.water_consumption.is_some_and(|rate| rate > 0.0));
            }
        }
        let services: Vec<_> = ServicePrototype::iter().collect();
        assert_eq!(services.len(), 8);
        for kind in [
            ServiceKind::Health,
            ServiceKind::Fire,
            ServiceKind::Police,
            ServiceKind::School,
            ServiceKind::Garbage,
            ServiceKind::Park,
            ServiceKind::WaterPump,
            ServiceKind::SewageOutlet,
        ] {
            let matching: Vec<_> = services
                .iter()
                .filter(|service| service.service_kind == kind)
                .collect();
            assert_eq!(matching.len(), 1, "{kind:?}");
            let service = matching[0];
            let company = GoodsCompanyID::from(service.id);
            assert!(company.is_service());
            assert_eq!(company.prototype().kind, CompanyKind::Store);
            assert!(company.prototype().recipe.is_none());
        }
        let pump = ServiceID::new("water-pump").prototype();
        assert!(pump.water_production.is_some_and(|rate| rate > 0.0));
        assert!(pump.power_consumption.is_some());
        let outlet = ServiceID::new("sewage-outlet").prototype();
        assert!(outlet.sewage_capacity.is_some_and(|rate| rate > 0.0));
        assert!(outlet.power_consumption.is_none());
        for company in GoodsCompanyPrototype::iter().filter(|company| !company.id.is_service()) {
            assert!(
                company.water_consumption.is_some_and(|rate| rate > 0.0),
                "{}",
                company.name
            );
        }

        for (name, local_only) in [
            ("goods", false),
            ("retail-goods", true),
            ("services", false),
        ] {
            assert_eq!(ItemID::new(name).prototype().optout_exttrade, local_only);
        }
    }
}

#[test]
fn zones2_profiled_base_registers_mixed_pairs() {
    let registry = zones2_base();
    assert_eq!(registry.residence.len(), 34);
    assert_eq!(registry.growable_company.len(), 11);
    assert!(registry.mixed_use_pair(Density::Low).is_none());

    let low_shop = &registry.goods_company[&GoodsCompanyID::new("commercial-l1")];
    let low_recipe = low_shop.recipe.as_ref().unwrap();
    for (density, density_name, l1_slots) in
        [(Density::Medium, "medium", 4), (Density::High, "high", 11)]
    {
        let pair = registry.mixed_use_pair(density).unwrap();
        let shop_name = format!("mixed-{density_name}-l1");
        assert_eq!(pair.shop.id, GrowableCompanyID::new(&shop_name));
        assert_eq!(
            registry.goods_company[&GoodsCompanyID::new(&shop_name)].kind,
            CompanyKind::Store
        );
        assert_eq!(pair.shop.n_workers, 12);
        assert_eq!(pair.shop.n_trucks, 0);
        assert_eq!(pair.shop.massing.unwrap().floors_by_level, [1; 5]);
        assert!(matches!(pair.shop.bgen, BuildingGen::CenteredDoor { .. }));
        assert_eq!(pair.home.capacity, l1_slots);
        assert!(pair.home.lot_size_min.max(pair.shop.lot_size_min) <= 900.0);

        let recipe = pair.shop.recipe.as_ref().unwrap();
        assert_eq!(recipe.duration, low_recipe.duration);
        assert_eq!(recipe.storage_multiplier, low_recipe.storage_multiplier);
        assert_eq!(
            recipe
                .consumption
                .iter()
                .map(|item| (item.id, item.amount))
                .collect::<Vec<_>>(),
            [(ItemID::new("goods"), 2)]
        );
        assert_eq!(
            recipe
                .production
                .iter()
                .map(|item| (item.id, item.amount))
                .collect::<Vec<_>>(),
            [
                (ItemID::new("retail-goods"), 1),
                (ItemID::new("shop-delivery"), 12)
            ]
        );

        for level in 1..=5 {
            let key = GrowableChainKey {
                zone_kind: GrowableZoneKind::MixedUse,
                density,
                level,
            };
            let name = format!("residence-mixed-{density_name}-l{level}");
            let home = registry.residence_for_chain(key).unwrap();
            assert_eq!(home.id, ResidencePrototypeID::new(&name));
            if level == 1 {
                assert_eq!(pair.home.id, home.id);
            }

            let pure_key = GrowableChainKey {
                zone_kind: GrowableZoneKind::Pure(ZoneKind::Residential),
                ..key
            };
            let pure = registry.residence_for_chain(pure_key).unwrap();
            assert_eq!(pure.name, format!("residence-{density_name}-l{level}"));
            let massing = home.massing.unwrap();
            assert_eq!(Some(massing), pure.massing);
            assert!(massing.nominal_floors(level).unwrap() > 1);
            let pure_height = massing.nominal_height_m(level, pure_key.zone_kind).unwrap();
            let mixed_height = massing.nominal_height_m(level, key.zone_kind).unwrap();
            assert!((mixed_height - pure_height - 1.3).abs() < 0.00001);
        }
    }
}

#[test]
fn zones2_pure_chains_keep_ids_and_profile_each_density() {
    use GrowableMassingStyle::{Block, House, Industrial, Tower};

    let registry = zones2_base();
    for (density, density_name, floors, cap, home_style, home_slots, jobs) in [
        (
            Density::Low,
            "low",
            [1, 2, 2, 2, 2],
            2,
            House,
            2,
            [12, 18, 6],
        ),
        (
            Density::Medium,
            "medium",
            [4, 5, 6, 7, 8],
            10,
            Block,
            5,
            [18, 27, 9],
        ),
        (
            Density::High,
            "high",
            [12, 15, 18, 21, 24],
            28,
            Tower,
            12,
            [24, 36, 12],
        ),
    ] {
        let home_kind = GrowableZoneKind::Pure(ZoneKind::Residential);
        let mut previous_capacity = 0;
        for level in 1..=5 {
            let key = GrowableChainKey {
                zone_kind: home_kind,
                density,
                level,
            };
            let home = registry.residence_for_chain(key).unwrap();
            assert_eq!(
                home.id,
                ResidencePrototypeID::new(&format!("residence-{density_name}-l{level}"))
            );
            assert!(home.capacity > previous_capacity);
            previous_capacity = home.capacity;
            if level == 1 {
                assert_eq!(home.capacity, home_slots);
            }
            let profile = home.massing.unwrap();
            assert_eq!(profile.style, home_style);
            assert_profile(profile, floors, cap);
        }

        for ((kind, name), workers) in [
            (ZoneKind::Commercial, "commercial"),
            (ZoneKind::Industrial, "industrial"),
            (ZoneKind::Office, "office"),
        ]
        .into_iter()
        .zip(jobs)
        {
            let key = GrowableChainKey {
                zone_kind: GrowableZoneKind::Pure(kind),
                density,
                level: 1,
            };
            let firm = registry.growable_company_for_chain(key).unwrap();
            let name = if density == Density::Low {
                format!("{name}-l1")
            } else {
                format!("{name}-{density_name}-l1")
            };
            assert_eq!(firm.id, GrowableCompanyID::new(&name));
            assert_eq!(firm.n_workers, workers);
            assert_eq!(
                firm.lot_size_min,
                if density == Density::High {
                    900.0
                } else {
                    400.0
                }
            );
            let expected_style = match (kind, density) {
                (ZoneKind::Industrial, _) => Industrial,
                (_, Density::High) => Tower,
                _ => Block,
            };
            let profile = firm.massing.unwrap();
            assert_eq!(profile.style, expected_style);
            assert_profile(profile, floors, cap);
        }
    }
}

#[test]
fn zones2_z1_staged_registry_keeps_legacy_capacity_and_utilities() {
    let registry = zones2_base();
    for (density, density_name, capacities) in [
        (Density::Low, "low", [2, 4, 6]),
        (Density::Medium, "medium", [3, 6, 9]),
        (Density::High, "high", [4, 8, 12]),
    ] {
        for (level, capacity) in (1..=3).zip(capacities) {
            let home = registry
                .legacy_residence(density, level)
                .expect("the staged registry must still supply manual homes");
            assert_eq!(
                home.name,
                format!("residence-legacy-{density_name}-l{level}")
            );
            assert!(home.massing.is_none());
            assert_eq!(home.capacity, capacity);
            assert_eq!(
                home.power_consumption,
                Some(crate::Power::new(i64::from(capacity) * 100))
            );
            assert_eq!(home.water_consumption, Some(capacity as f32));
        }
    }
}

#[test]
fn zones2_named_legacy_home_does_not_shadow_profiled_chain() {
    let registry = zones2_base_with(
        r#"data:extend {
            type = "residence", name = "legacy-home", label = "Legacy Home", order = "!",
            size = 20, bgen = "house", asset = "bakery.glb", price = 0,
            density = "medium", level = 1, lot_size_min = 400, capacity = 3,
        }"#,
    );
    let key = GrowableChainKey {
        zone_kind: GrowableZoneKind::Pure(ZoneKind::Residential),
        density: Density::Medium,
        level: 1,
    };
    assert_eq!(
        registry.residence_for_chain(key).unwrap().name,
        "residence-medium-l1"
    );
    let legacy = &registry.residence[&ResidencePrototypeID::new("legacy-home")];
    assert!(legacy.massing.is_none());
    assert_eq!(legacy.capacity, 3);
    assert_eq!(
        registry
            .legacy_residence(key.density, key.level)
            .unwrap()
            .id,
        legacy.id
    );
}

#[test]
fn zones2_profile_floors_require_raw_integers() {
    use mlua::FromLua;

    let lua = mlua::Lua::new();
    let good = "style = 'block', min_plot_width_m = 20, min_plot_depth_m = 20";
    for (floors, cap, field) in [
        ("{1.9, 5, 6, 7, 8}", "10", "floors_by_level"),
        ("{4.5, 5, 6, 7, 8}", "10", "floors_by_level"),
        ("{4, 5, 6, 7, 8}", "10.5", "max_floors"),
        ("{0, 5, 6, 7, 8}", "10", "floors_by_level"),
        ("{4, 5, 6, 7, 8}", "256", "max_floors"),
    ] {
        let spec = format!("return {{{good}, floors_by_level = {floors}, max_floors = {cap}}}");
        let value = lua.load(spec).eval().unwrap();
        let error = GrowableMassing::from_lua(value, &lua).expect_err(field);
        assert!(error.to_string().contains(field), "{error}");
    }
}

#[test]
fn zones2_profile_recipe_rejects_fractional_amounts() {
    for (change, field) in [
        ("shop.recipe.consumption[1][2] = 2.9", "consumption"),
        ("shop.recipe.production[1][2] = 1.9", "production"),
        (
            "shop.recipe.storage_multiplier = 12.5",
            "storage_multiplier",
        ),
    ] {
        let error = zones2_profile_parse_result(change).expect_err(field);
        assert!(error.contains(field), "{error}");
    }
}

#[test]
fn zones2_profile_recipe_rejects_invalid_duration_components() {
    for duration in [
        "{seconds = 360, ticks = 0/0}",
        "{seconds = 360, ticks = -0.01}",
        "math.huge",
    ] {
        let error = zones2_profile_parse_result(&format!("shop.recipe.duration = {duration}"))
            .expect_err(duration);
        assert!(error.contains("recipe.duration"), "{error}");
    }
}

fn zones2_profile_parse_result(change: &str) -> Result<(), String> {
    let lua = zones2_lua();
    let script = format!(
        r#"for _, shop in ipairs(data) do
            if shop.name == "mixed-medium-l1" then
                {change}
                return shop
            end
        end"#
    );
    let table: mlua::Table = lua.load(script).eval().unwrap();
    crate::Prototypes::default()
        .parse_prototype(table)
        .map_err(|error| error.to_string())
}

fn assert_profile(profile: GrowableMassing, floors: [u8; 5], cap: u8) {
    assert_eq!(profile.floors_by_level, floors);
    assert_eq!(profile.max_floors, cap);
    assert!((18.0..=30.0).contains(&profile.min_plot_width_m));
    assert!((18.0..=30.0).contains(&profile.min_plot_depth_m));
    assert_eq!(profile.nominal_floors(0), None);
    assert_eq!(profile.nominal_floors(6), None);
    for (level, floor_count) in (1..=5).zip(floors) {
        let height = profile.nominal_height_m(level, GrowableZoneKind::Pure(ZoneKind::Residential));
        assert!((height.unwrap() - f32::from(floor_count) * 3.2).abs() < 0.00001);
    }
}

fn zones2_base() -> crate::Prototypes {
    zones2_base_with("")
}

fn zones2_base_with(extra_lua: &str) -> crate::Prototypes {
    let lua = zones2_lua();
    lua.load(extra_lua).exec().unwrap();

    let mut registry = crate::Prototypes::default();
    let data: mlua::Table = lua.globals().get("data").unwrap();
    for table in data.sequence_values::<mlua::Table>() {
        registry.parse_prototype(table.unwrap()).unwrap();
    }
    crate::validation::validate(&registry).unwrap();
    registry.compute_orderings();
    registry
}

fn zones2_lua() -> mlua::Lua {
    let lua = mlua::Lua::new();
    let globals = lua.globals();
    globals
        .get::<_, mlua::Table>("package")
        .unwrap()
        .set("path", "../base_mod/?.lua")
        .unwrap();
    globals.set("zones2_profiles_enabled", true).unwrap();
    lua.load(include_str!("prototype_init.lua")).exec().unwrap();
    lua.load(include_str!("../../base_mod/data.lua"))
        .exec()
        .unwrap();

    drop(globals);
    lua
}

#[test]
fn service_validation() {
    use crate::{validation, Prototypes};

    let lua = mlua::Lua::new();
    let table = service_table(&lua);

    for (field, invalid) in [
        ("radius_m", 0.0),
        ("radius_m", 3001.0),
        ("radius_m", f64::NAN),
        ("radius_m", f64::INFINITY),
        ("capacity", 0.0),
        ("capacity", 10001.0),
        ("n_workers", 0.0),
        ("n_workers", 1001.0),
        ("upkeep_per_day", -1.0),
        ("n_trucks", 1.0),
    ] {
        let valid: mlua::Value = table.get(field).unwrap();
        table.set(field, invalid).unwrap();
        let mut registry = Prototypes::default();
        registry.parse_prototype(table.clone()).unwrap();
        let errors = validation::validate(&registry).expect_err(field);
        assert!(errors.to_string().contains(field));
        table.set(field, valid).unwrap();
    }

    let mut registry = Prototypes::default();
    registry.parse_prototype(table).unwrap();
    validation::validate(&registry).unwrap();
    let service = &registry.service[&ServiceID::new("test-service")];
    assert_eq!(service.pollution, Some(12.5));
}

fn service_table(lua: &mlua::Lua) -> mlua::Table<'_> {
    lua.load(
        r#"return {
                type = "service", name = "test-service", label = "Test Service",
                size = 20, bgen = "house", asset = "bakery.glb", price = 10,
                kind = "store", n_trucks = 0, n_workers = 1,
                service_kind = "health", radius_m = 3000, capacity = 10000,
                upkeep_per_day = 0, pollution = 12.5
            }"#,
    )
    .eval()
    .unwrap()
}

fn prototype_result<'lua>(
    tables: impl IntoIterator<Item = mlua::Table<'lua>>,
) -> Result<(), String> {
    let mut registry = crate::Prototypes::default();
    for table in tables {
        registry
            .parse_prototype(table)
            .map_err(|err| err.to_string())?;
    }
    crate::validation::validate(&registry).map_err(|err| err.to_string())
}

#[test]
fn p2_spine_building_rates_are_finite_and_nonnegative() {
    let lua = mlua::Lua::new();
    for kind in ["building", "goods-company", "service"] {
        for field in [
            "water_consumption",
            "water_production",
            "sewage_capacity",
            "pollution",
        ] {
            let table = service_table(&lua);
            table.set("type", kind).unwrap();
            for invalid in [-1.0, f64::NAN, f64::INFINITY] {
                table.set(field, invalid).unwrap();
                let error = prototype_result([table.clone()])
                    .expect_err(&format!("{kind} accepted {field} = {invalid}"));
                assert!(error.contains(field), "{error}");
            }
            table.set(field, 0.0).unwrap();
            prototype_result([table]).unwrap();
        }
    }
}

#[test]
fn p2_spine_utility_services_require_positive_rates() {
    let lua = mlua::Lua::new();
    for (kind, field) in [
        ("water_pump", "water_production"),
        ("sewage_outlet", "sewage_capacity"),
    ] {
        let table = service_table(&lua);
        table.set("service_kind", kind).unwrap();
        for invalid in [None, Some(0.0)] {
            table.set(field, invalid).unwrap();
            let error = prototype_result([table.clone()])
                .expect_err(&format!("{kind} accepted {field} = {invalid:?}"));
            assert!(error.contains(field), "{error}");
        }
        table.set(field, 1.0).unwrap();
        prototype_result([table]).unwrap();
    }
}

#[test]
fn p2_spine_service_capacity_is_integral() {
    let lua = mlua::Lua::new();
    let table = service_table(&lua);
    for invalid in [1.5, 10_000.5] {
        table.set("capacity", invalid).unwrap();
        let error =
            prototype_result([table.clone()]).expect_err(&format!("accepted capacity {invalid}"));
        assert!(error.contains("capacity"), "{error}");
    }
}

#[test]
fn p2_spine_service_name_cannot_collide_with_another_prototype_type() {
    let lua = mlua::Lua::new();
    for kind in ["goods-company", "solar-panel", "item"] {
        let service = service_table(&lua);
        let other = service_table(&lua);
        other.set("type", kind).unwrap();
        for tables in [
            [service.clone(), other.clone()],
            [other.clone(), service.clone()],
        ] {
            let error = prototype_result(tables)
                .expect_err(&format!("accepted service name shared with {kind}"));
            assert!(
                error.contains("test-service") && error.contains(kind),
                "{error}"
            );
        }
    }
}

#[test]
fn s5_cargo_and_storage_validation() {
    let lua = mlua::Lua::new();
    for (field, value) in [
        ("cargo_capacity_units", 0),
        ("dispatch_min_units", 17),
        ("dispatch_max_wait_ticks", 0),
        ("unlock_milestone", 4),
    ] {
        let company = s5_company_table(&lua);
        company.set(field, value).unwrap();
        let error = prototype_result([company]).expect_err(field);
        assert!(error.contains(field), "{error}");
    }

    for (field, value) in [
        ("capacity_units", 0),
        ("low_water_units", 48),
        ("high_water_units", 65),
    ] {
        let company = s5_company_table(&lua);
        let storage: mlua::Table = company.get("storage").unwrap();
        storage.set(field, value).unwrap();
        let error = prototype_result([company]).expect_err(field);
        assert!(error.contains(field), "{error}");
    }

    let company = s5_company_table(&lua);
    let storage: mlua::Table = company.get("storage").unwrap();
    storage.set("accepted_items", vec!["missing-item"]).unwrap();
    let error = prototype_result([company]).expect_err("unknown warehouse item");
    assert!(error.contains("accepted_items"), "{error}");
    prototype_result([s5_company_table(&lua)]).unwrap();
}

fn s5_company_table(lua: &mlua::Lua) -> mlua::Table<'_> {
    lua.load(
        r#"return {
            type = "goods-company", name = "test-depot", label = "Test Depot",
            size = 20, bgen = "house", asset = "bakery.glb", price = 10,
            kind = "factory", n_trucks = 1, n_workers = 1,
            cargo_capacity_units = 16, dispatch_min_units = 8,
            dispatch_max_wait_ticks = 1200,
            storage = { capacity_units = 64, accepted_items = {},
                        low_water_units = 16, high_water_units = 48,
                        default_policy = "balanced" }
        }"#,
    )
    .eval()
    .unwrap()
}

#[test]
fn s5_medium_density_lua_and_ordinal() {
    use mlua::FromLua;
    let lua = mlua::Lua::new();
    for (name, density, ordinal) in [
        ("low", Density::Low, 0),
        ("medium", Density::Medium, 1),
        ("high", Density::High, 2),
    ] {
        let value = mlua::Value::String(lua.create_string(name).unwrap());
        assert_eq!(Density::from_lua(value, &lua).unwrap(), density);
        assert_eq!(density.ordinal(), ordinal);
    }
}

fn zones2_validation_result(extra_lua: &str) -> Result<(), String> {
    let lua = zones2_lua();
    lua.load(extra_lua).exec().unwrap();
    let data: mlua::Table = lua.globals().get("data").unwrap();
    prototype_result(data.sequence_values::<mlua::Table>().map(Result::unwrap))
}

#[test]
fn zones2_validation_rejects_both_parent_shadow_orders() {
    for change in ["data:extend(shadow)", "table.insert(data, 1, shadow)"] {
        let error = zones2_validation_result(&format!(
            r#"local shadow = {{
                type = "goods-company", name = "mixed-medium-l1", label = "Shadow Shop",
                size = 20, bgen = "house", asset = "bakery.glb", price = 0,
                kind = "store", n_workers = 1025,
            }}
            {change}"#
        ))
        .expect_err(change);
        assert!(
            error.contains("mixed-medium-l1") && error.contains("prototype type"),
            "{error}"
        );
    }
}

#[test]
fn zones2_validation_rejects_mixed_warehouse_in_either_order() {
    for change in ["data:extend(warehouse)", "table.insert(data, 1, warehouse)"] {
        let error = zones2_validation_result(&format!(
            r#"local warehouse
            local index
            for i, shop in ipairs(data) do
                if shop.name == "mixed-medium-l1" then
                    warehouse = shop
                    index = i
                end
            end
            table.remove(data, index)
            warehouse.storage = {{
                capacity_units = 64, accepted_items = {{"goods"}},
                low_water_units = 16, high_water_units = 48,
            }}
            {change}"#
        ))
        .expect_err(change);
        assert!(
            error.contains("mixed-medium-l1") && error.contains("storage"),
            "{error}"
        );
    }
}

#[test]
fn zones2_validation_checks_canonical_low_recipe_parity() {
    for (change, field) in [
        ("shop.recipe.consumption[1][2] = 3", "recipe.consumption"),
        ("shop.recipe.production[1][2] = 2", "recipe.production"),
        ("shop.recipe.duration = '720s'", "recipe.duration"),
        (
            "shop.recipe.storage_multiplier = 8",
            "recipe.storage_multiplier",
        ),
    ] {
        let error = zones2_validation_result(&format!(
            r#"for _, shop in ipairs(data) do
                if shop.name == "mixed-medium-l1" then {change} end
            end"#
        ))
        .expect_err(field);
        assert!(
            error.contains("mixed-medium-l1") && error.contains(field),
            "{error}"
        );
    }
    zones2_validation_result(
        r#"for _, shop in ipairs(data) do
            if shop.name == "mixed-medium-l1" then
                local outputs = shop.recipe.production
                outputs[1], outputs[2] = outputs[2], outputs[1]
            end
        end"#,
    )
    .unwrap();
}

#[test]
fn zones2_validation_checks_stock_after_next_batch() {
    let production = |amount| {
        format!(
            r#"for _, firm in ipairs(data) do
                if firm.name == "commercial-medium-l1" then
                    firm.recipe.production[1][2] = {amount}
                end
            end"#
        )
    };
    let error =
        zones2_validation_result(&production(160_000_000)).expect_err("post-batch overflow");
    assert!(
        error.contains("commercial-medium-l1") && error.contains("post-batch"),
        "{error}"
    );
    zones2_validation_result(&production(153_391_689)).unwrap();
    let error =
        zones2_validation_result(&production(153_391_690)).expect_err("first unsafe amount");
    assert!(error.contains("post-batch"), "{error}");
}

#[test]
fn zones2_paint_kind_lua_accepts_all_five_kinds() {
    use crate::ZoneKind;

    let lua = mlua::Lua::new();
    for (ordinal, kind) in ZoneKind::ALL.into_iter().enumerate() {
        let token = lua.create_string(kind.lua_token()).unwrap();
        let parsed: ZoneKind = lua.unpack(mlua::Value::String(token)).unwrap();
        assert_eq!(parsed, kind);
        assert_eq!(usize::from(parsed.ordinal()), ordinal);
    }
    assert_eq!(ZoneKind::MixedUse.abbreviation(), "MX");
    assert!(!ZoneKind::MixedUse.is_taxable());
}

#[test]
fn pathpark_rejects_invalid_growable_parking() {
    for (name, parking) in [
        ("residence-low-l1", "{kind='surface', spaces=9}"),
        ("residence-low-l1", "{kind='surface', spaces=0}"),
        ("residence-low-l1", "{kind='surface', spaces=1.5}"),
        ("residence-low-l1", "{kind='surface', spaces='2'}"),
        ("residence-high-l1", "{kind='garage', spaces=257}"),
        (
            "residence-high-l1",
            "{kind='garage', spaces=8, fallback_surface_spaces=9}",
        ),
        (
            "residence-low-l1",
            "{kind='surface', spaces=1, fallback_surface_spaces=1}",
        ),
        ("residence-low-l1", "{kind='road', spaces=1}"),
        ("mixed-medium-l1", "{kind='garage', spaces=8}"),
    ] {
        let error = zones2_validation_result(&format!(
            "for _, p in ipairs(data) do if p.name == '{name}' then p.parking = {parking} end end"
        ))
        .expect_err(parking);
        assert!(error.contains(name) && error.contains("parking"), "{error}");
    }
    zones2_validation_result("").unwrap();
}
