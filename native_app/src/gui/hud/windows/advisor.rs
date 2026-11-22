use crate::gui::follow::FollowEntity;
use crate::gui::hud::toasts::{publish, severity_color};
use crate::gui::inspect::focus_building;
use crate::gui::notifications::{AlertTarget, NotificationCenter};
use crate::gui::overlays::OverlayMode;
use crate::uiworld::UiWorld;
use geom::Vec3;
use goryak::{
    mincolumn, minrow, on_secondary_container, outline, primary_link, textc, VertScrollSize, Window,
};
use simulation::map::BuildingID;
use simulation::map_dynamic::Transit;
use simulation::Simulation;
use yakui::widgets::Pad;
use yakui::{constrained, divider, Constraints, Vec2};

pub(super) const AVAILABLE: bool = true;

pub(super) fn render(uiw: &UiWorld, sim: &Simulation, opened: &mut bool) {
    let mut navigated = false;
    Window {
        title: "Advisor".into(),
        pad: Pad::all(14.0),
        radius: 10.0,
        opened,
        child_spacing: 6.0,
    }
    .show(|| {
        publish(uiw);
        let center = uiw.read::<NotificationCenter>();
        if center.iter().len() == 0 {
            textc(on_secondary_container(), "No notifications yet");
            return;
        }
        constrained(
            Constraints {
                min: Vec2::new(320.0, 0.0),
                max: Vec2::new(390.0, f32::INFINITY),
            },
            || {
                VertScrollSize::Fixed(335.0).show(|| {
                    mincolumn(8.0, || {
                        for (index, notice) in center.iter().rev().enumerate() {
                            if index != 0 {
                                divider(outline().with_alpha(0.4), 2.0, 1.0);
                            }
                            mincolumn(3.0, || {
                                minrow(12.0, || {
                                    textc(severity_color(notice.severity), notice.kind.label());
                                    textc(
                                        on_secondary_container(),
                                        format!("Tick {}", notice.tick.0),
                                    );
                                });
                                textc(on_secondary_container(), notice.text.clone());
                                match resolve_target(sim, notice.target) {
                                    TargetLink::Building(id, door) => {
                                        if primary_link("Go to building") {
                                            focus_building(uiw, id, door);
                                            navigated = true;
                                        }
                                    }
                                    TargetLink::Line(stop) => {
                                        if primary_link("Go to line") {
                                            uiw.write::<FollowEntity>().0 = None;
                                            uiw.camera_mut().targetpos = stop;
                                            *uiw.write::<OverlayMode>() = OverlayMode::Transit;
                                            navigated = true;
                                        }
                                    }
                                    TargetLink::Removed => {
                                        textc(on_secondary_container(), "(removed)")
                                    }
                                    TargetLink::NoLocation => {}
                                }
                            });
                        }
                    });
                });
            },
        );
    });
    if navigated {
        *opened = false;
    }
}

enum TargetLink {
    Building(BuildingID, Vec3),
    Line(Vec3),
    Removed,
    NoLocation,
}

fn resolve_target(sim: &Simulation, target: AlertTarget) -> TargetLink {
    match target {
        AlertTarget::Building(id) => sim.map().get(id).map_or(TargetLink::Removed, |building| {
            TargetLink::Building(id, building.door_pos)
        }),
        AlertTarget::Line(id) => {
            let transit = sim.read::<Transit>();
            let Some(line) = transit.lines.get(id) else {
                return TargetLink::Removed;
            };
            line.stops
                .iter()
                .find_map(|id| transit.stops.get(*id))
                .map_or(TargetLink::NoLocation, |stop| TargetLink::Line(stop.pos))
        }
        AlertTarget::CityWide => TargetLink::NoLocation,
    }
}

#[cfg(test)]
#[test]
fn p4g_advisor_resolves_removed_targets_safely() {
    use scenarios::ScenarioCtx;
    use simulation::SimulationOptions;

    let mut test = ScenarioCtx::new(SimulationOptions {
        terrain_size: 1,
        save_replay: false,
        ..SimulationOptions::LEGACY
    });
    scenarios::builders::grid_city(&mut test, geom::vec2(150.0, 150.0), 2, 2, 160.0);
    let house = scenarios::builders::fill_houses(&mut test, 1)[0];
    let target = AlertTarget::Building(house);
    assert!(matches!(
        resolve_target(&test.sim, target),
        TargetLink::Building(_, _)
    ));
    test.sim.apply_commands(
        &[simulation::world_command::WorldCommand::MapRemoveBuilding(
            house,
        )],
    );
    assert!(matches!(
        resolve_target(&test.sim, target),
        TargetLink::Removed
    ));
    assert!(matches!(
        resolve_target(&test.sim, AlertTarget::Line(Default::default())),
        TargetLink::Removed
    ));
}
