use super::{score_matched_offers, Cohort, MatchedScore, Observation, Offer, ScoreError};
use crate::builtin::traffic_repair::{
    prepare_repair_fixture_initialized, run_repair_arm, FixtureArm, FixtureOffer, ScheduledOffer,
};
use serde::Serialize;
use simulation::diagnostics::elapsed_tick;
use simulation::map::road_edit::{EditReceipt, JunctionEditReceipt, RoadEditStatus};
use simulation::world_command::{CommandOutcomes, WorldCommand};
use simulation::{Simulation, SnapshotRevision};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

const SNAPSHOT_LIMIT: usize = 32 * 1024 * 1024;
const MAX_OFFERS: usize = 1024;
const MAX_BATCHES: usize = 256;
const MAX_BATCH_COMMANDS: usize = 256;
const MAX_TICKS: u64 = 144_000;

/// Owned checkpoint bytes produced from an already loaded, registry-validated city.
pub struct VerifiedFixtureSnapshot {
    bytes: Vec<u8>,
    elapsed_tick: u64,
}

impl VerifiedFixtureSnapshot {
    pub fn capture(sim: &Simulation) -> Result<Self, ComparisonError> {
        Ok(Self {
            bytes: snapshot_bytes(sim)?,
            elapsed_tick: elapsed_tick(sim).0,
        })
    }

    fn load(&self) -> Result<Simulation, ComparisonError> {
        load_snapshot(&self.bytes)
    }
}

pub(crate) fn snapshot_bytes(sim: &Simulation) -> Result<Vec<u8>, ComparisonError> {
    sim.capture_slot_with_limit(SnapshotRevision::new(1), SNAPSHOT_LIMIT)
        .and_then(|capture| capture.encode_with_limit(SNAPSHOT_LIMIT))
        .map(|slot| slot.into_snapshot_bytes())
        .map_err(|err| ComparisonError::Snapshot(err.to_string()))
}

pub(crate) fn load_snapshot(bytes: &[u8]) -> Result<Simulation, ComparisonError> {
    Simulation::load_compressed_snapshot(bytes, SNAPSHOT_LIMIT)
        .map_err(|err| ComparisonError::Snapshot(err.to_string()))
}

pub struct AbsoluteSchedule<'a> {
    pub scheduled_offers: &'a [ScheduledOffer],
    pub scoring_offers: &'a [Offer],
    pub batches: &'a [ScheduledBatch<'a>],
    pub primary_cutoff: u64,
    pub tail_cutoff: u64,
}

/// A paused batch runs at the end of the preceding elapsed step, before the
/// named step. Consecutive paused batches keep their order in the slice.
pub struct ScheduledBatch<'a> {
    pub tick: u64,
    pub advances: bool,
    pub commands: &'a [WorldCommand],
}

#[derive(Clone, Copy)]
pub struct ComparisonRequest<'a> {
    pub repair_at: u64,
    pub repair: &'a WorldCommand,
    pub resume_at: Option<u64>,
    pub cancelled: &'a AtomicBool,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RepairComparison {
    pub primary: MatchedScore,
    pub tail: MatchedScore,
    pub receipt: ComparisonReceipt,
    pub control: ComparisonArm,
    pub treatment: ComparisonArm,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ComparisonArm {
    pub observations: Vec<Observation>,
    pub batches: Vec<ComparisonBatchOutcome>,
    pub repaired_road_tags: Vec<u32>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct ComparisonBatchOutcome {
    pub tick: u64,
    pub advances: bool,
    pub accepted: Vec<usize>,
    pub rejections: BTreeMap<usize, Vec<String>>,
    pub links: BTreeMap<usize, Vec<(usize, Option<String>)>>,
    pub batch_rejection: Option<String>,
}

impl ComparisonBatchOutcome {
    pub(crate) fn read(tick: u64, advances: bool, count: usize, sim: &Simulation) -> Self {
        let outcomes = sim.read::<CommandOutcomes>();
        Self {
            tick,
            advances,
            accepted: outcomes.accepted.clone(),
            rejections: (0..count)
                .filter_map(|index| {
                    let errors = outcomes.rejections_for(index);
                    (!errors.is_empty()).then(|| {
                        (
                            index,
                            errors
                                .iter()
                                .map(|(command, reason)| format!("{command}: {reason:?}"))
                                .collect(),
                        )
                    })
                })
                .collect(),
            links: (0..count)
                .filter_map(|index| {
                    let links = outcomes.link_outcomes(index);
                    (!links.is_empty()).then(|| {
                        (
                            index,
                            links
                                .iter()
                                .map(|(link, result)| {
                                    (*link, result.err().map(|reason| format!("{reason:?}")))
                                })
                                .collect(),
                        )
                    })
                })
                .collect(),
            batch_rejection: outcomes
                .batch_rejection()
                .map(|reason| format!("{reason:?}")),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ComparisonReceipt {
    Road(EditReceipt),
    Junction(JunctionEditReceipt),
}

impl ComparisonReceipt {
    pub(crate) fn read(
        sim: &Simulation,
        command: &WorldCommand,
        index: usize,
    ) -> Result<Self, ComparisonError> {
        let outcomes = sim.read::<CommandOutcomes>();
        let denied = |message| ComparisonError::RepairDenied(message);
        match command {
            WorldCommand::MapUpgradeRoad { .. } => match outcomes.road_edit(index) {
                Some(Ok(receipt)) if receipt.status == RoadEditStatus::Applied => {
                    Ok(Self::Road(receipt.clone()))
                }
                result => Err(denied(format!("{result:?}"))),
            },
            WorldCommand::MapSetJunctionRules { .. } => match outcomes.junction_edit(index) {
                Some(Ok(receipt)) if receipt.status == RoadEditStatus::Applied => {
                    Ok(Self::Junction(receipt.clone()))
                }
                result => Err(denied(format!("{result:?}"))),
            },
            _ => Err(ComparisonError::InvalidSchedule(
                "the candidate must be a quoted road or junction edit",
            )),
        }
    }
}

impl Serialize for ComparisonReceipt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "target", rename_all = "snake_case")]
        enum Applied<'a> {
            Road {
                id: simulation::map::RoadID,
                stamp: simulation::map::road_edit::RoadEditStamp,
                quote: simulation::map::road_edit::RoadQuote,
                debit_cents: i64,
                retained_lanes: &'a [simulation::map::LaneID],
                changed_lane_costs: usize,
            },
            Junction {
                id: simulation::map::IntersectionID,
                stamp: simulation::map::road_edit::JunctionEditStamp,
                quote: simulation::map::road_edit::RoadQuote,
            },
        }
        match self {
            Self::Road(receipt) => Applied::Road {
                id: receipt.road,
                stamp: receipt.stamp,
                quote: receipt.quote,
                debit_cents: receipt.debit.cents(),
                retained_lanes: &receipt.retained_lanes,
                changed_lane_costs: receipt.changed_lane_costs,
            },
            Self::Junction(receipt) => Applied::Junction {
                id: receipt.junction,
                stamp: receipt.stamp,
                quote: receipt.quote,
            },
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ComparisonError {
    Cancelled,
    Snapshot(String),
    Setup(String),
    InvalidSchedule(&'static str),
    RepairDenied(String),
    Observation(String),
    Score(ScoreError),
}

impl fmt::Display for ComparisonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("Traffic comparison was cancelled"),
            Self::Snapshot(error) => write!(f, "Traffic comparison snapshot failed ({error})"),
            Self::Setup(error) => write!(f, "Traffic comparison setup failed ({error})"),
            Self::InvalidSchedule(error) => {
                write!(f, "Traffic comparison schedule is invalid ({error})")
            }
            Self::RepairDenied(error) => {
                write!(f, "Traffic comparison repair was denied ({error})")
            }
            Self::Observation(error) => {
                write!(f, "Traffic comparison observation failed ({error})")
            }
            Self::Score(error) => write!(f, "Traffic comparison scoring failed ({error:?})"),
        }
    }
}

impl std::error::Error for ComparisonError {}

impl From<String> for ComparisonError {
    fn from(error: String) -> Self {
        Self::Observation(error)
    }
}

impl From<&str> for ComparisonError {
    fn from(error: &str) -> Self {
        Self::Observation(error.to_owned())
    }
}

/// Run the authored workload from independent loads of an identical checkpoint.
/// Callers initialize prototypes and the simulation registry before invoking this.
pub fn compare_repair(
    snapshot: &VerifiedFixtureSnapshot,
    inputs: &AbsoluteSchedule<'_>,
    request: ComparisonRequest<'_>,
) -> Result<RepairComparison, ComparisonError> {
    check_cancelled(request.cancelled)?;
    validate_schedule(snapshot.elapsed_tick, inputs, request)?;
    let (control, _) = run_repair_arm(snapshot.load()?, inputs, request, false)?;
    check_cancelled(request.cancelled)?;
    let (treatment, receipt) = run_repair_arm(snapshot.load()?, inputs, request, true)?;
    let receipt =
        receipt.ok_or_else(|| ComparisonError::RepairDenied("missing edit receipt".into()))?;
    let score = |cutoff| {
        score_matched_offers(
            inputs.scoring_offers,
            &control.observations,
            &treatment.observations,
            cutoff,
        )
        .map_err(ComparisonError::Score)
    };
    Ok(RepairComparison {
        primary: score(inputs.primary_cutoff)?,
        tail: score(inputs.tail_cutoff)?,
        receipt,
        control,
        treatment,
    })
}

/// Compare the shipped authored fixture. Call after initialization on the main thread.
pub fn compare_builtin_repair(
    arm: FixtureArm,
    cancelled: &AtomicBool,
) -> Result<RepairComparison, ComparisonError> {
    compare_builtin_repair_resumed(arm, cancelled, false)
}

pub(super) fn compare_builtin_repair_resumed(
    arm: FixtureArm,
    cancelled: &AtomicBool,
    resume: bool,
) -> Result<RepairComparison, ComparisonError> {
    check_cancelled(cancelled)?;
    let fixture = prepare_repair_fixture_initialized(arm).map_err(ComparisonError::Setup)?;
    let snapshot = VerifiedFixtureSnapshot::capture(&fixture.simulation)?;
    let scoring_offers = scoring_offers(&fixture.schedule)?;
    let request = ComparisonRequest {
        repair_at: fixture.repair_at,
        repair: &fixture.repair,
        resume_at: resume.then_some(fixture.checkpoint + 5000),
        cancelled,
    };
    let inputs = AbsoluteSchedule {
        scheduled_offers: &fixture.schedule,
        scoring_offers: &scoring_offers,
        batches: &[],
        primary_cutoff: fixture.primary_cutoff,
        tail_cutoff: fixture.tail_cutoff,
    };
    drop(fixture.simulation);
    compare_repair(&snapshot, &inputs, request)
}

pub(crate) fn check_cancelled(cancelled: &AtomicBool) -> Result<(), ComparisonError> {
    if cancelled.load(Ordering::Relaxed) {
        Err(ComparisonError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_schedule(
    checkpoint: u64,
    inputs: &AbsoluteSchedule<'_>,
    request: ComparisonRequest<'_>,
) -> Result<(), ComparisonError> {
    let inside = |tick: u64| tick > checkpoint && tick <= inputs.tail_cutoff;
    if !inside(request.repair_at)
        || !inside(inputs.primary_cutoff)
        || request.resume_at.is_some_and(|tick| !inside(tick))
        || inputs.tail_cutoff - checkpoint > MAX_TICKS
        || !matches!(
            request.repair,
            WorldCommand::MapUpgradeRoad { .. } | WorldCommand::MapSetJunctionRules { .. }
        )
    {
        return Err(ComparisonError::InvalidSchedule(
            "invalid comparison horizon or edit",
        ));
    }
    if inputs.batches.len() > MAX_BATCHES
        || inputs.batches.iter().any(|batch| {
            !inside(batch.tick)
                || batch.commands.is_empty()
                || batch.commands.len()
                    + usize::from(batch.advances && batch.tick == request.repair_at)
                    > MAX_BATCH_COMMANDS
        })
        || inputs.batches.windows(2).any(|pair| {
            pair[0].tick > pair[1].tick || (pair[0].tick == pair[1].tick && pair[0].advances)
        })
    {
        return Err(ComparisonError::InvalidSchedule("invalid command batches"));
    }
    if inputs.scheduled_offers.len() > MAX_OFFERS
        || inputs.scheduled_offers.len() != inputs.scoring_offers.len()
        || inputs
            .scheduled_offers
            .windows(2)
            .any(|pair| (pair[0].tick, pair[0].tag) >= (pair[1].tick, pair[1].tag))
    {
        return Err(ComparisonError::InvalidSchedule(
            "invalid offer order or count",
        ));
    }
    let mut tags = BTreeSet::new();
    let mut cohorts = BTreeSet::new();
    for (scheduled, score) in inputs.scheduled_offers.iter().zip(inputs.scoring_offers) {
        let (cohort, destination, units) = offer_destination(&scheduled.offer);
        if !inside(score.tick)
            || !tags.insert(scheduled.tag)
            || scheduled.group.is_empty()
            || units == 0
            || score.tick > inputs.primary_cutoff
            || scheduled.tick != score.tick
            || scheduled.tag != score.tag
            || scheduled.group != score.directed_group
            || cohort != score.cohort
            || destination != score.destination
            || units != score.units
        {
            return Err(ComparisonError::InvalidSchedule(
                "offer schedule and scoring ledger differ",
            ));
        }
        cohorts.insert(cohort);
    }
    if cohorts.len() != 2 {
        return Err(ComparisonError::InvalidSchedule(
            "the schedule needs both people and freight",
        ));
    }
    Ok(())
}

fn offer_destination(offer: &FixtureOffer) -> (Cohort, String, u64) {
    match offer {
        FixtureOffer::Person { destination, .. } => (Cohort::Person, format!("{destination:?}"), 1),
        FixtureOffer::Freight {
            receiver, units, ..
        } => (Cohort::Freight, format!("{receiver:?}"), *units),
    }
}

fn scoring_offers(scheduled: &[ScheduledOffer]) -> Result<Vec<Offer>, ComparisonError> {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/p7_wave0.json"))
            .map_err(|err| ComparisonError::Setup(err.to_string()))?;
    let references = &manifest["matched_cohort"]["free_flow_reference"]["ticks"];
    scheduled
        .iter()
        .map(|row| {
            let (cohort, destination, units) = offer_destination(&row.offer);
            let free_flow_ticks = references[row.group].as_u64().ok_or_else(|| {
                ComparisonError::Setup(format!("missing free-flow pin for {}", row.group))
            })?;
            Ok(Offer {
                tag: row.tag,
                cohort,
                directed_group: row.group.to_owned(),
                destination,
                tick: row.tick,
                free_flow_ticks,
                units,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "comparison_tests.rs"]
mod tests;
