use common::{FastMap, FastSet};
use engine::{Context, LampLights, LightChunkID};
use flat_spatial::AABBGrid;
use geom::{Vec3, AABB3, V3};
use simulation::map::{Map, ProjectFilter, ProjectKind, SubscriberChunkID};

pub struct LampsRender {
    lamp_memory: FastMap<LightChunkID, Vec<Vec3>>,
    lamp_road_memory: FastMap<SubscriberChunkID, Vec<(LightChunkID, Vec3)>>,
    staged: std::collections::BTreeMap<SubscriberChunkID, Vec<(LightChunkID, Vec3)>>,
}

impl LampsRender {
    pub fn new() -> Self {
        Self {
            lamp_memory: FastMap::default(),
            lamp_road_memory: FastMap::default(),
            staged: Default::default(),
        }
    }

    pub(super) fn stage_chunk(&mut self, map: &Map, chunk: SubscriberChunkID) {
        let mut by_chunk: AABBGrid<(), AABB3> = AABBGrid::new(LampLights::LIGHTCHUNK_SIZE as i32);

            let mut add_light = |p: Vec3| {
                by_chunk.insert(AABB3::centered(p, Vec3::splat(64.0)), ());
            };

            let mut chunk_roads = vec![];
            let mut chunk_inter = vec![];

            map.spatial_map()
                .query(chunk.bbox(), ProjectFilter::ROAD | ProjectFilter::INTER)
                .for_each(|proj| {
                    if SubscriberChunkID::new(proj.canonical_position(map)) != chunk {
                        return;
                    }
                    match proj {
                        ProjectKind::Road(rid) => chunk_roads.push(rid),
                        ProjectKind::Intersection(iid) => chunk_inter.push(iid),
                        _ => unreachable!(),
                    }
                });

            let roads = map.roads();
            let inters = map.intersections();

            for road in chunk_roads {
                let road = &roads[road];
                if road.lanes_iter().all(|(_, kind)| kind.is_rail()) {
                    continue;
                }
                for (point, _) in road.points().equipoints_dir(45.0, true) {
                    add_light(point + 8.0 * V3::Z);
                }
            }
            for i in chunk_inter {
                let i = &inters[i];
                if i.roads
                    .iter()
                    .filter_map(|&rid| map.roads().get(rid))
                    .all(|r| r.lanes_iter().all(|(_, kind)| kind.is_rail()))
                {
                    continue;
                }

                add_light(i.pos + 8.0 * V3::Z);
            }

        let mut lights = Vec::new();
        for (&(x, y), cell) in &by_chunk.storage().cells {
            if x < 0 || y < 0 {
                continue;
            }
            let key = (x as u16, y as u16);
            lights.extend(
                cell.objs
                    .iter()
                    .filter_map(|&(handle, _)| by_chunk.get(handle))
                    .map(|light| (key, light.aabb.center())),
            );
        }
        self.staged.insert(chunk, lights);
    }

    pub fn promote(&mut self, ctx: &mut Context) {
        profiling::scope!("lampposts");

        let mut to_reupload: FastSet<LightChunkID> = Default::default();
        for (chunk, staged) in std::mem::take(&mut self.staged) {
            let lamp_chunk_memory = self.lamp_road_memory.entry(chunk).or_default();
            for (chunk_id, lamp) in lamp_chunk_memory.drain(..) {
                let Some(lamps) = self.lamp_memory.get_mut(&chunk_id) else {
                    continue;
                };
                let Some(idx) = lamps.iter().position(|x| *x == lamp) else {
                    continue;
                };
                to_reupload.insert(chunk_id);
                lamps.swap_remove(idx);
            }

            for (key, lamp) in staged {
                self.lamp_memory.entry(key).or_default().push(lamp);
                lamp_chunk_memory.push((key, lamp));
                to_reupload.insert(key);
            }
        }

        for chunk in to_reupload {
            let lamps = &self.lamp_memory[&chunk];
            ctx.gfx
                .lamplights
                .register_update(chunk, lamps.iter().copied());
        }
    }
}
