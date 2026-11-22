#![cfg(test)]

use crate::load::load_prototypes;
use crate::{
    try_prototype, BuildingGen, CompanyKind, Density, GoodsCompanyID, GoodsCompanyPrototype,
    GrowableCompanyID, ItemID, ResidencePrototype, ServiceID, ServiceKind, ServicePrototype,
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
        }

        let residences: Vec<_> = ResidencePrototype::iter().collect();
        assert_eq!(residences.len(), 9);
        for density in [Density::Low, Density::Medium, Density::High] {
            for level in 1..=3 {
                let residence = residences
                    .iter()
                    .find(|r| r.density == density && r.level == level)
                    .unwrap();
                assert!(matches!(residence.bgen, BuildingGen::House));
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
