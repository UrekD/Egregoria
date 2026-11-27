use crate::gui::{ErrorTooltip, PotentialCommands, Tool};
use crate::inputmap::{DeckPicker, InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;
use geom::{LinearColor, Vec3};
use goryak::{
    checkbox_value, mincolumn, minrow, on_secondary_container, padxy, selectable_label_primary,
    textc,
};
use simulation::map::building_parking::{
    curb_edge, lot_footprint, lot_gate_position, lot_mouth_position, parking_entrance_near,
    LotFootprint, LotFrame, LotMouth, MouthUse, SurfaceLotDraft, BLOCK_SIZE, MAX_WIDTH_BLOCKS,
};
use simulation::map::{Map, PathEndpoint, ProjectFilter};
use simulation::world_command::WorldCommand;
use simulation::Simulation;

pub(crate) struct SurfaceLotTool {
    frame: Option<LotFrame>,
    draft: Option<SurfaceLotDraft>,
    use_ground_grid: bool,
    facing: u8,
    mouth_mode: MouthUse,
    picking_gate: bool,
    pending: Option<PendingLot>,
}

impl Default for SurfaceLotTool {
    fn default() -> Self {
        Self {
            frame: None,
            draft: None,
            use_ground_grid: false,
            facing: 0,
            mouth_mode: MouthUse::Both,
            picking_gate: false,
            pending: None,
        }
    }
}

struct PendingLot {
    index: usize,
    quote: u64,
    frames: u16,
}

impl SurfaceLotTool {
    pub(crate) fn cancel(&mut self) -> bool {
        let had_frame = self.frame.take().is_some();
        self.draft = None;
        self.pending = None;
        self.picking_gate = false;
        had_frame
    }
}

pub(crate) fn properties(uiw: &UiWorld) {
    let mut state = uiw.write::<SurfaceLotTool>();
    padxy(12.0, 8.0, || {
        mincolumn(4.0, || {
            let color = on_secondary_container();
            if state.pending.is_some() {
                textc(color, "Building parking lot…");
                return;
            }
            match state.draft {
                Some(draft) => {
                    minrow(8.0, || {
                        textc(color, "Vehicle entrance");
                        for mode in [MouthUse::In, MouthUse::Out, MouthUse::Both] {
                            if selectable_label_primary(
                                !state.picking_gate && state.mouth_mode == mode,
                                mode.label(),
                            )
                            .clicked
                            {
                                state.mouth_mode = mode;
                                state.picking_gate = false;
                            }
                        }
                        if selectable_label_primary(state.picking_gate, "Pedestrian gate").clicked {
                            state.picking_gate = !state.picking_gate;
                        }
                        if draft.foot_gate.is_some()
                            && selectable_label_primary(false, "Use entrance sidewalk").clicked
                        {
                            state.draft.as_mut().unwrap().foot_gate = None;
                            state.picking_gate = false;
                        }
                    });
                    let help = if state.picking_gate {
                        "Choose a sidewalk or footpath beside the lot"
                    } else {
                        "Choose up to two curb entrances, then click inside the lot to build"
                    };
                    textc(color, help);
                    textc(
                        color,
                        format!(
                            "{} blocks · {} vehicle entrances · {} daily upkeep",
                            draft.width_blocks * 2,
                            draft.mouths.iter().flatten().count(),
                            draft.daily_upkeep()
                        ),
                    );
                }
                None if state.frame.is_some() => textc(
                    color,
                    "Move along the frontage and click to set the lot width",
                ),
                None => {
                    minrow(8.0, || {
                        checkbox_value(&mut state.use_ground_grid, color, "Ground grid");
                        if state.use_ground_grid
                            && selectable_label_primary(false, "Rotate").clicked
                        {
                            state.facing = (state.facing + 1) % 4;
                        }
                        textc(
                            color,
                            if state.use_ground_grid {
                                "Choose the center of the front edge"
                            } else {
                                "Choose the center of a curb for the front edge"
                            },
                        );
                    });
                }
            }
        });
    });
}

fn has_pointer_over_ui(input: &InputMap) -> bool {
    goryak::pointer_over_ui(input.screen.x, input.screen.y)
        || engine::pointer_over_egui(input.screen)
}

fn frame_at(state: &SurfaceLotTool, map: &Map, pointer: Vec3) -> Option<LotFrame> {
    if state.use_ground_grid {
        let block = pointer.xy() / BLOCK_SIZE;
        return Some(LotFrame::Ground {
            front_block: [block.x.round() as i32, block.y.round() as i32],
            facing: state.facing,
        });
    }
    let anchor = parking_entrance_near(map, pointer)?;
    let (edge, _) = curb_edge(map, anchor)?;
    ((edge.z - pointer.z).abs() < 0.8 && edge.xy().distance2(pointer.xy()) < 6.0_f32.powi(2))
        .then_some(LotFrame::Curb(anchor))
}

fn width_at(pad: LotFootprint, pointer: Vec3) -> u8 {
    let width = 2.0 * (pointer - pad.front).xy().dot(pad.along).abs() / BLOCK_SIZE;
    (width.round() as u8).clamp(2, MAX_WIDTH_BLOCKS)
}

fn outline(draw: &mut ImmediateDraw, pad: LotFootprint, color: LinearColor) {
    for block in pad.blocks() {
        draw.obb(block, pad.front.z + 0.045).color(color.a(0.17));
        for i in 0..4 {
            draw.line(
                block.corners[i].z(pad.front.z + 0.08),
                block.corners[(i + 1) % 4].z(pad.front.z + 0.08),
                0.13,
            )
            .color(color.a(0.75));
        }
    }
    for stall in pad.stall_centers() {
        let a = stall + pad.along.z0() * 1.25;
        let offset = pad.outward.z0() * 2.5;
        draw.line((a - offset).up(0.1), (a + offset).up(0.1), 0.16)
            .color(LinearColor::WHITE);
    }
}

fn mouth_marker(
    draw: &mut ImmediateDraw,
    pad: LotFootprint,
    at: Vec3,
    mode: MouthUse,
    color: LinearColor,
) {
    let edge = at.up(0.18);
    let inward = pad.outward.z0();
    draw.stroke_circle(edge, 1.15, 0.36).color(color);
    for sign in [-1.0, 1.0] {
        if (sign > 0.0 && mode == MouthUse::Out) || (sign < 0.0 && mode == MouthUse::In) {
            continue;
        }
        let tip = edge + inward * (3.0 * sign);
        draw.line(edge, tip, 0.36).color(color);
        for side in [-1.0, 1.0] {
            draw.line(
                tip,
                tip - inward * sign + pad.along.z0() * side * 0.65,
                0.36,
            )
            .color(color);
        }
    }
}

pub(crate) fn update(sim: &Simulation, uiw: &UiWorld) {
    if *uiw.read::<Tool>() != Tool::ParkingLot {
        return;
    }
    let mut potential = uiw.write::<PotentialCommands>();
    potential.0.clear();
    let mut state = uiw.write::<SurfaceLotTool>();
    let input = uiw.read::<InputMap>();
    let pointer = input.unprojected.filter(|_| !has_pointer_over_ui(&input));
    if let Some(pending) = &mut state.pending {
        let matches = |command: &WorldCommand| matches!(command, WorldCommand::MapPlaceParkingLot { quote, .. } if *quote == pending.quote);
        let received = uiw.received_commands();
        if let Some(outcome) = received.placement(pending.index, matches) {
            state.pending = None;
            match outcome {
                Ok(()) => {
                    state.cancel();
                    return;
                }
                Err(Some(reason)) => {
                    *uiw.write::<ErrorTooltip>() = ErrorTooltip::new(
                        crate::gui::notifications::command_message("MapPlaceParkingLot", reason),
                    );
                }
                Err(None) => {}
            }
        } else {
            if received.iter().any(matches) {
                state.cancel();
                return;
            }
            pending.frames += 1;
            if pending.frames < 240 {
                return;
            }
            state.pending = None;
        }
    }
    let click =
        pointer.is_some() && input.just_act.contains(&InputAction::Select) && uiw.can_author();
    let map = sim.map();
    let mut draw = uiw.write::<ImmediateDraw>();
    let success: LinearColor = simulation::colors().gui_success.into();
    let danger: LinearColor = simulation::colors().gui_danger.into();
    let blue = LinearColor::new(0.28, 0.77, 1.0, 1.0);

    let Some(mut draft) = state.draft else {
        let Some(pointer) = pointer else {
            return;
        };
        let Some(frame) = state.frame.or_else(|| frame_at(&state, &map, pointer)) else {
            return;
        };
        let Some(minimum) = lot_footprint(&map, frame, 2) else {
            state.cancel();
            return;
        };
        let width_blocks = if state.frame.is_some() {
            width_at(minimum, pointer)
        } else {
            2
        };
        let pad = lot_footprint(&map, frame, width_blocks).unwrap_or(minimum);
        outline(&mut draw, pad, blue);
        if click {
            if state.frame.is_some() {
                state.draft = Some(SurfaceLotDraft {
                    frame,
                    width_blocks,
                    mouths: [None; 2],
                    foot_gate: None,
                });
            } else {
                state.frame = Some(frame);
            }
        }
        return;
    };
    let Some(pad) = lot_footprint(&map, draft.frame, draft.width_blocks) else {
        state.cancel();
        return;
    };
    let mut confirm = false;
    if let Some(pointer) = pointer {
        if state.picking_gate {
            let hit = input
                .ray
                .and_then(|ray| {
                    DeckPicker::new(&map).pick(
                        ray,
                        &input.surface_hits(map.environment()),
                        ProjectFilter::ROAD | ProjectFilter::INTER,
                    )
                })
                .unwrap_or_else(|| {
                    map.project(pointer, 3.0, ProjectFilter::ROAD | ProjectFilter::INTER)
                });
            if let Some((gate, at)) = map
                .pick_path_attachment(
                    hit.kind,
                    if hit.pos.z > pointer.z + 0.8 {
                        hit.pos
                    } else {
                        pointer
                    },
                )
                .and_then(|gate| lot_gate_position(&map, pad, gate).map(|at| (gate, at)))
            {
                draw.stroke_circle(at.up(0.16), 1.4, 0.3).color(blue);
                if click {
                    draft.foot_gate = Some(gate);
                    state.picking_gate = false;
                }
            }
        } else {
            let selected = draft.mouths.iter().position(|mouth| {
                mouth
                    .and_then(|mouth| lot_mouth_position(&map, pad, mouth.at))
                    .is_some_and(|at| at.xy().distance2(pointer.xy()) < 2.5_f32.powi(2))
            });
            let hovered = parking_entrance_near(&map, pointer)
                .and_then(|anchor| lot_mouth_position(&map, pad, anchor).map(|at| (anchor, at)))
                .filter(|(_, at)| at.xy().distance2(pointer.xy()) < 4.0_f32.powi(2));
            if let Some((anchor, at)) = hovered {
                mouth_marker(&mut draw, pad, at, state.mouth_mode, blue);
                if click {
                    if let Some(i) = selected {
                        let old = draft.mouths[i].unwrap();
                        if old.mode == state.mouth_mode {
                            draft.mouths[i] = None;
                        } else {
                            draft.mouths[i] = Some(LotMouth {
                                mode: state.mouth_mode,
                                ..old
                            });
                        }
                    } else if let Some(slot) = draft.mouths.iter_mut().find(|mouth| mouth.is_none())
                    {
                        *slot = Some(LotMouth {
                            at: anchor,
                            mode: state.mouth_mode,
                        });
                    } else {
                        *uiw.write::<ErrorTooltip>() =
                            ErrorTooltip::new("Click an entrance again to remove it");
                    }
                }
            } else {
                confirm = click && pad.shape.contains(pointer.xy());
            }
        }
    }
    state.draft = Some(draft);
    for (index, offset) in (0..draft.width_blocks * 2)
        .map(|i| (i, (f32::from(i) + 0.5) * 4.0))
        .filter(|(i, _)| *i > 0)
    {
        if index + 1 == draft.width_blocks * 2 {
            continue;
        }
        let point = pad.front
            + (pad.along * (offset - f32::from(draft.width_blocks) * BLOCK_SIZE * 0.5)).z0();
        if let Some(at) = parking_entrance_near(&map, point - pad.outward.z0())
            .and_then(|anchor| lot_mouth_position(&map, pad, anchor))
        {
            draw.stroke_circle(at.up(0.16), 0.45, 0.16)
                .color(LinearColor::WHITE.a(0.65));
        }
    }
    for mouth in draft.mouths.iter().flatten() {
        if let Some(at) = lot_mouth_position(&map, pad, mouth.at) {
            mouth_marker(&mut draw, pad, at, mouth.mode, success);
        }
    }
    let gate = draft.foot_gate.or_else(|| {
        draft
            .mouths
            .iter()
            .flatten()
            .next()
            .map(|mouth| PathEndpoint::Sidewalk(mouth.at))
    });
    if let Some(at) = gate.and_then(|gate| lot_gate_position(&map, pad, gate)) {
        draw.stroke_circle(at.up(0.23), 1.3, 0.27).color(blue);
    }
    drop(map);

    let preview = sim.quote_surface_lot(draft);
    let color = if preview.is_ok() {
        success
    } else if draft.mouths.iter().all(Option::is_none) {
        blue
    } else {
        danger
    };
    outline(&mut draw, pad, color);
    match preview {
        Ok((quote, _)) => {
            let command = WorldCommand::MapPlaceParkingLot { draft, quote };
            if confirm {
                let mut commands = uiw.commands();
                let index = commands.as_ref().len();
                commands.push(command);
                state.pending = Some(PendingLot {
                    index,
                    quote,
                    frames: 0,
                });
            } else {
                potential.set(command);
            }
        }
        Err(reason) if confirm => {
            *uiw.write::<ErrorTooltip>() = ErrorTooltip::new(
                crate::gui::notifications::command_message("MapPlaceParkingLot", reason),
            );
        }
        _ => {}
    }
}
