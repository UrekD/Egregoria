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

impl GroundOverlayUpload {
    fn submit(
        last: &mut Option<Self>,
        image: GroundOverlayImage<'_>,
        mode_changed: bool,
        upload: impl FnOnce(GroundOverlayImage<'_>) -> bool,
    ) -> bool {
        let already_uploaded = last
            .as_ref()
            .is_some_and(|last| last.revision == image.revision && last.size == image.size);
        if already_uploaded {
            return upload(image);
        }

        let now = Instant::now();
        if !mode_changed
            && last
                .as_ref()
                .is_some_and(|last| now.duration_since(last.at) < GROUND_OVERLAY_UPLOAD_INTERVAL)
        {
            return false;
        }
        let pending = Self {
            revision: image.revision,
            size: image.size,
            at: now,
        };
        if !upload(image) {
            return false;
        }
        *last = Some(pending);
        true
    }
}

pub struct TerrainRender {
    heightmap: HeightmapRender<CSIZE, CRESO>,
    terrain_sub: MapSubscriber,
    last_overlay_upload: Option<GroundOverlayUpload>,
}

impl TerrainRender {
    pub fn new(gfx: &mut GfxContext, sim: &Simulation) -> Self {
        crate::inputmap::reset_deck_picker_cache();
        let (w, h) = sim.map().environment().size();

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
        GroundOverlayUpload::submit(
            &mut self.last_overlay_upload,
            image,
            mode_changed,
            |image| self.heightmap.set_ground_overlay(gfx, image),
        )
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
        let ter = map.environment();

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
                let Some(chunk) = ter.get_chunk(chunkid) else {
                    continue;
                };
                changed = true;
                self.heightmap.update_chunk(
                    &mut ctx.gfx,
                    (chunkid.0 as u32, chunkid.1 as u32),
                    chunk,
                );
            }
        }

        if changed {
            self.heightmap.invalidate_height_normals(&ctx.gfx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geom::Vec2;

    fn overlay_image(revision: u64) -> GroundOverlayImage<'static> {
        GroundOverlayImage {
            origin: Vec2::ZERO,
            extent: geom::vec2(1.0, 1.0),
            size: [1, 1],
            rgba: &[0; 4],
            revision,
        }
    }

    #[test]
    fn ground_overlay_rejection_keeps_upload_history() {
        let mut last = None;
        assert!(!GroundOverlayUpload::submit(
            &mut last,
            overlay_image(1),
            false,
            |_| false
        ));
        assert!(last.is_none());
        assert!(GroundOverlayUpload::submit(
            &mut last,
            overlay_image(2),
            false,
            |_| true
        ));

        let accepted_at = last.as_ref().unwrap().at;
        assert!(!GroundOverlayUpload::submit(
            &mut last,
            overlay_image(3),
            true,
            |_| false
        ));
        let accepted = last.unwrap();
        assert_eq!(accepted.revision, 2);
        assert_eq!(accepted.at, accepted_at);
    }
}
