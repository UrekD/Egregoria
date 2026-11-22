use engine::heightmap::{GroundOverlayDisc, GroundOverlayImage};
use engine::{Context, FrameContext, GfxContext, Water};
#[cfg(feature = "debug-ground-overlay")]
use geom::{vec2, LinearColor, AABB};
use geom::{Camera, Circle, InfiniteFrustrum, Intersect3};
use map_mesh::MapMeshHandler;
use simulation::map::{Lane, LaneID, LaneKind, Map, ProjectFilter, ProjectKind, TrafficBehavior};
use simulation::Simulation;
use terrain::TerrainRender;

use crate::rendering::immediate::ImmediateDraw;
use crate::rendering::map_rendering::lamps::LampsRender;
use crate::rendering::map_rendering::trees::TreesRender;

mod lamps;
mod map_mesh;
mod terrain;
mod trees;

/// Render the entire map including the terrain, trees, water etc
pub struct MapRenderer {
    pub meshb: MapMeshHandler,
    pub terrain: TerrainRender,
    pub trees: TreesRender,
    pub water: Water,
    pub lamps: LampsRender,
}

pub struct MapRenderOptions {
    pub show_arrows: bool,
    pub show_lots: bool,
}

impl MapRenderer {
    pub fn new(gfx: &mut GfxContext, sim: &Simulation) -> Self {
        defer!(log::info!("finished init of road render"));
        let renderer = MapRenderer {
            meshb: MapMeshHandler::new(gfx, sim),
            trees: TreesRender::new(gfx, &sim.map()),
            terrain: TerrainRender::new(gfx, sim),
            water: Water::new(gfx, sim.map().environment.bounds()),
            lamps: LampsRender::new(&sim.map()),
        };
        #[cfg(feature = "debug-ground-overlay")]
        if std::env::var("EGR_DEBUG_GROUND_OVERLAY").as_deref() == Ok("1") {
            let mut renderer = renderer;
            renderer.set_debug_ground_overlay(gfx, sim.map().environment.bounds());
            return renderer;
        }
        renderer
    }

    pub fn update(&mut self, sim: &Simulation, ctx: &mut Context) {
        profiling::scope!("update map renderer");
        let map = sim.map();
        self.lamps.update(&map, ctx);
        self.terrain.update(ctx, &map);
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
        self.terrain.set_overlay_preview(gfx, preview);
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
        self.set_overlay_preview(
            gfx,
            Some(GroundOverlayDisc {
                center: center + vec2(0.28, -0.10) * radius,
                radius: radius * 0.35,
                color: LinearColor::new(0.02, 0.60, 1.0, 0.75),
            }),
        );
    }

    pub fn render(
        &mut self,
        map: &Map,
        time: u32,
        cam: &Camera,
        options: MapRenderOptions,
        draw: &mut ImmediateDraw,
        ctx: &mut FrameContext<'_>,
    ) {
        profiling::scope!("render map renderer");
        self.terrain.draw(cam, ctx);

        self.trees.draw(map, cam, ctx);

        self.meshb.latest_mesh(map, options, ctx);

        Self::signals_render(map, time, cam, &ctx.gfx.frustrum, draw);

        ctx.draw(self.water.clone());
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
        map: &Map,
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
        map: &Map,
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
