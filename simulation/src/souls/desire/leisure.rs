use geom::Vec3;
use ordered_float::OrderedFloat;
use prototypes::{
    try_prototype, GameTime, ServiceID, ServiceKind, TICKS_PER_HOUR, TICKS_PER_MINUTE,
    TICKS_PER_REALTIME_SECOND,
};

use crate::map::{BuildingID, Map, ProjectFilter, ProjectKind};
use crate::map_dynamic::{walk_fits, BuildingInfos, ElectricityFlow, WalkingReach};
use crate::souls::household::{HouseholdClaims, HouseholdStock};
use crate::transportation::Location;
use crate::world::CompanyEnt;
use crate::{CompanyID, HumanID, SoulID};
use slotmapd::HopSlotMap;

pub(crate) const PARK_DWELL: u64 = 35 * TICKS_PER_MINUTE;
pub(crate) const PARK_TRAVEL_LIMIT: u64 = TICKS_PER_HOUR;
pub(crate) const AFTER_WORK_RADIUS: f32 = 600.0;

pub(crate) fn leisure_score(stock: &HouseholdStock, time: &GameTime, now: u64) -> f32 {
    if now >= stock.next_leisure
        && (12..21).contains(&time.daytime.hour)
        && stock.needs.leisure < 0.59
    {
        0.67
    } else {
        0.0
    }
}

pub(crate) type Companies = HopSlotMap<CompanyID, CompanyEnt>;

fn operating_park(
    id: BuildingID,
    companies: &Companies,
    map: &Map,
    infos: &BuildingInfos,
    electricity: &ElectricityFlow,
) -> Option<(Vec3, usize, f32)> {
    let building = map.get(id).filter(|building| !building.is_orphaned())?;
    building.connected_road?;
    let proto = try_prototype(ServiceID::from(building.kind.as_goods_company()?))?;
    if proto.service_kind != ServiceKind::Park
        || map
            .electricity
            .net_id(id)
            .is_some_and(|net| electricity.blackout(net))
    {
        return None;
    }
    let SoulID::GoodsCompany(owner) = infos.owner(id)? else {
        return None;
    };
    let company = companies.get(owner)?;
    let capacity =
        (proto.capacity as f32 * company.raw_productivity(proto, building.zone.as_ref())) as usize;
    (capacity > 0).then_some((building.door_pos, capacity, proto.radius_m))
}

pub(crate) struct ParkChoices<'a> {
    pub(crate) companies: &'a Companies,
    pub(crate) map: &'a Map,
    pub(crate) infos: &'a BuildingInfos,
    pub(crate) electricity: &'a ElectricityFlow,
}

impl ParkChoices<'_> {
    pub(crate) fn still_has_slot(
        &self,
        claims: &HouseholdClaims,
        park: BuildingID,
        visitor: HumanID,
    ) -> bool {
        let Some((_, capacity, _)) =
            operating_park(park, self.companies, self.map, self.infos, self.electricity)
        else {
            return false;
        };
        claims
            .parks
            .get(&park)
            .is_some_and(|visitors| visitors.iter().take(capacity).any(|&id| id == visitor))
    }

    pub(crate) fn claim(
        &self,
        claims: &mut HouseholdClaims,
        visitor: HumanID,
        walking: &mut WalkingReach,
        origin: Vec3,
        speed: f32,
        loc: &Location,
    ) -> Option<BuildingID> {
        // The spatial query limits candidates to a viable one-hour walk. Use
        // current staffing and power for each park, as on arrival and at dwell.
        let reach = speed * PARK_TRAVEL_LIMIT as f32 / TICKS_PER_REALTIME_SECOND as f32;
        let mut candidates: Vec<_> = self
            .map
            .spatial_map()
            .query_around(origin.xy(), reach, ProjectFilter::BUILDING)
            .filter_map(|kind| {
                let ProjectKind::Building(id) = kind else {
                    return None;
                };
                if *loc == Location::Building(id) {
                    return None;
                }
                let (pos, capacity, park_radius) =
                    operating_park(id, self.companies, self.map, self.infos, self.electricity)?;
                let distance = pos.distance2(origin);
                if claims
                    .parks
                    .get(&id)
                    .is_some_and(|visitors| visitors.len() >= capacity)
                    || distance > reach.min(park_radius).powi(2)
                {
                    return None;
                }
                Some((OrderedFloat(distance), id))
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_unstable();
        let start = walking.component_at(self.map, origin)?;
        let (_, id) = candidates.into_iter().find(|&(_, id)| {
            let park = self.map.get(id).unwrap();
            walking.reaches_building(self.map, start, park)
                && walk_fits(self.map, origin, park.door_pos, speed, PARK_TRAVEL_LIMIT)
        })?;
        claims.parks.entry(id).or_default().insert(visitor);
        Some(id)
    }
}
