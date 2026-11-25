use crate::gui::zoning::{zone_color, zone_name};
use crate::uiworld::UiWorld;
use goryak::{
    blur_bg, is_hovered, mincolumn, minrow, monospace, on_secondary_container, padxy,
    secondary_container, sized_canvas, textc,
};
use simulation::economy::{DemandDriver, ZoneDemand};
use simulation::map::ZoneKind;
use simulation::Simulation;
use yakui::paint::PaintRect;
use yakui::{reflow, Alignment, Color, Dim2, Pivot, Rect, Vec2};

pub fn render(_: &UiWorld, sim: &Simulation) {
    let demand = sim.read::<ZoneDemand>();
    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(14.0, 56.0),
        || {
            blur_bg(secondary_container().with_alpha(0.9), 8.0, || {
                padxy(10.0, 7.0, || {
                    minrow(9.0, || {
                        for zone in ZoneKind::ALL {
                            let value = demand_value(&demand, zone);
                            let tint = zone_color(zone);
                            let color = Color::from_linear(<[f32; 4]>::from(tint).into());
                            let hovered = is_hovered(|| {
                                mincolumn(3.0, || {
                                    monospace(
                                        color,
                                        format!("{} {:>4}", zone.abbreviation(), score(value)),
                                    );
                                    bar(value.unwrap_or_default(), color);
                                });
                            });
                            if hovered.hovered {
                                reflow(
                                    Alignment::BOTTOM_LEFT,
                                    Pivot::TOP_LEFT,
                                    Dim2::pixels(0.0, 9.0),
                                    || {
                                        blur_bg(secondary_container(), 7.0, || {
                                            padxy(10.0, 7.0, || {
                                                mincolumn(3.0, || {
                                                    textc(
                                                        color,
                                                        format!("{} demand", zone_name(zone)),
                                                    );
                                                    demand_details(sim, &demand, zone);
                                                });
                                            });
                                        });
                                    },
                                );
                            }
                        }
                    });
                });
            });
        },
    );
}

fn score(value: Option<f32>) -> String {
    value.map_or_else(|| "—".into(), |value| format!("{value:+.0}"))
}

fn demand_value(demand: &ZoneDemand, zone: ZoneKind) -> Option<f32> {
    let sample = |kind| demand.values.get(&kind).copied().filter(|v| v.is_finite());
    match zone {
        ZoneKind::MixedUse => {
            Some(sample(ZoneKind::Residential)?.min(sample(ZoneKind::Commercial)?))
        }
        _ => sample(zone),
    }
}

fn demand_details(sim: &Simulation, demand: &ZoneDemand, zone: ZoneKind) {
    let color = on_secondary_container();
    if zone == ZoneKind::MixedUse {
        let residential = demand_value(demand, ZoneKind::Residential);
        let commercial = demand_value(demand, ZoneKind::Commercial);
        textc(
            color,
            format!(
                "Residential {} · Commercial {}",
                mixed_source_label(residential),
                mixed_source_label(commercial)
            ),
        );
        if let Some((residential, commercial)) = residential.zip(commercial) {
            let (limiting, threshold) = mixed_demand_status(residential, commercial);
            textc(color, format!("{limiting} · {threshold}"));
        } else {
            textc(color, "Growth requires both sources above +5");
        }
    } else {
        let driver = match demand_value(demand, zone) {
            None => "Waiting for the first demand update".to_owned(),
            Some(_) => top_driver(sim, zone).map_or_else(
                || "No active demand drivers".to_owned(),
                |(name, points)| format!("{name} ({points:+.1} points)"),
            ),
        };
        textc(color, driver);
    }
}

fn mixed_source_label(value: Option<f32>) -> String {
    let Some(value) = value else {
        return "—".into();
    };
    let threshold = if value > 5.0 {
        "above +5"
    } else if value < 5.0 {
        "below +5"
    } else {
        "at +5"
    };
    format!("{value:+.1} ({threshold})")
}

fn mixed_demand_status(residential: f32, commercial: f32) -> (&'static str, &'static str) {
    let limiting = match residential.partial_cmp(&commercial) {
        Some(std::cmp::Ordering::Less) => "Residential limits demand",
        Some(std::cmp::Ordering::Greater) => "Commercial limits demand",
        _ => "Both limit demand",
    };
    let threshold = if residential.min(commercial) > 5.0 {
        "Growth threshold above +5 met"
    } else {
        "Both need more than +5"
    };
    (limiting, threshold)
}

fn bar(value: f32, color: Color) {
    sized_canvas(Vec2::new(60.0, 7.0), Color::rgb(32, 39, 47), move |ctx| {
        let rect = ctx.layout.get(ctx.dom.current()).unwrap().rect;
        let zero = rect.pos() + Vec2::new(rect.size().x * 0.5, 0.0);
        let signed_width = value.clamp(-100.0, 100.0) * 0.005 * rect.size().x;
        let mut fill = PaintRect::new(Rect::from_pos_size(
            zero + Vec2::new(signed_width.min(0.0), 0.0),
            Vec2::new(signed_width.abs(), rect.size().y),
        ));
        fill.color = color;
        fill.add(ctx.paint);
        let mut marker = PaintRect::new(Rect::from_pos_size(zero, Vec2::new(1.0, rect.size().y)));
        marker.color = Color::WHITE.with_alpha(0.7);
        marker.add(ctx.paint);
    });
}

pub(crate) fn top_driver(sim: &Simulation, zone: ZoneKind) -> Option<(&'static str, f32)> {
    let (driver, points) = sim.read::<ZoneDemand>().top_driver(zone)?;
    let label = match driver {
        DemandDriver::Base => match zone {
            ZoneKind::Residential => "Early residential growth",
            ZoneKind::Industrial => "Early industrial growth",
            _ => "Growth baseline",
        },
        DemandDriver::Jobs => "Jobs balance",
        DemandDriver::Vacancies => "Vacant homes",
        DemandDriver::Tax => "Tax rate",
        DemandDriver::Shortfall if zone == ZoneKind::Commercial => "Retail supply gap",
        DemandDriver::Shortfall => "Goods supply gap",
        DemandDriver::Unemployment => "Unemployment",
        DemandDriver::Happiness => "Happiness",
    };
    Some((label, points))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zones2_native_mixed_source_qualifies_rounded_threshold_values() {
        assert_eq!(mixed_source_label(Some(5.01)), "+5.0 (above +5)");
        assert_eq!(mixed_source_label(Some(5.0)), "+5.0 (at +5)");
        assert_eq!(mixed_source_label(Some(4.99)), "+5.0 (below +5)");
        assert_eq!(mixed_source_label(None), "—");
    }

    #[test]
    fn zones2_native_mixed_demand_uses_both_smoothed_sources() {
        let mut demand = ZoneDemand::default();
        demand.values.insert(ZoneKind::Residential, 35.0);
        assert_eq!(demand_value(&demand, ZoneKind::MixedUse), None);
        demand.values.insert(ZoneKind::Commercial, 5.0);
        assert_eq!(demand_value(&demand, ZoneKind::MixedUse), Some(5.0));
        assert_eq!(
            mixed_demand_status(35.0, 5.0),
            ("Commercial limits demand", "Both need more than +5")
        );
        demand.values.insert(ZoneKind::Commercial, 50.0);
        assert_eq!(demand_value(&demand, ZoneKind::MixedUse), Some(35.0));
        assert_eq!(
            mixed_demand_status(35.0, 35.0),
            ("Both limit demand", "Growth threshold above +5 met")
        );
    }
}
