pub mod addtrain;
pub mod bulldozer;
pub mod inspected_aura;
pub mod lotbrush;
pub mod roadbuild;
pub mod roadeditor;
pub mod selectable;
pub mod specialbuilding;
pub mod terraforming;
pub mod zoneedit;
pub mod zoning;

use crate::gui::follow::FollowEntity;
use crate::gui::toolbox::building::BuildingTab;
use crate::gui::{
    InspectedBuilding, InspectedEntity, InspectedRoad, PlacementPreview, PotentialCommands, Tool,
};
use crate::inputmap::{InputAction, InputMap};
use crate::uiworld::UiWorld;
use bulldozer::BulldozerState;
use roadbuild::RoadBuildResource;
use specialbuilding::SpecialBuildingResource;
use terraforming::TerraformingResource;
use zoning::ZoningState;

/// Shortcuts every tool shares: right click cancels and B toggles the bulldozer.
pub fn tool_shortcuts(uiworld: &UiWorld) {
    let input = uiworld.read::<InputMap>();
    let cancel = input.just_act.contains(&InputAction::Cancel);
    let bulldozer = input.just_act.contains(&InputAction::Bulldozer);
    drop(input);
    if cancel {
        cancel_interaction(uiworld);
    }
    if bulldozer {
        let was_bulldozer = *uiworld.read::<Tool>() == Tool::Bulldozer;
        close_tool(uiworld);
        if !was_bulldozer {
            *uiworld.write::<Tool>() = Tool::Bulldozer;
        }
    }
}

/// Stop the current placement, then the tool, then clear the selection.
/// Returns false when there was nothing to cancel.
pub(crate) fn cancel_interaction(uiw: &UiWorld) -> bool {
    *uiw.write::<PlacementPreview>() = Default::default();
    let road = uiw.write::<InspectedRoad>().0.take().is_some();
    let tool = *uiw.read::<Tool>();
    if tool != Tool::Hand {
        let keep_road_tool =
            tool.is_roadbuild() && uiw.write::<RoadBuildResource>().cancel_placement();
        if !keep_road_tool {
            close_tool(uiw);
        }
        uiw.write::<PotentialCommands>().0.clear();
        return true;
    }

    let entity = uiw.write::<InspectedEntity>().e.take().is_some();
    let building = uiw.write::<InspectedBuilding>().e.take().is_some();
    let following = uiw.write::<FollowEntity>().0.take().is_some();
    entity || building || road || following
}

/// Drop the current tool's gesture and return to the hand.
pub(crate) fn close_tool(uiw: &UiWorld) {
    let mut tool = uiw.write::<Tool>();
    match *tool {
        Tool::RoadbuildStraight | Tool::RoadbuildCurved => {
            uiw.write::<RoadBuildResource>().cancel_placement();
        }
        Tool::Bulldozer => uiw.write::<BulldozerState>().reset_gesture(),
        Tool::SpecialBuilding => uiw.write::<SpecialBuildingResource>().opt = None,
        Tool::Terraforming => uiw.write::<TerraformingResource>().cancel_gesture(),
        Tool::Zoning => uiw.write::<ZoningState>().cancel_gesture(),
        _ => {}
    }
    *tool = Tool::Hand;
    uiw.write::<PotentialCommands>().0.clear();
}

/// Bottom toolbar categories, in toolbar order. Each opens a shelf of modes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToolCategory {
    Roads,
    Intersections,
    Zoning,
    Utilities,
    Services,
    Industry,
    Transport,
    Bulldoze,
    Landscaping,
}

/// One tool of a toolbar category: the tool it runs and, for buildings, the shelf it filters.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToolMode {
    RoadStraight,
    RoadCurved,
    Intersections,
    Zoning,
    Utilities,
    Services,
    Industry,
    BusStop,
    BusLine,
    Trains,
    Bulldoze,
    Landscaping,
}

impl ToolCategory {
    pub const ALL: [ToolCategory; 9] = [
        ToolCategory::Roads,
        ToolCategory::Intersections,
        ToolCategory::Zoning,
        ToolCategory::Utilities,
        ToolCategory::Services,
        ToolCategory::Industry,
        ToolCategory::Transport,
        ToolCategory::Bulldoze,
        ToolCategory::Landscaping,
    ];

    /// Modes in shelf order; the first is selected until another one is used.
    pub fn modes(self) -> &'static [ToolMode] {
        match self {
            ToolCategory::Roads => &[ToolMode::RoadStraight, ToolMode::RoadCurved],
            ToolCategory::Intersections => &[ToolMode::Intersections],
            ToolCategory::Zoning => &[ToolMode::Zoning],
            ToolCategory::Utilities => &[ToolMode::Utilities],
            ToolCategory::Services => &[ToolMode::Services],
            ToolCategory::Industry => &[ToolMode::Industry],
            ToolCategory::Transport => &[ToolMode::BusStop, ToolMode::BusLine, ToolMode::Trains],
            ToolCategory::Bulldoze => &[ToolMode::Bulldoze],
            ToolCategory::Landscaping => &[ToolMode::Landscaping],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ToolCategory::Roads => "Roads",
            ToolCategory::Intersections => "Intersections",
            ToolCategory::Zoning => "Zoning",
            ToolCategory::Utilities => "Utilities",
            ToolCategory::Services => "Services",
            ToolCategory::Industry => "Industry & commerce",
            ToolCategory::Transport => "Public transport",
            ToolCategory::Bulldoze => "Bulldozer",
            ToolCategory::Landscaping => "Landscaping",
        }
    }

    /// Name used by automation scripts, e.g. `"roads"`.
    pub fn path(self) -> &'static str {
        match self {
            ToolCategory::Roads => "roads",
            ToolCategory::Intersections => "intersections",
            ToolCategory::Zoning => "zoning",
            ToolCategory::Utilities => "utilities",
            ToolCategory::Services => "services",
            ToolCategory::Industry => "industry",
            ToolCategory::Transport => "transport",
            ToolCategory::Bulldoze => "bulldoze",
            ToolCategory::Landscaping => "landscaping",
        }
    }

    /// Keybinding that opens this category, if it has one.
    pub fn hotkey(self) -> Option<InputAction> {
        match self {
            ToolCategory::Bulldoze => Some(InputAction::Bulldozer),
            _ => None,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

impl ToolMode {
    pub fn category(self) -> ToolCategory {
        match self {
            ToolMode::RoadStraight | ToolMode::RoadCurved => ToolCategory::Roads,
            ToolMode::Intersections => ToolCategory::Intersections,
            ToolMode::Zoning => ToolCategory::Zoning,
            ToolMode::Utilities => ToolCategory::Utilities,
            ToolMode::Services => ToolCategory::Services,
            ToolMode::Industry => ToolCategory::Industry,
            ToolMode::BusStop | ToolMode::BusLine | ToolMode::Trains => ToolCategory::Transport,
            ToolMode::Bulldoze => ToolCategory::Bulldoze,
            ToolMode::Landscaping => ToolCategory::Landscaping,
        }
    }

    pub fn tool(self) -> Tool {
        match self {
            ToolMode::RoadStraight => Tool::RoadbuildStraight,
            ToolMode::RoadCurved => Tool::RoadbuildCurved,
            ToolMode::Intersections => Tool::RoadEditor,
            ToolMode::Zoning => Tool::Zoning,
            ToolMode::Utilities | ToolMode::Services | ToolMode::Industry => Tool::SpecialBuilding,
            ToolMode::BusStop => Tool::TransitStop,
            ToolMode::BusLine => Tool::TransitLine,
            ToolMode::Trains => Tool::Train,
            ToolMode::Bulldoze => Tool::Bulldozer,
            ToolMode::Landscaping => Tool::Terraforming,
        }
    }

    pub fn building_tab(self) -> Option<BuildingTab> {
        match self {
            ToolMode::Utilities => Some(BuildingTab::Utilities),
            ToolMode::Services => Some(BuildingTab::Services),
            ToolMode::Industry => Some(BuildingTab::Industry),
            _ => None,
        }
    }

    /// The mode the toolbar shows for a tool. The hand and debug tools have none.
    pub fn from_tool(tool: Tool, tab: BuildingTab) -> Option<ToolMode> {
        Some(match tool {
            Tool::Hand | Tool::LotBrush => return None,
            Tool::RoadbuildStraight => ToolMode::RoadStraight,
            Tool::RoadbuildCurved => ToolMode::RoadCurved,
            Tool::RoadEditor => ToolMode::Intersections,
            Tool::Zoning => ToolMode::Zoning,
            Tool::SpecialBuilding => match tab {
                BuildingTab::Utilities => ToolMode::Utilities,
                BuildingTab::Services => ToolMode::Services,
                BuildingTab::Industry => ToolMode::Industry,
            },
            Tool::TransitStop => ToolMode::BusStop,
            Tool::TransitLine => ToolMode::BusLine,
            Tool::Train => ToolMode::Trains,
            Tool::Bulldozer => ToolMode::Bulldoze,
            Tool::Terraforming => ToolMode::Landscaping,
        })
    }

    /// Label of the mode button inside its category's shelf.
    pub fn label(self) -> &'static str {
        match self {
            ToolMode::RoadStraight => "Straight road",
            ToolMode::RoadCurved => "Curved road",
            ToolMode::BusStop => "Bus stop",
            ToolMode::BusLine => "Bus line",
            ToolMode::Trains => "Trains",
            _ => self.category().label(),
        }
    }

    /// Name used by automation scripts, e.g. `"roads/curved"`.
    pub fn path(self) -> &'static str {
        match self {
            ToolMode::RoadStraight => "roads/straight",
            ToolMode::RoadCurved => "roads/curved",
            ToolMode::BusStop => "transport/bus-stop",
            ToolMode::BusLine => "transport/bus-line",
            ToolMode::Trains => "transport/trains",
            _ => self.category().path(),
        }
    }
}

/// A tool picked by name: the hand, a category's remembered mode or a specific mode.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToolSelection {
    Hand,
    Category(ToolCategory),
    Mode(ToolMode),
}

impl ToolSelection {
    /// Parses `"hand"`, a category such as `"transport"` or a mode such as `"roads/curved"`.
    pub fn parse(path: &str) -> Option<Self> {
        if path == "hand" {
            return Some(Self::Hand);
        }
        let modes = ToolCategory::ALL.iter().flat_map(|c| c.modes());
        if let Some(&mode) = modes.clone().find(|mode| mode.path() == path) {
            return Some(Self::Mode(mode));
        }
        ToolCategory::ALL
            .into_iter()
            .find(|category| category.path() == path)
            .map(Self::Category)
    }
}

impl<'de> serde::Deserialize<'de> for ToolSelection {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let path = String::deserialize(deserializer)?;
        Self::parse(&path).ok_or_else(|| {
            let known: Vec<_> = std::iter::once("hand")
                .chain(ToolCategory::ALL.iter().map(|c| c.path()))
                .chain(
                    ToolCategory::ALL
                        .iter()
                        .flat_map(|c| c.modes())
                        .map(|m| m.path())
                        .filter(|path| path.contains('/')),
                )
                .collect();
            serde::de::Error::custom(format!(
                "unknown tool {path:?}, expected one of {}",
                known.join(", ")
            ))
        })
    }
}

/// The last mode used in each toolbar category.
#[derive(Default)]
pub struct ToolbarMemory {
    last: [Option<ToolMode>; ToolCategory::ALL.len()],
}

impl ToolbarMemory {
    pub fn remember(&mut self, mode: ToolMode) {
        self.last[mode.category().index()] = Some(mode);
    }

    pub fn last_mode(&self, category: ToolCategory) -> ToolMode {
        self.last[category.index()].unwrap_or(category.modes()[0])
    }
}

#[cfg(test)]
mod ux2_tests {
    use super::*;
    use crate::gui::GuiState;
    use crate::session::SessionState;
    use geom::Vec3;
    use roadbuild::BuildState;
    use simulation::map::MapProject;
    use simulation::AnyEntity;

    fn playing_ui() -> UiWorld {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        UiWorld::with_defaults()
    }

    fn press(uiw: &UiWorld, action: InputAction) {
        uiw.write::<InputMap>().just_act.insert(action);
        tool_shortcuts(uiw);
        uiw.write::<InputMap>().just_act.clear();
    }

    #[test]
    fn ux2_right_click_stops_placement_then_tool_then_selection() {
        let uiw = playing_ui();
        for (name, open) in uiw.write::<GuiState>().windows.window_states() {
            *open = name == "Economy";
        }
        uiw.write::<InspectedEntity>().e = Some(AnyEntity::VehicleID(Default::default()));
        *uiw.write::<Tool>() = Tool::RoadbuildCurved;
        uiw.write::<RoadBuildResource>().build_state =
            BuildState::Start(MapProject::ground(Vec3::ZERO));

        press(&uiw, InputAction::Cancel);
        assert!(matches!(
            uiw.read::<RoadBuildResource>().build_state,
            BuildState::Hover
        ));
        assert!(*uiw.read::<Tool>() == Tool::RoadbuildCurved);
        press(&uiw, InputAction::Cancel);
        assert!(*uiw.read::<Tool>() == Tool::Hand);
        assert!(uiw.read::<InspectedEntity>().e.is_some());
        press(&uiw, InputAction::Cancel);
        assert!(uiw.read::<InspectedEntity>().e.is_none());

        // Unlike Escape, right click never pauses or closes windows.
        press(&uiw, InputAction::Cancel);
        assert!(!uiw.read::<SessionState>().paused);
        let mut gui = uiw.write::<GuiState>();
        assert!(gui
            .windows
            .window_states()
            .any(|(name, open)| name == "Economy" && *open));
    }

    #[test]
    fn ux2_right_click_closes_every_tool() {
        let uiw = playing_ui();
        for tool in [
            Tool::RoadEditor,
            Tool::Bulldozer,
            Tool::LotBrush,
            Tool::SpecialBuilding,
            Tool::Train,
            Tool::Terraforming,
            Tool::Zoning,
            Tool::TransitStop,
            Tool::TransitLine,
        ] {
            *uiw.write::<Tool>() = tool;
            uiw.write::<PlacementPreview>().ghost = Some((
                geom::OBB::new(geom::Vec2::ZERO, geom::Vec2::X, 1.0, 1.0),
                Default::default(),
            ));
            press(&uiw, InputAction::Cancel);
            assert!(*uiw.read::<Tool>() == Tool::Hand);
            assert!(uiw.read::<PlacementPreview>().ghost.is_none());
        }
        uiw.write::<ZoningState>().start_test_gesture();
        *uiw.write::<Tool>() = Tool::Zoning;
        press(&uiw, InputAction::Cancel);
        assert!(uiw.read::<ZoningState>().gesture_is_clear());
    }

    #[test]
    fn ux2_b_toggles_the_bulldozer() {
        let uiw = playing_ui();
        *uiw.write::<Tool>() = Tool::Zoning;
        press(&uiw, InputAction::Bulldozer);
        assert!(*uiw.read::<Tool>() == Tool::Bulldozer);
        uiw.write::<BulldozerState>().start_test_gesture();
        press(&uiw, InputAction::Bulldozer);
        assert!(*uiw.read::<Tool>() == Tool::Hand);
        assert!(uiw.read::<BulldozerState>().gesture_is_clear());
    }

    #[test]
    fn finalnative_b_closes_the_previous_tool_like_the_toolbar() {
        let uiw = playing_ui();
        uiw.write::<ZoningState>().start_test_gesture();
        *uiw.write::<Tool>() = Tool::Zoning;
        press(&uiw, InputAction::Bulldozer);
        assert!(*uiw.read::<Tool>() == Tool::Bulldozer);
        assert!(uiw.read::<ZoningState>().gesture_is_clear());

        // An armed building does not stay armed behind the bulldozer.
        let school = prototypes::GoodsCompanyID::new("school");
        *uiw.write::<Tool>() = Tool::SpecialBuilding;
        uiw.write::<specialbuilding::SpecialBuildingResource>().opt =
            Some(specialbuilding::SpecialBuildKind {
                company: Some(school),
                make: Box::new(|_| Vec::new()),
                size: school.prototype().size,
                asset: school.prototype().asset.clone(),
                road_snap: true,
            });
        uiw.write::<PotentialCommands>()
            .set(simulation::world_command::WorldCommand::MapLoadParis);
        press(&uiw, InputAction::Bulldozer);
        assert!(*uiw.read::<Tool>() == Tool::Bulldozer);
        assert!(uiw
            .read::<specialbuilding::SpecialBuildingResource>()
            .opt
            .is_none());
        assert!(uiw.read::<PotentialCommands>().0.is_empty());
    }
}

#[cfg(test)]
mod toolbar1_tests {
    use super::*;
    use common::saveload::Encoder;

    const TOOLS: [Tool; 12] = [
        Tool::Hand,
        Tool::RoadbuildStraight,
        Tool::RoadbuildCurved,
        Tool::RoadEditor,
        Tool::Bulldozer,
        Tool::LotBrush,
        Tool::SpecialBuilding,
        Tool::Train,
        Tool::Terraforming,
        Tool::Zoning,
        Tool::TransitStop,
        Tool::TransitLine,
    ];
    const TABS: [BuildingTab; 3] = [
        BuildingTab::Utilities,
        BuildingTab::Services,
        BuildingTab::Industry,
    ];

    #[test]
    fn toolbar1_every_tool_has_one_category_mode() {
        for tool in TOOLS {
            for tab in TABS {
                let Some(mode) = ToolMode::from_tool(tool, tab) else {
                    assert!(matches!(tool, Tool::Hand | Tool::LotBrush));
                    continue;
                };
                assert!(mode.tool() == tool);
                assert!(mode.category().modes().contains(&mode));
                assert!(mode.building_tab().map_or(true, |own| own == tab));
            }
        }
        for category in ToolCategory::ALL {
            for &mode in category.modes() {
                assert_eq!(mode.category(), category);
                let tab = mode.building_tab().unwrap_or_default();
                assert_eq!(ToolMode::from_tool(mode.tool(), tab), Some(mode));
            }
        }
        let category = |tool, tab| ToolMode::from_tool(tool, tab).map(ToolMode::category);
        let any = BuildingTab::default();
        assert_eq!(
            category(Tool::RoadbuildStraight, any),
            Some(ToolCategory::Roads)
        );
        assert_eq!(
            category(Tool::RoadbuildCurved, any),
            Some(ToolCategory::Roads)
        );
        assert_eq!(
            category(Tool::TransitStop, any),
            Some(ToolCategory::Transport)
        );
        assert_eq!(
            category(Tool::TransitLine, any),
            Some(ToolCategory::Transport)
        );
        assert_eq!(category(Tool::Train, any), Some(ToolCategory::Transport));
        assert_eq!(
            category(Tool::SpecialBuilding, BuildingTab::Utilities),
            Some(ToolCategory::Utilities)
        );
        assert_eq!(
            ToolCategory::Bulldoze.hotkey(),
            Some(InputAction::Bulldozer)
        );
    }

    #[test]
    fn toolbar1_tool_names_parse_back() {
        assert_eq!(ToolSelection::parse("hand"), Some(ToolSelection::Hand));
        assert_eq!(
            ToolSelection::parse("roads"),
            Some(ToolSelection::Category(ToolCategory::Roads))
        );
        assert_eq!(
            ToolSelection::parse("roads/curved"),
            Some(ToolSelection::Mode(ToolMode::RoadCurved))
        );
        for unknown in ["Zoning", "roads/freeform", "", "transport/"] {
            assert_eq!(ToolSelection::parse(unknown), None, "{unknown}");
        }
        let mut paths = Vec::new();
        for category in ToolCategory::ALL {
            for &mode in category.modes() {
                assert_eq!(
                    ToolSelection::parse(mode.path()),
                    Some(ToolSelection::Mode(mode))
                );
                assert!(!paths.contains(&mode.path()), "{}", mode.path());
                paths.push(mode.path());
            }
        }
        let err = common::saveload::JSON::decode::<ToolSelection>(br#""Curved""#)
            .err()
            .unwrap();
        assert!(format!("{err:?}").contains("roads/curved"), "{err:?}");
    }

    #[test]
    fn toolbar1_memory_starts_on_the_first_mode_and_keeps_the_last() {
        let mut memory = ToolbarMemory::default();
        for category in ToolCategory::ALL {
            assert_eq!(memory.last_mode(category), category.modes()[0]);
        }
        memory.remember(ToolMode::RoadCurved);
        memory.remember(ToolMode::Trains);
        assert_eq!(memory.last_mode(ToolCategory::Roads), ToolMode::RoadCurved);
        assert_eq!(memory.last_mode(ToolCategory::Transport), ToolMode::Trains);
        assert_eq!(memory.last_mode(ToolCategory::Zoning), ToolMode::Zoning);
        memory.remember(ToolMode::RoadStraight);
        assert_eq!(
            memory.last_mode(ToolCategory::Roads),
            ToolMode::RoadStraight
        );
    }
}
