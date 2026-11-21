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
        assert_eq!(residences.len(), 6);
        for density in [Density::Low, Density::High] {
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
    use mlua::{Lua, Table};

    let lua = Lua::new();
    let table: Table = lua
        .load(
            r#"return {
                type = "service", name = "test-service", label = "Test Service",
                size = 20, bgen = "house", asset = "bakery.glb", price = 10,
                kind = "store", n_trucks = 0, n_workers = 1,
                service_kind = "health", radius_m = 3000, capacity = 10000,
                upkeep_per_day = 0, pollution = 12.5
            }"#,
        )
        .eval()
        .unwrap();

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
