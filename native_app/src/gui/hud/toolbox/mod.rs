use yakui::widgets::{Button, List, Pad};
use yakui::{
    colored_box_container, column, constrained, image, opaque, reflow, spacer, Alignment, Color,
    Constraints, CrossAxisAlignment, Dim2, MainAxisSize, Pivot, Vec2,
};

use goryak::{
    blur_bg, button_primary, button_secondary, checkbox_value, constrained_viewport, fixed_spacer,
    icon_button, image_button, mincolumn, minrow, monospace, on_primary, on_secondary_container,
    outline, padxy, primary, primary_container, primary_link, round_rect, secondary_container,
    tooltip,
};
use simulation::economy::progression::{can_build, BuildCapability, LockedUntil};
use simulation::Simulation;

use crate::gui::bulldozer::BulldozerState;
use crate::gui::specialbuilding::SpecialBuildingResource;
use crate::gui::textures::UiTextures;
use crate::gui::windows::settings::Settings;
use crate::gui::{
    close_tool, GuiState, PlacementPreview, Tool, ToolCategory, ToolMode, ToolSelection,
    ToolbarMemory,
};
use crate::inputmap::{Bindings, InputAction};
use crate::service_icon::ServiceIcon;
use crate::uiworld::UiWorld;
use building::BuildingTab;
use transit::TransitTools;

pub mod building;
pub mod roadbuild;
pub mod roadedit;
pub mod terraforming;
pub mod train;
pub(crate) mod transit;

pub fn new_toolbox(uiworld: &UiWorld, sim: &Simulation, yakui_width: f32) {
    transit::update_lines(uiworld, sim);
    remember_current_mode(uiworld);
    reflow(Alignment::TOP_LEFT, Pivot::TOP_LEFT, Dim2::ZERO, || {
        constrained_viewport(|| {
            let mut l = List::column();
            l.cross_axis_alignment = CrossAxisAlignment::Stretch;
            l.show(|| {
                spacer(1);
                opaque(|| {
                    let mut l = List::column();
                    l.cross_axis_alignment = CrossAxisAlignment::Stretch;
                    l.show(|| {
                        let mut needs_outline = false;
                        blur_bg(primary_container().with_alpha(0.3), 0.0, || {
                            needs_outline = tool_properties(uiworld, sim, yakui_width);
                        });
                        if needs_outline {
                            colored_box_container(outline().with_alpha(0.5), || {
                                fixed_spacer((1.0, 1.0));
                            });
                        }
                        blur_bg(secondary_container().with_alpha(0.3), 0.0, || {
                            padxy(12.0, 10.0, || {
                                let mut toolbar = List::row();
                                toolbar.cross_axis_alignment = CrossAxisAlignment::Center;
                                toolbar.show(|| {
                                    spacer(1);
                                    minrow(10.0, || categories_list(uiworld, sim));
                                    spacer(1);
                                });
                            });
                        });
                    });
                });
            });
        });
    });
}

fn tool_properties(uiw: &UiWorld, sim: &Simulation, yakui_width: f32) -> bool {
    let tool = *uiw.read::<Tool>();

    match tool {
        Tool::Hand => return false,
        Tool::Bulldozer => {
            let mut settings = uiw.write::<Settings>();
            let before = settings.bulldozer_drag;
            padxy(10.0, 5.0, || {
                tooltip("Hold Select and sweep over roads and buildings", || {
                    checkbox_value(
                        &mut settings.bulldozer_drag,
                        on_secondary_container(),
                        "Drag to bulldoze",
                    );
                });
            });
            if before != settings.bulldozer_drag {
                settings.save();
                uiw.write::<BulldozerState>().reset_gesture();
            }
        }
        Tool::LotBrush => return false,
        Tool::TransitStop | Tool::TransitLine | Tool::Train => {
            transport_properties(uiw, sim, tool);
        }
        Tool::Zoning => crate::gui::zoning::properties(uiw, yakui_width),
        Tool::RoadbuildStraight | Tool::RoadbuildCurved => {
            roadbuild::roadbuild_properties(uiw);
        }
        Tool::RoadEditor => {
            roadedit::roadedit_properties(uiw);
        }
        Tool::SpecialBuilding => {
            building::special_building_properties(uiw, sim);
        }
        Tool::Terraforming => {
            terraforming::terraform_properties(uiw);
        }
    }
    true
}

/// Public transport shelf: the mode buttons above the current mode's controls.
fn transport_properties(uiw: &UiWorld, sim: &Simulation, tool: Tool) {
    mincolumn(0.0, || {
        padxy(12.0, 8.0, || {
            minrow(6.0, || {
                for &mode in ToolCategory::Transport.modes() {
                    if mode_button(uiw, sim, mode, tool == mode.tool()) {
                        select_mode(uiw, mode);
                    }
                }
                if tooltip("Show or hide the transit lines panel", || {
                    button_secondary("Lines panel").show()
                })
                .clicked
                {
                    toggle_lines_panel(uiw);
                }
            });
        });
        match tool {
            Tool::Train => train::train_properties(uiw),
            _ => transit::properties(uiw, tool),
        }
    });
}

/// Show or hide the transit lines window. Returns false if there is no such window.
fn toggle_lines_panel(uiw: &UiWorld) -> bool {
    let mut gui = uiw.write::<GuiState>();
    let panel = gui
        .windows
        .window_states()
        .find(|(name, _)| *name == "Transit");
    let Some((_, open)) = panel else {
        return false;
    };
    *open = !*open;
    true
}

/// A mode button inside a shelf. Locked modes show when they unlock.
fn mode_button(uiw: &UiWorld, sim: &Simulation, mode: ToolMode, active: bool) -> bool {
    let label = mode.label();
    if let Some(lock) = mode_lock(sim, mode) {
        tooltip(format!("Unlocks at {} citizens", lock.population), || {
            let mut button = button_secondary(label);
            button.style.fill = button.style.fill.with_alpha(0.45);
            button.style.text.color = button.style.text.color.with_alpha(0.55);
            button.hover_style = button.style.clone();
            button.down_style = button.style.clone();
            button.show();
        });
        return false;
    }
    if mode == ToolMode::BusStop {
        if let Some(texture) = uiw.read::<UiTextures>().service_icon(ServiceIcon::BusStop) {
            let color = if active { primary() } else { Color::WHITE };
            let mut clicked = false;
            mincolumn(2.0, || {
                let icon_clicked = image_button(
                    texture,
                    Vec2::splat(32.0),
                    color,
                    color.with_alpha(0.8),
                    primary(),
                    label,
                )
                .clicked;
                clicked = primary_link(label) || icon_clicked;
            });
            return clicked;
        }
    }
    tooltip(label, || {
        if active {
            button_primary(label).show()
        } else {
            button_secondary(label).show()
        }
    })
    .clicked
}

/// Progression lock on a whole mode. Building cards carry their own locks.
pub(crate) fn mode_lock(sim: &Simulation, mode: ToolMode) -> Option<LockedUntil> {
    match mode {
        ToolMode::BusLine => can_build(sim, BuildCapability::BusService).err(),
        _ => None,
    }
}

/// The toolbar mode of the current tool, if any.
pub(crate) fn current_mode(uiw: &UiWorld) -> Option<ToolMode> {
    ToolMode::from_tool(*uiw.read::<Tool>(), *uiw.read::<BuildingTab>())
}

/// Tools also change from hotkeys, scripts and windows; remember those modes too.
fn remember_current_mode(uiw: &UiWorld) {
    if let Some(mode) = current_mode(uiw) {
        uiw.write::<ToolbarMemory>().remember(mode);
    }
}

/// Switch to a mode, resetting the shelf state that belongs to the previous one.
pub(crate) fn select_mode(uiw: &UiWorld, mode: ToolMode) {
    // Straight and curved roads share the chained start point.
    let tool = *uiw.read::<Tool>();
    if tool != mode.tool() && !(tool.is_roadbuild() && mode.tool().is_roadbuild()) {
        close_tool(uiw);
    }
    if let Some(choice) = mode.building_tab() {
        let mut tab = uiw.write::<BuildingTab>();
        if *tab != choice {
            *tab = choice;
            uiw.write::<SpecialBuildingResource>().opt = None;
            *uiw.write::<PlacementPreview>() = PlacementPreview::default();
        }
    }
    if matches!(mode, ToolMode::BusStop | ToolMode::BusLine) {
        uiw.write::<TransitTools>().reset();
    }
    *uiw.write::<Tool>() = mode.tool();
    uiw.write::<ToolbarMemory>().remember(mode);
}

/// Toolbar click: open the category on its last unlocked mode, or close it when already open.
pub(crate) fn toggle_category(uiw: &UiWorld, sim: &Simulation, category: ToolCategory) {
    if current_mode(uiw).map(ToolMode::category) == Some(category) {
        close_tool(uiw);
        return;
    }
    open_category(uiw, sim, category);
}

fn open_category(uiw: &UiWorld, sim: &Simulation, category: ToolCategory) {
    let last = uiw.read::<ToolbarMemory>().last_mode(category);
    let mode = if mode_lock(sim, last).is_some() {
        category.modes()[0]
    } else {
        last
    };
    select_mode(uiw, mode);
}

/// Apply a tool picked by name, as automation scripts do. Locked modes stay locked, as
/// they do on the toolbar.
pub(crate) fn select_tool(uiw: &UiWorld, sim: &Simulation, selection: ToolSelection) {
    match selection {
        ToolSelection::Hand => close_tool(uiw),
        ToolSelection::Category(category) => open_category(uiw, sim, category),
        ToolSelection::Mode(mode) if mode_lock(sim, mode).is_some() => {
            let fallback = mode.category().modes()[0];
            log::warn!("{} is locked, opening {}", mode.path(), fallback.path());
            select_mode(uiw, fallback);
        }
        ToolSelection::Mode(mode) => select_mode(uiw, mode),
    }
}

fn category_icon(category: ToolCategory) -> (Option<&'static str>, &'static str) {
    match category {
        ToolCategory::Roads => (Some("toolbar_straight_road"), "road"),
        ToolCategory::Intersections => (Some("toolbar_road_edit"), "traffic-light"),
        ToolCategory::Zoning => (Some("toolbar_housetool"), "house"),
        ToolCategory::Utilities => (None, "bolt"),
        ToolCategory::Services => (None, "landmark"),
        ToolCategory::Industry => (Some("toolbar_companies"), "industry"),
        ToolCategory::Transport => (Some("toolbar_train"), "bus"),
        ToolCategory::Bulldoze => (Some("toolbar_bulldozer"), "trash"),
        ToolCategory::Landscaping => (Some("toolbar_terraform"), "mountain"),
    }
}

fn category_tooltip(uiw: &UiWorld, category: ToolCategory) -> String {
    if category == ToolCategory::Roads {
        let bindings = uiw.read::<Bindings>();
        return bindings
            .0
            .get(&InputAction::PrecisionRoad)
            .filter(|binding| !binding.0.is_empty())
            .map_or_else(
                || category.label().to_owned(),
                |binding| {
                    format!("Roads\nHold {binding} for 15° / 8 m snapping and road alignment")
                },
            );
    }
    let binding = category.hotkey().and_then(|action| {
        uiw.read::<Bindings>()
            .0
            .get(&action)
            .filter(|combinations| !combinations.0.is_empty())
            .map(ToString::to_string)
    });
    match binding {
        Some(binding) => format!("{} ({binding})", category.label()),
        None => category.label().to_string(),
    }
}

fn categories_list(uiworld: &UiWorld, sim: &Simulation) {
    let current = current_mode(uiworld).map(ToolMode::category);
    for category in ToolCategory::ALL {
        column(|| {
            let active = current == Some(category);
            let label = category_tooltip(uiworld, category);
            let (texture, glyph) = category_icon(category);
            let textures = uiworld.read::<UiTextures>();
            let texture = match category {
                ToolCategory::Utilities => textures.service_icon(ServiceIcon::Power),
                ToolCategory::Services => textures.service_icon(ServiceIcon::Services),
                _ => texture.and_then(|name| textures.try_get(name)),
            };
            let clicked = match texture {
                Some(texture) => {
                    let (default_col, hover_col) = if active {
                        let c = primary().lerp(&Color::WHITE, 0.3);
                        (c, c)
                    } else {
                        (Color::WHITE, Color::WHITE.with_alpha(0.7))
                    };
                    image_button(
                        texture,
                        Vec2::new(64.0, 64.0),
                        default_col,
                        hover_col,
                        primary(),
                        label,
                    )
                    .clicked
                }
                None => glyph_tile(glyph, active, label),
            };
            if clicked {
                toggle_category(uiworld, sim, category);
            }

            if active {
                select_triangle(uiworld);
            }
        });
    }
    crate::gui::overlays::land_tiles::toolbar(uiworld, sim);
}

/// A 64 px round toolbar tile drawn from an icon font glyph, for categories without artwork.
fn glyph_tile(glyph: &str, active: bool, label: String) -> bool {
    // Matches the translucent grey disc of the toolbar artwork.
    let disc = Color::rgb(232, 232, 232).with_alpha(0.6);
    let (fill, hover) = if active {
        let c = primary().lerp(&Color::WHITE, 0.3);
        (c, c)
    } else {
        (disc, disc.with_alpha(0.7))
    };
    let mut b = icon_button(Button::unstyled(glyph.to_string()));
    b.border_radius = 32.0;
    b.style.fill = fill;
    b.style.text.font_size = 30.0;
    b.style.text.color = Color::rgb(55, 55, 55);
    b.hover_style = b.style.clone();
    b.hover_style.fill = hover;
    b.down_style = b.style.clone();
    b.down_style.fill = primary();
    let mut clicked = false;
    constrained(Constraints::tight(Vec2::splat(64.0)), || {
        clicked = tooltip(label, || b.show()).clicked;
    });
    clicked
}

pub(crate) fn select_triangle(uiworld: &UiWorld) {
    let Some(triangle) = uiworld
        .read::<UiTextures>()
        .try_get("select_triangle_under")
    else {
        return;
    };
    reflow(
        Alignment::CENTER_LEFT,
        Pivot::TOP_LEFT,
        Dim2::pixels(0.0, 32.0),
        || {
            image(triangle, Vec2::new(64.0, 10.0));
        },
    );
}

pub fn updown_button(text: &str) -> Button {
    let mut b = icon_button(button_primary(text));
    b.padding = Pad::balanced(5.0, 2.0);
    b.style.text.font_size = 13.0;
    b.down_style.text.font_size = 13.0;
    b.hover_style.text.font_size = 13.0;
    b
}

pub fn updown_value(v: &mut f32, step: f32, suffix: &'static str) -> bool {
    let mut changed = false;
    let mut l = List::column();
    l.cross_axis_alignment = CrossAxisAlignment::Center;
    l.main_axis_size = MainAxisSize::Min;
    l.item_spacing = 3.0;
    l.show(|| {
        if tooltip("Increase", || updown_button("caret-up").show()).clicked {
            *v += step;
            changed = true;
        }
        round_rect(3.0, primary(), || {
            padxy(5.0, 1.0, || {
                monospace(on_primary(), format!("{:.0}{}", *v, suffix));
            });
        });
        if tooltip("Decrease", || updown_button("caret-down").show()).clicked {
            *v -= step;
            changed = true;
        }
    });
    changed
}

#[cfg(test)]
mod toolbar1_tests {
    use super::*;
    use crate::rendering::OrbitCamera;
    use geom::Camera;
    use goryak::ImageButtonWidget;
    use simulation::economy::progression::{ProgressionMode, ProgressionState};
    use yakui::font::{Font, FontSettings, Fonts};
    use yakui::widgets::ButtonWidget;
    use yakui::{Rect, WidgetId, Yakui};

    fn fixture() -> (UiWorld, Simulation) {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        (UiWorld::with_defaults(), Simulation::new(false))
    }

    fn tool(uiw: &UiWorld) -> Tool {
        *uiw.read::<Tool>()
    }

    #[test]
    fn toolbar1_categories_reopen_on_their_last_mode() {
        let (uiw, sim) = fixture();
        toggle_category(&uiw, &sim, ToolCategory::Roads);
        assert!(tool(&uiw) == Tool::RoadbuildStraight);
        select_mode(&uiw, ToolMode::RoadCurved);
        toggle_category(&uiw, &sim, ToolCategory::Zoning);
        assert!(tool(&uiw) == Tool::Zoning);
        toggle_category(&uiw, &sim, ToolCategory::Roads);
        assert!(tool(&uiw) == Tool::RoadbuildCurved);

        // Clicking the open category closes it.
        toggle_category(&uiw, &sim, ToolCategory::Roads);
        assert!(tool(&uiw) == Tool::Hand);

        // Tools picked by hotkeys, windows or scripts are remembered too.
        *uiw.write::<Tool>() = Tool::Train;
        remember_current_mode(&uiw);
        *uiw.write::<Tool>() = Tool::Hand;
        toggle_category(&uiw, &sim, ToolCategory::Transport);
        assert!(tool(&uiw) == Tool::Train);

        select_tool(&uiw, &sim, ToolSelection::Hand);
        assert!(tool(&uiw) == Tool::Hand);
        select_tool(&uiw, &sim, ToolSelection::Category(ToolCategory::Roads));
        assert!(tool(&uiw) == Tool::RoadbuildCurved);
        select_tool(&uiw, &sim, ToolSelection::Mode(ToolMode::Landscaping));
        assert!(tool(&uiw) == Tool::Terraforming);
    }

    #[test]
    fn toolbar1_building_categories_switch_the_shelf_and_disarm() {
        let (uiw, sim) = fixture();
        toggle_category(&uiw, &sim, ToolCategory::Services);
        assert!(tool(&uiw) == Tool::SpecialBuilding);
        assert_eq!(*uiw.read::<BuildingTab>(), BuildingTab::Services);
        assert_eq!(current_mode(&uiw), Some(ToolMode::Services));

        let school = prototypes::GoodsCompanyID::new("school");
        uiw.write::<PlacementPreview>().company = Some(school);
        uiw.write::<SpecialBuildingResource>().opt =
            Some(crate::gui::specialbuilding::SpecialBuildKind {
                company: Some(school),
                make: Box::new(|_| Vec::new()),
                size: school.prototype().size,
                asset: school.prototype().asset.clone(),
                road_snap: true,
            });
        toggle_category(&uiw, &sim, ToolCategory::Utilities);
        assert!(tool(&uiw) == Tool::SpecialBuilding);
        assert_eq!(current_mode(&uiw), Some(ToolMode::Utilities));
        assert!(uiw.read::<SpecialBuildingResource>().opt.is_none());
        assert!(uiw.read::<PlacementPreview>().company.is_none());

        toggle_category(&uiw, &sim, ToolCategory::Utilities);
        assert!(tool(&uiw) == Tool::Hand);
    }

    #[test]
    fn finalnative_switching_tools_closes_the_previous_one() {
        use crate::gui::roadbuild::{BuildState, RoadBuildResource};
        use simulation::map::MapProject;

        let (uiw, sim) = fixture();
        select_mode(&uiw, ToolMode::RoadStraight);
        uiw.write::<RoadBuildResource>().build_state =
            BuildState::Start(MapProject::ground(geom::Vec3::ZERO));
        toggle_category(&uiw, &sim, ToolCategory::Zoning);
        assert!(matches!(
            uiw.read::<RoadBuildResource>().build_state,
            BuildState::Hover
        ));

        // Arm a school, leave for the bulldozer and come back: it is no longer armed.
        toggle_category(&uiw, &sim, ToolCategory::Services);
        let school = prototypes::GoodsCompanyID::new("school");
        uiw.write::<SpecialBuildingResource>().opt =
            Some(crate::gui::specialbuilding::SpecialBuildKind {
                company: Some(school),
                make: Box::new(|_| Vec::new()),
                size: school.prototype().size,
                asset: school.prototype().asset.clone(),
                road_snap: true,
            });
        select_mode(&uiw, ToolMode::Bulldoze);
        toggle_category(&uiw, &sim, ToolCategory::Services);
        assert!(tool(&uiw) == Tool::SpecialBuilding);
        assert!(uiw.read::<SpecialBuildingResource>().opt.is_none());
    }

    #[test]
    fn toolbar1_guided_bus_line_stays_locked() {
        let (uiw, sim) = fixture();
        sim.write::<ProgressionState>().mode = ProgressionMode::Guided;
        assert!(mode_lock(&sim, ToolMode::BusLine).is_some());
        assert!(mode_lock(&sim, ToolMode::BusStop).is_none());

        uiw.write::<ToolbarMemory>().remember(ToolMode::BusLine);
        toggle_category(&uiw, &sim, ToolCategory::Transport);
        assert!(tool(&uiw) == Tool::TransitStop);

        // Scripts cannot pick it either.
        select_tool(&uiw, &sim, ToolSelection::Mode(ToolMode::BusLine));
        assert!(tool(&uiw) == Tool::TransitStop);

        sim.write::<ProgressionState>().mode = ProgressionMode::FreeBuild;
        close_tool(&uiw);
        uiw.write::<ToolbarMemory>().remember(ToolMode::BusLine);
        toggle_category(&uiw, &sim, ToolCategory::Transport);
        assert!(tool(&uiw) == Tool::TransitLine);
    }

    #[test]
    fn finalnative_lines_panel_button_finds_its_window() {
        let (uiw, _) = fixture();
        let open = |uiw: &UiWorld| {
            uiw.write::<GuiState>()
                .windows
                .window_states()
                .any(|(name, open)| name == "Transit" && *open)
        };
        assert!(!open(&uiw));
        assert!(toggle_lines_panel(&uiw));
        assert!(open(&uiw));
        assert!(toggle_lines_panel(&uiw));
        assert!(!open(&uiw));
    }

    #[test]
    fn toolbar1_bulldozer_tooltip_names_its_key() {
        let (uiw, _) = fixture();
        let label = category_tooltip(&uiw, ToolCategory::Bulldoze);
        assert!(
            label.starts_with("Bulldozer (") && label.ends_with(')'),
            "{label}"
        );
    }

    fn widget_rects<T: yakui::widget::Widget>(ui: &Yakui, id: WidgetId, rects: &mut Vec<Rect>) {
        let node = ui.dom().get(id).unwrap();
        if node.widget.is::<T>() {
            rects.push(ui.layout_dom().get(id).unwrap().rect);
        }
        for &child in &node.children {
            widget_rects::<T>(ui, child, rects);
        }
    }

    fn layout(uiw: &UiWorld, sim: &Simulation, viewport: Vec2) -> (Vec<Rect>, Vec<Rect>) {
        let mut ui = Yakui::new();
        let fonts = ui.dom().get_global_or_init(Fonts::default);
        fonts.add(
            Font::from_bytes(
                include_bytes!("../../../../../assets/font_awesome_solid_900.otf").as_slice(),
                FontSettings::default(),
            )
            .unwrap(),
            Some("icons"),
        );
        ui.set_surface_size(viewport);
        ui.set_unscaled_viewport(Rect::from_pos_size(Vec2::ZERO, viewport));
        let yakui_width = ui.layout_dom().viewport().size().x;
        ui.start();
        new_toolbox(uiw, sim, yakui_width);
        ui.finish();

        let (mut buttons, mut images) = (Vec::new(), Vec::new());
        widget_rects::<ButtonWidget>(&ui, ui.dom().root(), &mut buttons);
        widget_rects::<ImageButtonWidget>(&ui, ui.dom().root(), &mut images);
        let (tiles, mut shelf): (Vec<_>, Vec<_>) = buttons
            .into_iter()
            .partition(|rect| rect.size() == Vec2::splat(64.0));
        shelf.extend(images);
        (tiles, shelf)
    }

    #[test]
    fn toolbar1_toolbar_and_shelves_fit_common_viewports() {
        let (mut uiw, sim) = fixture();
        sim.write::<ProgressionState>().mode = ProgressionMode::Guided;
        // No GPU here: every category falls back to its glyph tile.
        uiw.insert(UiTextures::default());
        for (w, h) in [(1024.0, 768.0), (1280.0, 720.0), (1600.0, 900.0)] {
            let camera = Camera::new(geom::Vec3::ZERO, w, h);
            uiw.insert(OrbitCamera::from_camera(camera));
            let viewport = Rect::from_pos_size(Vec2::ZERO, Vec2::new(w, h));
            let inside = |rect: &Rect| {
                viewport.contains_point(rect.pos()) && viewport.contains_point(rect.max())
            };
            // Shelves whose controls draw without textures.
            for selection in [
                ToolSelection::Hand,
                ToolSelection::Mode(ToolMode::Zoning),
                ToolSelection::Mode(ToolMode::Utilities),
                ToolSelection::Mode(ToolMode::Services),
                ToolSelection::Mode(ToolMode::Industry),
                ToolSelection::Mode(ToolMode::BusStop),
                ToolSelection::Mode(ToolMode::BusLine),
                ToolSelection::Mode(ToolMode::Trains),
                ToolSelection::Mode(ToolMode::Bulldoze),
            ] {
                select_tool(&uiw, &sim, selection);
                let (tiles, shelf) = layout(&uiw, &sim, viewport.size());
                assert_eq!(
                    tiles.len(),
                    ToolCategory::ALL.len(),
                    "{w}x{h} {selection:?}"
                );
                for pair in tiles.windows(2) {
                    assert_eq!(pair[0].pos().y, pair[1].pos().y);
                    assert!(pair[0].max().x < pair[1].pos().x, "{pair:?}");
                }
                let bar_top = tiles[0].pos().y;
                assert!(tiles.iter().all(inside), "{w}x{h} {selection:?} {tiles:?}");
                for rect in &shelf {
                    assert!(inside(rect), "{w}x{h} {selection:?} control {rect:?}");
                    assert!(rect.max().y <= bar_top, "{w}x{h} {selection:?} {rect:?}");
                }
                let transport = matches!(
                    selection,
                    ToolSelection::Mode(mode) if mode.category() == ToolCategory::Transport
                );
                if transport {
                    // Bus stop, bus line, trains and the lines panel.
                    assert!(shelf.len() >= 4, "{w}x{h} {selection:?} {shelf:?}");
                }
                if selection == ToolSelection::Hand {
                    assert!(shelf.is_empty());
                }
            }
        }
    }

    #[test]
    fn zones2_zoning_palette_fits_narrow_viewport_at_both_egui_scales() {
        let (mut uiw, sim) = fixture();
        uiw.insert(UiTextures::default());
        let size = Vec2::new(640.0, 800.0);
        let camera = Camera::new(geom::Vec3::ZERO, size.x, size.y);
        uiw.insert(OrbitCamera::from_camera(camera));
        select_tool(&uiw, &sim, ToolSelection::Mode(ToolMode::Zoning));
        let viewport = Rect::from_pos_size(Vec2::ZERO, size);

        for scale in [0.5, 1.0] {
            uiw.write::<Settings>().gui_scale = scale;
            let (_, shelf) = layout(&uiw, &sim, size);
            // Six palette choices, three densities, three brush controls, two plot layouts.
            assert_eq!(shelf.len(), 14);
            for rect in shelf {
                assert!(
                    viewport.contains_point(rect.pos()) && viewport.contains_point(rect.max()),
                    "Egui scale {scale}: zoning control {rect:?} lies outside the viewport"
                );
            }
        }
    }

    #[test]
    fn roadtools_precision_binding_is_documented_in_roads_tooltip() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let uiw = UiWorld::with_defaults();
        let tooltip = category_tooltip(&uiw, ToolCategory::Roads);
        assert!(
            tooltip.contains("Shift") && tooltip.contains("15°") && tooltip.contains("8 m"),
            "{tooltip}"
        );
        uiw.write::<Bindings>()
            .0
            .get_mut(&InputAction::PrecisionRoad)
            .unwrap()
            .0
            .clear();
        assert_eq!(category_tooltip(&uiw, ToolCategory::Roads), "Roads");
    }
}
