use goryak::{
    blur_bg, fixed_spacer, mincolumn, minrow, on_secondary_container, padxy, secondary_container,
    textc,
};
use yakui::{colored_box_container, reflow, Alignment, Color, Dim2, Pivot, Vec4};

use super::OverlayMode;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Scale {
    pub(super) labels: [&'static str; 2],
    maximum: f32,
    colors: [[f32; 3]; 3],
}

const SERVICE_COLORS: [[f32; 3]; 3] = [[0.85, 0.07, 0.09], [0.98, 0.55, 0.03], [0.03, 0.80, 0.38]];

impl Scale {
    pub(super) fn color(self, value: f32) -> [f32; 4] {
        let t = (value / self.maximum).clamp(0.0, 1.0);
        let (from, to, weight) = if t <= 0.5 {
            (self.colors[0], self.colors[1], 2.0 * t)
        } else {
            (self.colors[1], self.colors[2], 2.0 * t - 1.0)
        };
        let channel = |i: usize| from[i] + (to[i] - from[i]) * weight;
        [channel(0), channel(1), channel(2), 0.62]
    }
}

impl OverlayMode {
    pub(super) fn scale(self) -> Option<Scale> {
        use OverlayMode::*;

        let labels = match self {
            Off => return None,
            Power => ["Blackout / disconnected", "Powered"],
            Water => ["No water", "Supplied"],
            Sewage => ["No sewage service", "Served"],
            Coverage(_) => ["0%", "100%"],
            Pollution => ["0", "100+"],
            LandValue | Happiness => ["0", "100"],
        };
        let maximum = match self {
            Power | Water | Sewage => 1.0,
            _ => 100.0,
        };
        let colors = match self {
            Pollution => [[0.08, 0.23, 0.65], [0.55, 0.06, 0.62], [0.98, 0.19, 0.02]],
            _ => SERVICE_COLORS,
        };
        Some(Scale {
            labels,
            maximum,
            colors,
        })
    }
}

pub(super) fn render(mode: OverlayMode, label: &str, empty: bool) {
    let Some(scale) = mode.scale() else {
        return;
    };
    reflow(
        Alignment::TOP_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(12.0, 112.0),
        || {
            blur_bg(secondary_container().with_alpha(0.94), 8.0, || {
                padxy(10.0, 6.0, || {
                    mincolumn(4.0, || {
                        textc(on_secondary_container(), label.to_owned());
                        if scale.maximum == 1.0 {
                            minrow(12.0, || {
                                for (value, label) in
                                    [(0.0, scale.labels[0]), (1.0, scale.labels[1])]
                                {
                                    minrow(4.0, || {
                                        swatch(scale, value, 12.0);
                                        textc(on_secondary_container(), label);
                                    });
                                }
                            });
                        } else {
                            minrow(0.0, || {
                                for step in 0..=10 {
                                    swatch(scale, step as f32 * scale.maximum / 10.0, 16.0);
                                }
                            });
                            minrow(8.0, || {
                                textc(on_secondary_container(), scale.labels[0]);
                                fixed_spacer((120.0, 0.0));
                                textc(on_secondary_container(), scale.labels[1]);
                            });
                        }
                        match (mode, empty) {
                            (OverlayMode::Pollution, true) => {
                                textc(on_secondary_container(), "No pollution recorded")
                            }
                            (OverlayMode::Happiness, true) => {
                                textc(on_secondary_container(), "No happiness scores yet")
                            }
                            _ => {}
                        }
                    });
                });
            });
        },
    );
}

fn swatch(scale: Scale, value: f32, width: f32) {
    let [r, g, b, _] = scale.color(value);
    colored_box_container(Color::from_linear(Vec4::new(r, g, b, 1.0)), || {
        fixed_spacer((width, 9.0));
    });
}
