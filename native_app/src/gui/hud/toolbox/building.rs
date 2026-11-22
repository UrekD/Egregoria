use std::fmt::Write;

use yakui::widgets::List;
use yakui::{Color, CrossAxisAlignment, MainAxisAlignment, MainAxisSize, TextureId, Vec2};

use common::FastMap;
use engine::wgpu::TextureFormat;
use engine::{Context, TextureBuilder};
use geom::{Camera, Degrees, Polygon, Vec3};
use goryak::{
    image_button, mincolumn, minrow, on_secondary_container, padxy, primary, textc, HorizScrollSize,
};
use prototypes::{
    prototypes_iter, try_prototype, BuildingPrototypeID, GoodsCompanyPrototype, Power,
    Prototype, RenderAsset, ServiceID, ServiceKind,
};
use simulation::economy::progression::{can_build, BuildCapability};
use simulation::map::{BuildingKind, Zone, MAX_ZONE_AREA};
use simulation::world_command::WorldCommand;
use simulation::Simulation;

use crate::gui::specialbuilding::{SpecialBuildKind, SpecialBuildingResource};
use crate::gui::{InspectedBuilding, InspectedEntity, PlacementPreview};
use crate::uiworld::UiWorld;

/// Which hand-placed buildings the shelf lists; each is its own toolbar category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum BuildingTab {
    Utilities,
    Services,
    #[default]
    Industry,
}

impl BuildingTab {
    /// The shelf a hand-placed company belongs to. Growables are never hand-placed.
    pub(crate) fn of(company: &GoodsCompanyPrototype) -> Option<Self> {
        if company.id.is_growable() {
            return None;
        }
        Some(match try_prototype(ServiceID::from(company.id)) {
            Some(service)
                if matches!(
                    service.service_kind,
                    ServiceKind::WaterPump | ServiceKind::SewageOutlet
                ) =>
            {
                Self::Utilities
            }
            Some(_) => Self::Services,
            None if company.power_production.is_some() => Self::Utilities,
            None => Self::Industry,
        })
    }
}

pub fn special_building_properties(uiw: &UiWorld, sim: &Simulation) {
    // Inspector windows are drawn over the shelf and can intercept card clicks.
    uiw.write::<InspectedBuilding>().e = None;
    uiw.write::<InspectedEntity>().e = None;
    let tab = *uiw.read::<BuildingTab>();
    minrow(0.0, || building_choices(uiw, sim, tab));
}

fn building_choices(uiw: &UiWorld, sim: &Simulation, tab: BuildingTab) {
    let mut state = uiw.write::<SpecialBuildingResource>();
    let icons = uiw.read::<BuildingIcons>();

    HorizScrollSize::Max.show(|| {
        padxy(50.0, 10.0, || {
            let mut l = List::row();
            l.main_axis_alignment = MainAxisAlignment::Center;
            l.cross_axis_alignment = CrossAxisAlignment::Center;
            l.main_axis_size = MainAxisSize::Min;
            l.item_spacing = 10.0;
            l.show(|| {
                for descr in prototypes_iter::<GoodsCompanyPrototype>()
                    .filter(|company| BuildingTab::of(company) == Some(tab))
                {
                    let Some(tex_id) = icons.ids.get(&descr.parent().id) else {
                        continue;
                    };

                    let service = try_prototype(ServiceID::from(descr.id));
                    let lock = can_build(sim, BuildCapability::Prototype(descr.id)).err();
                    mincolumn(3.0, || {
                        let selected =
                            state.opt.as_ref().and_then(|kind| kind.company) == Some(descr.id);
                        let default_col = if lock.is_some() {
                            Color::rgb(140, 140, 140).with_alpha(0.65)
                        } else if selected {
                            primary()
                        } else {
                            Color::WHITE
                        };
                        let (hover_col, active_col) = if lock.is_some() {
                            (default_col, default_col)
                        } else {
                            (primary(), default_col.with_alpha(0.5))
                        };

                        let resp = image_button(
                            *tex_id,
                            Vec2::splat(64.0),
                            default_col,
                            hover_col,
                            active_col,
                            match (lock, service.map(|service| service.service_kind)) {
                                (Some(lock), _) => {
                                    format!("Unlocks at {} citizens", lock.population).into()
                                }
                                (
                                    None,
                                    Some(ServiceKind::WaterPump | ServiceKind::SewageOutlet),
                                ) => crate::gui::overlays::UTILITY_EXPLANATION.into(),
                                (None, Some(_)) => std::borrow::Cow::Borrowed(""),
                                (None, None) => company_tooltip(descr).into(),
                            },
                        );

                        if let Some(service) = service {
                            textc(on_secondary_container(), &descr.label);
                            match service.service_kind {
                                ServiceKind::WaterPump => textc(
                                    on_secondary_container(),
                                    format!(
                                        "Network-wide · {:.0} production",
                                        descr.water_production.unwrap_or_default()
                                    ),
                                ),
                                ServiceKind::SewageOutlet => textc(
                                    on_secondary_container(),
                                    format!(
                                        "Network-wide · {:.0} capacity",
                                        descr.sewage_capacity.unwrap_or_default()
                                    ),
                                ),
                                _ => {
                                    textc(
                                        on_secondary_container(),
                                        format!("{:.0} m radius", service.radius_m),
                                    );
                                    textc(
                                        on_secondary_container(),
                                        format!("{} capacity", service.capacity),
                                    );
                                }
                            }
                            if matches!(
                                service.service_kind,
                                ServiceKind::WaterPump | ServiceKind::SewageOutlet
                            ) {
                                let power = match service.service_kind {
                                    ServiceKind::WaterPump => descr.power_consumption.map_or_else(
                                        || "Needs power".into(),
                                        |power| format!("Needs {power}"),
                                    ),
                                    _ => "No power needed".into(),
                                };
                                textc(
                                    on_secondary_container(),
                                    format!("{} workers · {power}", descr.n_workers),
                                );
                            }
                            textc(
                                on_secondary_container(),
                                format!("{} / day", service.upkeep_per_day),
                            );
                        }

                        if service.is_none() {
                            let color = if lock.is_some() {
                                on_secondary_container().with_alpha(0.65)
                            } else {
                                on_secondary_container()
                            };
                            // Power plants sit beside the pumps, so they get the same caption.
                            match descr.power_production {
                                Some(power) if tab == BuildingTab::Utilities => {
                                    textc(color, &descr.label);
                                    textc(color, power_production_text(descr, power));
                                }
                                _ if lock.is_some() => textc(color, &descr.label),
                                _ => {}
                            }
                        }
                        if resp.clicked && lock.is_none() {
                            *uiw.write::<PlacementPreview>() = PlacementPreview {
                                company: (!selected).then_some(descr.id),
                                ..PlacementPreview::default()
                            };
                            if selected {
                                state.opt = None;
                                return;
                            }
                            let bkind = BuildingKind::GoodsCompany(descr.id);
                            let bgen = descr.bgen;
                            let has_zone = descr.zone.is_some();
                            state.opt = Some(SpecialBuildKind {
                                company: Some(descr.id),
                                road_snap: true,
                                make: Box::new(move |args| {
                                    vec![WorldCommand::MapBuildSpecialBuilding {
                                        pos: args.obb,
                                        kind: bkind,
                                        gen: bgen,
                                        zone: has_zone.then(|| {
                                            Zone::new(
                                                Polygon::from(args.obb.corners.as_slice()),
                                                geom::Vec2::X,
                                            )
                                        }),
                                        connected_road: args.connected_road,
                                    }]
                                }),
                                size: descr.size,
                                asset: descr.asset.clone(),
                            });
                        }
                    });
                }
            });
        });
    });

    /*
    let bdescrpt_w = 180.0;

    if let Some(descr) = picked_descr {
        Window::new("Building description")
            .default_width(bdescrpt_w)
            .auto_sized()
            .fixed_pos([
                w - toolbox_w - building_select_w - bdescrpt_w,
                h * 0.5 - 30.0,
            ])
            .hscroll(false)
            .title_bar(true)
            .collapsible(false)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.label(format!("workers: {}", descr.n_workers));

                if let Some(ref recipe) = descr.recipe {
                    ui.add_space(10.0);
                    if !recipe.consumption.is_empty() {
                        ui.label("consumption:");
                        for item in &recipe.consumption {
                            item_icon(ui, uiworld, item.id, item.amount);
                        }
                        ui.add_space(10.0);
                    }
                    if !recipe.production.is_empty() {
                        ui.label("production:");
                        for item in &recipe.production {
                            item_icon(ui, uiworld, item.id, item.amount);
                        }
                        ui.add_space(10.0);
                    }
                    ui.label(format!("time: {}", recipe.duration));
                    ui.label(format!("storage multiplier: {}", recipe.storage_multiplier));
                }

                if let Some(p) = descr.power_consumption {
                    ui.add_space(10.0);
                    ui.label(format!("Power: {}", p));
                }
                if let Some(p) = descr.power_production {
                    ui.add_space(10.0);
                    ui.label(format!("Power production: {}", p));
                }
            });
     */
}

fn company_tooltip(company: &GoodsCompanyPrototype) -> String {
    let mut text = format!("{}\n{} workers", company.label, company.n_workers);
    if let Some(recipe) = &company.recipe {
        for (heading, items) in [
            ("Consumes", &recipe.consumption),
            ("Produces", &recipe.production),
        ] {
            if !items.is_empty() {
                let _ = write!(text, "\n{heading}");
                for item in items {
                    let _ = write!(text, "\n  {} × {}", item.amount, item.id.prototype().label);
                }
            }
        }
        let _ = write!(
            text,
            "\nProduction time {}\nStorage multiplier {}",
            recipe.duration, recipe.storage_multiplier
        );
    }
    if let Some(power) = company.power_consumption {
        let _ = write!(text, "\nPower demand {power}");
    }
    if let Some(power) = company.power_production {
        let _ = write!(text, "\n{}", power_production_text(company, power));
    }
    text
}

/// Zoned plants produce per square metre of their zone.
fn power_production_text(company: &GoodsCompanyPrototype, power: Power) -> String {
    if company.zone.is_some() {
        let density = power.watts() as f64 / f64::from(MAX_ZONE_AREA);
        format!("Power production {density:.2} W/m²")
    } else {
        format!("Power production {power}")
    }
}

#[derive(Default)]
pub struct BuildingIcons {
    ids: FastMap<BuildingPrototypeID, TextureId>,
}

pub fn do_icons(ctx: &mut Context, uiw: &UiWorld) {
    let mut state = uiw.write::<BuildingIcons>();

    let mut cam = Camera::new(Vec3::new(0.0, 0.0, 0.0), 256.0, 256.0);

    cam.fovy = 30.0;
    cam.pitch = Degrees(35.0).into();
    cam.yaw = Degrees(-130.0).into();

    state.ids.clear();

    let gfx = &mut ctx.gfx;

    for building in prototypes::BuildingPrototype::iter() {
        if state.ids.contains_key(&building.id) {
            continue;
        }
        if let RenderAsset::Sprite { ref path } = building.asset {
            let t = gfx.texture(path, "building icon");
            let tex_id = ctx.yakui.add_texture(&t);
            state.ids.insert(building.id, tex_id);
            continue;
        }

        let RenderAsset::Mesh { ref path } = building.asset else {
            continue;
        };
        let Ok(mesh) = gfx.mesh(path.as_ref()) else {
            continue;
        };

        let t = TextureBuilder::empty(128, 128, 1, TextureFormat::Rgba8UnormSrgb)
            .with_label("building icon")
            .with_usage(
                engine::wgpu::TextureUsages::COPY_DST
                    | engine::wgpu::TextureUsages::COPY_SRC
                    | engine::wgpu::TextureUsages::RENDER_ATTACHMENT
                    | engine::wgpu::TextureUsages::TEXTURE_BINDING,
            )
            .build_no_queue(&gfx.device);

        let t_msaa = TextureBuilder::empty(128, 128, 1, TextureFormat::Rgba8UnormSrgb)
            .with_label("building icon msaa")
            .with_usage(engine::wgpu::TextureUsages::RENDER_ATTACHMENT)
            .with_sample_count(4)
            .build_no_queue(&gfx.device);

        let aabb3 = mesh.lods[0].aabb3;
        cam.pos = aabb3.center();
        cam.dist = aabb3.ll.distance(aabb3.ur);
        cam.update();

        mesh.render_to_texture(&cam, gfx, &t, &t_msaa);

        let tex_id = ctx.yakui.add_texture(&t);

        state.ids.insert(building.id, tex_id);
    }
}

#[cfg(test)]
mod tests {
    use scenarios::ScenarioCtx;
    use simulation::SimulationOptions;
    use yakui::event::Event;
    use yakui::{offset, Rect, Yakui};

    use super::*;

    #[test]
    fn u3_company_tooltip_fits_a_1024_by_768_viewport_at_double_scale() {
        let _fixture = ScenarioCtx::new(SimulationOptions::LEGACY);
        let label = prototypes_iter::<GoodsCompanyPrototype>()
            .filter(|company| !company.id.is_growable() && !company.id.is_service())
            .map(company_tooltip)
            .max_by_key(|text| text.lines().count())
            .unwrap();
        let mut ui = Yakui::new();
        let size = Vec2::new(1024.0, 768.0);
        ui.set_surface_size(size);
        ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, size));
        ui.set_scale_factor(2.0);

        let frame = |ui: &mut Yakui| {
            ui.start();
            let mut button = None;
            offset(Vec2::new(446.0, 348.0), || {
                button = Some(goryak::tooltip(label.clone(), || yakui::button("Company")).id);
            });
            goryak::render_tooltip();
            ui.finish();
            ui.paint();
            ui.layout_dom().get(button.unwrap()).unwrap().rect
        };
        let card = frame(&mut ui);
        let cursor = (card.pos() + card.size() * 0.5) * 2.0;
        ui.handle_event(Event::CursorMoved(Some(cursor)));
        frame(&mut ui);

        let dom = ui.dom();
        let layer = *dom.get(dom.root()).unwrap().children.last().unwrap();
        let tooltip = dom.get(layer).unwrap().children[0];
        let layout = ui.layout_dom();
        let rect = layout.get(tooltip).unwrap().rect;
        assert!(rect.size().x > 0.0 && rect.size().y > 0.0);
        assert!(layout.viewport().contains_point(rect.pos()));
        assert!(layout.viewport().contains_point(rect.pos() + rect.size()));
    }
}

#[cfg(test)]
mod toolbar1_tests {
    use simulation::economy::progression::{ProgressionMode, ProgressionState};
    use yakui::event::Event;
    use yakui::input::MouseButton;
    use yakui::{Rect, WidgetId, Yakui};

    use super::*;
    use goryak::ImageButtonWidget;
    use prototypes::GoodsCompanyID;

    fn card_rects(ui: &Yakui, id: WidgetId, rects: &mut Vec<Rect>) {
        let node = ui.dom().get(id).unwrap();
        if node.widget.is::<ImageButtonWidget>() {
            rects.push(ui.layout_dom().get(id).unwrap().rect);
        }
        for &child in &node.children {
            card_rects(ui, child, rects);
        }
    }

    #[test]
    fn toolbar1_each_company_has_one_category() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let tab = |name: &str| BuildingTab::of(GoodsCompanyID::new(name).prototype());
        for name in [
            "water-pump",
            "sewage-outlet",
            "solar-panel",
            "coal-power-plant",
        ] {
            assert_eq!(tab(name), Some(BuildingTab::Utilities), "{name}");
        }
        for name in [
            "school",
            "clinic",
            "fire-station",
            "police-station",
            "garbage-depot",
            "park",
        ] {
            assert_eq!(tab(name), Some(BuildingTab::Services), "{name}");
        }
        for name in ["bakery", "flour-factory", "small-depot", "large-warehouse"] {
            assert_eq!(tab(name), Some(BuildingTab::Industry), "{name}");
        }
        for company in prototypes_iter::<GoodsCompanyPrototype>() {
            assert_eq!(BuildingTab::of(company).is_none(), company.id.is_growable());
        }
    }

    #[test]
    fn toolbar1_guided_locked_cards_stay_locked_in_every_category() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new(false);
        sim.write::<ProgressionState>().mode = ProgressionMode::Guided;
        let uiw = UiWorld::with_defaults();
        for (i, building) in prototypes::BuildingPrototype::iter().enumerate() {
            uiw.write::<BuildingIcons>()
                .ids
                .insert(building.id, TextureId::User(i as u64));
        }

        let viewport = Vec2::new(8000.0, 800.0);
        let mut ui = Yakui::new();
        ui.set_surface_size(viewport);
        ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));
        let frame = |ui: &mut Yakui| {
            ui.start();
            special_building_properties(&uiw, &sim);
            ui.finish();
            let mut rects = Vec::new();
            card_rects(ui, ui.dom().root(), &mut rects);
            rects
        };

        let (mut locked, mut unlocked) = (0, 0);
        for tab in [
            BuildingTab::Utilities,
            BuildingTab::Services,
            BuildingTab::Industry,
        ] {
            *uiw.write::<BuildingTab>() = tab;
            let cards: Vec<_> = prototypes_iter::<GoodsCompanyPrototype>()
                .filter(|company| BuildingTab::of(company) == Some(tab))
                .collect();
            for (i, company) in cards.iter().enumerate() {
                uiw.write::<SpecialBuildingResource>().opt = None;
                let rects = frame(&mut ui);
                assert_eq!(rects.len(), cards.len(), "{tab:?}");
                let center = rects[i].pos() + rects[i].size() * 0.5;
                ui.handle_event(Event::CursorMoved(Some(center)));
                for down in [true, false] {
                    ui.handle_event(Event::MouseButtonChanged {
                        button: MouseButton::One,
                        down,
                    });
                }
                frame(&mut ui);
                let armed = uiw
                    .read::<SpecialBuildingResource>()
                    .opt
                    .as_ref()
                    .and_then(|kind| kind.company);
                if can_build(&sim, BuildCapability::Prototype(company.id)).is_err() {
                    assert_eq!(armed, None, "{} is locked", company.name);
                    locked += 1;
                } else {
                    assert_eq!(armed, Some(company.id), "{}", company.name);
                    unlocked += 1;
                }
            }
        }
        assert!(
            locked > 0 && unlocked > 0,
            "{locked} locked, {unlocked} unlocked"
        );
    }
}
