use crate::gui::zoning::{zone_color, zone_name, ZONES};
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
                        for (zone, code) in ZONES.into_iter().zip(["R", "C", "I", "O"]) {
                            let value = demand.values.get(&zone).copied();
                            let tint = zone_color(zone);
                            let color = Color::from_linear(<[f32; 4]>::from(tint).into());
                            let hovered = is_hovered(|| {
                                mincolumn(3.0, || {
                                    monospace(
                                        color,
                                        format!(
                                            "{code} {:>4}",
                                            value.map_or("—".into(), |v| format!("{v:+.0}"))
                                        ),
                                    );
                                    bar(value.unwrap_or_default(), color);
                                });
                            });
                            if hovered.hovered {
                                let driver = match value {
                                    None => "Waiting for the first demand update".to_owned(),
                                    Some(_) => top_driver(sim, zone).map_or_else(
                                        || "No active demand drivers".to_owned(),
                                        |(name, points)| format!("{name} ({points:+.1} points)"),
                                    ),
                                };
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
                                                    textc(on_secondary_container(), driver);
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
