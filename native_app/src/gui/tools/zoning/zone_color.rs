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

/// Each density ramp decreases in OKLab lightness. Adjacent swatches are at least
/// 0.10 apart in Euclidean OKLab distance.
pub fn zone_cell_color(kind: ZoneKind, density: Density) -> LinearColor {
    let [r, g, b] = match (kind, density) {
        (ZoneKind::Residential, Density::Low) => [0.327, 0.726, 0.392],
        (ZoneKind::Residential, Density::Medium) => [0.109, 0.550, 0.208],
        (ZoneKind::Residential, Density::High) => [0.001, 0.316, 0.114],
        (ZoneKind::Commercial, Density::Low) => [0.360, 0.634, 0.946],
        (ZoneKind::Commercial, Density::Medium) => [0.163, 0.438, 0.846],
        (ZoneKind::Commercial, Density::High) => [0.039, 0.226, 0.635],
        (ZoneKind::Industrial, Density::Low) => [0.894, 0.527, 0.236],
        (ZoneKind::Industrial, Density::Medium) => [0.782, 0.308, 0.073],
        (ZoneKind::Industrial, Density::High) => [0.556, 0.149, 0.015],
        (ZoneKind::Office, Density::Low) => [0.713, 0.492, 0.963],
        (ZoneKind::Office, Density::Medium) => [0.514, 0.300, 0.842],
        (ZoneKind::Office, Density::High) => [0.309, 0.124, 0.601],
    };
    LinearColor::new(r, g, b, 1.0)
}
