use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use slotmapd::HopSlotMap;

use egui_inspect::Inspect;
use geom::{PolyLine3, Polyline3Queue, Transform, Vec3};
use prototypes::{RollingStockID, DELTA};

use crate::map::{
    IntersectionID, LaneID, LaneKind, Map, Traversable, TraverseDirection, TraverseKind,
};
use crate::map_dynamic::ItineraryFollower;
use crate::transportation::Speed;
use crate::utils::resources::Resources;
use crate::world::{TrainEnt, TrainID, WagonEnt};
use crate::{Itinerary, ItineraryLeader, Simulation, World};

#[derive(Default, Serialize, Deserialize)]
pub struct TrainReservations {
    pub reservations: BTreeMap<IntersectionID, TrainID>,
    pub localisations: BTreeMap<TraverseKind, BTreeMap<TrainID, f32>>,
}

#[derive(Serialize, Deserialize, Inspect)]
pub struct Locomotive {
    /// m/s
    pub max_speed: f32,
    /// m.s^2
    pub acc_force: f32,
    /// m.s^2
    pub dec_force: f32,
    /// m
    pub length: f32,
}

#[derive(Serialize, Deserialize, Inspect)]
pub struct LocomotiveReservation {
    pub cur_travers_dist: f32,
    pub waited_for: f32,
    past_travers: BTreeMap<TraverseKind, f32>,
    upcoming_inters: Vec<IntersectionID>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, Inspect)]
pub enum RailWagonKind {
    Locomotive,
    Passenger,
    Freight,
}

#[derive(Inspect, Serialize, Deserialize)]
pub struct RailWagon {
    pub kind: RailWagonKind,
    pub rolling_stock: RollingStockID,
}

pub fn calculate_locomotive(wagons: &[RollingStockID]) -> Locomotive {
    let info = wagons.iter().fold(
        (720.0, 0.0, 0.0, 0.0, 0),
        |(speed, acc, dec, length, mass): (f32, f32, f32, f32, u32), &id| {
            let rs = RollingStockID::prototype(id);
            (
                speed.min(rs.max_speed),
                acc + rs.acc_force,
                dec + rs.dec_force,
                length + rs.length,
                mass + rs.mass,
            )
        },
    );
    Locomotive {
        max_speed: info.0,
        acc_force: info.1 / info.4 as f32,
        dec_force: info.2 / info.4 as f32,
        length: info.3 + 10.0,
    }
}

pub fn wagons_loco_dists_lengths(
    wagons: &[RollingStockID],
) -> impl DoubleEndedIterator<Item = (f32, f32)> + '_ {
    let mut loco_dist = 0.0;
    wagons.iter().map(move |&id| {
        let length = RollingStockID::prototype(id).length;
        loco_dist += length;
        (loco_dist - length, length)
    })
}

pub fn wagons_positions_for_render<'a>(
    wagons: &'a [RollingStockID],
    points: &'a PolyLine3,
    dist: f32,
) -> impl Iterator<Item = (Vec3, Vec3, f32)> + 'a {
    wagons_loco_dists_lengths(wagons)
        .map(|(wagon_dist, length)| (wagon_dist + length * 0.5, length))
        .rev()
        .filter_map(move |(wagin_dist, length)| {
            let pos = dist - wagin_dist;
            if pos >= 0.0 {
                Some((pos, length))
            } else {
                None
            }
        })
        .map(move |(d, length)| {
            let (pos, dir) = points.point_dir_along(d);
            (pos, dir, length)
        })
}

pub fn train_length(wagons: &[RollingStockID]) -> f32 {
    wagons
        .iter()
        .map(|id| RollingStockID::prototype(*id).length)
        .sum::<f32>()
}

pub(crate) fn spawn_train(
    sim: &mut Simulation,
    wagons: &[RollingStockID],
    kind: RailWagonKind,
    lane: LaneID,
    dist: f32,
) -> Option<TrainID> {
    let (world, res) = sim.world_res();

    let map = res.read::<Map>();
    let lane = map.lanes().get(lane)?;
    let mut reservations = res.write::<TrainReservations>();
    if !can_start_train(&map, world, &reservations, wagons, lane.id, dist) {
        return None;
    }

    let (locopos, locodir) = lane.points.point_dir_along(dist);

    let (_, segment) = lane.points.project_segment(locopos);

    let mut points = lane
        .points
        .iter()
        .take(segment)
        .copied()
        .filter(|point| point.distance2(locopos) > 0.000001)
        .collect::<Vec<_>>();
    points.reverse();

    let locomotive = calculate_locomotive(wagons);
    let train_length = locomotive.length;

    let loco = world.insert(TrainEnt {
        trans: Transform::new_dir(locopos, locodir),
        speed: Default::default(),
        it: Itinerary::NONE,
        locomotive,
        res: LocomotiveReservation {
            cur_travers_dist: dist,
            waited_for: 0.0,
            past_travers: BTreeMap::from([(
                TraverseKind::Lane(lane.id),
                dist - lane.points.length(),
            )]),
            upcoming_inters: Default::default(),
        },
        leader: ItineraryLeader {
            past: Polyline3Queue::new(points.into_iter(), locopos, train_length + 20.0),
        },
    });
    reservations
        .localisations
        .entry(TraverseKind::Lane(lane.id))
        .or_default()
        .insert(loco, dist - lane.points.length());

    let leader = &world.trains.get(loco).unwrap().leader;

    let mut followers: Vec<_> = leader
        .past
        .mk_followers(
            wagons_loco_dists_lengths(wagons)
                .flat_map(|(dist, length)| [dist + length * 0.1, dist + length * 0.9]),
        )
        .collect();
    for (i, follower) in followers.chunks_exact_mut(2).enumerate() {
        let (pos, dir) = follower[0].update(&leader.past);
        let (pos2, dir2) = follower[1].update(&leader.past);
        world.wagons.insert(WagonEnt {
            trans: Transform::new_dir(pos * 0.5 + pos2 * 0.5, (0.5 * (dir + dir2)).normalize()),
            speed: Speed::default(),
            wagon: RailWagon {
                rolling_stock: wagons[i],
                kind: if i == 0 {
                    RailWagonKind::Locomotive
                } else {
                    kind
                },
            },
            itfollower: ItineraryFollower {
                leader: loco,
                head: follower[0],
                tail: follower[1],
            },
        });
    }

    log::info!("Spawned Train with {} wagons", wagons.len());
    Some(loco)
}

pub(crate) fn can_start_train(
    map: &Map,
    world: &World,
    reservations: &TrainReservations,
    wagons: &[RollingStockID],
    lane: LaneID,
    lead: f32,
) -> bool {
    let Some(track) = map.get(lane) else {
        return false;
    };
    let length = train_length(wagons);
    if lead < length || lead > track.points.length() || !(length > 0.0) {
        return false;
    }
    let kind = TraverseKind::Lane(lane);
    reservations
        .localisations
        .get(&kind)
        .is_none_or(|occupants| {
            occupants.iter().all(|(id, &progress)| {
                let other_lead = track.points.length() + progress;
                // Each owner's full interval is recorded with its current footprint.
                let Some(other) = world.trains.get(*id) else {
                    return true;
                };
                let other_length = other.locomotive.length - 10.0;
                lead <= other_lead - other_length || lead - length >= other_lead
            })
        })
}

pub fn traverse_forward<'a>(
    map: &'a Map,
    itin: &'a Itinerary,
    dist: f32,
    mut acc: f32,
    until_length: f32,
) -> impl Iterator<Item = (TraverseKind, f32, f32, f32)> + 'a {
    let route = itin.get_route();
    let lanes = map.lanes();
    let inters = map.intersections();
    let mut acc_inter = 0.0;
    route
        .into_iter()
        .flat_map(move |route| route.reversed_route.iter().rev())
        .filter_map(move |v| {
            let oldacc = acc;
            let l = v.kind.length(lanes, inters)?;

            match v.kind {
                TraverseKind::Turn(id) if inters.get(id.parent)?.roads.len() > 2 => acc_inter = 0.0,
                _ => acc_inter += l,
            }

            acc += l;
            Some((v.kind, oldacc, l, acc_inter))
        })
        .take_while(move |(_, acc, _, acc_l)| *acc < dist || *acc_l <= until_length)
}

pub(crate) fn occupied_lanes(
    reservation: &LocomotiveReservation,
) -> impl Iterator<Item = LaneID> + '_ {
    reservation
        .past_travers
        .keys()
        .filter_map(|kind| match kind {
            TraverseKind::Lane(lane) => Some(*lane),
            TraverseKind::Turn(_) => None,
        })
}

pub(crate) fn rear_clear_of_turns(reservation: &LocomotiveReservation) -> bool {
    reservation
        .past_travers
        .keys()
        .all(|kind| matches!(kind, TraverseKind::Lane(_)))
}

pub(crate) fn cancel_train_route(
    id: TrainID,
    train: &mut TrainEnt,
    reservations: &mut TrainReservations,
) {
    train.it = Itinerary::NONE;
    release_train_grants(id, &mut train.res, reservations);
}

pub(crate) fn release_train_grants(
    id: TrainID,
    physical: &mut LocomotiveReservation,
    reservations: &mut TrainReservations,
) {
    for intersection in physical.upcoming_inters.drain(..) {
        let occupied = physical
            .past_travers
            .keys()
            .any(|kind| matches!(kind, TraverseKind::Turn(turn) if turn.parent == intersection));
        if !occupied && reservations.reservations.get(&intersection) == Some(&id) {
            reservations.reservations.remove(&intersection);
        }
    }
}

/// Restore claims before any paused commands can inspect a decoded train.
pub(crate) fn restore_train_occupancy(
    sim: &mut Simulation,
    measured: bool,
) -> Result<(), &'static str> {
    let (world, resources) = sim.world_res();
    let map = resources.read::<Map>();
    let mut reservations = resources.write::<TrainReservations>();
    if !measured {
        reservations.localisations.clear();
        reservations.reservations.clear();
    }
    for (id, train) in world.trains.iter_mut() {
        let length = train.locomotive.length;
        if !length.is_finite() || length <= 10.0 || !train.trans.pos.is_finite() {
            return Err("invalid train footprint");
        }
        if !measured {
            train.res.upcoming_inters.clear();
            train.res.past_travers = recover_legacy_footprint(&map, train)
                .ok_or("legacy train has no recoverable rail footprint")?;
        }
        if train.res.past_travers.is_empty() {
            return Err("train has no measured rail footprint");
        }
        for (&kind, &progress) in &train.res.past_travers {
            if !progress.is_finite() || progress >= length {
                return Err("invalid train footprint distance");
            }
            reservations
                .localisations
                .entry(kind)
                .or_default()
                .insert(id, progress);
            if let TraverseKind::Turn(turn) = kind {
                // A physical claim always takes priority over a projection
                // from the pre-movement reservation phase.
                if reservations
                    .reservations
                    .insert(turn.parent, id)
                    .is_some_and(|owner| owner != id)
                {
                    return Err("multiple trains claim an occupied junction");
                }
            }
        }
    }
    if !measured {
        for owners in reservations.localisations.values() {
            let mut intervals: Vec<_> = owners
                .iter()
                .map(|(&id, &lead)| (lead, world.trains[id].locomotive.length - 10.0))
                .collect();
            intervals.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
            if intervals
                .windows(2)
                .any(|pair| pair[0].0 > pair[1].0 - pair[1].1 + 0.01)
            {
                return Err("overlapping legacy train footprints");
            }
        }
    }
    Ok(())
}

fn recover_legacy_footprint(map: &Map, train: &TrainEnt) -> Option<BTreeMap<TraverseKind, f32>> {
    let pos = train.trans.pos;
    let current = train.it.get_travers().map(|traversal| traversal.kind);
    let nearest = map
        .nearest_lane(pos, LaneKind::Rail, Some(4.0))
        .map(TraverseKind::Lane);
    let (head, offset, track_length) = train
        .res
        .past_travers
        .keys()
        .copied()
        .chain(current)
        .chain(nearest)
        .filter_map(|kind| {
            let points = Traversable::new(kind, TraverseDirection::Forward).raw_points(map)?;
            let projected = points.project(pos);
            let distance = projected.distance2(pos);
            let offset = points.length_at_proj(projected);
            (distance <= 0.25).then_some((
                ordered_float::OrderedFloat(distance),
                Some(kind) != current,
                kind,
                offset,
                points.length(),
            ))
        })
        .min_by_key(|&(distance, other, kind, _, _)| (distance, other, kind))
        .map(|(_, _, kind, offset, length)| (kind, offset, length))?;

    // P6 could age out a turnout with no movement. Walk the directed
    // predecessors within the consist length and claim any possible tail.
    let mut occupied = BTreeMap::from([(head, offset - track_length)]);
    let mut pending = vec![(head, offset)];
    while let Some((kind, behind)) = pending.pop() {
        if behind >= train.locomotive.length {
            continue;
        }
        let mut add = |previous| {
            let Some(length) = TraverseKind::length(&previous, map.lanes(), map.intersections())
            else {
                return;
            };
            if occupied.get(&previous).is_none_or(|&known| behind < known) {
                occupied.insert(previous, behind);
                pending.push((previous, behind + length));
            }
        };
        match kind {
            TraverseKind::Lane(lane) => {
                let intersection = map.get(map.get(lane)?.src)?;
                for (turn, direction) in intersection.turns_to(lane) {
                    if direction == TraverseDirection::Forward
                        && map
                            .get(turn.src)
                            .is_some_and(|lane| lane.kind == LaneKind::Rail)
                    {
                        add(TraverseKind::Turn(turn));
                    }
                }
            }
            TraverseKind::Turn(turn) => add(TraverseKind::Lane(turn.src)),
        }
    }
    Some(occupied)
}

pub fn train_reservations_update(world: &mut World, resources: &mut Resources) {
    profiling::scope!("transportation::train_reservations_update");
    let map = &*resources.read::<Map>();
    let reservations = &mut *resources.write::<TrainReservations>();
    let trace = resources.read::<crate::economy::cargo::FreightTrace>();
    let lanes = map.lanes();
    let inters = map.intersections();
    reservations.reservations.clear();
    reservations.localisations.clear();
    for (train, entity) in world.trains.iter_mut() {
        entity.res.upcoming_inters.clear();
        for (&traversal, &distance) in &entity.res.past_travers {
            reservations
                .localisations
                .entry(traversal)
                .or_default()
                .insert(train, distance);
            if let TraverseKind::Turn(turn) = traversal {
                reservations
                    .reservations
                    .entry(turn.parent)
                    .or_insert(train);
            }
        }
    }
    let mut ids: Vec<_> = world.trains.keys().collect();
    ids.sort_unstable();
    for me in ids {
        if !trace.may_move(me) {
            continue;
        }
        let train = &mut world.trains[me];
        let Some(travers) = train.it.get_travers() else {
            continue;
        };
        let current_length = travers.kind.length(lanes, inters).unwrap_or_default();
        let progress = train
            .res
            .past_travers
            .get(&travers.kind)
            .map_or(0.0, |distance| current_length + distance);
        train.res.cur_travers_dist = progress;
        let dist_to_next = (current_length - progress).max(0.0);
        let stop_dist = train.speed.0 * train.speed.0 / (2.0 * train.locomotive.dec_force);

        if reservations
            .localisations
            .get(&travers.kind)
            .is_some_and(|occupants| {
                occupants
                    .iter()
                    .any(|(&other, &distance)| other != me && distance > progress - current_length)
            })
        {
            continue;
        }
        let mut requested = Vec::new();
        let upcoming = traverse_forward(
            map,
            &train.it,
            stop_dist + 5.0,
            dist_to_next,
            train.locomotive.length + 25.0,
        )
        .map(|(kind, _, _, _)| kind);
        let can_reserve = std::iter::once(travers.kind).chain(upcoming).all(|kind| {
            if kind != travers.kind
                && reservations
                    .localisations
                    .get(&kind)
                    .is_some_and(|owners| owners.keys().any(|&id| id != me))
            {
                return false;
            }
            let TraverseKind::Turn(turn) = kind else {
                return true;
            };
            if reservations
                .reservations
                .get(&turn.parent)
                .is_some_and(|&owner| owner != me)
            {
                return false;
            }
            requested.push(turn.parent);
            true
        });
        if can_reserve {
            requested.sort_unstable();
            requested.dedup();
            for junction in requested {
                reservations.reservations.insert(junction, me);
                train.res.upcoming_inters.push(junction);
            }
        }
    }
}

/// Updates the occupied traversals from a segment that the lead actually moved.
pub(crate) fn record_train_motion(
    train: &mut LocomotiveReservation,
    length: f32,
    map: &Map,
    traversal: Traversable,
    from: Vec3,
    to: Vec3,
) {
    let moved = from.distance(to);
    if moved == 0.0 {
        return;
    }
    for progress in train.past_travers.values_mut() {
        *progress += moved;
    }
    train.past_travers.retain(|_, progress| *progress < length);
    if let Some(points) = traversal.raw_points(map) {
        let progress = points.length_at_proj(points.project(to));
        train.cur_travers_dist = progress;
        train
            .past_travers
            .insert(traversal.kind, progress - points.length());
    }
}

/// A train may enter only the rail turn it owns. Occupied track downstream
/// also closes the boundary until the other consist has cleared.
pub(crate) fn can_move_train(
    reservations: &TrainReservations,
    lengths: &BTreeMap<TrainID, f32>,
    train: TrainID,
    current: TraverseKind,
    track: &PolyLine3,
    physical: &LocomotiveReservation,
    to: Vec3,
) -> bool {
    if let TraverseKind::Turn(turn) = current {
        return reservations.reservations.get(&turn.parent) == Some(&train);
    }
    let Some(owners) = reservations.localisations.get(&current) else {
        return true;
    };
    let lead = track.length_at_proj(track.project(to)) - track.length();
    let old = physical
        .past_travers
        .get(&current)
        .copied()
        .unwrap_or(-track.length());
    owners.iter().all(|(&owner, &other_lead)| {
        owner == train
            || other_lead <= old
            || lengths
                .get(&owner)
                .is_some_and(|length| lead <= other_lead - length)
    })
}

pub fn locomotive_system(world: &mut World, resources: &mut Resources) {
    profiling::scope!("transportation::locomotive_system");
    let map: &Map = &resources.read();
    let reservs: &TrainReservations = &resources.read();

    // asume iter order stays the same
    let mut desired_speeds = Vec::with_capacity(world.trains.len());

    for (ent, train) in world.trains.iter() {
        desired_speeds.push(locomotive_desired_speed(
            ent,
            map,
            reservs,
            &world.trains,
            train,
        ));
    }

    for (t, desired_speed) in world.trains.values_mut().zip(desired_speeds) {
        let desired_dir =
            t.it.get_point()
                .and_then(|x| {
                    let d = x - t.trans.pos;
                    if d.mag2() < 0.5 {
                        return None;
                    }
                    d.try_normalize()
                })
                .unwrap_or(t.trans.dir);
        t.trans.dir = desired_dir;

        t.speed.0 += (desired_speed - t.speed.0).clamp(
            -DELTA * t.locomotive.dec_force,
            DELTA * t.locomotive.acc_force,
        );
        if t.speed.0 <= 0.001 {
            t.res.waited_for += DELTA;
        } else {
            t.res.waited_for = 0.0;
        }
    }
}

pub fn locomotive_desired_speed(
    me: TrainID,
    map: &Map,
    reservs: &TrainReservations,
    locos: &HopSlotMap<TrainID, TrainEnt>,
    t: &TrainEnt,
) -> f32 {
    if t.it.is_none_or_wait() {
        return 0.0;
    }

    let stop_dist = t.speed.0 * t.speed.0 / (2.0 * t.locomotive.dec_force);

    let mut lastid = None;
    let mydist = t.res.cur_travers_dist;
    if let Some(travers) = t.it.get_travers() {
        let lanes = map.lanes();

        let startl = travers
            .kind
            .length(lanes, map.intersections())
            .unwrap_or(0.0);
        let dist_to_next = startl - mydist;

        for (id, acc, travers_length, _) in std::iter::once((travers.kind, -mydist, startl, startl))
            .chain(traverse_forward(
                map,
                &t.it,
                stop_dist + 15.0,
                dist_to_next,
                -1.0,
            ))
        {
            if let Some(locs) = reservs.localisations.get(&id) {
                for (&train, &otherdist) in locs {
                    if train == me {
                        continue;
                    }
                    if let Some(otherloco) = locos.get(train) {
                        let dist_to_other = acc + otherdist + travers_length;
                        if dist_to_other > 0.0
                            && dist_to_other < otherloco.locomotive.length + stop_dist + 10.0
                        {
                            return 0.0;
                        }
                    }
                }
            }
            if let TraverseKind::Turn(id) = id {
                if let Some(inter) = map.intersections().get(id.parent) {
                    if inter.roads.len() > 2 {
                        if let Some(reserved_by) = reservs.reservations.get(&id.parent) {
                            if *reserved_by != me {
                                return 0.0;
                            }
                        } else {
                            return 0.0;
                        }
                    }
                }
            }
            lastid = Some(id);
        }
    }

    let mut on_last_lane = false;
    if let Some(r) = t.it.get_route() {
        if r.reversed_route.is_empty()
            || (lastid.is_some() && lastid == r.reversed_route.first().map(|x| x.kind))
        {
            on_last_lane = true;
        }
    }

    if t.it.is_simple() {
        on_last_lane = true
    }

    if on_last_lane {
        if let Some(howfar) = t.it.end_pos().map(|term| term.distance(t.trans.pos)) {
            if howfar + 0.1 <= stop_dist {
                return 0.0;
            }
        }
    }

    t.locomotive.max_speed
}

#[cfg(test)]
mod legacy_tests {
    use super::*;
    use crate::map::{LanePatternBuilder, MapProject};
    use crate::{SimulationDeser, WorldCommand};
    use common::saveload::{Bincode, Encoder};
    use geom::vec3;

    #[test]
    fn p8_load_p6_paused_spawn_reconstructs_lane_claim() {
        drop(crate::tests::TestCtx::new_fast());
        let mut sim = Simulation::new_authored_flat(8, 0).unwrap();
        sim.apply_commands([&WorldCommand::MapMakeConnection {
            from: MapProject::ground(vec3(200.0, 200.0, 0.0)),
            to: MapProject::ground(vec3(600.0, 200.0, 0.0)),
            inter: None,
            pat: LanePatternBuilder::new().rail(true).build(),
        }]);
        let lane = sim
            .map()
            .lanes()
            .values()
            .find(|lane| lane.kind == LaneKind::Rail)
            .unwrap()
            .id;
        let spawn = WorldCommand::SpawnTrain {
            wagons: vec![RollingStockID::new("locomotive")],
            lane,
            dist: 100.0,
        };
        sim.apply_commands([&spawn]);
        sim.write::<TrainReservations>().localisations.clear();
        let mut snapshot: SimulationDeser =
            Bincode::decode(&Bincode::encode(&sim).unwrap()).unwrap();
        snapshot.res.remove("cargo_progress");
        let mut restored = snapshot.into_sim().unwrap();
        restored.apply_commands([&spawn]);
        assert_eq!(restored.world().trains.len(), 1);
    }
}
