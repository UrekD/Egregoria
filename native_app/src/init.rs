use crate::debug_gui::debug_window::{DebugObjs, DebugState, TestFieldProperties};
use crate::game_loop::Timings;
use crate::gui::addtrain::TrainSpawnResource;
use crate::gui::bulldozer::BulldozerState;
use crate::gui::chat::GUIChatState;
use crate::gui::follow::FollowEntity;
use crate::gui::keybinds::{load_bindings, KeybindState};
use crate::gui::lotbrush::LotBrushResource;
use crate::gui::notifications::NotificationCenter;
use crate::gui::overlays::{self, OverlayMode};
use crate::gui::roadbuild::RoadBuildResource;
use crate::gui::roadeditor::RoadEditorResource;
use crate::gui::specialbuilding::SpecialBuildingResource;
use crate::gui::terraforming::TerraformingResource;
use crate::gui::toolbox::building::{BuildingIcons, BuildingTab};
use crate::gui::toolbox::transit::TransitTools;
use crate::gui::windows::economy::EconomyState;
use crate::gui::windows::load::LoadState;
use crate::gui::windows::settings::{AutoSaveEvery, Settings, SettingsState};
use crate::gui::windows::transit_lines::PanelState;
use crate::gui::zoneedit::ZoneEditState;
use crate::gui::zoning::ZoningState;
use crate::gui::{
    ErrorTooltip, ExitState, GuiState, InspectedBuilding, InspectedEntity, PotentialCommands,
    InspectedRoad, PlacementPreview, TimeAlways, ToastState, Tool, ToolbarMemory,
};
use crate::inputmap::{Bindings, InputMap};
use crate::network::NetworkState;
use crate::rendering::immediate::{ImmediateDraw, ImmediateSound};
use crate::session::save_coordinator::SaveCoordinator;
use crate::session::{SessionScreen, SessionState, SessionUi, WorldResetHook, WorldResetHooks};
use crate::uiworld::{ReceivedCommands, SaveLoadState, UiWorld};
use common::saveload::Encoder;
use serde::de::DeserializeOwned;
use serde::Serialize;
use simulation::world_command::WorldCommands;
use std::sync::Once;

/// init is called at the beginning of the program to initialize the globals
/// It is mostly to register types for serialization and initialization of the engine
pub fn init() {
    simulation::init::init();
    static INIT: Once = Once::new();
    INIT.call_once(register_ui_resources);
}

fn register_ui_resources() {
    register_resource::<Settings>("settings");
    #[cfg(feature = "multiplayer")]
    register_resource::<crate::gui::windows::network::NetworkConnectionInfo>("netinfo");
    register_resource::<LotBrushResource>("lot_brush");
    register_resource_noserialize::<Bindings>();
    unsafe {
        (*std::ptr::addr_of_mut!(SAVELOAD_FUNCS)).push(SaveLoadFunc {
            save: Box::new(|uiw| {
                uiw.write::<KeybindState>()
                    .save_bindings(&uiw.read::<Bindings>(), None);
            }),
            load: Box::new(|uiw| uiw.insert(load_bindings())),
        });
    }

    register_resource_noserialize::<SessionScreen>();
    register_resource_noserialize::<SessionState>();
    register_resource_noserialize::<SessionUi>();
    register_resource_noserialize::<SaveCoordinator>();
    register_resource_noserialize::<WorldResetHooks>();
    register_resource_noserialize::<GuiState>();
    register_world_resource::<TerraformingResource>();
    register_world_resource::<BulldozerState>();
    register_resource_noserialize::<DebugObjs>();
    register_resource_noserialize::<DebugState>();
    register_world_resource::<ErrorTooltip>();
    register_world_resource::<ExitState>();
    register_world_resource::<FollowEntity>();
    register_resource_noserialize::<GUIChatState>();
    register_resource_noserialize::<TimeAlways>();
    register_resource_noserialize::<ImmediateDraw>();
    register_world_resource::<ImmediateSound>();
    register_resource_noserialize::<InputMap>();
    register_world_resource::<InspectedEntity>();
    register_world_resource::<InspectedBuilding>();
    register_world_resource::<InspectedRoad>();
    register_world_resource::<PlacementPreview>();
    register_resource_noserialize::<OverlayMode>();
    register_world_resource::<ToastState>();
    register_resource_noserialize::<NetworkState>();
    register_world_resource::<PotentialCommands>();
    register_world_resource::<ZoneEditState>();
    register_world_resource::<ZoningState>();
    register_resource_noserialize::<TestFieldProperties>();
    register_world_resource::<ReceivedCommands>();
    register_world_resource::<RoadBuildResource>();
    register_world_resource::<RoadEditorResource>();
    register_world_resource::<SpecialBuildingResource>();
    register_world_resource::<TrainSpawnResource>();
    register_resource_noserialize::<Timings>();
    register_world_resource::<Tool>();
    register_world_resource::<WorldCommands>();
    register_world_resource::<LoadState>();
    register_resource_noserialize::<SaveLoadState>();
    register_resource_noserialize::<EconomyState>();
    register_resource_noserialize::<SettingsState>();
    register_resource_noserialize::<BuildingIcons>();
    register_resource_noserialize::<BuildingTab>();
    register_resource_noserialize::<ToolbarMemory>();
    register_resource_noserialize::<KeybindState>();
    register_resource_noserialize::<NotificationCenter>();
    register_world_reset(|uiw, _| uiw.write::<NotificationCenter>().clear());
    register_world_resource::<TransitTools>();
    register_world_resource::<PanelState>();
    register_world_reset(|uiw, _| uiw.write::<DebugState>().connectivity = Default::default());
    register_world_reset(|uiw, _| {
        let mut draw = uiw.write::<ImmediateDraw>();
        draw.orders.clear();
        draw.clear_persistent();
        overlays::reset_world_cache();
    });
}

pub(crate) fn configure_script_settings(uiw: &UiWorld) {
    let mut settings = uiw.write::<Settings>();
    settings.time_warp = 0;
    settings.auto_save_every = AutoSaveEvery::Never;
}

pub struct InitFunc {
    pub f: Box<dyn Fn(&mut UiWorld) + 'static>,
}

pub struct SaveLoadFunc {
    pub save: Box<dyn Fn(&UiWorld) + 'static>,
    pub load: Box<dyn Fn(&mut UiWorld) + 'static>,
}

pub static mut INIT_FUNCS: Vec<InitFunc> = Vec::new();
pub static mut SAVELOAD_FUNCS: Vec<SaveLoadFunc> = Vec::new();

fn register_resource_noserialize<T: 'static + Default>() {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(|uiw| uiw.insert(T::default())),
        });
    }
}

fn register_resource<T: 'static + Default + Serialize + DeserializeOwned>(name: &'static str) {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(|uiw| uiw.insert(T::default())),
        });
        SAVELOAD_FUNCS.push(SaveLoadFunc {
            save: Box::new(move |uiworld| {
                <common::saveload::JSONPretty as Encoder>::save(&*uiworld.read::<T>(), name);
            }),
            load: Box::new(move |uiworld| {
                if let Ok(res) = <common::saveload::JSON as Encoder>::load::<T>(name) {
                    uiworld.insert(res);
                }
            }),
        });
    }
}

fn register_world_resource<T: 'static + Default>() {
    register_resource_noserialize::<T>();
    register_world_reset(|uiw, _| *uiw.write::<T>() = T::default());
}

fn register_world_reset(reset: WorldResetHook) {
    unsafe {
        INIT_FUNCS.push(InitFunc {
            f: Box::new(move |uiw| uiw.write::<WorldResetHooks>().register(reset)),
        });
    }
}
