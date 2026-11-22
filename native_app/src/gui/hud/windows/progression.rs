use goryak::{on_secondary_container, outline, primary, textc, titlec, ProgressBar, Window};
use simulation::economy::progression::{
    milestone_reward, progression_status, ObjectiveId, ObjectiveProgress, ProgressionMode,
    OBJECTIVE_REWARD,
};
use simulation::Simulation;
use yakui::widgets::Pad;
use yakui::{constrained, divider, Constraints, Vec2};

use crate::uiworld::UiWorld;

pub(crate) fn render(_uiw: &UiWorld, sim: &Simulation, opened: &mut bool) {
    let color = on_secondary_container();
    Window {
        title: "Progression".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 7.0,
    }
    .show(|| {
        let status = progression_status(sim);
        constrained(
            Constraints {
                min: Vec2::new(340.0, 0.0),
                max: Vec2::new(420.0, f32::INFINITY),
            },
            || {
                goryak::mincolumn(7.0, || {
                    textc(color, format!("Population · {}", status.population));
                    if status.mode == ProgressionMode::FreeBuild {
                        textc(color, "Free build");
                        return;
                    }

                    if let Some((milestone, needed, unlocks)) = status.next {
                        titlec(color, format!("Milestone {milestone} · {needed} citizens"));
                        ProgressBar {
                            value: (status.population as f32 / needed as f32).min(1.0),
                            size: Vec2::new(340.0, 9.0),
                            color: primary(),
                        }
                        .show();
                        for unlock in unlocks {
                            textc(color, format!("Unlocks {unlock}"));
                        }
                        if let Some(reward) = milestone_reward(milestone) {
                            textc(color, format!("{reward} grant"));
                        }
                    } else {
                        titlec(color, "All milestones reached");
                    }

                    divider(outline().with_alpha(0.4), 2.0, 1.0);
                    titlec(color, "Objectives");
                    for (id, progress) in &status.objectives {
                        let (name, description) = match id {
                            ObjectiveId::PositiveBalance => (
                                "Positive Balance",
                                "Two positive operating days in a row with\n95% or more utility demand served.",
                            ),
                            ObjectiveId::StockedShops => (
                                "Stocked Shops",
                                "Fill 90% or more of at least 20 baskets\ndue during one day.",
                            ),
                        };
                        textc(color, format!("{name} · {OBJECTIVE_REWARD}"));
                        textc(color, description);
                        match progress {
                            ObjectiveProgress::Unavailable => textc(color, "Unavailable"),
                            ObjectiveProgress::InProgress { completed, required } => {
                                textc(color, format!("{completed} / {required} qualifying days"));
                            }
                            ObjectiveProgress::Claimed => textc(primary(), "Claimed"),
                        }
                    }

                    divider(outline().with_alpha(0.4), 2.0, 1.0);
                    titlec(color, "Claimed rewards");
                    let mut claimed = false;
                    for milestone in 1..=status.highest_milestone {
                        if let Some(reward) = milestone_reward(milestone) {
                            textc(color, format!("Milestone {milestone} · {reward}"));
                            claimed = true;
                        }
                    }
                    for (id, progress) in status.objectives {
                        if progress == ObjectiveProgress::Claimed {
                            let name = match id {
                                ObjectiveId::PositiveBalance => "Positive Balance",
                                ObjectiveId::StockedShops => "Stocked Shops",
                            };
                            textc(color, format!("{name} · {OBJECTIVE_REWARD}"));
                            claimed = true;
                        }
                    }
                    if !claimed {
                        textc(color, "No rewards claimed yet");
                    }
                });
            },
        );
    });
}
