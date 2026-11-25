use geom::{Color, LinearColor};
use prototypes::{try_prototype, GoodsCompanyID, ServiceID, ServiceKind};

// sRGB box colours of the placeholder buildings. scripts/icons/generate_service_icons.py
// paints the roof sprites with the same values; neither may use a zone hue.
const CIVIC: u64 = 0xFAF6EE;
const UTILITY: u64 = 0x42464E;
const WAREHOUSE: u64 = 0x202226;

/// Box colour for services and warehouses, which render as a solid box with their
/// lettered sprite on the roof. Other companies keep their own asset.
pub fn placeholder_color(id: GoodsCompanyID) -> Option<LinearColor> {
    let hex = if let Some(service) = try_prototype(ServiceID::from(id)) {
        service_hex(service.service_kind)
    } else if id.prototype().storage.is_some() {
        WAREHOUSE
    } else {
        return None;
    };
    Some(Color::from_hex(hex).into())
}

fn service_hex(kind: ServiceKind) -> u64 {
    match kind {
        ServiceKind::Health
        | ServiceKind::Fire
        | ServiceKind::Police
        | ServiceKind::School
        | ServiceKind::Garbage
        | ServiceKind::Park => CIVIC,
        ServiceKind::WaterPump | ServiceKind::SewageOutlet => UTILITY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::zoning::{zone_cell_color, zone_color};
    use simulation::map::{Density, ZoneKind};

    fn oklab(hex: u64) -> geom::Vec3 {
        let LinearColor { r, g, b, .. } = Color::from_hex(hex).into();
        oklab_linear(LinearColor::new(r, g, b, 1.0))
    }

    fn oklab_linear(LinearColor { r, g, b, .. }: LinearColor) -> geom::Vec3 {
        let l = (0.41222146 * r + 0.53633255 * g + 0.051445995 * b).cbrt();
        let m = (0.2119035 * r + 0.6806995 * g + 0.10739696 * b).cbrt();
        let s = (0.08830246 * r + 0.28171885 * g + 0.6299787 * b).cbrt();
        geom::vec3(
            0.21045426 * l + 0.7936178 * m - 0.004072047 * s,
            1.9779985 * l - 2.4285922 * m + 0.4505937 * s,
            0.025904037 * l + 0.78277177 * m - 0.80867577 * s,
        )
    }

    #[test]
    fn svcvis_placeholder_colors_stay_clear_of_zone_hues_and_grass() {
        let kinds = [
            ServiceKind::Health,
            ServiceKind::Fire,
            ServiceKind::Police,
            ServiceKind::School,
            ServiceKind::Garbage,
            ServiceKind::Park,
            ServiceKind::WaterPump,
            ServiceKind::SewageOutlet,
        ];
        let mut references = vec![(
            "grass".to_string(),
            oklab_linear(LinearColor::new(0.071, 0.155, 0.018, 1.0)),
        )];
        for kind in ZoneKind::ALL {
            references.push((format!("{kind:?}"), oklab_linear(zone_color(kind))));
            for density in [Density::Low, Density::Medium, Density::High]
                .into_iter()
                .filter(|&density| kind.supports_density(density))
            {
                references.push((
                    format!("{kind:?} {density:?}"),
                    oklab_linear(zone_cell_color(kind, density)),
                ));
            }
        }
        let placeholders = kinds
            .map(|kind| (format!("{kind:?}"), service_hex(kind)))
            .into_iter()
            .chain([("warehouse".to_string(), WAREHOUSE)]);
        for (name, hex) in placeholders {
            let color = oklab(hex);
            for (reference, other) in &references {
                assert!(
                    color.distance(*other) >= 0.15,
                    "{name} ({hex:06X}) is {} from {reference}",
                    color.distance(*other)
                );
            }
        }
        assert!(oklab(UTILITY).distance(oklab(WAREHOUSE)) >= 0.1);
        assert_eq!(service_hex(ServiceKind::Fire), CIVIC);
        assert_eq!(service_hex(ServiceKind::WaterPump), UTILITY);
    }
}
