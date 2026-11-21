use crate::gui::zoning::{zone_color, zone_name, ZONES};
use crate::uiworld::UiWorld;
use goryak::{
    blur_bg, is_hovered, mincolumn, minrow, monospace, on_secondary_container, padxy,
    secondary_container, sized_canvas, textc,
};
use prototypes::{try_prototype, GrowableCompanyID, ItemID, RecipeItem, DELTA_F64, TICKS_PER_HOUR};
use simulation::economy::balance::*;
use simulation::economy::{Budget, Market, ZoneDemand};
use simulation::map::ZoneKind;
use simulation::souls::residence::Residences;
use simulation::{Simulation, SoulID};
use std::collections::BTreeMap;
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
                                                    let driver = match value {
                                                None => "Waiting for the first demand update".to_owned(),
                                                Some(_) => top_driver(sim, zone).map_or_else(
                                                    || "No active demand drivers".to_owned(),
                                                    |(name, points)| format!("{name} ({points:+.1} points)"),
                                                ),
                                            };
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

/// ZoneDemand stores the smoothed score. Read live inputs for the current strongest contribution.
pub(crate) fn top_driver(sim: &Simulation, zone: ZoneKind) -> Option<(&'static str, f32)> {
    let world = sim.world();
    let population = world.humans.len();
    let tax = sim.read::<Budget>().tax_rate(zone);
    let mut drivers = vec![(
        "Tax rate",
        -K_TAX * (f32::from(tax) - f32::from(DEFAULT_TAX_RATE_PERMILLE)) / 10.0,
    )];
    let jobs = ItemID::new("job-opening");
    let per_capita = 100.0 / population.max(MIN_DEMAND_POPULATION) as f32;

    let unemployed = || {
        world
            .humans
            .values()
            .filter(|human| {
                human.work.is_none() && human.bought.0.get(&jobs).is_none_or(Vec::is_empty)
            })
            .count() as f32
    };

    match zone {
        ZoneKind::Residential => {
            let mut occupants = BTreeMap::new();
            for human in world.humans.values() {
                *occupants.entry(human.home.house).or_insert(0u32) += 1;
            }
            let vacancies: u64 = sim
                .read::<Residences>()
                .by_building
                .iter()
                .map(|(id, home)| {
                    u64::from(
                        home.capacity
                            .saturating_sub(occupants.get(id).copied().unwrap_or(0)),
                    )
                })
                .sum();
            let open_jobs: u64 = sim.read::<Market>().inner().get(&jobs).map_or(0, |market| {
                world
                    .companies
                    .keys()
                    .filter_map(|id| market.sell_order(SoulID::GoodsCompany(id)))
                    .map(|order| u64::from(order.qty))
                    .sum()
            });
            if population < R_BOOTSTRAP_POPULATION {
                drivers.push(("Early residential growth", R_BASE));
            }
            drivers.push((
                "Job openings and unemployment",
                K_JOBS * (open_jobs as f32 - unemployed()) * per_capita,
            ));
            drivers.push(("Vacant homes", -K_VAC * vacancies as f32 * per_capita));
        }
        ZoneKind::Commercial => {
            let (_, _, supply) = company_rates(sim);
            let need = population as f64 * RETAIL_PER_CAPITA_PER_HOUR;
            drivers.push(("Retail supply gap", K_RET * shortfall(need, supply)));
        }
        ZoneKind::Industrial => {
            let (need, supply, _) = company_rates(sim);
            if world.companies.len() < I_BOOTSTRAP_COMPANIES {
                drivers.push(("Early industrial growth", I_BASE));
            }
            drivers.push(("Goods supply gap", K_GOODS * shortfall(need, supply)));
            drivers.push(("Unemployment", K_UNEMP * unemployed() * per_capita));
        }
        ZoneKind::Office => {
            drivers.push(("Unemployment", K_UNEMP / 3.0 * unemployed() * per_capita));
        }
    }

    drivers
        .into_iter()
        .filter(|(_, points)| *points != 0.0)
        .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
}

fn shortfall(need: f64, supply: f64) -> f32 {
    ((need - supply) / need.max(1.0) * 100.0).clamp(-100.0, 100.0) as f32
}

// Keep this in step with simulation::economy::demand::company_rates.
fn company_rates(sim: &Simulation) -> (f64, f64, f64) {
    let goods = ItemID::new("goods");
    let retail = ItemID::new("retail-goods");
    let amount = |items: &[RecipeItem], kind| {
        items
            .iter()
            .filter(|item| item.id == kind)
            .map(|item| f64::from(item.amount))
            .sum::<f64>()
    };
    let (mut goods_need, mut goods_supply, mut retail_supply) = (0.0, 0.0, 0.0);
    for company in sim.world().companies.values() {
        let Some(proto) = try_prototype(GrowableCompanyID::from(company.comp.proto)) else {
            continue;
        };
        let Some(recipe) = &proto.recipe else {
            continue;
        };
        let duration = recipe.duration.seconds();
        if duration <= 0.0 {
            continue;
        }
        let batches = TICKS_PER_HOUR as f64 * DELTA_F64 / duration;
        let staffed = batches * f64::from(company.raw_productivity(proto, None));
        match proto.zone_kind {
            ZoneKind::Commercial => {
                goods_need += amount(&recipe.consumption, goods) * batches;
                retail_supply += amount(&recipe.production, retail) * staffed;
            }
            ZoneKind::Industrial => goods_supply += amount(&recipe.production, goods) * staffed,
            ZoneKind::Residential | ZoneKind::Office => {}
        }
    }
    (goods_need, goods_supply, retail_supply)
}
