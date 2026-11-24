use common::saveload::{Encoder, JSON};
use geom::{Vec2, Vec3, OBB};
use simulation::map::{
    ProjectFilter, TerraformKind, WaterBasin, WaterBodyId, WaterEdit, WaterEditError,
    WaterEditOutcome, WaterKind, WaterStroke, MAX_WATER_VERTICES,
};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::Simulation;

use crate::gui::Tool;
use crate::inputmap::{DeckPicker, InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::{water_color, WATER_TILE_PITCH};
use crate::uiworld::UiWorld;

#[allow(unused_imports)]
use w0_preview::{PreviewWater as _, WaterOutcome as _};

pub struct TerraformingResource {
    pub kind: TerraformKind,
    pub radius: f32,
    pub amount: f32,
    level: Option<f32>,
    slope_start: Option<Vec3>,
    slope_end: Option<Vec3>,
    ground_radius: Option<f32>,
    pub(crate) water: WaterTools,
}

impl TerraformingResource {
    pub(crate) fn cancel_gesture(&mut self) {
        self.level = None;
        self.slope_start = None;
        self.slope_end = None;
        self.water.cancel();
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

    if inp.act.contains(&InputAction::Select) || inp.act.contains(&InputAction::SecondarySelect) {
        if res.kind == TerraformKind::Level && res.level.is_none() {
            return;
        }
        if res.kind == TerraformKind::Slope && res.slope_end.is_none() {
            return;
        }
        uiworld.commands().push(WorldCommand::Terraform {
            center: mpos.xy(),
            radius: res.radius,
            amount: res.amount * amount_multiplier,
            level: res.level.unwrap_or(0.0),
            kind: res.kind,
            slope: res.slope_start.zip(res.slope_end),
        })
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
                .color(simulation::colors().gui_primary.a(0.2));
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
            .color(simulation::colors().gui_primary.a(0.2));
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
    stroke: Option<WaterGesture>,
    preview: Option<CachedWaterPreview>,
    submitted: Option<(u64, usize, String)>,
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
            stroke: None,
            preview: None,
            submitted: None,
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
        self.status = None;
        self.preview = None;
        self.submitted = None;
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
        self.read_outcome(&sim.read::<CommandOutcomes>());
        let revision = env.water_revision();

        // Toolbox clicks are explicit confirmations, even when they capture the pointer.
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
        let held = input.act.contains(&InputAction::Select);
        let pressed = input.just_act.contains(&InputAction::Select);
        if self.mode == WaterMode::Remove {
            if let Some(hit) = hits.water.filter(|hit| hit.sample.body != WaterBodyId::SEA) {
                if input.ray.is_some_and(|ray| {
                    DeckPicker::new(&map)
                        .pick(ray, &hits, ProjectFilter::ROAD | ProjectFilter::INTER)
                        .is_some()
                }) {
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
        if pressed {
            self.status = None;
            self.stroke = Some(WaterGesture::new(point));
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

        let release = !held
            || pointer
                .released_buttons
                .contains(&engine::MouseButton::Left);
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
        let edit = match edit {
            Ok(edit) => edit,
            Err(error) => {
                self.status = Some(error.to_string());
                return;
            }
        };
        let Some(command) = water_command(&edit) else {
            self.status = Some("Water editing requires the W0 command dispatcher".into());
            return;
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
                text
            });
            self.preview = Some(CachedWaterPreview {
                snapshot,
                edit: edit.clone(),
                result,
            });
        }
        let summary = match &self.preview.as_ref().unwrap().result {
            Ok(summary) => summary.clone(),
            Err(error) => {
                self.status = Some(error.to_string());
                return;
            }
        };
        let mut commands = uiworld.commands();
        let index = commands.as_ref().len();
        commands.push(command);
        self.status = Some(format!("{summary} · Submitted"));
        self.submitted = Some((sequence, index, summary));
    }

    fn read_outcome(&mut self, outcomes: &CommandOutcomes) {
        let Some((sequence, index, summary)) = &self.submitted else {
            return;
        };
        if outcomes.batch_seq() <= *sequence {
            return;
        }
        if let Some(result) = outcomes.water_edit(*index) {
            self.status = Some(match result {
                Ok(outcome) if outcome.changed => format!("{summary} · Applied"),
                Ok(_) => format!("{summary} · No change"),
                Err(error) => error.to_string(),
            });
            self.submitted = None;
        }
    }
}

struct WaterGesture {
    points: Vec<Vec3>,
    overflow: bool,
}

impl WaterGesture {
    fn new(point: Vec3) -> Self {
        Self {
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
    result: Result<String, WaterEditError>,
}

fn water_command(edit: &WaterEdit) -> Option<WorldCommand> {
    // TODO(W0): Use WorldCommand::EditWater directly once the W0 dispatcher lands.
    // The tagged command decodes only with a simulator that handles it.
    #[derive(serde::Serialize)]
    enum WireWaterCommand<'a> {
        EditWater(&'a WaterEdit),
    }
    JSON::decode(&JSON::encode(&WireWaterCommand::EditWater(edit)).ok()?).ok()
}

#[allow(dead_code)]
mod w0_preview {
    use super::*;

    pub(super) struct UnavailablePreview {
        pub strict_before: bool,
        pub strict_after: bool,
        pub proposed_surface_dm: Option<i16>,
    }

    // TODO(W0): Remove this fallback once Simulation::preview_water_edit lands.
    pub(super) trait PreviewWater {
        fn preview_water_edit(
            &self,
            edit: &WaterEdit,
        ) -> Result<UnavailablePreview, WaterEditError>;
    }

    impl PreviewWater for Simulation {
        fn preview_water_edit(&self, _: &WaterEdit) -> Result<UnavailablePreview, WaterEditError> {
            Err(WaterEditError::Stale)
        }
    }

    // TODO(W0): Use the inherent indexed water outcome once integrated.
    pub(super) trait WaterOutcome {
        fn water_edit(&self, index: usize) -> Option<&Result<WaterEditOutcome, WaterEditError>>;
    }

    impl WaterOutcome for CommandOutcomes {
        fn water_edit(&self, _index: usize) -> Option<&Result<WaterEditOutcome, WaterEditError>> {
            None
        }
    }
}

#[cfg(test)]
mod water_w4_tests {
    use super::*;
    use geom::vec3;

    #[test]
    fn water_w4_river_stroke_preserves_direction_and_keeps_the_release_endpoint() {
        let mut stroke = WaterGesture::new(vec3(20.0, 30.0, 4.0));
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
        let mut stroke = WaterGesture::new(Vec3::ZERO);
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
        self.water.stroke = Some(WaterGesture::new(Vec3::ZERO));
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
