use std::time::{Duration, Instant};

use engine::heightmap::{GroundOverlayDisc, GroundOverlayImage, HeightmapRender};
use engine::{FrameContext, GfxContext};
use geom::Camera;
use simulation::map::{Map, SubscriberChunkID};

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
    last_overlay_upload: Option<GroundOverlayUpload>,
}

impl TerrainRender {
    pub fn new(gfx: &mut GfxContext, map: &Map) -> Self {
        let (w, h) = map.environment().size();

        let terrain = HeightmapRender::new(gfx, w as u32, h as u32);

        Self {
            heightmap: terrain,
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

    pub(super) fn upload_bucket(
        &mut self,
        gfx: &mut GfxContext,
        map: &Map,
        bucket: SubscriberChunkID,
    ) {
        for id in bucket.convert() {
            if let Some(chunk) = map.environment().get_chunk(id) {
                self.heightmap
                    .update_chunk(gfx, (id.0 as u32, id.1 as u32), chunk);
            }
        }
    }

    pub(super) fn invalidate_normals(&mut self, gfx: &GfxContext) {
        self.heightmap.invalidate_height_normals(gfx);
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
