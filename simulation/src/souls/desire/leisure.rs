use std::collections::BTreeMap;

use geom::Vec3;
use ordered_float::OrderedFloat;
use prototypes::{
    try_prototype, GameTime, ServiceID, ServiceKind, TICKS_PER_HOUR, TICKS_PER_MINUTE,
};

use crate::map::{BuildingID, Map};
use crate::map_dynamic::{BuildingInfos, ElectricityFlow, WalkingReach};
use crate::souls::household::HouseholdStock;
use crate::transportation::Location;
use crate::{SoulID, World};

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

struct Park {
    position: Vec3,
    capacity: usize,
    radius: f32,
    claimed: usize,
}

pub(crate) struct ParkChoices {
    parks: BTreeMap<BuildingID, Park>,
}

impl ParkChoices {
    pub(crate) fn new(
        world: &World,
        map: &Map,
        infos: &BuildingInfos,
        electricity: &ElectricityFlow,
    ) -> Self {
        let mut parks: BTreeMap<_, _> = world
            .companies
            .iter()
            .filter_map(|(id, company)| {
                let proto = try_prototype(ServiceID::from(company.comp.proto))?;
                if proto.service_kind != ServiceKind::Park {
                    return None;
                }
                let building = map
                    .get(company.comp.building)
                    .filter(|b| !b.is_orphaned())?;
                building.connected_road?;
                if infos.owner(building.id) != Some(SoulID::GoodsCompany(id))
                    || map
                        .electricity
                        .net_id(building.id)
                        .is_some_and(|net| electricity.blackout(net))
                {
                    return None;
                }
                let capacity = (proto.capacity as f32
                    * company.raw_productivity(proto, building.zone.as_ref()))
                    as usize;
                (capacity > 0).then_some((
                    building.id,
                    Park {
                        position: building.door_pos,
                        capacity,
                        radius: proto.radius_m,
                        claimed: 0,
                    },
                ))
            })
            .collect();
        if !parks.is_empty() {
            for h in world.humans.values() {
                if let Some(park) = h.household.leisure_visit.and_then(|id| parks.get_mut(&id)) {
                    park.claimed += 1;
                }
            }
        }
        Self { parks }
    }

    pub(crate) fn claim(
        &mut self,
        map: &Map,
        walking: &mut WalkingReach,
        origin: Vec3,
        loc: &Location,
        after_work: bool,
    ) -> Option<BuildingID> {
        let mut component = None;
        let (&id, park) = self
            .parks
            .iter_mut()
            .filter_map(|(id, park)| {
                let distance = park.position.distance2(origin);
                let radius = if after_work {
                    park.radius.min(AFTER_WORK_RADIUS)
                } else {
                    park.radius
                };
                if park.claimed >= park.capacity
                    || distance > radius.powi(2)
                    || *loc == Location::Building(*id)
                {
                    return None;
                }
                let start = *component.get_or_insert_with(|| walking.component_at(map, origin));
                if !start.is_some_and(|start| {
                    map.get(*id)
                        .is_some_and(|park| walking.reaches_building(map, start, park))
                }) {
                    return None;
                }
                Some(((OrderedFloat(distance), *id), (id, park)))
            })
            .min_by_key(|(key, _)| *key)?
            .1;
        park.claimed += 1;
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestCtx;
    use geom::{vec2, vec3};

    #[test]
    fn e1_park_claims_use_capacity_and_stable_id_ties() {
        let test = TestCtx::new_fast();
        test.build_roads(&[vec3(100.0, 100.0, 0.0), vec3(400.0, 100.0, 0.0)]);
        let first = test.build_house_near(vec2(150.0, 122.0));
        let second = test.build_house_near(vec2(300.0, 122.0));
        let (first, second) = (first.min(second), first.max(second));
        let map = test.g.map();
        let origin = map.get(first).unwrap().door_pos;
        let park = || Park {
            position: origin,
            capacity: 1,
            radius: 500.0,
            claimed: 0,
        };
        let mut choices = ParkChoices {
            parks: BTreeMap::from([(second, park()), (first, park())]),
        };
        let mut reach = WalkingReach::default();
        let loc = Location::Outside;
        assert_eq!(
            choices.claim(&map, &mut reach, origin, &loc, false),
            Some(first)
        );
        assert_eq!(
            choices.claim(&map, &mut reach, origin, &loc, false),
            Some(second)
        );
        assert_eq!(choices.claim(&map, &mut reach, origin, &loc, false), None);
    }
}
