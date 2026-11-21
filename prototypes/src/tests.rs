#![cfg(test)]

use crate::load::load_prototypes;
use crate::{
    try_prototype, BuildingGen, Density, GoodsCompanyID, GrowableCompanyID, ItemID,
    ResidencePrototype, SolarPanelID, ZoneKind,
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
            }
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
