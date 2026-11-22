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
use crate::gui::{
    InspectedBuilding, InspectedEntity, InspectedRoad, PlacementPreview, PotentialCommands, Tool,
};
use crate::inputmap::{InputAction, InputMap};
use crate::uiworld::UiWorld;
use bulldozer::BulldozerState;
use roadbuild::RoadBuildResource;
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
        let mut tool = uiworld.write::<Tool>();
        if *tool == Tool::Bulldozer {
            uiworld.write::<BulldozerState>().reset_gesture();
            *tool = Tool::Hand;
        } else {
            *tool = Tool::Bulldozer;
        }
    }
}

/// Stop the current placement, then the tool, then clear the selection.
/// Returns false when there was nothing to cancel.
pub(crate) fn cancel_interaction(uiw: &UiWorld) -> bool {
    *uiw.write::<PlacementPreview>() = Default::default();
    let road = uiw.write::<InspectedRoad>().0.take().is_some();
    let mut tool = uiw.write::<Tool>();
    if *tool != Tool::Hand {
        let keep_road_tool =
            tool.is_roadbuild() && uiw.write::<RoadBuildResource>().cancel_placement();
        if !keep_road_tool {
            match *tool {
                Tool::Bulldozer => uiw.write::<BulldozerState>().reset_gesture(),
                Tool::Terraforming => uiw.write::<TerraformingResource>().cancel_gesture(),
                Tool::Zoning => uiw.write::<ZoningState>().cancel_gesture(),
                _ => {}
            }
            *tool = Tool::Hand;
        }
        uiw.write::<PotentialCommands>().0.clear();
        return true;
    }
    drop(tool);

    let entity = uiw.write::<InspectedEntity>().e.take().is_some();
    let building = uiw.write::<InspectedBuilding>().e.take().is_some();
    let following = uiw.write::<FollowEntity>().0.take().is_some();
    entity || building || road || following
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
}
