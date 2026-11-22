use geom::Color;
use simulation::map_dynamic::transit::validate;
use simulation::map_dynamic::{LineID, StopID, Transit};
use simulation::world_command::WorldCommand;
use simulation::Simulation;

use crate::inputmap::{InputAction, InputMap};
use crate::rendering::immediate::ImmediateDraw;
use crate::uiworld::UiWorld;

use super::transit_stop::{draw_stops, stop_at};

const MAX_STOPS: usize = 30;

#[derive(Default)]
pub(super) struct LineDraft {
    editing: Option<LineID>,
    stops: Vec<StopID>,
}

impl LineDraft {
    pub(super) fn begin_edit(&mut self, line: LineID) {
        self.cancel();
        self.editing = Some(line);
    }

    pub(super) fn cancel(&mut self) {
        self.editing = None;
        self.stops.clear();
    }

    pub(super) fn editing(&self) -> bool {
        self.editing.is_some()
    }

    pub(super) fn len(&self) -> usize {
        self.stops.len()
    }

    fn select_stop(&mut self, stop: StopID) -> Option<WorldCommand> {
        if self.stops.first() == Some(&stop) && self.stops.len() >= 2 {
            let stops = self.stops.clone();
            return Some(match self.editing {
                Some(line) => WorldCommand::TransitEditLine { line, stops },
                None => WorldCommand::TransitCreateLine { stops },
            });
        }
        if self.stops.len() < MAX_STOPS && !self.stops.contains(&stop) {
            self.stops.push(stop);
        }
        None
    }
}

pub(super) fn update(sim: &Simulation, uiworld: &UiWorld, draft: &mut LineDraft) {
    let transit = sim.read::<Transit>();
    if draft
        .editing
        .is_some_and(|line| !transit.lines.contains_key(line))
        || draft
            .stops
            .iter()
            .any(|stop| !transit.stops.contains_key(*stop))
    {
        draft.cancel();
    }

    let input = uiworld.read::<InputMap>();
    let hovered = input.unprojected.and_then(|pos| stop_at(&transit, pos));

    let mut draw = uiworld.write::<ImmediateDraw>();
    draw_stops(
        &transit,
        &mut draw,
        hovered,
        simulation::colors().gui_primary,
    );

    let point = |stop| transit.stops.get(stop).map(|stop| stop.walk_pos.up(1.0));
    let points: Vec<_> = draft.stops.iter().copied().filter_map(point).collect();
    for (index, &pos) in points.iter().enumerate() {
        let color = if index == 0 {
            Color::WHITE
        } else {
            simulation::colors().gui_primary
        };
        draw.stroke_circle(pos, 6.0, 1.5).color(color);
    }
    if points.len() >= 2 {
        draw.polyline(points.clone(), 2.0, false)
            .color(simulation::colors().gui_primary);
    }

    let Some(stop) = hovered else {
        return;
    };
    if let (Some(&last), Some(next)) = (points.last(), point(stop)) {
        if last.distance2(next) > 0.01 {
            draw.line(last, next, 1.5)
                .color(simulation::colors().gui_primary.a(0.65));
        }
    }

    if input.just_act.contains(&InputAction::Select) {
        if let Some(command) = draft.select_stop(stop) {
            if validate(sim, &command).is_ok() {
                draft.cancel();
            }
            uiworld.commands().push(command);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use slotmapd::KeyData;

    fn stop(index: u64) -> StopID {
        StopID::from(KeyData::from_ffi((1 << 32) | index))
    }

    #[test]
    fn p3d_line_preserves_click_order_and_closes_at_the_first_stop() {
        let mut draft = LineDraft::default();
        assert!(draft.select_stop(stop(3)).is_none());
        assert!(draft.select_stop(stop(2)).is_none());
        assert!(draft.select_stop(stop(1)).is_none());
        assert!(draft.select_stop(stop(2)).is_none());
        let Some(WorldCommand::TransitCreateLine { stops }) = draft.select_stop(stop(3)) else {
            panic!("clicking the first stop should submit a loop");
        };
        assert_eq!(stops, [stop(3), stop(2), stop(1)]);
    }

    #[test]
    fn p3d_line_cancels_and_requires_two_stops_to_close() {
        let mut draft = LineDraft::default();
        assert!(draft.select_stop(stop(1)).is_none());
        assert!(draft.select_stop(stop(1)).is_none());
        draft.cancel();
        assert!(draft.select_stop(stop(2)).is_none());
        assert!(draft.select_stop(stop(1)).is_none());
        let Some(WorldCommand::TransitCreateLine { stops }) = draft.select_stop(stop(2)) else {
            panic!("the first click after cancel should start a new loop");
        };
        assert_eq!(stops, [stop(2), stop(1)]);
    }

    #[test]
    fn p3d_line_closes_an_edit_against_the_selected_line() {
        let line = LineID::from(KeyData::from_ffi((1 << 32) | 1));
        let mut draft = LineDraft::default();
        draft.begin_edit(line);
        assert!(draft.select_stop(stop(1)).is_none());
        assert!(draft.select_stop(stop(2)).is_none());
        let Some(WorldCommand::TransitEditLine { line: id, stops }) = draft.select_stop(stop(1))
        else {
            panic!("closing an edit should replace the route");
        };
        assert_eq!(id, line);
        assert_eq!(stops, [stop(1), stop(2)]);
    }
}
