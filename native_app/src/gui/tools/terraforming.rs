use std::collections::VecDeque;

use geom::{Vec2, Vec3, OBB};
use simulation::map::{
    TerraformKind, WaterBasin, WaterBodyId, WaterEdit, WaterEditError, WaterEditOutcome, WaterKind,
    WaterProviderImpact, WaterProviderImpactSample, WaterStroke, MAX_WATER_VERTICES,
};
use simulation::world_command::{CommandOutcomes, Rejection, WorldCommand};
use simulation::Simulation;

use crate::gui::{overlays::land_tiles::show_edit_error, Tool};
use crate::inputmap::{InputAction, InputMap, UnitInput};
use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::{water_color, WATER_TILE_PITCH};
use crate::uiworld::UiWorld;

pub struct TerraformingResource {
    pub kind: TerraformKind,
    pub radius: f32,
    pub amount: f32,
    level: Option<f32>,
    slope_start: Option<Vec3>,
    slope_end: Option<Vec3>,
    ground_radius: Option<f32>,
    terrain_preview: Option<CachedTerrainPreview>,
    pub(crate) water: WaterTools,
}

#[derive(Clone, Copy, PartialEq)]
struct TerrainBrush {
    kind: TerraformKind,
    center: Vec2,
    radius: f32,
    amount: f32,
    level: f32,
    slope: Option<(Vec3, Vec3)>,
}

impl TerrainBrush {
    fn command(self) -> WorldCommand {
        WorldCommand::Terraform {
            kind: self.kind,
            center: self.center,
            radius: self.radius,
            amount: self.amount,
            level: self.level,
            slope: self.slope,
        }
    }
}

struct CachedTerrainPreview {
    brush: TerrainBrush,
    revision: (u64, u64, u64, u64, usize),
    tick: u64,
    sequence: u64,
    result: Result<(), Rejection>,
}

impl TerraformingResource {
    pub(crate) fn cancel_gesture(&mut self) {
        self.level = None;
        self.terrain_preview = None;
        self.slope_start = None;
        self.slope_end = None;
        self.water.cancel();
    }

    fn preview_brush(&mut self, sim: &Simulation, brush: TerrainBrush) -> Result<(), Rejection> {
        let map = sim.map();
        if map.land_ownership().extent().is_none() && !map.environment().has_water_configuration() {
            return Ok(());
        }
        let revision = map.road_preview_revision();
        drop(map);
        let tick = sim.get_tick();
        let sequence = sim.read::<CommandOutcomes>().batch_seq();
        if let Some(preview) = &self.terrain_preview {
            if preview.brush == brush
                && preview.revision == revision
                && preview.tick == tick
                && preview.sequence == sequence
            {
                return preview.result;
            }
        }
        let result = brush.command().preview(sim).map(|_| ());
        self.terrain_preview = Some(CachedTerrainPreview {
            brush,
            revision,
            tick,
            sequence,
            result,
        });
        result
    }

    pub(crate) fn water_active(&self) -> bool {
        self.ground_radius.is_some()
    }

    pub(crate) fn set_water_active(&mut self, active: bool) {
        if self.water_active() == active {
            return;
        }
        self.cancel_gesture();
        if active {
            self.ground_radius = Some(std::mem::take(&mut self.radius));
        } else if let Some(radius) = self.ground_radius.take() {
            self.radius = radius;
        }
    }
}

/// Terraform the ground or author static bodies of water.
pub fn terraforming(sim: &Simulation, uiworld: &UiWorld) {
    profiling::scope!("gui::terraforming");
    let mut res = uiworld.write::<TerraformingResource>();
    let tool = *uiworld.read::<Tool>();
    let inp = uiworld.read::<InputMap>();
    let mut draw = uiworld.write::<ImmediateDraw>();

    if !matches!(tool, Tool::Terraforming) {
        res.cancel_gesture();
        return;
    }

    if res.water_active() {
        res.water.update(sim, uiworld, &inp, &mut draw);
        return;
    }

    if inp.act.contains(&InputAction::SizeUp) {
        res.radius *= 1.1;
    }
    if inp.act.contains(&InputAction::SizeDown) {
        res.radius /= 1.1;
    }

    if !inp.act.contains(&InputAction::Select) {
        res.level = None;
    }
    let ground_reference = matches!(res.kind, TerraformKind::Level | TerraformKind::Slope);
    let mpos = if ground_reference {
        inp.surface_hits(sim.map().environment())
            .ground
            .map(|(point, _)| point)
            .or(inp.unprojected)
    } else {
        inp.unprojected
    };
    let mpos = unwrap_ret!(mpos);

    let mut amount_multiplier = 1.0;

    // handle actions
    match res.kind {
        TerraformKind::Elevation => {
            if inp.act.contains(&InputAction::SecondarySelect) {
                amount_multiplier = -1.0;
            }
        }
        TerraformKind::Smooth => {}
        TerraformKind::Level => {
            // set level on first click
            if res.level.is_none() && inp.just_act.contains(&InputAction::Select) {
                res.level = Some(mpos.z);
            }
        }
        TerraformKind::Slope => {
            // Set the end slope (second click)
            if res.slope_start.is_some()
                && res.slope_end.is_none()
                && inp.just_act.contains(&InputAction::Select)
                && !res.slope_start.unwrap().is_close(mpos, 5.0)
            {
                res.slope_end = Some(mpos);
            }

            // Set the start slope (first click)
            if res.slope_start.is_none() && inp.just_act.contains(&InputAction::Select) {
                res.slope_start = Some(mpos);
            }
        }
        TerraformKind::Erode => {}
    }

    let slope = res.slope_start.zip(res.slope_end);
    let preview_slope = slope.or_else(|| {
        res.slope_start
            .filter(|start| !start.is_close(mpos, 5.0))
            .map(|start| (start, mpos))
    });
    let brush = TerrainBrush {
        kind: res.kind,
        center: mpos.xy(),
        radius: res.radius,
        amount: res.amount * amount_multiplier,
        level: res.level.unwrap_or(mpos.z),
        slope: preview_slope,
    };
    let result = if res.kind != TerraformKind::Slope || preview_slope.is_some() {
        res.preview_brush(sim, brush)
    } else {
        Ok(())
    };
    let colors = simulation::colors();
    let color = match result {
        Ok(()) => colors.gui_primary,
        Err(reason) => {
            show_edit_error(uiworld, "Terraform", reason);
            draw.stroke_circle(mpos.up(0.8), res.radius, 1.5)
                .color(colors.gui_danger);
            colors.gui_danger
        }
    };
    let ready = match res.kind {
        TerraformKind::Slope => slope.is_some(),
        TerraformKind::Level => res.level.is_some(),
        _ => true,
    };
    if ready
        && result.is_ok()
        && (inp.act.contains(&InputAction::Select)
            || inp.act.contains(&InputAction::SecondarySelect))
    {
        uiworld.commands().push(brush.command());
    }

    // Draw the state
    match res.kind {
        TerraformKind::Elevation => {}
        TerraformKind::Smooth => {}
        TerraformKind::Level => {
            if !inp.act.contains(&InputAction::Select) {
                draw.obb(
                    OBB::new(
                        mpos.xy(),
                        Vec2::X,
                        res.radius * std::f32::consts::FRAC_1_SQRT_2,
                        res.radius * std::f32::consts::FRAC_1_SQRT_2,
                    ),
                    res.level.unwrap_or(mpos.z) - 0.5,
                )
                .color(color.a(0.2));
            }
        }
        TerraformKind::Slope => {
            if res.slope_start.is_none() {
                draw.circle(mpos, res.radius * 0.7)
            } else if res.slope_end.is_none() {
                draw.line(res.slope_start.unwrap(), mpos, res.radius)
            } else {
                draw.line(res.slope_start.unwrap(), res.slope_end.unwrap(), res.radius)
            }
            .color(color.a(0.2));
        }
        TerraformKind::Erode => {}
    }
}

impl Default for TerraformingResource {
    fn default() -> Self {
        Self {
            kind: TerraformKind::Elevation,
            radius: 200.0,
            amount: 300.0,
            level: None,
            slope_start: None,
            slope_end: None,
            ground_radius: None,
            terrain_preview: None,
            water: WaterTools::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum WaterMode {
    #[default]
    River,
    Lake,
    Remove,
    Sea,
}

pub(crate) struct WaterTools {
    pub mode: WaterMode,
    pub width_m: f32,
    pub depth_m: f32,
    pub lake_level_dm: i16,
    pub sea_level_dm: i16,
    pub current_sea_dm: Option<i16>,
    pub sea_action: Option<Option<i16>>,
    pub status: Option<String>,
    pub(crate) confirmation: Option<WaterProviderImpact>,
    pub(crate) confirm_action: bool,
    stroke: Option<WaterGesture>,
    preview: Option<CachedWaterPreview>,
    pending: VecDeque<WaterEdit>,
    submitted_summary: Option<String>,
}

impl Default for WaterTools {
    fn default() -> Self {
        Self {
            mode: WaterMode::River,
            width_m: 64.0,
            depth_m: 5.0,
            lake_level_dm: 0,
            sea_level_dm: 0,
            current_sea_dm: None,
            sea_action: None,
            status: None,
            confirmation: None,
            confirm_action: false,
            stroke: None,
            preview: None,
            pending: VecDeque::new(),
            submitted_summary: None,
        }
    }
}

impl WaterTools {
    pub(crate) fn set_mode(&mut self, mode: WaterMode) {
        if self.mode != mode {
            self.cancel();
            self.mode = mode;
        }
    }

    fn cancel(&mut self) {
        self.stroke = None;
        self.sea_action = None;
        self.dismiss_confirmation();
        self.submitted_summary = None;
    }

    pub(crate) fn dismiss_confirmation(&mut self) {
        self.confirmation = None;
        self.confirm_action = false;
        self.preview = None;
        self.status = None;
    }

    fn update(
        &mut self,
        sim: &Simulation,
        uiworld: &UiWorld,
        input: &InputMap,
        draw: &mut ImmediateDraw,
    ) {
        let map = sim.map();
        let env = map.environment();
        let sea = env.sea_level_dm();
        if self.current_sea_dm != sea {
            self.current_sea_dm = sea;
            self.sea_level_dm = sea.unwrap_or_default();
        }
        let revision = env.water_revision();

        if self.confirmation.is_some() {
            drop(map);
            if std::mem::take(&mut self.confirm_action) {
                let edit = self.preview.as_ref().unwrap().edit.clone();
                self.submit(sim, uiworld, revision, Ok(edit));
            }
            return;
        }

        // Sea controls stage edits even when they capture the pointer.
        if self.mode == WaterMode::Sea {
            drop(map);
            if let Some(level_dm) = self.sea_action.take() {
                self.submit(
                    sim,
                    uiworld,
                    revision,
                    Ok(WaterEdit::SetSeaLevel { level_dm }),
                );
            }
            return;
        }

        let pointer = engine::pointer_frame();
        if pointer.blocked
            || pointer.interrupted
            || goryak::pointer_over_ui(input.screen.x, input.screen.y)
        {
            self.stroke = None;
            return;
        }
        let hits = input.surface_hits(env);
        let pressed = input.just_act.contains(&InputAction::Select);
        if self.mode == WaterMode::Remove {
            if let Some(hit) = hits.water.filter(|hit| hit.sample.body != WaterBodyId::SEA) {
                if input.visible_deck(&map).is_some() {
                    return;
                }
                drop(map);
                draw.stroke_circle(hit.point.up(0.2), 12.0, 2.0)
                    .color(simulation::colors().gui_danger);
                if pressed {
                    self.submit(
                        sim,
                        uiworld,
                        revision,
                        Ok(WaterEdit::RemoveFreshWater {
                            body: hit.sample.body,
                        }),
                    );
                }
            }
            return;
        }

        drop(map);
        let Some(point) = hits.nearest() else {
            self.stroke = None;
            return;
        };
        if self.stroke.is_none() {
            if let Some(primary) = input.select_press() {
                self.status = None;
                self.submitted_summary = None;
                self.stroke = Some(WaterGesture::new(point, Some(primary)));
            }
        }

        let color = water_color(match self.mode {
            WaterMode::River => WaterKind::River,
            _ => WaterKind::Lake,
        });
        let Some(stroke) = &mut self.stroke else {
            draw.stroke_circle(point.up(0.2), self.cursor_radius(), 2.0)
                .color(color);
            return;
        };

        let Some(primary) = stroke.primary.as_ref() else {
            self.stroke = None;
            return;
        };
        if !input.select_input_allowed(primary) {
            self.stroke = None;
            return;
        }
        let release = match primary {
            UnitInput::Mouse(button) => pointer.released_buttons.contains(button),
            _ => !input.select_input_held(primary),
        };
        if !release && !input.select_input_held(primary) {
            self.stroke = None;
            return;
        }
        stroke.sample(point, self.mode == WaterMode::Lake, release);
        let thickness = if self.mode == WaterMode::River {
            self.width_m
        } else {
            3.0
        };
        draw.polyline(
            stroke
                .points
                .iter()
                .map(|point| point.up(0.2))
                .collect::<Vec<_>>(),
            thickness,
            self.mode == WaterMode::Lake && stroke.points.len() >= 3,
        )
        .color(color.a(0.72));

        if release {
            let stroke = self.stroke.take().unwrap();
            self.submit(sim, uiworld, revision, stroke.into_edit(self));
        }
    }

    fn cursor_radius(&self) -> f32 {
        match self.mode {
            WaterMode::River => self.width_m * 0.5,
            _ => WATER_TILE_PITCH * 0.5,
        }
    }

    fn submit(
        &mut self,
        sim: &Simulation,
        uiworld: &UiWorld,
        water_revision: u64,
        edit: Result<WaterEdit, WaterEditError>,
    ) {
        self.submitted_summary = None;
        let shown_impact = self.confirmation.take();
        let edit = match edit {
            Ok(edit) => edit,
            Err(error) => {
                self.status = Some(error.to_string());
                return;
            }
        };
        let sequence = sim.read::<CommandOutcomes>().batch_seq();
        let snapshot = (sim.get_tick(), sequence, water_revision);
        if self
            .preview
            .as_ref()
            .is_none_or(|p| p.snapshot != snapshot || p.edit != edit)
        {
            let result = sim.preview_water_edit(&edit).map(|preview| {
                let mut text = match preview.proposed_surface_dm {
                    Some(level) => format!("Surface {:.1} m", f32::from(level) / 10.0),
                    None => "Water edit".to_owned(),
                };
                if !preview.strict_before && preview.strict_after {
                    text.push_str(" · Enables strict water siting");
                }
                (text, preview.provider_impact)
            });
            self.preview = Some(CachedWaterPreview {
                snapshot,
                edit: edit.clone(),
                result,
            });
        }
        let (summary, impact) = match &self.preview.as_ref().unwrap().result {
            Ok((summary, impact)) => (summary.clone(), impact.clone()),
            Err(error) => {
                self.status = Some(error.to_string());
                return;
            }
        };
        // Revalidate using the current snapshot on confirmation. Let the player
        // review again if the edit would now produce a different provider impact.
        if impact.count != 0 && shown_impact.as_ref() != Some(&impact) {
            self.status = Some(summary);
            self.confirmation = Some(impact);
            return;
        }

        self.status = Some(format!("{summary} · Submitted"));
        self.submitted_summary = Some(summary);
        self.pending.push_back(edit.clone());
        uiworld.commands().push(WorldCommand::EditWater(edit));
    }

    pub(crate) fn receive_outcome(
        &mut self,
        edit: &WaterEdit,
        result: &Result<WaterEditOutcome, WaterEditError>,
    ) {
        // Peer filtering and per-batch indexing happen at the dispatcher. The
        // remaining local water edits arrive in the same order as the UI queue.
        if self.pending.front() != Some(edit) {
            return;
        }
        self.pending.pop_front();
        if self.pending.is_empty() {
            if let Some(summary) = self.submitted_summary.take() {
                self.status = Some(match result {
                    Ok(outcome) if outcome.changed => format!("{summary} · Applied"),
                    Ok(_) => format!("{summary} · No change"),
                    Err(error) => error.to_string(),
                });
            }
        }
    }

    #[cfg(feature = "multiplayer")]
    pub(crate) fn discard_pending(&mut self) {
        self.pending.clear();
        if self.submitted_summary.take().is_some() {
            self.status = None;
        }
    }
}

struct WaterGesture {
    primary: Option<UnitInput>,
    points: Vec<Vec3>,
    overflow: bool,
}

impl WaterGesture {
    fn new(point: Vec3, primary: Option<UnitInput>) -> Self {
        Self {
            primary,
            points: vec![point],
            overflow: false,
        }
    }

    fn sample(&mut self, point: Vec3, closed: bool, released: bool) {
        let xy = point.xy();
        let last = self.points.last().unwrap().xy();
        let gap = if released {
            0.5
        } else {
            WATER_TILE_PITCH * 0.5
        };
        if xy.distance2(last) < gap * gap
            || closed && released && xy.distance2(self.points[0].xy()) < 1.0
        {
            return;
        }
        if self.points.len() > 1 {
            let previous = self.points[self.points.len() - 2].xy();
            let side = xy - previous;
            let along = last - previous;
            if along.dot(side) > 0.0
                && along.mag2() < side.mag2()
                && along.cross(side).abs() <= side.mag()
            {
                *self.points.last_mut().unwrap() = point;
                return;
            }
        }
        if self.points.len() == MAX_WATER_VERTICES {
            self.overflow = true;
        } else {
            self.points.push(point);
        }
    }

    fn into_edit(self, state: &WaterTools) -> Result<WaterEdit, WaterEditError> {
        if self.overflow {
            return Err(WaterEditError::Limit);
        }
        let mut points: Vec<_> = self.points.into_iter().map(Vec3::xy).collect();
        match state.mode {
            WaterMode::River => Ok(WaterEdit::CarveRiver {
                polyline: WaterStroke::new(&points)?,
                width_m: state.width_m,
                depth_m: state.depth_m,
                level_dm: None,
            }),
            WaterMode::Lake => {
                if points.len() > 1 && points.first() == points.last() {
                    points.pop();
                }
                Ok(WaterEdit::FillLake {
                    basin: WaterBasin::new(&points)?,
                    level_dm: state.lake_level_dm,
                    depth_m: state.depth_m,
                })
            }
            _ => Err(WaterEditError::InvalidInput),
        }
    }
}

struct CachedWaterPreview {
    snapshot: (u64, u64, u64),
    edit: WaterEdit,
    result: Result<(String, WaterProviderImpact), WaterEditError>,
}

pub(crate) fn provider_preview_line(sample: &WaterProviderImpactSample) -> String {
    match sample.proposed_site {
        Ok(_) => format!("{:?} will be eligible for service", sample.building),
        Err(reason) => format!("{:?} · {reason}", sample.building),
    }
}

#[cfg(test)]
mod water_w4_tests {
    use super::*;
    use common::saveload::{Encoder, JSON};
    use geom::vec3;

    #[test]
    fn water_w4_provider_impact_is_shown_before_submission() {
        use geom::{Polygon, Vec2, OBB};
        use prototypes::{try_prototype, GoodsCompanyID};
        use simulation::map::{BuildingKind, LanePatternBuilder, Map, MapProject};

        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        crate::init::init();
        let sim = Simulation::new_authored_flat(2, 0).unwrap();
        let pump = {
            let mut map = sim.write::<Map>();
            let (_, road) = map
                .make_connection(
                    MapProject::ground(vec3(32.0, 176.0, 0.0)),
                    MapProject::ground(vec3(432.0, 176.0, 0.0)),
                    None,
                    &LanePatternBuilder::default().build(),
                )
                .unwrap();
            let id = GoodsCompanyID::new("water-pump");
            map.build_special_building(
                &OBB::new(Vec2::new(128.0, 212.0), Vec2::X, 20.0, 20.0),
                BuildingKind::GoodsCompany(id),
                try_prototype(id).unwrap().bgen,
                None,
                Some(road),
            )
            .unwrap()
        };
        let ui = UiWorld::with_defaults();
        let mut tools = WaterTools::default();
        let edit = WaterEdit::FillLake {
            basin: WaterBasin::new(
                Polygon::centered_rect(Vec2::new(704.0, 320.0), 32.0, 32.0).as_slice(),
            )
            .unwrap(),
            level_dm: -10,
            depth_m: 2.0,
        };
        assert_eq!(
            sim.preview_water_edit(&edit).unwrap().provider_impact.count,
            1
        );
        tools.submit(&sim, &ui, 0, Ok(edit.clone()));
        assert!(
            ui.commands().is_empty(),
            "submitted before warning about {pump:?}"
        );
        let impact = tools.confirmation.as_ref().unwrap();
        assert_eq!(impact.count, 1);
        let [sample] = impact.sample.as_slice() else {
            panic!("expected one provider warning");
        };
        assert_eq!(sample.building, pump);
        assert!(provider_preview_line(sample).contains("Pump needs a river or lake shore"));

        tools.confirm_action = true;
        tools.update(
            &sim,
            &ui,
            &InputMap::default(),
            &mut ImmediateDraw::default(),
        );
        assert!(tools.confirmation.is_none());
        let commands = ui.commands();
        assert!(matches!(commands.as_ref(), [WorldCommand::EditWater(queued)] if queued == &edit));
    }

    #[test]
    fn water_w4_control_does_not_release_a_stroke() {
        stroke_waits_for_physical_release(false);
    }

    #[test]
    fn water_w4_left_release_does_not_commit_a_rebound_stroke() {
        stroke_waits_for_physical_release(true);
    }

    fn stroke_waits_for_physical_release(remapped: bool) {
        use crate::inputmap::Bindings;
        use engine::{InputContext, Key, MouseButton, PointerFrame};
        use geom::Ray3;

        scenarios::ensure_repo_root_cwd();
        simulation::init::init();
        let sim = Simulation::new_authored_flat(4, 0).unwrap();
        crate::init::init();
        let ui = UiWorld::with_defaults();
        let mut input = InputMap::default();
        let mut bindings = Bindings::default();
        if remapped {
            bindings.0.insert(
                InputAction::Select,
                JSON::decode(br#"[[{"Mouse":"Middle"}]]"#).unwrap(),
            );
        }
        input.build_input_tree(&mut bindings);
        let primary = if remapped {
            MouseButton::Middle
        } else {
            MouseButton::Left
        };
        let mut raw = InputContext::default();
        let mut water = WaterTools::default();
        let mut draw = ImmediateDraw::default();
        let mut frame = |raw: &InputContext, x, released: Option<MouseButton>| {
            input.prepare_frame(raw, true, true);
            input.ray = Some(Ray3::new(vec3(x, 1400.0, 100.0), -Vec3::Z));
            PointerFrame {
                held_buttons: raw.mouse.pressed.clone(),
                released_buttons: released.into_iter().collect(),
                ..Default::default()
            }
            .publish();
            water.update(&sim, &ui, &input, &mut draw);
        };
        raw.mouse.pressed.insert(primary);
        frame(&raw, 1200.0, None);
        frame(&raw, 1270.0, None);
        if !remapped {
            raw.keyboard.pressed.insert(Key::Control);
        }
        frame(&raw, 1300.0, remapped.then_some(MouseButton::Left));
        assert!(
            ui.commands().is_empty(),
            "committed before {primary:?} release"
        );
        raw.mouse.pressed.remove(&primary);
        frame(&raw, 1340.0, Some(primary));
        let commands = ui.commands();
        let [WorldCommand::EditWater(WaterEdit::CarveRiver { polyline, .. })] = commands.as_ref()
        else {
            panic!("expected one river edit after {primary:?} release");
        };
        assert_eq!(polyline.points().last().unwrap().x, 1340.0);
        PointerFrame::default().publish();
    }

    #[test]
    fn water_w4_river_stroke_preserves_direction_and_keeps_the_release_endpoint() {
        let mut stroke = WaterGesture::new(vec3(20.0, 30.0, 4.0), None);
        for x in 21..240 {
            stroke.sample(vec3(x as f32, 30.0, 4.0), false, false);
        }
        stroke.sample(vec3(241.0, 30.0, 4.0), false, true);
        let WaterEdit::CarveRiver { polyline, .. } =
            stroke.into_edit(&WaterTools::default()).unwrap()
        else {
            panic!("expected a river edit");
        };
        assert_eq!(
            polyline.points(),
            [Vec2::new(20.0, 30.0), Vec2::new(241.0, 30.0)]
        );
    }

    #[test]
    fn water_w4_lake_gesture_closes_once() {
        let mut stroke = WaterGesture::new(Vec3::ZERO, None);
        stroke.sample(vec3(64.0, 0.0, 0.0), true, false);
        stroke.sample(vec3(64.0, 64.0, 0.0), true, false);
        stroke.sample(vec3(0.0, 64.0, 0.0), true, false);
        stroke.sample(Vec3::ZERO, true, true);
        let state = WaterTools {
            mode: WaterMode::Lake,
            ..WaterTools::default()
        };
        let WaterEdit::FillLake { basin, .. } = stroke.into_edit(&state).unwrap() else {
            panic!("expected a lake edit");
        };
        assert_eq!(basin.points().len(), 4);
    }
}

#[cfg(test)]
impl TerraformingResource {
    pub(crate) fn start_test_gesture(&mut self) {
        self.level = Some(20.0);
        self.slope_start = Some(Vec3::X);
        self.slope_end = Some(Vec3::Y);
        self.water.stroke = Some(WaterGesture::new(Vec3::ZERO, None));
        self.water.sea_action = Some(Some(10));
    }

    pub(crate) fn gesture_is_clear(&self) -> bool {
        self.level.is_none()
            && self.slope_start.is_none()
            && self.slope_end.is_none()
            && self.water.stroke.is_none()
            && self.water.sea_action.is_none()
    }
}
