use std::fs;
use std::io::{self, ErrorKind};

use yakui::widgets::Layer;
use yakui::{center, reflow, Alignment, Dim2, Pivot};

use common::saveload::{write_atomic, AtomicWriteError, Encoder, JSONPretty, JSON};
use engine::InputContext;
use goryak::{blur_bg, constrained_viewport, mincolumn, on_secondary, primary, textc, titlec};
use simulation::Simulation;

use crate::inputmap::{Bindings, InputAction, InputCombination, InputMap, UnitInput};
use crate::uiworld::UiWorld;

const BINDINGS_SAVE_NAME: &str = "bindings";

#[derive(Default)]
pub struct KeybindState {
    pub enabled: Option<KeybindStateInner>,
    pub(crate) save_failure: Option<BindingSaveFailure>,
}

pub(crate) enum BindingSaveFailure {
    All,
    Action(InputAction),
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

        let action = state.to_bind_to.clone();
        self.enabled = None;
        self.apply_and_save_bindings(bindings, input_map, Some(action));
    }

    pub(crate) fn apply_and_save_bindings(
        &mut self,
        bindings: &mut Bindings,
        input_map: &mut InputMap,
        action: Option<InputAction>,
    ) {
        input_map.build_input_tree(bindings);
        self.save_bindings(bindings, action);
    }

    pub(crate) fn reset_binding(
        &mut self,
        bindings: &mut Bindings,
        input_map: &mut InputMap,
        action: &InputAction,
    ) {
        let default = Bindings::default().0.remove(action).unwrap();
        bindings.0.insert(action.clone(), default);
        self.apply_and_save_bindings(bindings, input_map, Some(action.clone()));
    }

    pub(crate) fn save_bindings(&mut self, bindings: &Bindings, action: Option<InputAction>) {
        self.save_failure = save_bindings_file(bindings).err().map(|error| {
            log::error!("Could not save bindings.json ({error})");
            action.map_or(BindingSaveFailure::All, BindingSaveFailure::Action)
        });
    }
}

pub(crate) fn load_bindings() -> Bindings {
    match JSON::load(BINDINGS_SAVE_NAME) {
        Ok(bindings) => bindings,
        Err(error) => {
            if error.kind() != ErrorKind::NotFound {
                log::warn!("Using default bindings after loading bindings.json failed ({error})");
            }
            Bindings::default()
        }
    }
}

fn save_bindings_file(bindings: &Bindings) -> Result<(), AtomicWriteError> {
    let path = JSON::filename(BINDINGS_SAVE_NAME);
    // Back up the actual file being replaced, even if it changed after startup.
    match fs::read(&path) {
        Ok(bytes) if JSON::decode::<Bindings>(&bytes).is_err() => {
            let backup = path.with_extension("json.bak");
            write_atomic(&backup, &bytes).map_err(|error| {
                AtomicWriteError::PreCommit(io::Error::other(format!(
                    "could not back up bindings.json to bindings.json.bak ({error})"
                )))
            })?;
            log::warn!("Backed up unreadable bindings to {}", backup.display());
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(AtomicWriteError::PreCommit(error)),
    }
    JSONPretty::save_atomic(bindings, BINDINGS_SAVE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::saveload;
    use engine::Key;
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    struct TestSaveDir(PathBuf);

    impl Drop for TestSaveDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // The save directory is process-wide and can only be set once.
    fn with_save_dir(test_name: &str, test: impl FnOnce(&Path)) {
        const SAVE_DIR_ENV: &str = "EGR_KEYBIND_TEST_SAVE_DIR";
        const COMPLETION_MARKER: &str = "completed";
        if let Some(root) = std::env::var_os(SAVE_DIR_ENV).map(PathBuf::from) {
            saveload::set_save_dir(root.clone()).unwrap();
            test(&root);
            fs::write(root.join(COMPLETION_MARKER), b"").unwrap();
            return;
        }

        let root = std::env::temp_dir().join(format!(
            "egr-keybind-{}-{:016x}",
            std::process::id(),
            crate::session::random_seed()
        ));
        fs::create_dir(&root).unwrap();
        let root = TestSaveDir(root);
        // File-backed output lets the child keep writing while the parent polls.
        let log_path = root.0.join("child.log");
        let log = File::create(&log_path).unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{test_name}"), "--nocapture"])
            .env(SAVE_DIR_ENV, &root.0)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                child.wait().unwrap();
                panic!(
                    "child test {test_name} timed out\n{}",
                    fs::read_to_string(&log_path).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            status.success() && root.0.join(COMPLETION_MARKER).is_file(),
            "child test {test_name} did not complete ({status})\n{}",
            fs::read_to_string(&log_path).unwrap_or_default()
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

            let mut state = KeybindState::default();
            state.reset_binding(&mut bindings, &mut input_map, &action);
            assert!(state.save_failure.is_none());
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
                ..Default::default()
            };
            let mut bindings = Bindings::default();
            let mut input_map = InputMap::default();
            let mut input = InputContext::default();
            input.keyboard.pressed.insert(Key::F8);
            state.update(&mut bindings, &mut input_map, &input);
            assert!(state.enabled.is_none());
            assert!(state.save_failure.is_none());

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

    #[test]
    fn keybind_load_skips_unknown_actions() {
        with_save_dir("keybind_load_skips_unknown_actions", |root| {
            let path = root.join("bindings.json");
            fs::write(
                &path,
                br#"{"FutureAction":{"new_format":true},"OpenDebugMenu":[[{"Key":"F8"}]]}"#,
            )
            .unwrap();

            let mut expected = InputCombination::default();
            expected.push_unique(UnitInput::Key(Key::F8));
            let bindings = load_bindings();
            assert_eq!(bindings.0.len(), 1);
            assert_eq!(bindings.0[&InputAction::OpenDebugMenu].0, [expected]);

            save_bindings_file(&bindings).unwrap();
            assert!(!root.join("bindings.json.bak").exists());
        });
        assert!(JSON::decode::<Bindings>(br#"{"OpenDebugMenu":{"new_format":true}}"#).is_err());
    }

    #[test]
    fn keybind_invalid_file_survives_first_generic_save_as_backup() {
        with_save_dir(
            "keybind_invalid_file_survives_first_generic_save_as_backup",
            |root| {
                scenarios::ensure_repo_root_cwd();
                crate::init::init();
                let path = root.join("bindings.json");
                let backup = root.join("bindings.json.bak");
                let invalid = b"{\"OpenDebugMenu\":";
                fs::write(&path, invalid).unwrap();

                let uiw = UiWorld::init();
                let action = InputAction::OpenDebugMenu;
                assert_eq!(
                    uiw.read::<Bindings>().0[&action].0,
                    Bindings::default().0[&action].0
                );
                assert_eq!(fs::read(&path).unwrap(), invalid);

                uiw.save_to_disk();
                assert_eq!(
                    fs::read(&backup).expect("corrupt bindings were not backed up"),
                    invalid
                );
                assert!(JSON::load::<Bindings>("bindings").is_ok());

                uiw.save_to_disk();
                assert_eq!(fs::read(&backup).unwrap(), invalid);
            },
        );
    }

    #[test]
    fn keybind_rebind_cannot_overwrite_invalid_file_without_backup() {
        with_save_dir(
            "keybind_rebind_cannot_overwrite_invalid_file_without_backup",
            |root| {
                let path = root.join("bindings.json");
                let invalid = b"{\"OpenDebugMenu\":";
                fs::write(&path, invalid).unwrap();
                fs::create_dir(root.join("bindings.json.bak")).unwrap();

                let action = InputAction::OpenDebugMenu;
                let mut state = KeybindState {
                    enabled: Some(KeybindStateInner {
                        to_bind_to: action.clone(),
                        bind_index: 0,
                        cur: InputCombination::default(),
                    }),
                    ..Default::default()
                };
                let mut bindings = Bindings::default();
                let mut input_map = InputMap::default();
                let mut input = InputContext::default();
                input.keyboard.pressed.insert(Key::F8);
                state.update(&mut bindings, &mut input_map, &input);

                assert_eq!(fs::read(&path).unwrap(), invalid);
                assert!(matches!(
                    &state.save_failure,
                    Some(BindingSaveFailure::Action(failed_action)) if failed_action == &action
                ));
                input_map.prepare_frame(&input, true, false);
                assert!(input_map.act.contains(&action));

                let backup = root.join("bindings.json.bak");
                fs::remove_dir(&backup).unwrap();
                state.save_bindings(&bindings, None);
                assert!(state.save_failure.is_none());
                assert_eq!(fs::read(&backup).unwrap(), invalid);
                assert_eq!(
                    JSON::load::<Bindings>("bindings").unwrap().0[&action].0,
                    bindings.0[&action].0
                );
            },
        );
    }
}
