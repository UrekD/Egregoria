use goryak::{
    button_primary, button_secondary, constrained_viewport, error, mincolumn, minrow,
    on_secondary_container, selectable_label_primary, text_edit, textc, VertScrollSize,
};
use simulation::economy::{population::PopulationScale, progression::ProgressionMode};
use simulation::map::LotLayout;
use simulation::{MapPreset, NewGameOptions, Simulation};

use super::{self as session, PreparedWorld, SessionUi, WorldJobKind, WorldReplacement};
use crate::uiworld::UiWorld;

#[derive(Default)]
pub(super) struct NewGameForm {
    name: String,
    seed: String,
    size: String,
    money: String,
    preset: MapPreset,
    progression: ProgressionMode,
    lot_layout: LotLayout,
    population_scale: PopulationScale,
    advanced: bool,
    record_replay: bool,
    error: Option<&'static str>,
}

impl NewGameForm {
    pub(super) fn fresh() -> Self {
        Self {
            name: "New City".into(),
            seed: session::random_seed().to_string(),
            size: "50".into(),
            money: "150000".into(),
            progression: ProgressionMode::Guided,
            ..Self::default()
        }
    }

    fn options(&self) -> Result<NewGameOptions, &'static str> {
        let seed = self
            .seed
            .trim()
            .parse()
            .map_err(|_| "Enter a whole seed from 0 to 18446744073709551615.")?;
        let terrain_chunks = self
            .size
            .trim()
            .parse()
            .ok()
            .filter(|size| {
                (NewGameOptions::MIN_TERRAIN_CHUNKS..=simulation::map::MAX_NATIVE_LAND_SIDE)
                    .contains(size)
            })
            .ok_or("Enter a map size from 16 to 50 chunks.")?;
        let starting_money_bucks = self
            .money
            .trim()
            .parse()
            .ok()
            .filter(|money| {
                (NewGameOptions::MIN_STARTING_MONEY_BUCKS
                    ..=NewGameOptions::MAX_STARTING_MONEY_BUCKS)
                    .contains(money)
            })
            .ok_or("Enter starting money from 10,000 to 10,000,000 bucks.")?;
        Ok(NewGameOptions {
            preset: self.preset,
            seed,
            terrain_chunks,
            starting_money_bucks,
            progression: self.progression,
            lot_layout: self.lot_layout,
            land_mode: Default::default(),
            persons_per_agent: self.population_scale.persons_per_agent(),
        })
    }
}

pub(super) fn render(uiw: &UiWorld) {
    let mut ui = uiw.write::<SessionUi>();
    let form = &mut ui.new_game;
    let mut generate = false;
    let mut back = false;
    constrained_viewport(|| {
        mincolumn(8.0, || {
            // Reserve space for window chrome and keep the actions outside the scroll area.
            VertScrollSize::Percent(0.8).show(|| {
                mincolumn(8.0, || render_fields(form));
            });
            if let Some(message) = form.error {
                textc(error(), message);
            }
            minrow(8.0, || {
                generate = button_primary("Create City").show().clicked;
                back = button_secondary("Back").show().clicked;
            });
        });
    });

    if back {
        drop(ui);
        session::back(uiw);
    } else if generate {
        let name = form.name.trim().to_owned();
        if name.is_empty() {
            form.error = Some("Enter a city name.");
            return;
        }
        let options = match form.options() {
            Ok(options) => options,
            Err(message) => {
                form.error = Some(message);
                return;
            }
        };
        let record = form.record_replay;
        form.error = None;
        drop(ui);
        let existing = session::known_slot_ids(uiw);
        session::start_world_job(uiw, WorldJobKind::NewGame, move || {
            let slot = session::choose_city_slot(common::saveload::save_dir(), &name, existing)
                .map_err(|error| format!("Could not choose a save slot. {error}"))?;
            let sim = if record {
                Simulation::new_game_with_replay(&options)
            } else {
                Simulation::new_game(&options)
            }
            .map_err(|error| error.to_string())?;
            Ok(PreparedWorld {
                replacement: WorldReplacement::NewGame(sim, slot),
                name,
            })
        });
    }
}

fn render_fields(form: &mut NewGameForm) {
    textc(on_secondary_container(), "City name");
    text_edit(275.0, &mut form.name, "City name");

    textc(on_secondary_container(), "Terrain preset");
    minrow(8.0, || {
        for (preset, label) in [
            (MapPreset::Procedural, "Procedural"),
            (MapPreset::Flat, "Flat"),
            (MapPreset::Waters, "Waters"),
        ] {
            if selectable_label_primary(form.preset == preset, label).clicked {
                form.preset = preset;
            }
        }
    });

    textc(on_secondary_container(), "City mode");
    minrow(8.0, || {
        for (mode, label) in [
            (ProgressionMode::Guided, "Guided"),
            (ProgressionMode::FreeBuild, "Free build"),
        ] {
            if selectable_label_primary(form.progression == mode, label).clicked {
                form.progression = mode;
            }
        }
    });

    crate::gui::zoning::lot_layout_picker(form.lot_layout, |layout| {
        form.lot_layout = layout;
    });

    population_options(&mut form.population_scale);

    textc(on_secondary_container(), "Seed (0 uses legacy generation)");
    minrow(8.0, || {
        text_edit(230.0, &mut form.seed, "Seed");
        if button_secondary("Randomize").show().clicked {
            form.seed = session::random_seed().to_string();
        }
    });

    textc(on_secondary_container(), "Map size (16–80 chunks)");
    minrow(8.0, || {
        text_edit(65.0, &mut form.size, "Size");
        for (size, label) in [("16", "Small"), ("50", "Standard"), ("80", "Large")] {
            if selectable_label_primary(form.size == size, label).clicked {
                form.size = size.into();
            }
        }
    });

    textc(on_secondary_container(), "Starting money (bucks)");
    text_edit(175.0, &mut form.money, "Starting money");

    if button_secondary("Advanced").show().clicked {
        form.advanced ^= true;
    }
    if form.advanced && selectable_label_primary(form.record_replay, "Record debug replay").clicked
    {
        form.record_replay ^= true;
    }
}

fn population_options(selected: &mut PopulationScale) {
    textc(
        on_secondary_container(),
        format!("Population scale · {selected}"),
    );
    minrow(8.0, || {
        for n in PopulationScale::PRESETS {
            let scale = PopulationScale::try_new(n).expect("valid preset");
            let label = if scale == PopulationScale::REALISTIC {
                "Realistic".into()
            } else {
                format!("1:{n}")
            };
            if selectable_label_primary(*selected == scale, &label).clicked {
                *selected = scale;
            }
        }
    });
}

#[cfg(test)]
#[test]
fn population_scale_new_game_picker_passes_the_clicked_preset_to_bootstrap() {
    use yakui::event::Event;
    use yakui::input::MouseButton;
    use yakui::widgets::ButtonWidget;
    use yakui::{Rect, Vec2, WidgetId, Yakui};

    fn button_rects(ui: &Yakui, id: WidgetId, output: &mut Vec<Rect>) {
        let node = ui.dom().get(id).unwrap();
        if node.widget.is::<ButtonWidget>() {
            output.push(ui.layout_dom().get(id).unwrap().rect);
        }
        for &child in &node.children {
            button_rects(ui, child, output);
        }
    }

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let mut form = NewGameForm {
        preset: MapPreset::Flat,
        size: "16".into(),
        ..NewGameForm::fresh()
    };
    assert_eq!(form.options().unwrap().persons_per_agent, 1);
    let viewport = Vec2::new(1024.0, 768.0);
    let mut ui = Yakui::new();
    ui.set_surface_size(viewport);
    ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));
    let render = |ui: &mut Yakui, form: &mut NewGameForm| {
        ui.start();
        yakui::column(|| population_options(&mut form.population_scale));
        ui.finish();
    };
    render(&mut ui, &mut form);
    let mut rects = Vec::new();
    button_rects(&ui, ui.dom().root(), &mut rects);
    assert_eq!(rects.len(), PopulationScale::PRESETS.len());
    assert!(rects.iter().all(|rect| rect.max().x <= viewport.x));
    let eight = rects[3];
    ui.handle_event(Event::CursorMoved(Some(eight.pos() + eight.size() * 0.5)));
    for down in [true, false] {
        ui.handle_event(Event::MouseButtonChanged {
            button: MouseButton::One,
            down,
        });
    }
    render(&mut ui, &mut form);
    let options = form.options().unwrap();
    assert_eq!(options.persons_per_agent, 8);
    let city = Simulation::new_game(&options).unwrap();
    assert_eq!(city.population_scale().to_string(), "Scaled (1:8)");
}

#[cfg(test)]
#[test]
fn p7_create_city_keeps_stock_traffic_after_build_and_reload() {
    use geom::vec3;
    use simulation::map::{LanePatternBuilder, MapProject};
    use simulation::world_command::{CommandOutcomes, WorldCommand};
    use simulation::{SlotId, SnapshotRevision};

    scenarios::ensure_repo_root_cwd();
    crate::init::init();
    let form = NewGameForm {
        size: "16".into(),
        seed: "6042".into(),
        money: "150000".into(),
        preset: MapPreset::Flat,
        ..NewGameForm::default()
    };
    let options = form.options().unwrap();
    assert_eq!(options.land_mode, simulation::map::LandMode::Unrestricted);
    let mut city = Simulation::new_game(&options).unwrap();
    let opening_stock = city.map().uses_stock_traffic();
    city.apply_commands([&WorldCommand::MapMakeConnection {
        from: MapProject::ground(vec3(100.0, 200.0, 0.0)),
        to: MapProject::ground(vec3(350.0, 200.0, 0.0)),
        inter: None,
        pat: LanePatternBuilder::new().build(),
    }]);
    assert_eq!(city.read::<CommandOutcomes>().accepted, [0]);
    let root = std::env::temp_dir().join(format!("egregoria-p7-create-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let slot = SlotId::new("create-city").unwrap();
    city.save_to_slot(&root, &slot, SnapshotRevision::new(1))
        .unwrap();
    let reopened = Simulation::load_from_slot(&root, &slot).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(reopened.map().roads().len(), 1);
    assert!(
        opening_stock && city.map().uses_stock_traffic() && reopened.map().uses_stock_traffic()
    );
}
