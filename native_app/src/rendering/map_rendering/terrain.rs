use std::time::{Duration, Instant};

use engine::heightmap::{GroundOverlayDisc, GroundOverlayImage, HeightmapRender};
use engine::{Context, FrameContext, GfxContext};
use geom::Camera;
use simulation::map::{Map, MapSubscriber, UpdateType};
use simulation::Simulation;

const CSIZE: u32 = simulation::map::Heightmap::SIZE;
const CRESO: usize = simulation::map::Heightmap::RESOLUTION;
const GROUND_OVERLAY_UPLOAD_INTERVAL: Duration = Duration::from_millis(500);

struct GroundOverlayUpload {
    revision: u64,
    size: [u32; 2],
    at: Instant,
}

pub struct TerrainRender {
    heightmap: HeightmapRender<CSIZE, CRESO>,
    terrain_sub: MapSubscriber,
    last_overlay_upload: Option<GroundOverlayUpload>,
}

impl TerrainRender {
    pub fn new(gfx: &mut GfxContext, sim: &Simulation) -> Self {
        let (w, h) = sim.map().environment.size();

        let terrain = HeightmapRender::new(gfx, w as u32, h as u32);

        Self {
            heightmap: terrain,
            terrain_sub: sim.map().subscribe(UpdateType::Terrain),
            last_overlay_upload: None,
        }
    }

    pub(super) fn set_ground_overlay(
        &mut self,
        gfx: &mut GfxContext,
        image: Option<GroundOverlayImage<'_>>,
        mode_changed: bool,
    ) -> bool {
        let Some(image) = image else {
            self.last_overlay_upload = None;
            self.heightmap.clear_ground_overlay(gfx);
            return true;
        };
        let already_uploaded = self
            .last_overlay_upload
            .as_ref()
            .is_some_and(|last| last.revision == image.revision && last.size == image.size);
        if !already_uploaded {
            let now = Instant::now();
            if !mode_changed
                && self.last_overlay_upload.as_ref().is_some_and(|last| {
                    now.duration_since(last.at) < GROUND_OVERLAY_UPLOAD_INTERVAL
                })
            {
                return false;
            }
            self.last_overlay_upload = Some(GroundOverlayUpload {
                revision: image.revision,
                size: image.size,
                at: now,
            });
        }
        self.heightmap.set_ground_overlay(gfx, image);
        true
    }

    pub(super) fn set_overlay_preview(
        &mut self,
        gfx: &mut GfxContext,
        preview: Option<GroundOverlayDisc>,
    ) {
        self.heightmap.set_overlay_preview(gfx, preview);
    }

    pub fn draw(&mut self, cam: &Camera, fctx: &mut FrameContext<'_>) {
        self.heightmap.draw_heightmap(cam, fctx);
    }

    pub fn update(&mut self, ctx: &mut Context, map: &Map) {
        let ter = &map.environment;

        if self.terrain_sub.take_cleared() {
            for (chunk_id, chunk) in ter.chunks() {
                self.heightmap.update_chunk(
                    &mut ctx.gfx,
                    (chunk_id.0 as u32, chunk_id.1 as u32),
                    chunk,
                );
            }

            self.heightmap.invalidate_height_normals(&ctx.gfx);
            return;
        }

        let mut changed = false;
        for cell in self.terrain_sub.take_updated_chunks() {
            for chunkid in cell.convert() {
                let chunk =
                    unwrap_retlog!(ter.get_chunk(chunkid), "trying to update nonexistent chunk");

                self.heightmap.update_chunk(
                    &mut ctx.gfx,
                    (chunkid.0 as u32, chunkid.1 as u32),
                    chunk,
                );
            }
            changed = true;
        }

        if changed {
            self.heightmap.invalidate_height_normals(&ctx.gfx);
        }
    }
}
