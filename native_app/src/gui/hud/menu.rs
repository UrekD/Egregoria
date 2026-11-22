use std::sync::atomic::Ordering;
use std::time::Instant;

use yakui::widgets::{List, Pad};
use yakui::{column, opaque, reflow, spacer, Alignment, CrossAxisAlignment, Dim2, Pivot};

use goryak::{
    blur_bg, button_primary, button_secondary, constrained_viewport, on_primary_container,
    on_secondary_container, padxy, secondary_container, textc, tooltip, Window,
};
use simulation::economy::Government;
use simulation::Simulation;

use crate::gui::{ExitState, GuiState};
use crate::inputmap::{InputAction, InputMap};
use crate::session::{self, SessionScreen};
use crate::uiworld::{SaveLoadState, UiWorld};

pub fn menu_bar(uiworld: &UiWorld, sim: &Simulation) {
    profiling::scope!("hud::menu_bar");

    reflow(Alignment::TOP_LEFT, Pivot::TOP_LEFT, Dim2::ZERO, || {
        constrained_viewport(|| {
            column(|| {
                opaque(|| {
                    blur_bg(secondary_container().with_alpha(0.5), 0.0, || {
                        padxy(5.0, 5.0, || {
                            let mut l = List::row();
                            l.item_spacing = 10.0;
                            l.cross_axis_alignment = CrossAxisAlignment::Center;

                            l.show(|| {
                                let mut gui = uiworld.write::<GuiState>();
                                gui.windows.menu(uiworld);
                                save_window(&mut gui, uiworld);
                                textc(
                                    on_primary_container(),
                                    format!("Money: {}", sim.read::<Government>().money),
                                );
                                crate::gui::overlays::selector(uiworld);
                                gui.windows.additional_menu(session::can_save(uiworld));
                            });
                        });
                    });
                });
                spacer(1);
            });
        });
    });
}

pub(super) fn main_menu(uiw: &UiWorld, settings_open: &mut bool) {
    if tooltip("Enter the current city", || button_primary("Play").show()).clicked {
        session::set_screen(uiw, SessionScreen::Playing);
    }
    if button_primary("New Game").show().clicked {
        session::set_screen(uiw, SessionScreen::NewGame);
    }
    if button_primary("Load").show().clicked {
        session::set_screen(uiw, SessionScreen::LoadGame);
    }
    if button_primary("Settings").show().clicked {
        *settings_open ^= true;
    }
    if button_secondary("Quit").show().clicked {
        std::process::exit(0);
    }
}

fn save_window(gui: &mut GuiState, uiw: &UiWorld) {
    if !session::can_save(uiw) {
        if session::world_interaction(uiw) && button_secondary("Exit").show().clicked {
            std::process::exit(0);
        }
        return;
    }
    let mut slstate = uiw.write::<SaveLoadState>();
    if slstate.saving_status.load(Ordering::SeqCst) {
        textc(on_secondary_container(), "Saving...");
    } else if button_primary("Save").show().clicked {
        slstate.please_save = true;
        gui.last_save = Instant::now();
        uiw.save_to_disk();
    }

    let mut estate = uiw.write::<ExitState>();

    match *estate {
        ExitState::NoExit => {}
        ExitState::ExitAsk | ExitState::Saving => {
            let mut opened = true;
            Window {
                title: "Exit Menu".into(),
                pad: Pad::all(15.0),
                radius: 10.0,
                opened: &mut opened,
                child_spacing: 5.0,
            }
            .show(|| {
                if let ExitState::Saving = *estate {
                    textc(on_secondary_container(), "Saving...");
                    if !slstate.please_save && !slstate.saving_status.load(Ordering::SeqCst) {
                        std::process::exit(0);
                    }
                    return;
                }
                if button_secondary("Save and exit").show().clicked {
                    if let ExitState::ExitAsk = *estate {
                        slstate.please_save = true;
                        *estate = ExitState::Saving;
                    }
                }
                if button_secondary("Exit without saving").show().clicked {
                    std::process::exit(0);
                }
                if button_secondary("Cancel").show().clicked {
                    *estate = ExitState::NoExit;
                }
            });

            if !opened {
                *estate = ExitState::NoExit;
            }

            if uiw
                .read::<InputMap>()
                .just_act
                .contains(&InputAction::Close)
            {
                *estate = ExitState::NoExit;
            }
        }
    }

    match *estate {
        ExitState::NoExit => {
            if button_secondary("Exit").show().clicked {
                *estate = ExitState::ExitAsk;
            }
        }
        ExitState::ExitAsk => {
            if button_secondary("Save and exit").show().clicked {
                if let ExitState::ExitAsk = *estate {
                    slstate.please_save = true;
                    *estate = ExitState::Saving;
                }
            }
        }
        ExitState::Saving => {
            textc(on_secondary_container(), "Saving...");
        }
    }
}
