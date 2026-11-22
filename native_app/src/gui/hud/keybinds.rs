use yakui::widgets::Layer;
use yakui::{center, reflow, Alignment, Dim2, Pivot};

use common::saveload::{Encoder, JSONPretty};
use engine::InputContext;
use goryak::{blur_bg, constrained_viewport, mincolumn, on_secondary, primary, textc, titlec};
use simulation::Simulation;

use crate::inputmap::{Bindings, InputAction, InputCombination, InputMap, UnitInput};
use crate::uiworld::UiWorld;

#[derive(Default)]
pub struct KeybindState {
    pub enabled: Option<KeybindStateInner>,
}

pub struct KeybindStateInner {
    pub to_bind_to: InputAction,
    pub bind_index: usize,
    pub cur: InputCombination,
}

pub fn keybind_modal(uiw: &UiWorld, _: &Simulation) {
    profiling::scope!("hud::keybind_modal");

    let mut state = uiw.write::<KeybindState>();
    let Some(state) = &mut state.enabled else {
        return;
    };

    Layer::new().show(|| {
        reflow(
            Alignment::TOP_LEFT,
            Pivot::TOP_LEFT,
            Dim2::pixels(0.0, 0.0),
            || {
                blur_bg(primary().with_alpha(0.5), 0.0, || {
                    constrained_viewport(|| {
                        center(|| {
                            mincolumn(10.0, || {
                                titlec(on_secondary(), format!("{}", state.to_bind_to));
                                textc(on_secondary(), "Press key/mouse to bind to action");
                            });
                        });
                    });
                })
            },
        );
    });
}

impl KeybindState {
    pub fn update(
        &mut self,
        bindings: &mut Bindings,
        input_map: &mut InputMap,
        inp: &InputContext,
    ) {
        let Some(state) = &mut self.enabled else {
            return;
        };

        for key in &inp.keyboard.pressed {
            state.cur.push_unique(UnitInput::Key(key.clone()));
        }
        for &mouse in &inp.mouse.pressed {
            state.cur.push_unique(UnitInput::Mouse(mouse));
        }
        if inp.mouse.wheel_delta > 0.0 {
            state.cur.push_unique(UnitInput::WheelUp);
        }
        if inp.mouse.wheel_delta < 0.0 {
            state.cur.push_unique(UnitInput::WheelDown);
        }

        if state.cur.is_modifiers_only() {
            return;
        }

        if !state.cur.is_valid() {
            state.cur.clear();
            return;
        }

        let comb = &mut bindings.0.get_mut(&state.to_bind_to).unwrap().0;

        let mut cur = std::mem::take(&mut state.cur);
        cur.sort();
        if state.bind_index < comb.len() {
            comb[state.bind_index] = cur;
        } else {
            comb.push(cur);
        }
        comb.dedup_by(|a, b| a == b);

        apply_and_save_bindings(bindings, input_map);

        self.enabled = None;
    }
}

pub(crate) fn apply_and_save_bindings(bindings: &mut Bindings, input_map: &mut InputMap) {
    input_map.build_input_tree(bindings);
    JSONPretty::save_silent(bindings, "bindings");
}

pub(crate) fn reset_binding(
    bindings: &mut Bindings,
    input_map: &mut InputMap,
    action: &InputAction,
) {
    let default = Bindings::default().0.remove(action).unwrap();
    bindings.0.insert(action.clone(), default);
    apply_and_save_bindings(bindings, input_map);
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::saveload::{self, JSON};
    use engine::Key;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    // The save directory is process-wide and can only be set once.
    fn with_save_dir(test_name: &str, test: impl FnOnce(&Path)) {
        const SAVE_DIR_ENV: &str = "EGR_KEYBIND_TEST_SAVE_DIR";
        if let Some(root) = std::env::var_os(SAVE_DIR_ENV).map(PathBuf::from) {
            saveload::set_save_dir(root.clone()).unwrap();
            test(&root);
            return;
        }

        let root = std::env::temp_dir().join(format!(
            "egr-keybind-{}-{:016x}",
            std::process::id(),
            crate::session::random_seed()
        ));
        std::fs::create_dir(&root).unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{test_name}"), "--nocapture"])
            .env(SAVE_DIR_ENV, &root)
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn keybind_reset_one_action_rebuilds_live_input_tree() {
        with_save_dir("keybind_reset_one_action_rebuilds_live_input_tree", |_| {
            let action = InputAction::OpenDebugMenu;
            let mut bindings = Bindings::default();
            let mut custom = InputCombination::default();
            custom.push_unique(UnitInput::Key(Key::F8));
            bindings.0.get_mut(&action).unwrap().0[0] = custom;
            let mut input_map = InputMap::default();
            input_map.build_input_tree(&mut bindings);

            let mut input = InputContext::default();
            input.keyboard.pressed.insert(Key::F8);
            input_map.prepare_frame(&input, true, false);
            assert!(input_map.act.contains(&action));

            reset_binding(&mut bindings, &mut input_map, &action);
            input_map.prepare_frame(&input, true, false);
            assert!(
                !input_map.act.contains(&action),
                "custom binding is still active"
            );

            input.keyboard.pressed.clear();
            input.keyboard.pressed.insert(Key::F3);
            input_map.prepare_frame(&input, true, false);
            assert!(
                input_map.act.contains(&action),
                "default binding is inactive"
            );

            let saved = JSON::load::<Bindings>("bindings").unwrap();
            assert_eq!(saved.0[&action].0, Bindings::default().0[&action].0);
        });
    }

    #[test]
    fn keybind_rebind_saves_without_city_save() {
        with_save_dir("keybind_rebind_saves_without_city_save", |root| {
            let action = InputAction::GoForward;
            let mut state = KeybindState {
                enabled: Some(KeybindStateInner {
                    to_bind_to: action.clone(),
                    bind_index: 1,
                    cur: InputCombination::default(),
                }),
            };
            let mut bindings = Bindings::default();
            let mut input_map = InputMap::default();
            let mut input = InputContext::default();
            input.keyboard.pressed.insert(Key::F8);
            state.update(&mut bindings, &mut input_map, &input);
            assert!(state.enabled.is_none());

            assert!(
                root.join("bindings.json").is_file(),
                "rebind did not save bindings"
            );
            let mut saved = JSON::load::<Bindings>("bindings").unwrap();
            assert_eq!(saved.0[&action].0[0], Bindings::default().0[&action].0[0]);
            let mut reloaded_map = InputMap::default();
            reloaded_map.build_input_tree(&mut saved);
            reloaded_map.prepare_frame(&input, true, false);
            assert!(reloaded_map.act.contains(&action));
        });
    }
}
