//! Pure Wave 0 scoring for matched offer ledgers. The live comparator supplies
//! these observations from tagged person arrivals and manifest unload events.

use prototypes::TICKS_PER_MINUTE;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub use crate::builtin::traffic_repair::{
    prepare_repair_fixture, FixtureArm, FixtureOffer, RepairFixture, ScheduledOffer,
};

pub const OD_DELAY_LIMIT_TICKS: u64 = 35 * TICKS_PER_MINUTE;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureClass {
    Diagnostic,
    Release,
}

/// Fingerprints computed by staging from the bytes and the run configuration.
/// Schedule hashes include the options, seed, cohort/anchors and score horizons.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureFingerprint {
    pub source_commit: String,
    pub run_source_commit: String,
    pub prototype_tree: String,
    pub base_mod_tree: String,
    pub asset_tree: String,
    pub build_profile: String,
    pub features: Vec<String>,
    pub snapshot_schema: String,
    pub snapshot_revision: String,
    pub snapshot_sha256: String,
    pub metadata_sha256: String,
    pub replay_sha256: Option<String>,
    pub schedule_sha256: String,
    pub initial_elapsed_tick: u64,
    pub workers: u16,
    pub opening: Option<OpeningFingerprint>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpeningFingerprint {
    pub manifest_sha256: String,
    pub report_sha256: String,
    pub population: u64,
    pub passed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureRejection {
    WrongClass,
    MissingOpening,
    MalformedFingerprint,
    FingerprintMismatch,
}

/// Check a predeclared pin before a staged run takes its first scored step.
pub fn verify_fixture(
    class: FixtureClass,
    requested: FixtureClass,
    expected: &FixtureFingerprint,
    staged: &FixtureFingerprint,
) -> Result<(), FixtureRejection> {
    if class != requested {
        return Err(FixtureRejection::WrongClass);
    }
    if class == FixtureClass::Release
        && !expected
            .opening
            .as_ref()
            .is_some_and(|opening| opening.passed && opening.population > 0)
    {
        return Err(FixtureRejection::MissingOpening);
    }
    if !expected.valid() {
        return Err(FixtureRejection::MalformedFingerprint);
    }
    if expected != staged {
        return Err(FixtureRejection::FingerprintMismatch);
    }
    Ok(())
}

impl FixtureFingerprint {
    fn valid(&self) -> bool {
        let hex = |text: &str, len| {
            text.len() == len
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        };
        [
            &self.source_commit,
            &self.run_source_commit,
            &self.prototype_tree,
            &self.base_mod_tree,
            &self.asset_tree,
        ]
        .into_iter()
        .all(|pin| hex(pin, 40))
            && [
                &self.snapshot_sha256,
                &self.metadata_sha256,
                &self.schedule_sha256,
            ]
            .into_iter()
            .all(|pin| hex(pin, 64))
            && self.replay_sha256.as_ref().is_none_or(|pin| hex(pin, 64))
            && self.opening.as_ref().is_none_or(|opening| {
                hex(&opening.manifest_sha256, 64) && hex(&opening.report_sha256, 64)
            })
            && hex(&self.snapshot_revision, 32)
            && !self.build_profile.is_empty()
            && !self.snapshot_schema.is_empty()
            && self.workers > 0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cohort {
    Person,
    Freight,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    pub tag: u32,
    pub cohort: Cohort,
    pub directed_group: String,
    pub destination: String,
    pub tick: u64,
    pub free_flow_ticks: u64,
    pub units: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub tag: u32,
    pub admitted_at: Option<u64>,
    pub terminal: Option<Terminal>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Terminal {
    Completed {
        tick: u64,
        destination: String,
        units: u64,
    },
    Rejected {
        tick: u64,
    },
    Cancelled {
        tick: u64,
    },
    Unreconciled {
        tick: u64,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CohortScore {
    pub offered: usize,
    pub admitted: usize,
    pub credited: u64,
    pub attrition: usize,
    pub p95_delay_ticks: u64,
    pub directed_p95_delay_ticks: BTreeMap<String, u64>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct MatchedScore {
    pub control: BTreeMap<Cohort, CohortScore>,
    pub treatment: BTreeMap<Cohort, CohortScore>,
    pub passes_improvement: bool,
    pub preserves_credit: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ScoreError {
    InvalidOffer(u32),
    InvalidObservation(u32),
    MissingCohort,
    OfferSetMismatch,
    CreditOverflow,
}

#[derive(Clone, Copy)]
struct Sample {
    admitted: bool,
    credit: u64,
    attrition: bool,
    delay: u64,
}

type Arm = BTreeMap<Cohort, CohortScore>;

/// Score both ledgers at one inclusive elapsed cutoff against the same offers.
/// One observation per tag is required, including tags waiting for admission.
pub fn score_matched_offers(
    offers: &[Offer],
    control: &[Observation],
    treatment: &[Observation],
    cutoff: u64,
) -> Result<MatchedScore, ScoreError> {
    let control_by_tag = by_tag(control, offers.len())?;
    let treatment_by_tag = by_tag(treatment, offers.len())?;
    let mut control_samples = Vec::with_capacity(offers.len());
    let mut treatment_samples = Vec::with_capacity(offers.len());
    let mut seen = std::collections::BTreeSet::new();
    for offer in offers {
        if !seen.insert(offer.tag) {
            return Err(ScoreError::OfferSetMismatch);
        }
        if offer.tick > cutoff
            || offer.destination.is_empty()
            || offer.directed_group.is_empty()
            || offer.units == 0
            || (offer.cohort == Cohort::Person && offer.units != 1)
        {
            return Err(ScoreError::InvalidOffer(offer.tag));
        }
        let stock = control_by_tag
            .get(&offer.tag)
            .ok_or(ScoreError::OfferSetMismatch)?;
        let repair = treatment_by_tag
            .get(&offer.tag)
            .ok_or(ScoreError::OfferSetMismatch)?;
        control_samples.push((offer, sample(offer, stock, cutoff)?));
        treatment_samples.push((offer, sample(offer, repair, cutoff)?));
    }
    let control = summarize(&control_samples)?;
    let treatment = summarize(&treatment_samples)?;
    let passes_improvement = [Cohort::Person, Cohort::Freight].into_iter().all(|cohort| {
        let stock = &control[&cohort];
        let repair = &treatment[&cohort];
        repair.credited > stock.credited
            && repair.attrition <= stock.attrition
            && stock.p95_delay_ticks > 0
            && percentage_at_most(repair.p95_delay_ticks, stock.p95_delay_ticks, 90)
            && stock
                .directed_p95_delay_ticks
                .iter()
                .all(|(group, &ticks)| {
                    percentage_at_most(repair.directed_p95_delay_ticks[group], ticks, 102)
                })
    });
    let preserves_credit =
        control_samples
            .iter()
            .zip(&treatment_samples)
            .all(|((_, stock), (_, repair))| {
                repair.credit >= stock.credit && (!repair.attrition || stock.attrition)
            });
    Ok(MatchedScore {
        control,
        treatment,
        passes_improvement,
        preserves_credit,
    })
}

fn by_tag(
    observations: &[Observation],
    offer_count: usize,
) -> Result<BTreeMap<u32, &Observation>, ScoreError> {
    let by_tag: BTreeMap<_, _> = observations.iter().map(|row| (row.tag, row)).collect();
    if observations.len() != offer_count || by_tag.len() != offer_count {
        return Err(ScoreError::OfferSetMismatch);
    }
    Ok(by_tag)
}

fn sample(offer: &Offer, row: &Observation, cutoff: u64) -> Result<Sample, ScoreError> {
    let invalid = ScoreError::InvalidObservation(offer.tag);
    if row.admitted_at.is_some_and(|tick| tick < offer.tick) {
        return Err(invalid);
    }
    let mut sample = Sample {
        admitted: row.admitted_at.is_some_and(|tick| tick <= cutoff),
        credit: 0,
        attrition: false,
        delay: OD_DELAY_LIMIT_TICKS + 1,
    };
    let Some(terminal) = &row.terminal else {
        return Ok(sample);
    };
    let tick = match terminal {
        Terminal::Completed {
            tick,
            destination,
            units,
        } => {
            if destination != &offer.destination
                || *units == 0
                || *units > offer.units
                || row.admitted_at.is_none()
            {
                return Err(invalid);
            }
            if *tick <= cutoff {
                sample.credit = *units;
                sample.delay = tick
                    .saturating_sub(offer.tick)
                    .saturating_sub(offer.free_flow_ticks);
            }
            *tick
        }
        Terminal::Rejected { tick } => {
            if row.admitted_at.is_some() {
                return Err(invalid);
            }
            sample.attrition = *tick <= cutoff;
            *tick
        }
        Terminal::Cancelled { tick } | Terminal::Unreconciled { tick } => {
            sample.attrition = *tick <= cutoff;
            *tick
        }
    };
    if tick < row.admitted_at.unwrap_or(offer.tick) {
        return Err(invalid);
    }
    Ok(sample)
}

fn summarize(samples: &[(&Offer, Sample)]) -> Result<Arm, ScoreError> {
    let mut result = BTreeMap::new();
    for cohort in [Cohort::Person, Cohort::Freight] {
        let mut score = CohortScore::default();
        let mut pooled = Vec::new();
        let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for (offer, sample) in samples.iter().filter(|(offer, _)| offer.cohort == cohort) {
            score.offered += 1;
            score.admitted += usize::from(sample.admitted);
            score.credited = score
                .credited
                .checked_add(sample.credit)
                .ok_or(ScoreError::CreditOverflow)?;
            score.attrition += usize::from(sample.attrition);
            pooled.push(sample.delay);
            groups
                .entry(offer.directed_group.clone())
                .or_default()
                .push(sample.delay);
        }
        if pooled.is_empty() {
            return Err(ScoreError::MissingCohort);
        }
        score.p95_delay_ticks = p95(&mut pooled);
        score.directed_p95_delay_ticks = groups
            .into_iter()
            .map(|(group, mut delays)| (group, p95(&mut delays)))
            .collect();
        result.insert(cohort, score);
    }
    Ok(result)
}

fn percentage_at_most(value: u64, baseline: u64, pct: u128) -> bool {
    u128::from(value) * 100 <= u128::from(baseline) * pct
}

fn p95(samples: &mut [u64]) -> u64 {
    samples.sort_unstable();
    // Nearest rank, ceil(0.95*n), expressed as an index.
    samples[samples.len() - 1 - samples.len() / 20]
}
