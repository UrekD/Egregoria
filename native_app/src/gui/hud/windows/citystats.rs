use super::budget::money;
use crate::uiworld::UiWorld;
use goryak::{
    mincolumn, minrow, on_secondary_container, selectable_label_primary, sized_canvas, textc,
    Window,
};
use simulation::economy::{CityStats, CityStatsSample, LEVEL_NAMES};
use simulation::Simulation;
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

        type Series = (&'static str, String, fn(&CityStatsSample) -> f64);
        let series: [Series; 6] = [
            ("Population", stats.population.to_string(), |s| {
                f64::from(s.population)
            }),
            ("Employed", stats.employed.to_string(), |s| {
                f64::from(s.employed)
            }),
            ("Residences", stats.residences.to_string(), |s| {
                f64::from(s.residences)
            }),
            ("Companies", stats.companies.to_string(), |s| {
                f64::from(s.companies)
            }),
            ("Vehicles", stats.vehicles.to_string(), |s| {
                f64::from(s.vehicles)
            }),
            (
                "Treasury",
                money(i128::from(stats.money.inner()), false),
                |s| s.money.inner() as f64,
            ),
        ];

        for series_row in series.chunks(2) {
            minrow(12.0, || {
                for (label, current, extract) in series_row {
                    mincolumn(4.0, || {
                        textc(on_secondary_container(), format!("{label}  {current}"));
                        chart(samples.iter().map(|sample| extract(sample)).collect());
                    });
                }
            });
        }
    });
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
