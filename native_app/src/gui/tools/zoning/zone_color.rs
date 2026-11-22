use geom::LinearColor;
use simulation::map::{Density, ZoneKind};

pub fn zone_color(kind: ZoneKind) -> LinearColor {
    match kind {
        ZoneKind::Residential => LinearColor::new(0.22, 0.78, 0.32, 1.0),
        ZoneKind::Commercial => LinearColor::new(0.19, 0.55, 0.99, 1.0),
        ZoneKind::Industrial => LinearColor::new(0.98, 0.63, 0.13, 1.0),
        ZoneKind::Office => LinearColor::new(0.69, 0.38, 0.94, 1.0),
    }
}

pub fn zone_cell_color(kind: ZoneKind, density: Density) -> LinearColor {
    let (scale, white) = match density {
        Density::Low => (0.16, 0.0),
        Density::Medium => (0.64, 0.01),
        Density::High => (0.48, 0.52),
    };
    let LinearColor { r, g, b, .. } = zone_color(kind);
    LinearColor::new(r * scale + white, g * scale + white, b * scale + white, 1.0)
}
