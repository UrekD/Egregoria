use crate::map::{LanePattern, MAX_ZONE_AREA};
use crate::world_command::WorldCommand;
use crate::{BuildingKind, Simulation};
use geom::Vec3;
use prototypes::{try_prototype, Money};
use serde::{Deserialize, Serialize};

/// The government represents the player.
#[derive(Serialize, Deserialize)]
pub struct Government {
    pub money: Money,
}

impl Default for Government {
    fn default() -> Self {
        Self {
            money: Money::new_bucks(150_000),
        }
    }
}

impl Government {
    pub fn action_cost(action: &WorldCommand, sim: &Simulation) -> Money {
        Self::checked_action_cost(action, sim).unwrap_or(Money::MAX)
    }

    pub(crate) fn checked_action_cost(action: &WorldCommand, sim: &Simulation) -> Option<Money> {
        match action {
            WorldCommand::MapMakeWalkingPath { .. } => {
                action.preview(sim).ok().map(|quote| quote.cost)
            }
            WorldCommand::MapPlaceParkingLot { draft, .. } => Some(draft.cost()),
            WorldCommand::MapBuildHouse(_) => Self::checked_bucks(100),
            WorldCommand::SpawnTrain { wagons, .. } => {
                let count = i64::try_from(wagons.len()).ok()?;
                Self::checked_bucks(count.checked_mul(100)?.checked_add(1000)?)
            }
            WorldCommand::MapMakeConnection { from, to, pat, .. } => {
                let map = sim.map();
                let from = map.resolve_connection_endpoint(*from)?;
                let to = map.resolve_connection_endpoint(*to)?;
                Self::connection_cost(from.pos, to.pos, pat)
            }
            WorldCommand::UpdateZone { building, zone } => {
                let m = sim.map();
                let b = m.get(*building)?;
                let proto = try_prototype(b.kind.as_goods_company()?)?;
                let description = proto.zone.as_ref()?;
                let old_area = b.zone.as_ref()?.area;
                let new_area = zone.validated_area()?;
                Self::zone_cost(description.price_per_area, new_area - old_area)
            }
            WorldCommand::MapMakeMultipleConnections(projects, links) => {
                let map = sim.map();
                let total = links.iter().try_fold(0i64, |total, (from, to, _, pat)| {
                    let from = map.resolve_connection_endpoint(*projects.get(*from)?)?;
                    let to = map.resolve_connection_endpoint(*projects.get(*to)?)?;
                    let cost = Self::connection_cost(from.pos, to.pos, pat)?;
                    total.checked_add(cost.inner())
                })?;
                Some(Money::new_inner(total))
            }
            WorldCommand::MapBuildSpecialBuilding { kind, zone, .. } => match *kind {
                BuildingKind::GoodsCompany(id) => {
                    let proto = try_prototype(id)?;
                    let mut price = proto.price.inner();
                    if let Some(description) = &proto.zone {
                        let area = zone.as_ref()?.validated_area()?;
                        price = price.checked_add(
                            Self::zone_cost(description.price_per_area, area)?.inner(),
                        )?;
                    }
                    Some(Money::new_inner(price))
                }
                _ => Self::special_building_price(*kind),
            },
            _ => Some(Money::ZERO),
        }
    }

    pub(crate) fn special_building_price(kind: BuildingKind) -> Option<Money> {
        match kind {
            BuildingKind::GoodsCompany(id) => Some(try_prototype(id)?.price),
            BuildingKind::RailFreightStation(id) => Some(try_prototype(id)?.price),
            BuildingKind::TrainStation => Some(Money::new_bucks(1000)),
            _ => Some(Money::ZERO),
        }
    }

    pub(crate) fn connection_cost(p1: Vec3, p2: Vec3, pat: &LanePattern) -> Option<Money> {
        Self::connection_cost_at_length(p1.distance(p2), pat)
    }

    pub(crate) fn connection_cost_at_length(dist: f32, pat: &LanePattern) -> Option<Money> {
        let lanes = i64::try_from(
            pat.lanes_forward
                .len()
                .checked_add(pat.lanes_backward.len())?,
        )
        .ok()?;
        let units = 0.03 * dist;
        if !units.is_finite() || units >= i64::MAX as f32 {
            return None;
        }
        Self::checked_bucks(
            ((units as i64).max(1))
                .checked_mul(lanes)?
                .checked_add(50)?,
        )
    }

    fn zone_cost(price: Money, area: f32) -> Option<Money> {
        if !area.is_finite() {
            return None;
        }
        let raw = i128::from(area as i64) * i128::from(price.inner()) / MAX_ZONE_AREA as i128;
        Some(Money::new_inner(i64::try_from(raw).ok()?))
    }

    fn checked_bucks(bucks: i64) -> Option<Money> {
        Some(Money::new_inner(
            bucks.checked_mul(Money::new_bucks(1).inner())?,
        ))
    }
}
