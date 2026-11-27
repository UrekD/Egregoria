use engine::frame_profile::CpuPhase;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use engine::heightmap::{GroundOverlayDisc, GroundOverlayImage};
use engine::{Context, FrameContext, GfxContext};
#[cfg(feature = "debug-ground-overlay")]
use geom::{vec2, LinearColor, AABB};
use geom::{Camera, Circle, InfiniteFrustrum, Intersect3};
use map_mesh::MapMeshHandler;
use simulation::map::{
    Lane, LaneID, LaneKind, MapSubscribers, ProjectFilter, ProjectKind, RenderMap,
    SubscriberChunkID, TrafficBehavior, UpdateType,
};
use terrain::TerrainRender;

use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::map_rendering::lamps::LampsRender;
use crate::rendering::map_rendering::trees::TreesRender;

mod lamps;
mod map_mesh;
mod placeholder_color;
mod terrain;
mod trees;
mod water;

pub(crate) use map_mesh::intersection_mesh;
use water::WaterRender;
pub(crate) use water::{water_color, WATER_OVERVIEW_POOL, WATER_TILE_PITCH};

fn active_overlay_preview(
    preview: Option<GroundOverlayDisc>,
    debug_preview: Option<&GroundOverlayDisc>,
) -> Option<GroundOverlayDisc> {
    preview.or_else(|| {
        debug_preview.map(|disc| GroundOverlayDisc {
            center: disc.center,
            radius: disc.radius,
            color: disc.color,
        })
    })
}

#[cfg(test)]
#[test]
fn client_max_ground_preview_restores_debug_disc() {
    let disc = |radius| GroundOverlayDisc {
        center: geom::Vec2::ZERO,
        radius,
        color: geom::LinearColor::WHITE,
    };
    assert_eq!(
        active_overlay_preview(None, Some(&disc(4.0))).map(|x| x.radius),
        Some(4.0)
    );
    assert_eq!(
        active_overlay_preview(Some(disc(2.0)), Some(&disc(4.0))).map(|x| x.radius),
        Some(2.0)
    );
    assert!(active_overlay_preview(None, None).is_none());
}

/// Render the entire map including the terrain, trees, water etc
pub struct MapRenderer {
    pub meshb: MapMeshHandler,
    pub terrain: TerrainRender,
    pub trees: TreesRender,
    water: WaterRender,
    pub lamps: LampsRender,
    changes: MapSubscribers,
    debug_preview: Option<GroundOverlayDisc>,
}

pub struct MapRenderOptions {
    pub show_arrows: bool,
    pub show_lots: bool,
}

impl MapRenderer {
    pub fn new(gfx: &mut GfxContext, map: &RenderMap) -> Self {
        defer!(log::info!("finished init of road render"));
        let changes = MapSubscribers::default();
        let renderer = MapRenderer {
            meshb: MapMeshHandler::new(gfx, &changes),
            trees: TreesRender::new(gfx),
            terrain: TerrainRender::new(gfx, map),
            water: WaterRender::new(&changes),
            lamps: LampsRender::new(),
            changes,
            debug_preview: None,
        };
        #[cfg(feature = "debug-ground-overlay")]
        if std::env::var("EGR_DEBUG_GROUND_OVERLAY").as_deref() == Ok("1") {
            let mut renderer = renderer;
            renderer.set_debug_ground_overlay(gfx, map.environment().bounds());
            return renderer;
        }
        renderer
    }

    pub(crate) fn stage(
        &mut self,
        map: &RenderMap,
        remaining: &mut BTreeMap<SubscriberChunkID, UpdateType>,
        gfx: &mut GfxContext,
        full_refresh: bool,
    ) {
        let deadline = Instant::now() + Duration::from_millis(4);
        while let Some((chunk, kinds)) = remaining.pop_first() {
            if kinds.intersects(UpdateType::Road | UpdateType::RoadSupport | UpdateType::Building) {
                self.meshb.stage_chunk(map, chunk, kinds, gfx);
            }
            if kinds.contains(UpdateType::Terrain) {
                self.trees.stage_chunk(map, chunk, gfx);
                if full_refresh {
                    self.terrain.upload_bucket(gfx, map, chunk);
                }
            }
            if kinds.contains(UpdateType::Road) {
                self.lamps.stage_chunk(map, chunk);
            }
            if Instant::now() >= deadline {
                break;
            }
        }
    }

    pub(crate) fn promote(
        &mut self,
        map: &RenderMap,
        chunks: &BTreeMap<SubscriberChunkID, UpdateType>,
        ctx: &mut Context,
        full_refresh: bool,
    ) {
        self.meshb.promote();
        self.trees.promote();
        if full_refresh {
            ctx.gfx.lamplights.reset(&ctx.gfx.device, &ctx.gfx.queue);
            self.changes
                .dispatch_all_terrain(map.environment().chunks().map(|(chunk, _)| chunk));
        }
        let mut terrain_changed = false;
        for (&chunk, &kinds) in chunks {
            self.changes.dispatch_chunk(kinds, chunk);
            if kinds.contains(UpdateType::Terrain) {
                if !full_refresh {
                    self.terrain.upload_bucket(&mut ctx.gfx, map, chunk);
                }
                terrain_changed = true;
            }
        }
        if terrain_changed {
            self.terrain.invalidate_normals(&ctx.gfx);
        }
        self.lamps.promote(ctx);
    }

    /// Submit the atlas from the pre-terrain preparation hook, or clear it with `None`.
    /// Pass `mode_changed` for an immediate mode switch. Retry with the latest
    /// image next frame if an ordinary refresh is throttled. Returns false if
    /// the submission is throttled or rejected.
    pub(crate) fn set_ground_overlay(
        &mut self,
        gfx: &mut GfxContext,
        image: Option<GroundOverlayImage<'_>>,
        mode_changed: bool,
    ) -> bool {
        self.terrain.set_ground_overlay(gfx, image, mode_changed)
    }

    pub(crate) fn set_overlay_preview(
        &mut self,
        gfx: &mut GfxContext,
        preview: Option<GroundOverlayDisc>,
    ) {
        self.terrain.set_overlay_preview(
            gfx,
            active_overlay_preview(preview, self.debug_preview.as_ref()),
        );
    }

    #[cfg(feature = "debug-ground-overlay")]
    fn set_debug_ground_overlay(&mut self, gfx: &mut GfxContext, bounds: AABB) {
        let extent = bounds.ur - bounds.ll;
        let pitch = 32.0;
        let size = [
            (extent.x / pitch).ceil() as u32,
            (extent.y / pitch).ceil() as u32,
        ];
        if size.contains(&0) {
            return;
        }

        let center = bounds.ll + extent * 0.5;
        let radius = extent.x.min(extent.y) * 0.36;
        let mut rgba = Vec::with_capacity((size[0] * size[1] * 4) as usize);
        for y in 0..size[1] {
            for x in 0..size[0] {
                let pos = bounds.ll + (vec2(x as f32, y as f32) + vec2(0.5, 0.5)) * pitch;
                let t = (pos.distance(center) / radius).min(1.0);
                let alpha = 200.0 * (1.0 - t);
                rgba.extend([
                    alpha.round() as u8,
                    (alpha * (0.08 + 0.38 * t)).round() as u8,
                    (alpha * 0.035).round() as u8,
                    alpha.round() as u8,
                ]);
            }
        }
        self.set_ground_overlay(
            gfx,
            Some(GroundOverlayImage {
                origin: bounds.ll,
                extent,
                size,
                rgba: &rgba,
                revision: 0,
            }),
            true,
        );
        self.debug_preview = Some(GroundOverlayDisc {
            center: center + vec2(0.28, -0.10) * radius,
            radius: radius * 0.35,
            color: LinearColor::new(0.02, 0.60, 1.0, 0.75),
        });
        self.set_overlay_preview(gfx, None);
    }

    pub fn render(
        &mut self,
        map: &RenderMap,
        time: u32,
        cam: &Camera,
        options: MapRenderOptions,
        draw: &mut ImmediateDraw,
        ctx: &mut FrameContext<'_>,
    ) {
        profiling::scope!("render map renderer");
        let map = &sim.map();
        let map_start = ctx.gfx.profile_timer();
        self.terrain.draw(cam, ctx);
        ctx.gfx.profile_elapsed(CpuPhase::MapMesh, map_start);

        let instances_start = ctx.gfx.profile_timer();
        self.trees.draw(map, cam, ctx);
        ctx.gfx.profile_elapsed(CpuPhase::Instances, instances_start);

        let map_start = ctx.gfx.profile_timer();
        self.meshb.draw(map, options, ctx);

        Self::signals_render(map, time, cam, &ctx.gfx.frustrum, draw);
        ctx.gfx.profile_elapsed(CpuPhase::MapMesh, map_start);

        let water_start = ctx.gfx.profile_timer();
        self.water.draw(map.environment(), ctx);
        ctx.gfx.profile_elapsed(CpuPhase::Water, water_start);
        if let Some(profile) = &mut ctx.gfx.frame_profile {
            profile.pending_water_chunks = Some(self.water.pending_chunks());
        }
    }

    fn render_lane_signals(n: &Lane, draw: &mut ImmediateDraw, time: u32) {
        if n.control.is_always() {
            return;
        }

        let dir = n.orientation_from(n.dst);
        let dir_perp = dir.perpendicular();

        let r_center = n.points.last() + (dir_perp * -5.2 + dir * -1.5).z(0.02);

        if n.control.is_stop_sign() {
            draw.mesh("stop_sign.glb", r_center, dir_perp.z(0.0));
            return;
        }

        let mesh = match n.control.get_behavior(time) {
            TrafficBehavior::RED | TrafficBehavior::STOP => "traffic_light_red.glb",
            TrafficBehavior::ORANGE => "traffic_light_orange.glb",
            TrafficBehavior::GREEN => "traffic_light_green.glb",
        };

        draw.mesh(mesh, r_center, dir_perp.z(0.0));
    }

    fn render_lanes(
        map: &RenderMap,
        lanes: impl Iterator<Item = (LaneID, LaneKind)>,
        draw: &mut ImmediateDraw,
        time: u32,
    ) {
        let mut peek = lanes.peekable();

        while let Some((lane_id, kind)) = peek.next() {
            let peek = peek.peek();
            if let Some((_, peek_kind)) = peek {
                if peek_kind == &kind {
                    continue;
                }
            }
            let Some(lane) = map.lanes().get(lane_id) else {
                continue;
            };
            Self::render_lane_signals(lane, draw, time);
        }
    }

    fn signals_render(
        map: &RenderMap,
        time: u32,
        cam: &Camera,
        frustrum: &InfiniteFrustrum,
        draw: &mut ImmediateDraw,
    ) {
        let pos = cam.pos;

        for kind in map
            .spatial_map()
            .query(Circle::new(pos.xy(), 200.0), ProjectFilter::ROAD)
        {
            let ProjectKind::Road(id) = kind else {
                continue;
            };
            let Some(r) = map.roads().get(id) else {
                continue;
            };
            if !frustrum.intersects(&r.points.bbox().expand(r.width)) {
                continue;
            }

            if !r.lanes_iter().all(|(_, kind)| kind.is_rail()) {
                let has_sidewalks = r.has_sidewalks();
                let offset = if has_sidewalks {
                    LaneKind::Walking.width()
                } else {
                    0.0
                };
                let w = r.width * 0.5 - offset;
                for (point, dir) in r.interfaced_points().equipoints_dir(45.0, true) {
                    draw.mesh("streetlamp.glb", point - dir.perp_up() * w, dir.perp_up());
                }
            }

            Self::render_lanes(
                map,
                r.outgoing_lanes_from(r.dst).iter().copied(),
                draw,
                time,
            );
            Self::render_lanes(
                map,
                r.outgoing_lanes_from(r.src).iter().copied(),
                draw,
                time,
            );
        }
    }
}
