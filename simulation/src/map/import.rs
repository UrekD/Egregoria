//! Transient, source-independent network import contracts.
use crate::economy::{Budget, BudgetCategory, Government};
use crate::init::INIT_FUNCS;
use crate::map::{
    BuildingID, BuildingKind, Density, IntersectionID, LaneKind, LanePattern, LightPolicy, Lot,
    LotID, Map, RoadID, RoadSegmentKind, RoundaboutPolicy, ROAD_Z_OFFSET,
};
use crate::map_dynamic::BuildingInfos;
use crate::souls::{growth, residence};
use crate::world_command::WorldCommand;
use crate::{NewGameOptions, Simulation, SimulationOptions};
use geom::{Vec2, Vec3, OBB};
use prototypes::{BuildingGen, Money};
use std::collections::BTreeMap;
use std::ptr::addr_of;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportedCity {
    pub junctions: Vec<ImportedJunction>,
    pub links: Vec<ImportedLink>,
    pub boundary_stubs: Vec<BoundaryStub>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportedJunction {
    pub id: usize,
    pub position: Vec2,
    pub roundabout_radius: Option<f32>,
    pub light_policy: LightPolicy,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportedLink {
    pub source_way_ids: Vec<u64>,
    pub from: usize,
    pub to: usize,
    pub points: Vec<Vec2>,
    pub pattern: LanePattern,
    pub frontage: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundaryStub {
    pub junction: usize,
    pub link: usize,
    pub at_start: bool,
}

#[derive(Debug)]
pub struct ImportedNetwork {
    pub junctions: BTreeMap<usize, IntersectionID>,
    pub roads: BTreeMap<usize, RoadID>,
    pub short_lanes: Vec<(usize, f32)>,
}

const LINK_MIN_METRES: f32 = 10.0;
const MAX_LANES_AT_JUNCTION: usize = 96;

impl Simulation {
    /// Assemble an initial network without replay recording or starter infrastructure.
    /// The caller must initialize the simulation and prototype registries first.
    pub fn from_imported_city(
        city: &ImportedCity,
        options: &NewGameOptions,
    ) -> Result<(Self, ImportedNetwork), String> {
        options.validate().map_err(|error| error.to_string())?;
        let mut sim = Self {
            world: Default::default(),
            resources: Default::default(),
        };
        unsafe {
            for system in &*addr_of!(INIT_FUNCS) {
                (system.f)(&mut sim);
            }
        }
        WorldCommand::bootstrap_import(
            &mut sim,
            SimulationOptions {
                terrain_size: options.terrain_chunks,
                save_replay: false,
                preset: options.preset,
                seed: options.seed,
                starting_money_bucks: options.starting_money_bucks,
                progression: options.progression,
            },
        )
        .map_err(|error| format!("import bootstrap: {error:?}"))?;
        sim.map_mut().lot_layout = options.lot_layout;
        let network = import_network(&mut sim.map_mut(), city)?;
        sim.record_constructor_end();
        Ok((sim, network))
    }
}

impl Simulation {
    /// Set the handoff treasury after commissioning and post the adjustment to the ledger.
    pub fn fund_imported_opening(&self, balance: Money) -> Result<(), &'static str> {
        if !self.map().import_lane_reserve {
            return Err("opening funding requires an imported map");
        }
        let current = self.read::<Government>().money;
        let delta = balance
            .inner()
            .checked_sub(current.inner())
            .ok_or("opening funding exceeds the supported money range")?;
        Budget::apply(
            &self.resources,
            BudgetCategory::StarterInfrastructure,
            Money::new_inner(delta),
        );
        Ok(())
    }

    /// Filter the generated trees during import setup, before the map is displayed.
    pub fn filter_imported_trees<E>(
        &self,
        mut retain: impl FnMut(Vec2) -> Result<bool, E>,
    ) -> Result<(), E> {
        let mut error = None;
        self.map_mut().environment.retain_imported_trees(|pos| {
            if error.is_some() {
                return true;
            }
            match retain(pos) {
                Ok(keep) => keep,
                Err(e) => {
                    error = Some(e);
                    true
                }
            }
        });
        error.map_or(Ok(()), Err)
    }

    /// Seed a growable on an existing imported frontage lot without waiting for demand.
    pub fn seed_imported_growable(
        &mut self,
        lot: LotID,
        kind: BuildingKind,
        density: Density,
    ) -> Option<BuildingID> {
        let home = if kind == BuildingKind::House {
            Some(residence::prototype(density, 1)?)
        } else {
            None
        };
        let id = self.map_mut().place_growable(lot, kind)?;
        growth::on_growable_placed(self, id, kind);
        if let Some(home) = home.filter(|home| home.density != Density::Low) {
            residence::register(self, id, home);
        }
        Some(id)
    }

    /// Seed a mapped residence whose parcel is outside the generated frontage lots.
    pub fn seed_imported_residence(
        &mut self,
        shape: OBB,
        road: RoadID,
        density: Density,
    ) -> Option<BuildingID> {
        let home = residence::prototype(density, 1)?;
        let id = self.map_mut().build_special_building(
            &shape,
            BuildingKind::House,
            BuildingGen::House,
            None,
            Some(road),
        )?;
        self.write::<BuildingInfos>().insert(id);
        residence::register(self, id, home);
        Some(id)
    }
}

fn import_network(map: &mut Map, city: &ImportedCity) -> Result<ImportedNetwork, String> {
    let mut supplied = BTreeMap::new();
    for junction in &city.junctions {
        if !junction.position.is_finite()
            || junction
                .roundabout_radius
                .is_some_and(|radius| !(10.0..=50.0).contains(&radius))
            || supplied.insert(junction.id, junction).is_some()
        {
            return Err(format!("invalid or duplicate junction {}", junction.id));
        }
    }

    let height = |point: Vec2| {
        if !point.is_finite() {
            return Err("non-finite road sample".to_owned());
        }
        map.environment
            .height(point)
            .ok_or_else(|| format!("road sample {point:?} outside the terrain"))
    };
    let mut used = BTreeMap::<usize, (Vec3, usize, bool)>::new();
    let mut profiles = Vec::with_capacity(city.links.len());
    for (index, link) in city.links.iter().enumerate() {
        let lanes = link.pattern.lanes().count();
        let rail = link
            .pattern
            .lanes()
            .any(|(kind, _, _)| kind == LaneKind::Rail);
        if !(1..=16).contains(&lanes)
            || link.pattern.lanes().any(|(kind, _, limit)| {
                !limit.is_finite() || limit <= 0.0 || kind.is_rail() != rail
            })
            || link.from == link.to
            || link.points.len() < 2
        {
            return Err(format!("invalid imported link {index}"));
        }
        for id in [link.from, link.to] {
            let junction = supplied
                .get(&id)
                .ok_or_else(|| format!("missing junction {id} on link {index}"))?;
            let entry = used.entry(id).or_insert((
                junction.position.z(height(junction.position)?),
                0,
                rail,
            ));
            entry.1 += lanes;
            if entry.1 > MAX_LANES_AT_JUNCTION || entry.2 != rail {
                return Err(format!("mixed-mode or over-capacity junction {id}"));
            }
        }
        let mut points = Vec::with_capacity(link.points.len());
        for &point in &link.points {
            points.push(point.z(height(point)? + ROAD_Z_OFFSET));
        }
        let last = points.len() - 1;
        points[0] = used[&link.from].0 + Vec3::Z * ROAD_Z_OFFSET;
        points[last] = used[&link.to].0 + Vec3::Z * ROAD_Z_OFFSET;
        let points = clean_samples(points);
        let length = points.windows(2).map(|p| p[0].distance(p[1])).sum::<f32>();
        if length < LINK_MIN_METRES || points[0].xy().distance(points.last().unwrap().xy()) < 1.0 {
            return Err(format!(
                "imported link {index} has degenerate geometry (source {:?})",
                link.source_way_ids
            ));
        }
        profiles.push(points);
    }

    let mut junctions = BTreeMap::new();
    for (&key, &(position, _, _)) in &used {
        let id = map.add_intersection(position);
        let junction = &mut map.intersections[id];
        junction.light_policy = supplied[&key].light_policy;
        if let Some(radius) = supplied[&key].roundabout_radius {
            junction.turn_policy.roundabout = Some(RoundaboutPolicy { radius });
        }
        junctions.insert(key, id);
    }
    let mut roads = BTreeMap::new();
    for (index, (link, points)) in city.links.iter().zip(profiles).enumerate() {
        let kind = RoadSegmentKind::Arbitrary(points.into());
        let road = map
            .connect(
                junctions[&link.from],
                junctions[&link.to],
                &link.pattern,
                kind,
            )
            .ok_or_else(|| format!("could not insert imported link {index}"))?;
        roads.insert(index, road);
    }
    map.finalize_import(
        &junctions.values().copied().collect::<Vec<_>>(),
        &roads.values().copied().collect::<Vec<_>>(),
    );
    let short_lanes = validate_geometry(map, &roads, &junctions)?;
    for (&index, &road) in &roads {
        if city.links[index].frontage {
            Lot::generate_along_road(map, road);
        }
    }
    map.check_invariants();
    Ok(ImportedNetwork {
        junctions,
        roads,
        short_lanes,
    })
}

fn clean_samples(mut points: Vec<Vec3>) -> Vec<Vec3> {
    let last = points.pop().expect("validated polyline");
    points.dedup_by(|next, previous| next.xy().distance2(previous.xy()) < 0.04);
    while points.len() > 1 && points.last().unwrap().xy().distance2(last.xy()) < 0.04 {
        points.pop();
    }
    points.push(last);
    points
}

fn validate_geometry(
    map: &Map,
    roads: &BTreeMap<usize, RoadID>,
    junctions: &BTreeMap<usize, IntersectionID>,
) -> Result<Vec<(usize, f32)>, String> {
    let mut short_lanes = Vec::new();
    for (&index, &id) in roads {
        let road = &map.roads[id];
        let mut shortest = f32::INFINITY;
        for (id, _) in road.lanes_iter() {
            let lane = &map.lanes[id];
            if lane.points.n_points() < 2
                || !lane.points.iter().all(|point| point.is_finite())
                || lane.points.length() < 1.0
            {
                return Err(format!(
                    "unusable {:?} lane {id:?} after trimming imported link {index}: lane {:.2} m, road {:.2} m at {:?}",
                    lane.kind, lane.points.length(), road.length(), road.points().first()
                ));
            }
            if matches!(lane.kind, LaneKind::Driving | LaneKind::Rail) {
                shortest = shortest.min(lane.points.length());
            }
        }
        if shortest < LINK_MIN_METRES {
            short_lanes.push((index, shortest));
        }
    }
    for &id in junctions.values() {
        for turn in map.intersections[id].turns() {
            let start = map.lanes[turn.id.src].get_inter_node_pos(id);
            let end = map.lanes[turn.id.dst].get_inter_node_pos(id);
            if !turn.points.iter().all(|point| point.is_finite())
                || !turn.points.first().is_close(start, 0.01)
                || !turn.points.last().is_close(end, 0.01)
            {
                return Err(format!(
                    "turn {:?} disagrees with finalized lane geometry",
                    turn.id
                ));
            }
        }
    }
    Ok(short_lanes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::LanePatternBuilder;
    use crate::tests::TestCtx;
    use crate::{MapPreset, NewGameOptions, Simulation};
    use geom::vec2;

    #[test]
    fn osm_ordinary_junction_edits_continue_after_reload() {
        assert_junction_edit_continuation(false, 28.0);
    }

    #[test]
    fn osm_imported_junction_edits_continue_after_reload_without_binding_links() {
        assert_junction_edit_continuation(true, 28.5);
    }

    fn assert_junction_edit_continuation(imported: bool, length: f32) {
        let _ctx = TestCtx::new();
        let options = NewGameOptions {
            preset: MapPreset::Flat,
            terrain_chunks: 16,
            ..Default::default()
        };
        let a = vec2(500.0, 500.0);
        let b = a + vec2(length, 0.0);
        let pattern = LanePatternBuilder::default()
            .parking(false)
            .sidewalks(false)
            .build();
        let (mut sim, road) = if imported {
            let city = ImportedCity {
                junctions: [a, b]
                    .into_iter()
                    .enumerate()
                    .map(|(id, position)| ImportedJunction {
                        id,
                        position,
                        roundabout_radius: None,
                        light_policy: LightPolicy::Auto,
                    })
                    .collect(),
                links: vec![ImportedLink {
                    source_way_ids: vec![],
                    from: 0,
                    to: 1,
                    points: vec![a, b],
                    pattern: pattern.clone(),
                    frontage: false,
                }],
                boundary_stubs: vec![],
            };
            let (sim, network) = Simulation::from_imported_city(&city, &options).unwrap();
            (sim, network.roads[&0])
        } else {
            let mut sim = Simulation {
                world: Default::default(),
                resources: Default::default(),
            };
            unsafe {
                for system in &*addr_of!(INIT_FUNCS) {
                    (system.f)(&mut sim);
                }
            }
            WorldCommand::bootstrap_import(
                &mut sim,
                SimulationOptions {
                    preset: options.preset,
                    terrain_size: options.terrain_chunks,
                    save_replay: false,
                    ..Default::default()
                },
            )
            .unwrap();
            let road = {
                let mut map = sim.map_mut();
                let ground = map.environment.height(a).unwrap();
                let long_start = map.add_intersection(vec2(500.0, 800.0).z(ground));
                let long_end = map.add_intersection(vec2(600.0, 800.0).z(ground));
                map.connect(long_start, long_end, &pattern, RoadSegmentKind::Straight)
                    .unwrap();
                map.update_intersection(long_start, |junction| {
                    junction.turn_policy.back_turns = true;
                });
                let src = map.add_intersection(a.z(ground));
                let dst = map.add_intersection(b.z(ground));
                let points = [a, b].map(|p| p.z(ground + ROAD_Z_OFFSET));
                map.connect(
                    src,
                    dst,
                    &pattern,
                    RoadSegmentKind::Arbitrary(points.to_vec().into()),
                )
                .unwrap()
            };
            (sim, road)
        };
        let junction = sim.map().roads()[road].src;
        let edit_policy = |sim: &mut Simulation| {
            let (mut turn, light) = {
                let map = sim.map();
                let inter = &map.intersections()[junction];
                (inter.turn_policy, inter.light_policy)
            };
            turn.back_turns = !turn.back_turns;
            sim.apply_commands([&WorldCommand::MapUpdateIntersectionPolicy {
                inter: junction,
                turn,
                light,
            }]);
        };
        let length_after = |sim: &Simulation| sim.map().roads()[road].interfaced_points().length();
        edit_policy(&mut sim);
        assert!((length_after(&sim) - (length - 18.0)).abs() < 0.01);

        let root = std::env::temp_dir().join(format!(
            "egr-osm-continuation-{}-{imported}",
            std::process::id()
        ));
        let id = crate::SlotId::new("continuation").unwrap();
        sim.save_to_slot(&root, &id, crate::SnapshotRevision::new(1))
            .unwrap();
        let mut loaded = Simulation::load_from_slot(&root, &id).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(length_after(&sim), length_after(&loaded));
        for sim in [&mut sim, &mut loaded] {
            {
                let mut map = sim.map_mut();
                let pos = a + vec2(75.0, 30.0);
                let ground = map.environment.height(pos).unwrap();
                let spur = map.add_intersection(pos.z(ground));
                map.connect(junction, spur, &pattern, RoadSegmentKind::Straight)
                    .unwrap();
            }
            edit_policy(sim);
        }
        let continuous = length_after(&sim);
        let resumed = length_after(&loaded);
        assert_eq!(continuous, resumed, "interfaced lane length");
        assert_eq!(continuous >= 9.99, imported);
        assert_eq!(sim.hashes(), loaded.hashes());
    }

    #[test]
    fn osm_import_retains_short_link_geometry_after_reload_and_policy_edit() {
        let _ctx = TestCtx::new();
        let positions = [
            (200.0, 200.0),
            (212.0, 200.0),
            (100.0, 200.0),
            (312.0, 200.0),
            (200.0, 100.0),
            (200.0, 300.0),
            (212.0, 100.0),
            (212.0, 300.0),
        ];
        let city = ImportedCity {
            junctions: positions
                .iter()
                .enumerate()
                .map(|(id, &(x, y))| ImportedJunction {
                    id,
                    position: vec2(x, y),
                    roundabout_radius: None,
                    light_policy: LightPolicy::Auto,
                })
                .collect(),
            links: [(0, 1), (0, 2), (1, 3), (0, 4), (0, 5), (1, 6), (1, 7)]
                .into_iter()
                .map(|(from, to)| ImportedLink {
                    source_way_ids: vec![],
                    from,
                    to,
                    points: [from, to]
                        .map(|i| vec2(positions[i].0, positions[i].1))
                        .to_vec(),
                    pattern: LanePatternBuilder::default().build(),
                    frontage: false,
                })
                .collect(),
            boundary_stubs: vec![],
        };
        let (sim, network) = Simulation::from_imported_city(
            &city,
            &NewGameOptions {
                preset: MapPreset::Flat,
                terrain_chunks: 16,
                ..Default::default()
            },
        )
        .unwrap();
        let id = crate::SlotId::new("osm_edit").unwrap();
        let root = std::env::temp_dir().join(format!("egr-osm-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        sim.save_to_slot(&root, &id, crate::SnapshotRevision::new(1))
            .unwrap();
        let mut loaded = Simulation::load_from_slot(&root, &id).unwrap();
        std::fs::remove_dir_all(root).unwrap();

        let junction = network.junctions[&1];
        let (turn, light) = {
            let map = loaded.map();
            let inter = &map.intersections[junction];
            (inter.turn_policy.clone(), inter.light_policy)
        };
        loaded.apply_commands([&WorldCommand::MapUpdateIntersectionPolicy {
            inter: junction,
            turn,
            light,
        }]);
        let map = loaded.map();
        let road = &map.roads[network.roads[&0]];
        assert!(road.interfaced_points().length() >= 9.99);
        validate_geometry(&map, &network.roads, &network.junctions).unwrap();
    }

    #[test]
    fn osm_import_finalizes_network_without_starter_assets() {
        let _ctx = TestCtx::new();
        let city = ImportedCity {
            junctions: [100.0, 220.0, 340.0]
                .into_iter()
                .enumerate()
                .map(|(id, x)| ImportedJunction {
                    id,
                    position: vec2(x, 400.0),
                    roundabout_radius: None,
                    light_policy: LightPolicy::Auto,
                })
                .collect(),
            links: (0..2)
                .map(|id| ImportedLink {
                    source_way_ids: vec![id as u64],
                    from: id,
                    to: id + 1,
                    points: vec![
                        vec2(100.0 + id as f32 * 120.0, 400.0),
                        vec2(220.0 + id as f32 * 120.0, 400.0),
                    ],
                    pattern: LanePatternBuilder::new().build(),
                    frontage: id == 0,
                })
                .collect(),
            boundary_stubs: vec![],
        };
        let options = NewGameOptions {
            preset: MapPreset::Flat,
            seed: 42,
            terrain_chunks: 16,
            starting_money_bucks: 1_000_000,
            progression: crate::economy::progression::ProgressionMode::FreeBuild,
            lot_layout: crate::map::LotLayout::Varied,
        };
        let (sim, report) = Simulation::from_imported_city(&city, &options).unwrap();
        let map = sim.map();
        assert_eq!(map.roads().len(), 2);
        assert_eq!(map.buildings().len(), 0);
        assert_eq!(
            sim.read::<crate::economy::Government>().money,
            prototypes::Money::new_bucks(1_000_000)
        );
        assert_eq!(report.roads.len(), 2);
        assert!(map
            .lots()
            .values()
            .all(|lot| lot.parent == report.roads[&0]));
        assert!(!map.lots().is_empty());
        assert!(map
            .intersections()
            .values()
            .any(|junction| junction.turns().next().is_some()));
        map.check_invariants();
    }
}
