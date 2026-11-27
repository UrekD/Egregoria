use std::collections::{BTreeMap, BTreeSet};
use std::hash::Hasher;

use common::saveload::{Bincode, Encoder};
use prototypes::{Money, Tick, TICKS_PER_MINUTE};
use serde::{Deserialize, Serialize};

use crate::economy::{Budget, BudgetCategory, Government};
use crate::map::{BuildingID, BuildingKind, LandDenial, Map};
use crate::world_command::Rejection;
use crate::Simulation;

use super::events::{deserialize_map, elapsed_tick, fire_incident};

const JOB_LIMIT: usize = 1024;
const CLEARANCE_BUDGET: usize = 32;
const RETRY_TICKS: u64 = 5 * TICKS_PER_MINUTE;
const POLICY: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SiteRemnantCause {
    Fire,
    Decline,
    PlayerDemolition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemolitionPhase {
    WaitingForFire,
    Working,
    ClearanceBlocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClearanceBlocker {
    Land(LandDenial),
    Water,
    WorkLimit,
    Responders,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemolitionQuote {
    pub policy: u16,
    pub signature: u64,
    pub minutes: u16,
    pub price: Money,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemolitionPreview {
    pub site: BuildingID,
    pub cause: Option<SiteRemnantCause>,
    pub original_kind: BuildingKind,
    pub quote: DemolitionQuote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemolitionReceipt {
    pub site: BuildingID,
    pub quote: DemolitionQuote,
    pub charged: Money,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemolitionView {
    pub cause: SiteRemnantCause,
    pub quote: Option<DemolitionQuote>,
    pub phase: Option<DemolitionPhase>,
    pub progress_percent: u8,
    pub blocker: Option<ClearanceBlocker>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct DemolitionJob {
    quote: DemolitionQuote,
    accepted_at: Tick,
    work_started: Option<Tick>,
    next_update: Tick,
    blocked: Option<ClearanceBlocker>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct SiteRemnant {
    original_kind: BuildingKind,
    cause: SiteRemnantCause,
    job: Option<DemolitionJob>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct SiteRemnants {
    #[serde(deserialize_with = "deserialize_sites")]
    sites: BTreeMap<BuildingID, SiteRemnant>,
    #[serde(skip)]
    due: BTreeSet<(Tick, BuildingID)>,
}

impl SiteRemnants {
    pub fn blocked_jobs(&self) -> impl Iterator<Item = BuildingID> + '_ {
        self.due.iter().filter_map(|&(_, site)| {
            self.sites[&site]
                .job
                .and_then(|job| job.blocked)
                .map(|_| site)
        })
    }
}

fn deserialize_sites<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<BuildingID, SiteRemnant>, D::Error> {
    // The map's occupied site table is separately validated against these records.
    deserialize_map::<_, _, 262_144>(d)
}

pub(crate) fn decode_wave1(bytes: &[u8]) -> std::io::Result<SiteRemnants> {
    #[derive(Deserialize)]
    struct Wave1Job {
        quote: DemolitionQuote,
        accepted_at: Tick,
        work_started: Option<Tick>,
        next_update: Tick,
        blocked: Option<LandDenial>,
    }
    #[derive(Deserialize)]
    struct Wave1Remnant {
        original_kind: BuildingKind,
        cause: SiteRemnantCause,
        job: Option<Wave1Job>,
    }
    #[derive(Deserialize)]
    struct Wave1Remnants {
        #[serde(deserialize_with = "deserialize_map::<_, _, 262_144>")]
        sites: BTreeMap<BuildingID, Wave1Remnant>,
    }
    let legacy: Wave1Remnants = Bincode::decode(bytes)?;
    Ok(SiteRemnants {
        sites: legacy.sites.into_iter().map(|(site, remnant)| {
            (site, SiteRemnant {
                original_kind: remnant.original_kind,
                cause: remnant.cause,
                job: remnant.job.map(|job| DemolitionJob {
                    quote: job.quote,
                    accepted_at: job.accepted_at,
                    work_started: job.work_started,
                    next_update: job.next_update,
                    blocked: job.blocked.map(ClearanceBlocker::Land),
                }),
            })
        }).collect(),
        due: BTreeSet::new(),
    })
}

pub type DemolitionError = Rejection;

pub fn demolition_quote(
    sim: &Simulation,
    site: BuildingID,
) -> Result<DemolitionPreview, DemolitionError> {
    quote(&sim.map(), &sim.read::<SiteRemnants>(), site)
}

fn quote(
    map: &Map,
    remnants: &SiteRemnants,
    site: BuildingID,
) -> Result<DemolitionPreview, DemolitionError> {
    let building = map.get(site).ok_or(Rejection::MissingReference)?;
    let remnant = remnants.sites.get(&site);
    let cause = remnant.map(|remnant| remnant.cause);
    let original_kind = remnant.map_or(building.kind, |remnant| remnant.original_kind);
    if let Some(job) = remnant.and_then(|remnant| remnant.job) {
        return Ok(DemolitionPreview {
            site,
            cause,
            original_kind,
            quote: job.quote,
        });
    }
    let ruin = matches!(
        cause,
        Some(SiteRemnantCause::Fire | SiteRemnantCause::Decline)
    );
    let [width, length] = building.obb.axis();
    let footprint = width.mag() * length.mag();
    let area = footprint + building.zone.as_ref().map_or(0.0, |zone| zone.area);
    let extra = ((area / 400.0).ceil() as u16).saturating_sub(1).min(120);
    let minutes = if ruin { 30 } else { 60 } + extra;
    let rebuild = original_kind
        .as_goods_company()
        .and_then(prototypes::try_prototype)
        .map_or_else(
            || {
                Money::new_bucks(if original_kind == BuildingKind::House {
                    100
                } else {
                    1_000
                })
            },
            |company| company.price,
        );
    let price = Money::new_inner(
        (rebuild.inner() / if ruin { 10 } else { 5 })
            .max(Money::new_bucks(1).inner())
            .saturating_add(Money::new_bucks(i64::from(extra) * 5).inner()),
    );

    let frontage = building.connected_road.and_then(|road| map.get(road));
    let terms = Bincode::encode(&(
        POLICY,
        site,
        original_kind,
        cause,
        building.obb,
        building.height.to_bits(),
        building.door_pos,
        &building.zone,
        building.connected_road,
        frontage.map(|road| (&road.points, road.width)),
        minutes,
        price,
    ))
    .map_err(|_| Rejection::InvalidBuilding)?;
    #[allow(deprecated)]
    let mut hash = std::hash::SipHasher::new_with_keys(0, 0);
    hash.write(&terms);
    Ok(DemolitionPreview {
        site,
        cause,
        original_kind,
        quote: DemolitionQuote {
            policy: POLICY,
            signature: hash.finish(),
            minutes,
            price,
        },
    })
}

pub fn demolition_status(sim: &Simulation, site: BuildingID) -> Option<DemolitionView> {
    let remnant = *sim.read::<SiteRemnants>().sites.get(&site)?;
    let now = elapsed_tick(sim);
    let (phase, progress_percent) = if let Some(job) = remnant.job {
        if job.blocked.is_some() {
            (Some(DemolitionPhase::ClearanceBlocked), 100)
        } else if let Some(start) = job.work_started {
            let work = now.0.saturating_sub(start.0).min(duration(job.quote));
            let percent = (work * 100 / duration(job.quote)) as u8;
            (Some(DemolitionPhase::Working), percent)
        } else {
            (Some(DemolitionPhase::WaitingForFire), 0)
        }
    } else {
        (None, 0)
    };
    Some(DemolitionView {
        cause: remnant.cause,
        quote: remnant.job.map(|job| job.quote),
        phase,
        progress_percent,
        blocker: remnant.job.and_then(|job| job.blocked),
    })
}

const fn duration(quote: DemolitionQuote) -> u64 {
    quote.minutes as u64 * TICKS_PER_MINUTE
}

fn valid_terms(quote: DemolitionQuote) -> bool {
    quote.policy == POLICY && (30..=180).contains(&quote.minutes) && quote.price > Money::ZERO
}

pub(crate) fn prepare_demolition(
    sim: &Simulation,
    site: BuildingID,
    submitted: DemolitionQuote,
    work: &mut usize,
) -> Result<Money, DemolitionError> {
    if !valid_terms(submitted) {
        return Err(Rejection::StaleDemolitionQuote);
    }
    if let Some(job) = sim
        .read::<SiteRemnants>()
        .sites
        .get(&site)
        .and_then(|remnant| remnant.job)
    {
        return if job.quote == submitted {
            Ok(Money::ZERO)
        } else {
            Err(Rejection::StaleDemolitionQuote)
        };
    }
    if sim.read::<SiteRemnants>().due.len() >= JOB_LIMIT {
        return Err(Rejection::DemolitionWorkLimit);
    }
    {
        let map = sim.map();
        if map.get(site).is_none() {
            return Err(Rejection::MissingReference);
        }
        map.authorize_building_removal_budgeted(site, work)?;
    }
    if demolition_quote(sim, site)?.quote != submitted {
        return Err(Rejection::StaleDemolitionQuote);
    }
    if sim.read::<Government>().money < submitted.price {
        return Err(Rejection::InsufficientFunds);
    }
    Ok(submitted.price)
}

pub(crate) fn start_demolition(
    sim: &mut Simulation,
    site: BuildingID,
    submitted: DemolitionQuote,
    work: &mut usize,
) -> Result<DemolitionReceipt, DemolitionError> {
    let price = prepare_demolition(sim, site, submitted, work)?;
    let receipt = DemolitionReceipt {
        site,
        quote: submitted,
        charged: price,
    };
    if price == Money::ZERO {
        return Ok(receipt);
    }
    let now = elapsed_tick(sim);
    let burning = fire_incident(sim, site).is_some();
    if demolition_status(sim, site).is_none() {
        ruin_site(sim, site, SiteRemnantCause::PlayerDemolition)?;
    }
    let mut remnants = sim.write::<SiteRemnants>();
    let job = DemolitionJob {
        quote: submitted,
        work_started: (!burning).then_some(now),
        accepted_at: now,
        next_update: Tick(now.0.saturating_add(if burning {
            TICKS_PER_MINUTE
        } else {
            duration(submitted)
        })),
        blocked: None,
    };
    remnants
        .sites
        .get_mut(&site)
        .expect("occupied closing site")
        .job = Some(job);
    remnants.due.insert((job.next_update, site));
    drop(remnants);
    Budget::apply(&sim.resources, BudgetCategory::Construction, -price);
    Ok(receipt)
}

pub(crate) fn ruin_site(
    sim: &mut Simulation,
    site: BuildingID,
    cause: SiteRemnantCause,
) -> Result<(), DemolitionError> {
    let exists = {
        let mut remnants = sim.write::<SiteRemnants>();
        if let Some(remnant) = remnants.sites.get_mut(&site) {
            if cause == SiteRemnantCause::Fire {
                remnant.cause = cause;
            }
            true
        } else {
            false
        }
    };
    if exists {
        if cause == SiteRemnantCause::Fire {
            sim.map_mut().mark_ruined(site);
        }
        return Ok(());
    }
    let original_kind = sim.map().get(site).ok_or(Rejection::MissingReference)?.kind;
    if !super::lifecycle::close_site(sim, site) {
        return Err(Rejection::InvalidBuilding);
    }
    sim.write::<SiteRemnants>().sites.insert(
        site,
        SiteRemnant {
            original_kind,
            cause,
            job: None,
        },
    );
    if cause != SiteRemnantCause::PlayerDemolition {
        sim.map_mut().mark_ruined(site);
    }
    Ok(())
}

pub(crate) fn forget_site(sim: &Simulation, site: BuildingID) {
    let mut remnants = sim.write::<SiteRemnants>();
    if let Some(job) = remnants.sites.remove(&site).and_then(|remnant| remnant.job) {
        remnants.due.remove(&(job.next_update, site));
    }
}

pub(crate) fn demolition_update(sim: &mut Simulation) {
    if sim.read::<SiteRemnants>().due.is_empty() {
        return;
    }
    let now = elapsed_tick(sim);
    let mut remaining = crate::map::MAX_WATER_BATCH_WORK;
    for _ in 0..CLEARANCE_BUDGET {
        let Some((due, site)) = sim.read::<SiteRemnants>().due.first().copied() else {
            break;
        };
        if due > now {
            break;
        }
        let mut job = sim.read::<SiteRemnants>().sites[&site]
            .job
            .expect("scheduled clearance");
        if job.work_started.is_none() {
            if fire_incident(sim, site).is_some() {
                job.next_update = Tick(now.0.saturating_add(TICKS_PER_MINUTE));
            } else {
                job.work_started = Some(now);
                job.next_update = Tick(now.0.saturating_add(duration(job.quote)));
            }
        } else {
            let result = if super::service_dispatch::clearance_blocked(sim, site) {
                Err(ClearanceBlocker::Responders)
            } else {
                sim.map().prepare_demolition_clearance(site, &mut remaining)
            };
            match result {
                Ok(()) => {
                    super::service_dispatch::release_station(sim, site);
                    sim.map_mut()
                        .clear_prepared_building(site)
                        .expect("scheduled site still occupies the map");
                    forget_site(sim, site);
                    continue;
                }
                Err(reason) => {
                    job.blocked = Some(reason);
                    job.next_update = Tick(now.0.saturating_add(RETRY_TICKS));
                }
            }
        }
        let mut remnants = sim.write::<SiteRemnants>();
        remnants.due.remove(&(due, site));
        remnants.due.insert((job.next_update, site));
        remnants.sites.get_mut(&site).expect("scheduled site").job = Some(job);
    }
}

pub(crate) fn restore_remnants(sim: &mut Simulation) -> Result<(), &'static str> {
    let map = sim.map();
    let now = elapsed_tick(sim);
    let mut remnants = sim.write::<SiteRemnants>();
    let mut due = BTreeSet::new();
    for (site, building) in map.buildings() {
        if (building.kind == BuildingKind::SiteRemnant) != remnants.sites.contains_key(&site) {
            return Err("building and remnant authority disagree");
        }
    }
    for (&site, remnant) in &remnants.sites {
        if map.get(site).is_none()
            || remnant.original_kind == BuildingKind::SiteRemnant
            || !crate::building_kind_prototype_exists(remnant.original_kind)
            || (remnant.cause == SiteRemnantCause::PlayerDemolition && remnant.job.is_none())
        {
            return Err("invalid occupied remnant");
        }
        let Some(job) = remnant.job else {
            continue;
        };
        if !valid_terms(job.quote)
            || job.accepted_at > now
            || job
                .work_started
                .is_some_and(|start| start < job.accepted_at || start > now)
            || job.next_update <= job.accepted_at
            || (job.blocked.is_some()
                && job
                    .work_started
                    .is_none_or(|start| now.0 - start.0 < duration(job.quote)))
        {
            return Err("invalid paid clearance authority");
        }
        due.insert((job.next_update, site));
    }
    if due.len() > JOB_LIMIT {
        return Err("clearance job limit exceeded");
    }
    remnants.due = due;
    let ruined = remnants
        .sites
        .iter()
        .filter_map(|(&site, record)| {
            (record.cause != SiteRemnantCause::PlayerDemolition).then_some(site)
        })
        .collect::<Vec<_>>();
    drop(remnants);
    drop(map);
    let mut map = sim.map_mut();
    for site in ruined {
        map.mark_ruined(site);
    }
    Ok(())
}
