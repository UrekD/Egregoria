use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use geom::{LinearColor, PolyLine3};
use goryak::{
    button_primary, button_secondary, mincolumn, minrow, on_primary_container,
    on_secondary_container, textc,
};
use scenarios::traffic_repair::{
    compare_builtin_repair, compare_captured_city, Cohort, FixtureArm, NaturalRepairComparison,
    RepairComparison,
};
use simulation::map::road_edit::{
    junction_edit_stamp, preview_road_edit, road_edit_stamp, EditReceipt, JunctionEditReceipt,
    RoadEditPreview, RoadEditRequest, RoadEditStatus, RoadQuote, RoadUpgrade,
};
use simulation::map::{
    ApproachRole, IntersectionID, JunctionRules, LaneID, LaneKind, LanePattern, Map, Road, RoadID,
    TurnID, TurnKind,
};
use simulation::transportation::queue_diagnostics::{
    explain_queue, parking_pressure, BlockerChainEnd, ParkingAvailability, ParkingPressure,
    QueueCoverage, QueueExplanation, QueueHoldReason, QueueLimits, QueueRevision,
};
use simulation::world_command::{
    CommandOutcomes, PrepareBudget, Rejection, WorldCommand, WorldCommands,
};
use simulation::{Simulation, SlotCapture};
use slotmapd::Key;

use crate::gui::{notifications::command_message, InspectedRoad};
use crate::rendering::immediate::ImmediateDraw;
use crate::session::SessionState;
use crate::sim_thread::{AppliedBatch, BatchId, Request, SimThread};
use crate::uiworld::UiWorld;

const PREVIEW_WORK: usize = 65_536;
const SAMPLE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditTarget {
    Road(RoadID),
    Junction(IntersectionID),
}

impl EditTarget {
    fn command_name(self) -> &'static str {
        match self {
            Self::Road(_) => "MapUpgradeRoad",
            Self::Junction(_) => "MapSetJunctionRules",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EditKey {
    target: EditTarget,
    quote: RoadQuote,
}

impl EditKey {
    fn of(command: &WorldCommand) -> Option<Self> {
        match *command {
            WorldCommand::MapUpgradeRoad { road, quote, .. } => Some(Self {
                target: EditTarget::Road(road),
                quote,
            }),
            WorldCommand::MapSetJunctionRules {
                junction, quote, ..
            } => Some(Self {
                target: EditTarget::Junction(junction),
                quote,
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrafficEditResult {
    Road(EditReceipt),
    Junction(JunctionEditReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrafficEditReceipt {
    pub generation: u64,
    pub frame: u64,
    pub sender: Option<u32>,
    pub command_index: usize,
    pub sequence: u64,
    pub local: bool,
    key: EditKey,
    pub result: Result<TrafficEditResult, Rejection>,
}

impl TrafficEditReceipt {
    pub(crate) fn from_outcome(
        generation: u64,
        frame: u64,
        sender: Option<u32>,
        command_index: usize,
        local: bool,
        flattened_index: usize,
        command: &WorldCommand,
        outcomes: &CommandOutcomes,
    ) -> Option<Self> {
        let key = EditKey::of(command)?;
        let result = match key.target {
            EditTarget::Road(_) => outcomes
                .road_edit(flattened_index)
                .map(|result| result.clone().map(TrafficEditResult::Road)),
            EditTarget::Junction(_) => outcomes
                .junction_edit(flattened_index)
                .map(|result| result.map(TrafficEditResult::Junction)),
        }
        .or_else(|| {
            outcomes
                .batch_rejection()
                .or_else(|| {
                    outcomes
                        .rejections_for(flattened_index)
                        .first()
                        .map(|&(_, reason)| reason)
                })
                .map(Err)
        })?;
        Some(Self {
            generation,
            frame,
            sender,
            command_index,
            sequence: outcomes.batch_seq(),
            local,
            key,
            result,
        })
    }
}

#[cfg(feature = "multiplayer")]
pub(crate) fn network_reset(uiw: &UiWorld, generation: u64) {
    let watch = uiw.write::<TrafficRepairUi>().watch.take();
    reset(uiw, generation);
    uiw.write::<TrafficRepairUi>().watch = watch;
}

struct Quote {
    summary: String,
    command: WorldCommand,
    preview: RoadEditPreview,
}

impl Quote {
    fn new(sim: &Simulation, request: RoadEditRequest) -> Result<Self, Rejection> {
        let preview =
            preview_road_edit(sim, &request, &mut PrepareBudget::for_preview(PREVIEW_WORK))?;
        let command = match (request, &preview) {
            (RoadEditRequest::Upgrade { change, .. }, RoadEditPreview::Upgrade(p)) => {
                WorldCommand::MapUpgradeRoad {
                    road: p.road,
                    expected: p.stamp,
                    change,
                    quote: p.quote,
                }
            }
            (RoadEditRequest::JunctionRules { rules, .. }, RoadEditPreview::JunctionRules(p)) => {
                WorldCommand::MapSetJunctionRules {
                    junction: p.junction,
                    expected: p.stamp,
                    rules,
                    quote: p.quote,
                }
            }
            _ => unreachable!("road edit preview corresponds to its request"),
        };
        let summary = match &command {
            WorldCommand::MapUpgradeRoad {
                change: RoadUpgrade::ReverseOneWay,
                ..
            } => "Reverse driving direction".into(),
            WorldCommand::MapUpgradeRoad {
                change: RoadUpgrade::Replace(pattern),
                ..
            } => {
                let driving = |side: &[(LaneKind, f32)]| {
                    side.iter()
                        .filter(|&&(kind, _)| kind == LaneKind::Driving)
                        .count()
                };
                let forward = driving(&pattern.lanes_forward);
                let backward = driving(&pattern.lanes_backward);
                let (min, max) = pattern
                    .lanes()
                    .filter(|(kind, _, _)| *kind == LaneKind::Driving)
                    .fold((f32::INFINITY, 0.0_f32), |(min, max), (_, _, speed)| {
                        (min.min(speed), max.max(speed))
                    });
                let speed = if min == max {
                    format!("{:.0} km/h", max * 3.6)
                } else {
                    format!("{:.0}–{:.0} km/h", min * 3.6, max * 3.6)
                };
                format!("Driving lanes {forward} forward · {backward} backward · {speed}")
            }
            WorldCommand::MapSetJunctionRules {
                junction, rules, ..
            } => {
                let map = sim.map();
                let current = map.junction_rules(*junction);
                let mut changes = Vec::new();
                for (road, role) in rules.approaches() {
                    if current.and_then(|previous| previous.role(*road)) != Some(*role) {
                        changes.push(format!("Approach {:?} becomes {role:?}", road.data()));
                    }
                }
                for (road, _) in current.into_iter().flat_map(JunctionRules::approaches) {
                    if rules.role(*road).is_none() {
                        changes.push(format!("Approach {:?} returns to Default", road.data()));
                    }
                }
                for turn in rules.forbidden() {
                    if current.is_none_or(|previous| previous.allows(*turn)) {
                        changes.push(format!(
                            "Restrict lane {:?} to {:?}",
                            turn.src.data(),
                            turn.dst.data()
                        ));
                    }
                }
                for turn in current.into_iter().flat_map(JunctionRules::forbidden) {
                    if rules.allows(*turn) {
                        changes.push(format!(
                            "Restore lane {:?} to {:?}",
                            turn.src.data(),
                            turn.dst.data()
                        ));
                    }
                }
                changes.join(" · ")
            }
            _ => unreachable!("traffic repair quote command"),
        };
        Ok(Self {
            command,
            preview,
            summary,
        })
    }

    fn committable(&self) -> bool {
        match &self.preview {
            RoadEditPreview::Upgrade(p) => p.projected_topology.is_none(),
            RoadEditPreview::JunctionRules(_) => true,
        }
    }
}

#[derive(Clone, Copy)]
struct Pending {
    generation: u64,
    after_sequence: u64,
    index: usize,
    key: EditKey,
    batch: Option<BatchId>,
}

impl Pending {
    fn queued_in(self, commands: &WorldCommands) -> bool {
        self.batch.is_none()
            && commands.as_ref().get(self.index).and_then(EditKey::of) == Some(self.key)
    }

    fn matches(self, receipt: &TrafficEditReceipt, batch: Option<BatchId>) -> bool {
        receipt.local
            && receipt.generation == self.generation
            && receipt.sequence > self.after_sequence
            && receipt.key == self.key
            && self.batch == batch
            && (batch.is_none() || receipt.command_index == self.index)
    }
}

struct DirectedSample {
    road: RoadID,
    lane: LaneID,
    revision: QueueRevision,
    sampled: Instant,
    queue: QueueExplanation,
    parking: Option<ParkingPressure>,
}

#[derive(Default)]
pub(crate) struct TrafficRepairUi {
    target: Option<EditTarget>,
    quote: Option<Quote>,
    pending: Option<Pending>,
    receipt: Option<TrafficEditReceipt>,
    error: Option<(EditTarget, String)>,
    selected_lane: Option<LaneID>,
    approach: usize,
    movement: usize,
    sample: Option<DirectedSample>,
    watch: Option<(RoadID, LaneID)>,
    comparison: Comparison,
    selected_comparison: SelectedComparison,
}

impl TrafficRepairUi {
    pub(crate) fn select(&mut self, target: Option<EditTarget>) {
        if self.target != target {
            self.target = target;
            self.quote = None;
            self.selected_comparison.invalidate();
            self.selected_lane = None;
            self.approach = 0;
            self.movement = 0;
            self.sample = None;
        }
    }

    fn request(
        &mut self,
        sim: &Simulation,
        target: EditTarget,
        request: Result<RoadEditRequest, Rejection>,
    ) {
        self.quote = None;
        self.selected_comparison.invalidate();
        self.receipt = None;
        self.error = None;
        match request.and_then(|request| Quote::new(sim, request)) {
            Ok(quote) => self.quote = Some(quote),
            Err(reason) => {
                self.error = Some((
                    target,
                    command_message(target.command_name(), reason).into_owned(),
                ))
            }
        }
    }

    fn confirm(&mut self, sim: &Simulation, uiw: &UiWorld) {
        if self.pending.is_some() || !uiw.can_author() || !crate::session::world_interaction(uiw) {
            return;
        }
        let Some(quote) = self.quote.take() else {
            return;
        };
        if !quote.committable() {
            self.quote = Some(quote);
            return;
        }
        let key = EditKey::of(&quote.command).expect("traffic repair quote command");
        let mut commands = uiw.commands();
        self.pending = Some(Pending {
            generation: uiw.read::<SessionState>().generation,
            after_sequence: sim.read::<CommandOutcomes>().batch_seq(),
            index: commands.as_ref().len(),
            key,
            batch: None,
        });
        commands.push(quote.command);
    }

    fn receive(&mut self, receipts: &[TrafficEditReceipt], batch: Option<BatchId>) {
        let Some(pending) = self.pending else { return };
        if let Some(receipt) = receipts
            .iter()
            .find(|receipt| pending.matches(receipt, batch))
        {
            self.pending = None;
            self.receipt = Some(receipt.clone());
            self.sample = None;
        }
    }

    fn reject(&mut self, message: &str) {
        if let Some(pending) = self.pending.take() {
            self.error = Some((pending.key.target, message.to_owned()));
        }
    }

    fn show_result(&mut self, sim: &Simulation, uiw: &UiWorld, target: EditTarget) {
        if let Some((_, error)) = self.error.as_ref().filter(|(edit, _)| *edit == target) {
            textc(goryak::error(), error.clone());
        }
        if let Some(receipt) = self
            .receipt
            .as_ref()
            .filter(|receipt| receipt.key.target == target)
        {
            match &receipt.result {
                Ok(TrafficEditResult::Road(r)) => {
                    let status = status_text(r.status);
                    textc(
                        on_primary_container(),
                        format!(
                            "{status} · Charged {} · {} lane IDs retained",
                            r.debit,
                            r.retained_lanes.len()
                        ),
                    );
                }
                Ok(TrafficEditResult::Junction(r)) => {
                    textc(on_primary_container(), status_text(r.status))
                }
                Err(reason) => textc(
                    goryak::error(),
                    command_message(target.command_name(), *reason),
                ),
            }
        }
        if self
            .pending
            .is_some_and(|pending| pending.key.target == target)
        {
            textc(on_secondary_container(), "Waiting for simulation receipt");
        }
        if let Some(quote) = &self.quote {
            if !quote.summary.is_empty() {
                textc(on_primary_container(), quote.summary.clone());
            }
            match &quote.preview {
                RoadEditPreview::Upgrade(p) => {
                    textc(
                        on_primary_container(),
                        format!(
                            "Cost {} · {} lane IDs retained",
                            p.cost,
                            p.retained_lanes.len()
                        ),
                    );
                    textc(
                        on_secondary_container(),
                        format!(
                            "{} connected buildings · {} frontage lots · {} lane speed changes",
                            p.attached_buildings, p.attached_lots, p.changed_lane_costs
                        ),
                    );
                    if let Some(projection) = &p.projected_topology {
                        textc(
                            on_secondary_container(),
                            format!(
                                "{} nearby roads · {} additional roads{}",
                                projection.affected_roads.len(),
                                projection.omitted_roads,
                                if projection.crosses_water {
                                    " · Water crossing"
                                } else {
                                    ""
                                },
                            ),
                        );
                        textc(
                            goryak::error(),
                            "Live changes to road width are unavailable in this session.",
                        );
                    }
                }
                RoadEditPreview::JunctionRules(p) => {
                    textc(on_primary_container(), format!("Cost {}", p.cost));
                    if !p.changed {
                        textc(on_secondary_container(), "Rules already match");
                    }
                }
            }
            let enabled = quote.committable() && uiw.can_author() && self.pending.is_none();
            let mut compare_clicked = false;
            minrow(8.0, || {
                if enabled
                    && self.comparison.current.is_none()
                    && !self.selected_comparison.is_running()
                    && button_secondary("Compare quoted edit").show().clicked
                {
                    compare_clicked = true;
                }
                if enabled && button_primary("Confirm edit").show().clicked {
                    self.confirm(sim, uiw);
                }
                if button_secondary("Discard quote").show().clicked {
                    self.quote = None;
                }
            });
            if compare_clicked {
                self.selected_comparison.result = None;
                self.selected_comparison.request = self.quote.as_ref().map(|quote| quote.command.clone());
            }
        }
        self.selected_comparison.render(uiw.read::<SessionState>().generation);
    }
}

fn status_text(status: RoadEditStatus) -> &'static str {
    match status {
        RoadEditStatus::Applied => "Applied",
        RoadEditStatus::NoChange => "No change",
    }
}

pub(crate) fn reset(uiw: &UiWorld, _: u64) {
    let mut state = uiw.write::<TrafficRepairUi>();
    let current = state.comparison.current.take();
    if let Some(job) = &current {
        job.cancel.store(true, Ordering::Relaxed);
    }
    let selected = state.selected_comparison.job.take();
    if let Some(job) = &selected {
        job.cancel.store(true, Ordering::Relaxed);
    }
    *state = TrafficRepairUi {
        comparison: Comparison {
            current,
            result: None,
        },
        selected_comparison: SelectedComparison { job: selected, ..Default::default() },
        ..Default::default()
    };
}

pub(crate) fn worker_batch_queued(uiw: &UiWorld, commands: &WorldCommands) -> bool {
    uiw.read::<TrafficRepairUi>()
        .pending
        .is_some_and(|p| p.queued_in(commands))
}

pub(crate) fn bind_worker_batch(uiw: &UiWorld, id: BatchId) {
    if let Some(pending) = &mut uiw.write::<TrafficRepairUi>().pending {
        if pending.batch.is_none() && pending.generation == id.generation {
            pending.batch = Some(id);
        }
    }
}

pub(crate) fn receive_worker_batch(uiw: &UiWorld, batch: &AppliedBatch) {
    uiw.write::<TrafficRepairUi>()
        .receive(&batch.traffic_edits, Some(batch.id));
}

#[cfg(feature = "multiplayer")]
pub(crate) fn receive_network(uiw: &UiWorld, receipts: &[TrafficEditReceipt]) {
    uiw.write::<TrafficRepairUi>().receive(receipts, None);
}

pub(crate) fn reject_local_batch(uiw: &UiWorld, commands: &WorldCommands) {
    if worker_batch_queued(uiw, commands) {
        uiw.write::<TrafficRepairUi>()
            .reject("The edit exceeds the simulation's batch limit.");
    }
}

pub(crate) fn reject_worker_batch(uiw: &UiWorld, id: BatchId, error: &str) {
    let mut state = uiw.write::<TrafficRepairUi>();
    if state.pending.is_some_and(|p| p.batch == Some(id)) {
        state.reject(error);
    }
}

pub(crate) fn render_road(uiw: &UiWorld, sim: &Simulation, map: &Map, road: &Road) {
    let target = EditTarget::Road(road.id);
    let mut state = uiw.write::<TrafficRepairUi>();
    state.select(Some(target));
    let lanes: Vec<_> = road
        .lanes_iter()
        .filter_map(|(lane, kind)| (kind == LaneKind::Driving).then_some(lane))
        .take(32)
        .collect();

    if lanes.is_empty() {
        return;
    }
    let selected = state
        .selected_lane
        .and_then(|lane| lanes.iter().position(|&id| id == lane))
        .unwrap_or(0);
    mincolumn(4.0, || {
        minrow(8.0, || {
            textc(on_primary_container(), "Directed traffic");
            if lanes.len() > 1 && button_secondary("Next lane").show().clicked {
                state.selected_lane = Some(lanes[(selected + 1) % lanes.len()]);
            } else {
                state.selected_lane = Some(lanes[selected]);
            }
        });
        let lane = state.selected_lane.unwrap();
        if let Some(lane) = map.get(lane) {
            textc(
                on_secondary_container(),
                format!(
                    "Lane {:?} · Junction {:?} to {:?}",
                    lane.id.data(),
                    lane.src.data(),
                    lane.dst.data(),
                ),
            );
        }
        if let Some(sample) = &state.sample {
            if sample.lane == lane && sample.revision == QueueRevision::current(map) {
                render_diagnostics(sample);
            }
        }
    });

    mincolumn(4.0, || {
        textc(on_primary_container(), "Road repair");
        if state.pending.is_none() && uiw.can_author() {
            let pattern = road.pattern(map.lanes());
            minrow(6.0, || {
                let one_way = pattern
                    .lanes_forward
                    .iter()
                    .any(|&(kind, _)| kind == LaneKind::Driving)
                    != pattern
                        .lanes_backward
                        .iter()
                        .any(|&(kind, _)| kind == LaneKind::Driving);
                for (change, label) in [(-2.0, "Lower speed"), (2.0, "Raise speed")] {
                    if button_secondary(label).show().clicked {
                        state.request(
                            sim,
                            target,
                            road_request(sim, road.id, speed_upgrade(road, map, change)),
                        );
                    }
                }
                if one_way && button_secondary("Reverse traffic").show().clicked {
                    state.request(
                        sim,
                        target,
                        road_request(sim, road.id, RoadUpgrade::ReverseOneWay),
                    );
                }
            });
            minrow(6.0, || {
                for (widen, label) in [
                    (false, "Preview narrower road"),
                    (true, "Preview wider road"),
                ] {
                    if let Some(change) = change_width(&pattern, widen) {
                        if button_secondary(label).show().clicked {
                            state.request(sim, target, road_request(sim, road.id, change));
                        }
                    }
                }
            });
        }
        state.show_result(sim, uiw, target);
        let can_start = !state.selected_comparison.is_running();
        state
            .comparison
            .render(uiw.read::<SessionState>().generation, can_start);
    });
}

fn change_width(pattern: &LanePattern, widen: bool) -> Option<RoadUpgrade> {
    let mut next = pattern.clone();
    for side in [&mut next.lanes_forward, &mut next.lanes_backward] {
        let count = side
            .iter()
            .take_while(|&&(kind, _)| kind == LaneKind::Driving)
            .count();
        if count == 0 {
            continue;
        }
        if widen && count < 3 {
            side.insert(0, side[0]);
        } else if !widen && count > 1 {
            side.remove(0);
        } else {
            return None;
        }
    }
    Some(RoadUpgrade::Replace(next))
}

fn speed_upgrade(road: &Road, map: &Map, delta: f32) -> RoadUpgrade {
    let mut pattern = road.pattern(map.lanes());
    for (kind, speed) in pattern
        .lanes_forward
        .iter_mut()
        .chain(&mut pattern.lanes_backward)
    {
        if *kind == LaneKind::Driving {
            *speed = (*speed + delta).clamp(4.0, 40.0);
        }
    }
    RoadUpgrade::Replace(pattern)
}

fn road_request(
    sim: &Simulation,
    road: RoadID,
    change: RoadUpgrade,
) -> Result<RoadEditRequest, Rejection> {
    let expected = road_edit_stamp(sim, road, &mut PrepareBudget::for_preview(PREVIEW_WORK))?;
    Ok(RoadEditRequest::Upgrade {
        road,
        expected,
        change,
    })
}

fn junction_request(
    sim: &Simulation,
    junction: IntersectionID,
    approaches: Vec<(RoadID, ApproachRole)>,
    forbidden: Vec<TurnID>,
) -> Result<RoadEditRequest, Rejection> {
    let rules =
        JunctionRules::new(approaches, forbidden).map_err(Rejection::InvalidJunctionRules)?;
    let expected =
        junction_edit_stamp(sim, junction, &mut PrepareBudget::for_preview(PREVIEW_WORK))?;
    Ok(RoadEditRequest::JunctionRules {
        junction,
        expected,
        rules,
    })
}

pub(crate) fn render_junction(uiw: &UiWorld, sim: &Simulation, id: IntersectionID) -> bool {
    let map = sim.map();
    let Some(junction) = map.get(id) else {
        return false;
    };
    let target = EditTarget::Junction(id);
    let mut state = uiw.write::<TrafficRepairUi>();
    state.select(Some(target));
    let rules = map.junction_rules(id).cloned().unwrap_or_default();
    let approaches: Vec<_> = junction
        .roads
        .iter()
        .copied()
        .filter(|&road| {
            map.get(road).is_some_and(|road| {
                road.incoming_lanes_to(id)
                    .iter()
                    .any(|&(_, kind)| kind == LaneKind::Driving)
            })
        })
        .take(32)
        .collect();
    let turns: Vec<_> = junction
        .turns()
        .filter(|turn| turn.kind == TurnKind::Driving)
        .map(|turn| turn.id)
        .chain(rules.forbidden().iter().copied())
        .take(256)
        .collect();

    let mut opened = true;
    goryak::Window {
        title: format!("Junction repair {:?}", id.data()).into(),
        pad: yakui::widgets::Pad::all(14.0),
        radius: 10.0,
        opened: &mut opened,
        child_spacing: 10.0,
    }
    .show(|| {
        let author = uiw.can_author() && state.pending.is_none();
        if let Some(&road) = approaches.get(state.approach % approaches.len().max(1)) {
            mincolumn(4.0, || {
                let role = match rules.role(road) {
                    Some(ApproachRole::Priority) => "Priority",
                    Some(ApproachRole::Yield) => "Yield",
                    Some(ApproachRole::Stop) => "Stop",
                    None => "Default",
                };
                minrow(8.0, || {
                    textc(
                        on_primary_container(),
                        format!("Approach {:?} · {role}", road.data()),
                    );
                    if approaches.len() > 1 && button_secondary("Next approach").show().clicked {
                        state.approach = (state.approach + 1) % approaches.len();
                    }
                });
                if author {
                    minrow(6.0, || {
                        for (role, label) in [
                            (None, "Default"),
                            (Some(ApproachRole::Priority), "Priority"),
                            (Some(ApproachRole::Yield), "Yield"),
                            (Some(ApproachRole::Stop), "Stop"),
                        ] {
                            if button_secondary(label).show().clicked {
                                let mut approaches = rules.approaches().to_vec();
                                approaches.retain(|&(approach, _)| approach != road);
                                if let Some(role) = role {
                                    approaches.push((road, role));
                                }
                                state.request(
                                    sim,
                                    target,
                                    junction_request(
                                        sim,
                                        id,
                                        approaches,
                                        rules.forbidden().to_vec(),
                                    ),
                                );
                            }
                        }
                    });
                }
            });
        }
        if let Some(&turn) = turns.get(state.movement % turns.len().max(1)) {
            mincolumn(4.0, || {
                let allowed = rules.allows(turn);
                textc(
                    on_primary_container(),
                    format!(
                        "Lane {:?} to {:?} · {}",
                        turn.src.data(),
                        turn.dst.data(),
                        if allowed { "Allowed" } else { "Restricted" }
                    ),
                );
                minrow(6.0, || {
                    if turns.len() > 1 && button_secondary("Next turn").show().clicked {
                        state.movement = (state.movement + 1) % turns.len();
                    }
                    let action = if allowed {
                        "Restrict turn"
                    } else {
                        "Restore turn"
                    };
                    if author && button_secondary(action).show().clicked {
                        let mut forbidden = rules.forbidden().to_vec();
                        if allowed {
                            forbidden.push(turn);
                        } else {
                            forbidden.retain(|&old| old != turn);
                        }
                        state.request(
                            sim,
                            target,
                            junction_request(sim, id, rules.approaches().to_vec(), forbidden),
                        );
                    }
                });
            });
        }
        state.show_result(sim, uiw, target);
        let can_start = !state.selected_comparison.is_running();
        state
            .comparison
            .render(uiw.read::<SessionState>().generation, can_start);
    });
    opened
}

fn render_diagnostics(sample: &DirectedSample) {
    let queue = &sample.queue;
    match queue.coverage {
        QueueCoverage::Complete | QueueCoverage::Truncated => {
            textc(
                on_secondary_container(),
                format!(
                    "{} at stop line · {} standing midblock",
                    queue.stop_line.len(),
                    queue.midblock
                ),
            );
        }
        _ => textc(
            on_secondary_container(),
            "Waiting for a directed traffic sample",
        ),
    }
    if let Some(turn) = queue.requested_turn {
        textc(
            on_secondary_container(),
            format!(
                "Requested turn · Lane {:?} to {:?}",
                turn.src.data(),
                turn.dst.data()
            ),
        );
    }
    if let Some(reason) = queue.reason {
        let time = queue.hold_age_ticks.map_or(String::new(), |ticks| {
            format!(
                " · {:.1} game min",
                ticks as f64 / prototypes::TICKS_PER_MINUTE as f64
            )
        });
        textc(
            on_primary_container(),
            format!("{}{time}", hold_label(reason)),
        );
    }
    for hop in &queue.chain {
        if let Some(blocker) = hop.blocker {
            textc(
                on_secondary_container(),
                format!(
                    "Vehicle {:?} · {} · {blocker:?}",
                    hop.vehicle.data(),
                    hold_label(hop.reason)
                ),
            );
        }
    }
    match queue.chain_end {
        BlockerChainEnd::Cycle(_) => textc(goryak::error(), "Blockers form a cycle"),
        BlockerChainEnd::Truncated => textc(
            on_secondary_container(),
            "Blocker chain exceeds the read limit",
        ),
        BlockerChainEnd::Unseen if !queue.stop_line.is_empty() => textc(
            on_secondary_container(),
            "Waiting for a blocker observation",
        ),
        _ => {}
    }
    if queue.coverage == QueueCoverage::Truncated || queue.omitted_holds > 0 {
        textc(
            on_secondary_container(),
            format!(
                "Partial observation · {} occupants, {} holds, {} alternatives omitted",
                queue.omitted_occupants, queue.omitted_holds, queue.omitted_alternatives
            ),
        );
    }
    for turn in &queue.alternatives {
        textc(
            on_secondary_container(),
            format!("Open receiving lane · {:?}", turn.dst.data()),
        );
    }
    if let Some(parking) = sample.parking {
        let label = match parking.availability {
            ParkingAvailability::NoCurb => "No curb parking",
            ParkingAvailability::NoLocalAccess => "No curb access",
            ParkingAvailability::Free => "Curb space available",
            ParkingAvailability::Full => "Curb parking full",
            ParkingAvailability::Truncated => "Partial curb count",
        };
        textc(on_primary_container(), label);
        if parking.total > 0 {
            textc(
                on_secondary_container(),
                format!(
                    "{} parked · {} reserved ({} inbound) · {} free",
                    parking.parked, parking.reserved, parking.reserved_inbound, parking.free
                ),
            );
        }
        if let Some(search) = parking.search.filter(|search| search.attempts > 0) {
            textc(
                on_secondary_container(),
                format!(
                    "{} observed parking searches · {} exhausted · {} found another road",
                    search.attempts, search.exhausted, search.other_road
                ),
            );
        }
        if parking.omitted_spots + parking.omitted_lanes + parking.unclassified_reservations > 0 {
            textc(
                on_secondary_container(),
                format!(
                    "{} spaces, {} lanes omitted · {} unclassified reservations",
                    parking.omitted_spots, parking.omitted_lanes, parking.unclassified_reservations
                ),
            );
        }
    }
}

fn hold_label(reason: QueueHoldReason) -> &'static str {
    match reason {
        QueueHoldReason::Leader(_) => "Vehicle ahead",
        QueueHoldReason::Pedestrian(_) => "Pedestrian crossing",
        QueueHoldReason::Conflict(_) => "Conflicting movement",
        QueueHoldReason::ExitFull => "Receiving lane full",
        QueueHoldReason::Signal => "Traffic signal",
        QueueHoldReason::StopDwell => "Stop sign dwell",
        QueueHoldReason::BusDwell => "Bus stop dwell",
        QueueHoldReason::InvalidTraversal => "Movement changed",
        QueueHoldReason::NoRoute => "Route unavailable",
        QueueHoldReason::Unknown => "Hold reason unknown",
    }
}

pub(crate) fn draw_arrow(draw: &mut ImmediateDraw, points: &PolyLine3, color: LinearColor) {
    let length = points.length();
    let (tip, heading) = points.point_dir_along(length * 0.72);
    let Some(dir) = heading.xy().try_normalize() else {
        return;
    };
    let size = (length * 0.15).clamp(0.6, 2.8);
    let back = tip - dir.z0() * size;
    let side = dir.perpendicular().z0() * (size * 0.55);
    draw.line((back - side).up(0.65), tip.up(0.65), 0.38)
        .color(color);
    draw.line((back + side).up(0.65), tip.up(0.65), 0.38)
        .color(color);
}

pub(crate) fn draw_selected(sim: &Simulation, uiw: &UiWorld, draw: &mut ImmediateDraw) {
    profiling::scope!("traffic repair overlay");
    use crate::gui::{roadeditor::RoadEditorResource, InspectedBuilding, InspectedEntity, Tool};
    use simulation::transportation::queue_diagnostics::{
        unwatch_parking, unwatch_queue, watch_parking, watch_queue,
    };

    let Some(mut state) = uiw.try_write::<TrafficRepairUi>() else {
        return;
    };
    let map = sim.map();
    let hidden = uiw.read::<crate::gui::GuiState>().hidden;
    let selected = match *uiw.read::<Tool>() {
        _ if hidden => None,
        Tool::Hand
            if uiw.read::<InspectedBuilding>().e.is_none()
                && uiw.read::<InspectedEntity>().e.is_none() =>
        {
            uiw.read::<InspectedRoad>()
                .0
                .filter(|&id| map.get(id).is_some())
                .map(EditTarget::Road)
        }
        Tool::RoadEditor => uiw
            .read::<RoadEditorResource>()
            .inspect
            .as_ref()
            .map(|intersection| intersection.id)
            .filter(|&id| map.get(id).is_some())
            .map(EditTarget::Junction),
        _ => None,
    };
    state.select(selected);
    let requested = selected.and_then(|target| {
        let EditTarget::Road(id) = target else {
            return None;
        };
        let road = map.get(id)?;
        let lane = state
            .selected_lane
            .filter(|&lane| {
                map.get(lane)
                    .is_some_and(|lane| lane.parent == id && lane.kind == LaneKind::Driving)
            })
            .or_else(|| {
                road.lanes_iter()
                    .find_map(|(lane, kind)| (kind == LaneKind::Driving).then_some(lane))
            })?;
        Some((id, lane))
    });
    if state.watch.is_some_and(|watch| Some(watch) != requested) {
        let (road, lane) = state.watch.take().unwrap();
        unwatch_queue(sim, lane);
        if requested.is_none_or(|(id, _)| id != road) {
            unwatch_parking(sim, road);
        }
    }
    if let Some((road, lane)) = requested {
        if state.watch.is_none() {
            watch_queue(sim, lane);
            watch_parking(sim, road);
            state.watch = Some((road, lane));
        }
        state.selected_lane = Some(lane);
        let now = Instant::now();
        let revision = QueueRevision::current(&map);
        if state.sample.as_ref().is_none_or(|sample| {
            sample.road != road
                || sample.lane != lane
                || sample.revision != revision
                || now - sample.sampled >= SAMPLE_INTERVAL
        }) {
            state.sample = Some(DirectedSample {
                road,
                lane,
                revision,
                sampled: now,
                queue: explain_queue(sim, lane, revision, QueueLimits::default()),
                parking: parking_pressure(sim, road, QueueLimits::default()),
            });
        }
        let queue = &state.sample.as_ref().unwrap().queue;
        if let Some(lane) = map.get(lane) {
            let color = if queue.stop_line.is_empty() {
                simulation::colors().gui_primary
            } else {
                simulation::colors().gui_danger
            };
            draw_path(draw, &lane.points, 0.85, color.into());
            draw_arrow(draw, &lane.points, color.into());
            if let Some(offset) = queue.head_offset {
                draw.stroke_circle(lane.points.point_along(offset).up(0.75), 2.5, 0.55)
                    .color(color);
            }
        }
        for &turn in &queue.alternatives {
            if let Some(turn) = map
                .get(turn.parent)
                .and_then(|junction| junction.find_turn(turn))
            {
                let color = simulation::colors().gui_success.into();
                draw_path(draw, &turn.points, 0.50, color);
                draw_arrow(draw, &turn.points, color);
            }
        }
    }
    if let Some(Quote {
        preview: RoadEditPreview::Upgrade(preview),
        ..
    }) = &state.quote
    {
        draw_path(
            draw,
            &preview.deck,
            2.0,
            simulation::colors().gui_success.into(),
        );
    }
}

fn draw_path(draw: &mut ImmediateDraw, points: &PolyLine3, width: f32, color: LinearColor) {
    // At most 128 vertices per observed path, including both endpoints.
    let stride = points.len().div_ceil(127).max(1);
    let mut sample: Vec<_> = points.iter().step_by(stride).map(|p| p.up(0.6)).collect();
    let last = points.last().up(0.6);
    if sample.last().copied() != Some(last) {
        sample.push(last);
    }
    if sample.len() >= 2 {
        draw.polyline(sample, width, false).color(color);
    }
}

#[derive(Default)]
struct Comparison {
    current: Option<ComparisonJob<RepairComparison>>,
    result: Option<Result<RepairComparison, String>>,
}

struct ComparisonJob<R> {
    generation: u64,
    cancel: Arc<AtomicBool>,
    reply: mpsc::Receiver<Result<R, String>>,
}

impl ComparisonJob<RepairComparison> {
    fn start(generation: u64) -> std::io::Result<Self> {
        Self::spawn(generation, |cancelled| {
            compare_builtin_repair(FixtureArm::Repair, cancelled).map_err(|err| err.to_string())
        })
    }
}

impl<R: Send + 'static> ComparisonJob<R> {
    fn spawn(
        generation: u64,
        evaluate: impl FnOnce(&AtomicBool) -> Result<R, String> + Send + 'static,
    ) -> std::io::Result<Self> {
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, reply) = mpsc::sync_channel(1);
        let cancellation = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("traffic-comparison".into())
            .spawn(move || {
                let result = rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .map_err(|error| error.to_string())
                    .and_then(|pool| pool.install(|| evaluate(&cancellation)));
                let _ = sender.send(result);
            })?;
        Ok(Self {
            generation,
            cancel,
            reply,
        })
    }
}

impl<R> Drop for ComparisonJob<R> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct SelectedComparison {
    request: Option<WorldCommand>,
    pending: Option<(BatchId, WorldCommand)>,
    job: Option<ComparisonJob<NaturalRepairComparison>>,
    result: Option<Result<NaturalRepairComparison, String>>,
}

impl SelectedComparison {
    fn invalidate(&mut self) {
        self.result = None;
        self.request = None;
        self.pending = None;
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn is_running(&self) -> bool {
        self.request.is_some() || self.pending.is_some() || self.job.is_some()
    }

    fn render(&mut self, generation: u64) {
        if let Some(job) = &self.job {
            let result = match job.reply.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("Comparison worker stopped.".into())),
            };
            if let Some(result) = result {
                if job.generation == generation && !job.cancel.load(Ordering::Relaxed) {
                    self.result = Some(result);
                }
                self.job = None;
            }
        }
        if let Some(job) = &self.job {
            minrow(8.0, || {
                textc(on_secondary_container(), "Comparing the quoted edit");
                if button_secondary("Cancel comparison").show().clicked {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            });
        } else if self.is_running() {
            textc(on_secondary_container(), "Waiting for a city snapshot");
        }
        match &self.result {
            Some(Ok(report)) => {
                textc(on_secondary_container(), format!(
                    "Driving arrivals · {} to {}",
                    report.control.journeys_by_mode["car"].completed,
                    report.treatment.journeys_by_mode["car"].completed,
                ));
                textc(on_secondary_container(), format!(
                    "Peak vehicles stopped for five game minutes · {} to {}",
                    report.control.peak_stationary, report.treatment.peak_stationary,
                ));
            }
            Some(Err(err)) => textc(goryak::error(), err.clone()),
            None => {}
        }
    }
}

pub(crate) fn submit_comparison_capture(uiw: &UiWorld, driver: Option<&mut SimThread>) {
    let mut state = uiw.write::<TrafficRepairUi>();
    let selected = &mut state.selected_comparison;
    let Some(edit) = selected.request.take() else { return };
    let Some(driver) = driver else {
        selected.result = Some(Err("City comparison is available in local play.".into()));
        return;
    };
    let capture = Request::CaptureComparison {
        revision: crate::session::snapshot_revision(),
        limit: scenarios::traffic_repair::COMPARISON_CAPTURE_LIMIT,
    };
    match driver.submit(capture) {
        Ok(id) => selected.pending = Some((id, edit)),
        Err(_) => selected.result = Some(Err("The simulation could not queue a city snapshot.".into())),
    }
}

pub(crate) fn receive_comparison_capture(
    uiw: &UiWorld, id: BatchId, captured: Result<SlotCapture, String>,
) {
    let generation = uiw.read::<SessionState>().generation;
    let mut state = uiw.write::<TrafficRepairUi>();
    let selected = &mut state.selected_comparison;
    if !selected.pending.as_ref().is_some_and(|(request, _)| *request == id) {
        return;
    }
    let (_, edit) = selected.pending.take().unwrap();
    if id.generation != generation {
        return;
    }
    match captured.and_then(|capture| ComparisonJob::spawn(generation, move |cancelled| {
        compare_captured_city(capture, &edit, cancelled).map_err(|err| err.to_string())
    }).map_err(|err| err.to_string())) {
        Ok(job) => selected.job = Some(job),
        Err(err) => selected.result = Some(Err(err)),
    }
}

impl Comparison {
    fn poll(&mut self, generation: u64) {
        let Some(job) = &self.current else { return };
        let result = match job.reply.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("Comparison worker stopped.".to_owned()),
        };
        if job.generation == generation && !job.cancel.load(Ordering::Relaxed) {
            self.result = Some(result);
        }
        self.current = None;
    }

    fn start(&mut self, generation: u64) {
        if self.current.is_some() {
            return;
        }
        self.result = None;
        match ComparisonJob::start(generation) {
            Ok(job) => self.current = Some(job),
            Err(error) => self.result = Some(Err(error.to_string())),
        }
    }

    fn render(&mut self, generation: u64, can_start: bool) {
        self.poll(generation);
        mincolumn(4.0, || {
            textc(on_primary_container(), "Matched corridor comparison");
            match &self.current {
                Some(job) if job.cancel.load(Ordering::Relaxed) => {
                    textc(on_secondary_container(), "Stopping comparison");
                }
                Some(job) => {
                    textc(
                        on_secondary_container(),
                        "Comparing the built-in tagged corridor",
                    );
                    if button_secondary("Cancel comparison").show().clicked {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                }
                None => {
                    if can_start && button_secondary("Compare built-in corridor").show().clicked {
                        self.start(generation);
                    }
                }
            }
            if let Some(result) = &self.result {
                match result {
                    Ok(comparison) => {
                        for (cohort, label) in [
                            (Cohort::Person, "Person arrivals"),
                            (Cohort::Freight, "Freight units unloaded"),
                        ] {
                            let stock = &comparison.primary.control[&cohort];
                            let edited = &comparison.primary.treatment[&cohort];
                            let minutes =
                                |ticks: u64| ticks as f64 / prototypes::TICKS_PER_MINUTE as f64;
                            textc(
                                on_secondary_container(),
                                format!(
                                    "{label} · {} to {} · p95 offered delay {:.1} to {:.1} game min",
                                    stock.credited,
                                    edited.credited,
                                    minutes(stock.p95_delay_ticks),
                                    minutes(edited.p95_delay_ticks),
                                ),
                            );
                        }
                        textc(
                            on_primary_container(),
                            format!(
                                "{} · {}",
                                if comparison.primary.passes_improvement {
                                    "Delay target passed"
                                } else {
                                    "Delay target missed"
                                },
                                if comparison.tail.preserves_credit {
                                    "Tail arrivals and unloads preserved"
                                } else {
                                    "Tail arrivals or unloads fell"
                                },
                            ),
                        );
                    }
                    Err(error) => textc(goryak::error(), error.clone()),
                }
            }
        });
    }
}

#[cfg(test)]
pub(crate) fn test_road_upgrade(sim: &Simulation) -> (RoadID, WorldCommand) {
    use geom::vec3;
    use simulation::map::{LanePatternBuilder, MapProject, ProjectKind};

    let road = {
        let mut map = sim.write::<Map>();
        let endpoint = |x| {
            let xy = geom::vec2(x, 180.0);
            let z = map.environment().height(xy).unwrap();
            MapProject {
                pos: vec3(x, 180.0, z),
                kind: ProjectKind::Ground,
            }
        };
        let from = endpoint(120.0);
        let to = endpoint(280.0);
        map.make_connection(from, to, None, &LanePatternBuilder::default().build())
            .unwrap()
            .1
    };
    let map = sim.map();
    let change = speed_upgrade(map.get(road).unwrap(), &map, 2.0);
    drop(map);
    let request = road_request(sim, road, change).unwrap();
    (road, Quote::new(sim, request).unwrap().command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p7_native_builtin_comparison_matches_headless() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();

        let job = ComparisonJob::start(7).unwrap();
        let native = job
            .reply
            .recv_timeout(Duration::from_secs(120))
            .expect("native comparison worker timed out")
            .expect("native comparison failed");
        let headless = compare_builtin_repair(FixtureArm::Repair, &AtomicBool::new(false))
            .expect("headless comparison failed");
        assert_eq!(native, headless);
        assert!(native.primary.passes_improvement);
        assert!(native.tail.preserves_credit);
    }

    #[test]
    fn p7_native_quoted_city_comparison_uses_worker_capture() {
        use crate::sim_thread::ReplyKind;

        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let sim = Simulation::new_authored_flat(1, 6042).unwrap();
        let (_, edit) = test_road_upgrade(&sim);
        let expected = sim.capture_slot(crate::session::snapshot_revision()).unwrap();
        let uiw = UiWorld::with_defaults();
        uiw.write::<SessionState>().generation = 7;
        uiw.write::<TrafficRepairUi>().selected_comparison.request = Some(edit.clone());
        let mut driver = SimThread::start(sim, Simulation::schedule(), 7, None);
        submit_comparison_capture(&uiw, Some(&mut driver));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(reply) = driver.poll_reply() {
                if let ReplyKind::Captured(capture) = reply.kind {
                    receive_comparison_capture(&uiw, reply.id, capture.map(|(capture, _)| capture));
                    break;
                }
            }
            assert!(Instant::now() < deadline, "missing comparison capture");
            std::thread::sleep(Duration::from_millis(2));
        }
        let job = uiw.write::<TrafficRepairUi>().selected_comparison.job.take().unwrap();
        let native = job.reply.recv_timeout(Duration::from_secs(120)).unwrap().unwrap();
        let headless = compare_captured_city(expected, &edit, &AtomicBool::new(false)).unwrap();
        assert_eq!(native, headless);
    }

    #[test]
    fn p7_receipt_confirmation_queues_once_and_discards_old_generation() {
        scenarios::ensure_repo_root_cwd();
        crate::init::init();
        let mut sim = Simulation::new_game(&simulation::NewGameOptions {
            preset: simulation::MapPreset::Flat,
            ..Default::default()
        })
        .unwrap();
        let uiw = UiWorld::with_defaults();
        *uiw.write::<crate::session::SessionScreen>() = crate::session::SessionScreen::Playing;
        uiw.write::<SessionState>().generation = 7;
        let (road, command) = test_road_upgrade(&sim);
        let target = EditTarget::Road(road);
        let WorldCommand::MapUpgradeRoad {
            expected, change, ..
        } = command
        else {
            unreachable!()
        };
        {
            let mut state = uiw.write::<TrafficRepairUi>();
            state.select(Some(target));
            state.request(
                &sim,
                target,
                Ok(RoadEditRequest::Upgrade {
                    road,
                    expected,
                    change,
                }),
            );
            state.confirm(&sim, &uiw);
            state.confirm(&sim, &uiw);
        }
        let commands = std::mem::take(&mut *uiw.commands());
        assert_eq!(commands.as_ref().len(), 1);
        let batch = BatchId {
            generation: 7,
            sequence: 13,
        };
        assert!(worker_batch_queued(&uiw, &commands));
        bind_worker_batch(&uiw, batch);
        sim.apply_commands(commands.iter());
        let mut receipt = TrafficEditReceipt::from_outcome(
            7,
            sim.get_tick(),
            None,
            0,
            true,
            0,
            &commands.as_ref()[0],
            &sim.read::<CommandOutcomes>(),
        )
        .unwrap();
        assert!(
            matches!(&receipt.result, Ok(TrafficEditResult::Road(e)) if e.road == road && e.status == RoadEditStatus::Applied)
        );
        let mut state = uiw.write::<TrafficRepairUi>();
        receipt.generation = 6;
        state.receive(std::slice::from_ref(&receipt), Some(batch));
        assert!(state.pending.is_some() && state.receipt.is_none());
        receipt.generation = 7;
        receipt.command_index = 1;
        state.receive(std::slice::from_ref(&receipt), Some(batch));
        assert!(state.pending.is_some() && state.receipt.is_none());
        receipt.command_index = 0;
        state.receive(&[receipt], Some(batch));
        assert!(state.pending.is_none() && state.receipt.is_some());
        drop(state);
        reset(&uiw, 8);
        assert!(uiw.read::<TrafficRepairUi>().receipt.is_none());
    }
}

#[derive(serde::Serialize)]
pub(crate) struct TrafficRepairScriptState {
    pub road: Option<RoadID>,
    pub lane: Option<LaneID>,
    pub junction: Option<IntersectionID>,
    pub legal_turns: usize,
    pub speed_limit: Option<f32>,
    pub quote_cost: Option<i64>,
    pub pending: bool,
    pub applied: bool,
    pub debit: Option<i64>,
    pub outcome_frame: Option<u64>,
    pub outcome_sequence: Option<u64>,
    pub outcome_generation: Option<u64>,
    pub error: Option<String>,
    pub queue_coverage: Option<String>,
    pub stop_line_vehicles: Option<usize>,
    pub requested_turn: Option<TurnID>,
    pub hold_reason: Option<&'static str>,
    pub parked: Option<usize>,
    pub parking_reserved: Option<usize>,
    pub parking_free: Option<usize>,
    pub comparison_passed: Option<bool>,
}

impl TrafficRepairUi {
    pub(crate) fn completed_comparison(&self) -> Option<&RepairComparison> {
        self.comparison.result.as_ref()?.as_ref().ok()
    }

    pub(crate) fn script_state(&self, sim: &Simulation) -> TrafficRepairScriptState {
        let map = sim.map();
        let road = self.target.and_then(|target| match target {
            EditTarget::Road(id) => Some(id),
            EditTarget::Junction(_) => None,
        });
        let junction = self.target.and_then(|target| match target {
            EditTarget::Junction(id) => Some(id),
            EditTarget::Road(_) => None,
        });
        let receipt = self
            .receipt
            .as_ref()
            .filter(|receipt| Some(receipt.key.target) == self.target);
        let success = receipt.and_then(|receipt| receipt.result.as_ref().ok());
        let (applied, debit) = match success {
            Some(TrafficEditResult::Road(result)) => (
                result.status == RoadEditStatus::Applied,
                Some(result.debit.inner()),
            ),
            Some(TrafficEditResult::Junction(result)) => {
                (result.status == RoadEditStatus::Applied, Some(0))
            }
            None => (false, None),
        };
        let sample = self.sample.as_ref();
        let parking = sample.and_then(|sample| sample.parking);
        TrafficRepairScriptState {
            road,
            lane: self.selected_lane,
            junction,
            legal_turns: junction.and_then(|id| map.get(id)).map_or(0, |junction| {
                junction
                    .turns()
                    .filter(|turn| turn.kind == TurnKind::Driving)
                    .count()
            }),
            speed_limit: self
                .selected_lane
                .and_then(|id| map.get(id))
                .map(|lane| lane.speed_limit),
            quote_cost: self.quote.as_ref().map(|quote| match &quote.preview {
                RoadEditPreview::Upgrade(quote) => quote.cost.inner(),
                RoadEditPreview::JunctionRules(quote) => quote.cost.inner(),
            }),
            pending: self.pending.is_some(),
            applied,
            debit,
            outcome_frame: receipt.map(|receipt| receipt.frame),
            outcome_sequence: receipt.map(|receipt| receipt.sequence),
            outcome_generation: receipt.map(|receipt| receipt.generation),
            error: self
                .error
                .as_ref()
                .filter(|(target, _)| Some(*target) == self.target)
                .map(|(_, message)| message.clone())
                .or_else(|| {
                    receipt.and_then(|receipt| {
                        receipt.result.as_ref().err().map(|&reason| {
                            command_message(receipt.key.target.command_name(), reason).into_owned()
                        })
                    })
                }),
            queue_coverage: sample.map(|sample| format!("{:?}", sample.queue.coverage)),
            stop_line_vehicles: sample.map(|sample| sample.queue.stop_line.len()),
            requested_turn: sample.and_then(|sample| sample.queue.requested_turn),
            hold_reason: sample.and_then(|sample| sample.queue.reason.map(hold_label)),
            parked: parking.map(|parking| parking.parked),
            parking_reserved: parking.map(|parking| parking.reserved),
            parking_free: parking.map(|parking| parking.free),
            comparison_passed: self
                .comparison
                .result
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .map(|report| report.primary.passes_improvement && report.tail.preserves_credit),
        }
    }

    pub(crate) fn confirm_script(&mut self, sim: &Simulation, uiw: &UiWorld) -> Result<(), String> {
        self.confirm(sim, uiw);
        if self.pending.is_some() {
            Ok(())
        } else {
            Err("No authorizable traffic quote was submitted".into())
        }
    }

    pub(crate) fn start_script_comparison(&mut self, generation: u64) {
        self.comparison.start(generation);
    }

    pub(crate) fn script_comparison_ready(&mut self, generation: u64) -> Result<bool, String> {
        self.comparison.poll(generation);
        match self.comparison.result.as_ref() {
            Some(Ok(_)) => Ok(true),
            Some(Err(error)) => Err(error.clone()),
            None if self.comparison.current.is_some() => Ok(false),
            None => Err("The comparison is no longer running".into()),
        }
    }
}

pub(crate) fn script_preview_road(
    uiw: &UiWorld,
    sim: &Simulation,
    anchor: [f32; 2],
    denied: bool,
) -> Result<(), String> {
    use crate::gui::{selectable::pick_road, InspectedBuilding, InspectedEntity, Tool};
    let map = sim.map();
    let point = geom::Vec2::from(anchor);
    let mut roads: Vec<_> = map
        .roads()
        .values()
        .filter(|road| road.lanes_iter().any(|(_, kind)| kind == LaneKind::Driving))
        .collect();
    roads.sort_by(|a, b| {
        let score = |road: &Road| road.interfaced_points().middle().xy().distance2(point);
        score(a).total_cmp(&score(b)).then_with(|| a.id.cmp(&b.id))
    });
    let (road, focus, quote) = roads
        .into_iter()
        .take(128)
        .find_map(|road| {
            let change = if denied {
                change_width(&road.pattern(map.lanes()), true)?
            } else {
                speed_upgrade(road, &map, 2.0)
            };
            let quote =
                road_request(sim, road.id, change).and_then(|request| Quote::new(sim, request));
            let eligible = if denied {
                matches!(
                    &quote,
                    Err(Rejection::LandDenied(_)
                        | Rejection::WaterConnection(_)
                        | Rejection::BusyConflict)
                )
            } else {
                quote.as_ref().is_ok_and(Quote::committable)
            };
            if !eligible {
                return None;
            }
            let focus = road.interfaced_points().middle();
            let terrain = map
                .environment()
                .height(focus.xy())
                .map(|z| focus.xy().z(z));
            let ray = geom::Ray3::new(focus.up(80.0), -geom::Vec3::Z);
            (pick_road(&map, ray, terrain) == Some(road.id)).then_some((road.id, focus, quote))
        })
        .ok_or_else(|| {
            format!(
                "No {} visible road near {anchor:?}",
                if denied { "denied" } else { "editable" }
            )
        })?;
    *uiw.write::<Tool>() = Tool::Hand;
    uiw.write::<InspectedRoad>().0 = Some(road);
    uiw.write::<InspectedBuilding>().e = None;
    uiw.write::<InspectedEntity>().e = None;
    frame_traffic_detail(uiw, focus);
    let target = EditTarget::Road(road);
    let mut state = uiw.write::<TrafficRepairUi>();
    state.select(Some(target));
    state.receipt = None;
    state.error = None;
    match quote {
        Ok(quote) => state.quote = Some(quote),
        Err(reason) => {
            state.error = Some((
                target,
                command_message(target.command_name(), reason).into_owned(),
            ))
        }
    }
    Ok(())
}

pub(crate) fn script_select_junction(uiw: &UiWorld, sim: &Simulation) -> Result<(), String> {
    use crate::gui::{
        roadeditor::{IntersectionComponent, RoadEditorResource},
        Tool,
    };
    let road = uiw
        .read::<InspectedRoad>()
        .0
        .ok_or("Inspect a road first")?;
    let map = sim.map();
    let road = map.get(road).ok_or("The inspected road no longer exists")?;
    let junction = [road.dst, road.src]
        .into_iter()
        .filter_map(|id| map.get(id))
        .find(|intersection| {
            intersection
                .turns()
                .any(|turn| turn.kind == TurnKind::Driving)
        })
        .ok_or("The inspected road has no live driving turns")?;
    *uiw.write::<Tool>() = Tool::RoadEditor;
    uiw.write::<RoadEditorResource>().inspect = Some(IntersectionComponent {
        id: junction.id,
        turn_policy: junction.turn_policy,
        light_policy: junction.light_policy,
    });
    frame_traffic_detail(uiw, junction.pos);
    Ok(())
}

fn frame_traffic_detail(uiw: &UiWorld, focus: geom::Vec3) {
    let mut orbit = uiw.camera_mut();
    // Keep the selected geometry beside the centered inspector in scripted captures.
    let side = orbit.camera.yaw.vec2().perpendicular().z0();
    orbit.targetpos = focus + side * 95.0;
    orbit.camera.pos = orbit.targetpos;
    orbit.targetdist = 155.0;
    orbit.camera.dist = 155.0;
}
