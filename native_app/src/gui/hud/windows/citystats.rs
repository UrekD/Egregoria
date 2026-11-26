use super::budget::money;
use crate::uiworld::UiWorld;
use goryak::{
    mincolumn, minrow, on_secondary_container, selectable_label_primary, sized_canvas, textc,
    Window,
};
use prototypes::ItemID;
use simulation::economy::{CityStats, CityStatsSample, Market, LEVEL_NAMES};
use simulation::map_dynamic::{active_business, BuildingInfos};
use simulation::{PopulationScale, Simulation, SoulID};
use yakui::paint::{PaintMesh, PaintRect, Vertex};
use yakui::widgets::Pad;
use yakui::{use_state, Color, Rect, Vec2};

const PLOT_SIZE: Vec2 = Vec2::new(235.0, 62.0);

pub fn citystats(_: &UiWorld, sim: &Simulation, opened: &mut bool) {
    Window {
        title: "City Stats".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 10.0,
    }
    .show(|| {
        let stats = sim.read::<CityStats>();
        let scale = sim.population_scale();
        textc(on_secondary_container(), scale.to_string());
        let level = use_state(|| 1usize);
        minrow(6.0, || {
            for (index, name) in LEVEL_NAMES.into_iter().enumerate() {
                if selectable_label_primary(level.get() == index, name).clicked {
                    level.set(index);
                }
            }
        });

        let samples = stats.history.samples(level.get()).collect::<Vec<_>>();
        if samples.is_empty() {
            textc(on_secondary_container(), "Waiting for history samples");
        }

        let workforce = Workforce::read(sim);
        textc(on_secondary_container(), workforce.label(scale));

        type Series = (&'static str, String, fn(&CityStatsSample) -> f64, f64);
        let person_multiplier = f64::from(scale.persons_per_agent());
        let series: [Series; 6] = [
            (
                "Population",
                scale.persons(workforce.population()).to_string(),
                |s| f64::from(s.population),
                person_multiplier,
            ),
            (
                "Employed",
                scale.persons(workforce.employed).to_string(),
                |s| f64::from(s.employed),
                person_multiplier,
            ),
            (
                "Residences",
                stats.residences.to_string(),
                |s| f64::from(s.residences),
                1.0,
            ),
            (
                "Companies",
                stats.companies.to_string(),
                |s| f64::from(s.companies),
                1.0,
            ),
            (
                "Vehicles",
                stats.vehicles.to_string(),
                |s| f64::from(s.vehicles),
                1.0,
            ),
            (
                "Treasury",
                money(i128::from(stats.money.inner()), false),
                |s| s.money.inner() as f64,
                1.0,
            ),
        ];

        for series_row in series.chunks(2) {
            minrow(12.0, || {
                for (label, current, extract, multiplier) in series_row {
                    mincolumn(4.0, || {
                        textc(on_secondary_container(), format!("{label}  {current}"));
                        chart(
                            samples
                                .iter()
                                .map(|sample| extract(sample) * multiplier)
                                .collect(),
                        );
                    });
                }
            });
        }
    });
}

#[derive(Default)]
struct Workforce {
    employed: usize,
    pending_hires: usize,
    unemployed: usize,
    posted_jobs: usize,
}

impl Workforce {
    fn read(sim: &Simulation) -> Self {
        let world = sim.world();
        let jobs = ItemID::new("job-opening");
        let mut workforce = Self::default();
        for human in world.humans.values() {
            if human.work.is_some() {
                workforce.employed += 1;
            } else if human
                .bought
                .0
                .get(&jobs)
                .is_some_and(|trades| !trades.is_empty())
            {
                workforce.pending_hires += 1;
            } else {
                workforce.unemployed += 1;
            }
        }

        let market = sim.read::<Market>();
        if let Some(openings) = market.inner().get(&jobs) {
            let map = sim.map();
            let infos = sim.read::<BuildingInfos>();
            workforce.posted_jobs = world
                .companies
                .iter()
                .filter_map(|(id, company)| {
                    let order = openings.sell_order(SoulID::GoodsCompany(id))?;
                    let building = map.get(company.comp.building)?;
                    let business = active_business(building, &infos, world)?;
                    (business.company == id).then_some(order.qty as usize)
                })
                .sum();
        }
        workforce
    }

    fn population(&self) -> usize {
        self.employed + self.pending_hires + self.unemployed
    }

    fn label(&self, scale: PopulationScale) -> String {
        format!(
            "{} in workforce · {} pending hires · {} unemployed · {} vacant jobs",
            scale.persons(self.population()),
            scale.persons(self.pending_hires),
            scale.persons(self.unemployed),
            scale.persons(self.posted_jobs),
        )
    }
}

fn chart(samples: Vec<f64>) {
    sized_canvas(PLOT_SIZE, Color::hex(0x222831), move |ctx| {
        let rect = ctx.layout.get(ctx.dom.current()).unwrap().rect;
        let area = rect.size() - Vec2::splat(12.0);
        let origin = rect.pos() + Vec2::splat(6.0);
        let line_color = Color::hex(0x84d9e3);
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let max = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mut points = samples.iter().enumerate().map(|(index, value)| {
            let x = if samples.len() == 1 {
                1.0
            } else {
                index as f32 / (samples.len() - 1) as f32
            };
            let y = if min == max {
                0.5
            } else {
                ((max - value) / (max - min)) as f32
            };
            origin + Vec2::new(area.x * x, area.y * y)
        });
        let Some(mut previous) = points.next() else {
            return;
        };
        for point in points {
            let tangent = (point - previous).normalize();
            let normal = Vec2::new(-tangent.y, tangent.x) * 1.25;
            let vertices = [
                previous + normal,
                previous - normal,
                point + normal,
                point - normal,
            ]
            .map(|pos| Vertex::new(pos, Vec2::ZERO, line_color.to_linear()));
            ctx.paint
                .add_mesh(PaintMesh::new(vertices, [0, 1, 2, 2, 1, 3]));
            previous = point;
        }
        let mut dot = PaintRect::new(Rect::from_pos_size(
            previous - Vec2::splat(2.0),
            Vec2::splat(4.0),
        ));
        dot.color = line_color;
        dot.add(ctx.paint);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::vec2;
    use prototypes::Money;
    use scenarios::builders::{fill_houses, grid_city, place_company};
    use simulation::economy::{Trade, TradeTarget};
    use simulation::souls::desire::{Work, WorkKind};
    use simulation::souls::human::spawn_human;
    use simulation::{SimulationOptions, SoulID};

    #[test]
    fn population_scale_city_stats_uses_live_labor_and_posted_jobs() {
        let mut ctx = scenarios::ScenarioCtx::new(SimulationOptions {
            terrain_size: 1,
            save_replay: false,
            persons_per_agent: 8,
            ..SimulationOptions::LEGACY
        });
        let origin = vec2(150.0, 150.0);
        grid_city(&mut ctx, origin, 3, 3, 160.0);
        let building = place_company(&mut ctx, "clinic", origin + vec2(80.0, 40.0)).unwrap();
        let employer = ctx
            .sim
            .world()
            .companies
            .iter()
            .find_map(|(id, company)| (company.comp.building == building).then_some(id))
            .unwrap();
        let houses = fill_houses(&mut ctx, 2);
        let [pending_house, unemployed_house] = houses[..] else {
            panic!("expected two houses")
        };
        let pending_hire = spawn_human(&mut ctx.sim, pending_house).unwrap();
        spawn_human(&mut ctx.sim, unemployed_house).unwrap();
        let jobs = ItemID::new("job-opening");
        ctx.sim
            .world_mut_unchecked()
            .humans
            .get_mut(pending_hire)
            .unwrap()
            .bought
            .0
            .insert(
                jobs,
                vec![Trade {
                    buyer: TradeTarget(SoulID::Human(pending_hire)),
                    seller: TradeTarget(SoulID::GoodsCompany(employer)),
                    qty: 1,
                    kind: jobs,
                    money_delta: Money::ZERO,
                }],
            );
        let stats = ctx.sim.read::<CityStats>();
        assert_eq!(
            stats.population, 0,
            "the live edit precedes the next sample"
        );
        drop(stats);
        let scale = ctx.sim.population_scale();
        let employer = SoulID::GoodsCompany(employer);
        ctx.sim.write::<Market>().sell(employer, origin, jobs, 2, 0);
        assert_eq!(
            Workforce::read(&ctx.sim).label(scale),
            "16 in workforce · 8 pending hires · 8 unemployed · 16 vacant jobs",
        );
        ctx.sim.write::<Market>().sell(employer, origin, jobs, 0, 0);
        assert_eq!(
            Workforce::read(&ctx.sim).label(scale),
            "16 in workforce · 8 pending hires · 8 unemployed · 0 vacant jobs",
        );
        let human = ctx
            .sim
            .world_mut_unchecked()
            .humans
            .get_mut(pending_hire)
            .unwrap();
        human.work = Some(Work::new(building, WorkKind::Worker, 0.0));
        let workforce = Workforce::read(&ctx.sim);
        assert_eq!(scale.persons(workforce.employed), 8);
        assert_eq!(
            workforce.label(scale),
            "16 in workforce · 0 pending hires · 8 unemployed · 0 vacant jobs",
        );
    }
}
