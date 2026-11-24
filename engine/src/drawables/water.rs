use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    CompiledModule, Drawable, GfxContext, Mesh, MeshBuilder, MeshVertex, PipelineBuilder,
    PipelineKey, Texture, TextureBuilder, TL,
};
use geom::Matrix4;
use wgpu::{RenderPass, RenderPipeline, ShaderModuleDescriptor, ShaderSource};

#[derive(Clone)]
pub struct Water {
    mesh: Arc<WaterMesh>,
    n_indices: u32,
    overview: Option<Arc<WaterOverviewAtlas>>,
}

struct WaterMesh {
    prepared: Mutex<Option<MeshBuilder<false>>>,
    uploaded: OnceLock<Option<Mesh>>,
}

/// CPU-staged water coverage. Texture allocation and region writes run when the
/// engine records the water pass, after the native simulation guard is released.
pub struct WaterOverviewAtlas {
    size: [u32; 2],
    region_side: u32,
    pending: Mutex<BTreeMap<[u32; 2], Vec<u8>>>,
    gpu: OnceLock<(Texture, wgpu::BindGroup)>,
}

impl WaterOverviewAtlas {
    pub fn new(size: [u32; 2], region_side: u32) -> Self {
        Self {
            size,
            region_side,
            pending: Mutex::default(),
            gpu: OnceLock::new(),
        }
    }

    pub fn stage(&self, origin: [u32; 2], rgba: Vec<u8>) {
        debug_assert_eq!(rgba.len(), (self.region_side.pow(2) * 4) as usize);
        self.pending.lock().unwrap().insert(origin, rgba);
    }

    #[cfg(test)]
    fn pending_bytes(&self) -> usize {
        self.pending.lock().unwrap().values().map(Vec::len).sum()
    }

    fn bind_group(&self, gfx: &GfxContext) -> &wgpu::BindGroup {
        let (texture, group) = self.gpu.get_or_init(|| {
            let texture = TextureBuilder::empty(
                self.size[0],
                self.size[1],
                1,
                wgpu::TextureFormat::Rgba8Unorm,
            )
            .with_label("water overview atlas")
            .with_usage(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST)
            .build_no_queue(&gfx.device);
            let layout = gfx
                .get_pipeline(WaterOverviewPipeline)
                .get_bind_group_layout(2);
            let group = Texture::multi_bindgroup(&[&texture], &gfx.device, &layout);
            (texture, group)
        });
        let side = self.region_side;
        for ([x, y], rgba) in std::mem::take(&mut *self.pending.lock().unwrap()) {
            gfx.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &texture.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &rgba,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(side * 4),
                    rows_per_image: Some(side),
                },
                wgpu::Extent3d {
                    width: side,
                    height: side,
                    depth_or_array_layers: 1,
                },
            );
        }
        group
    }
}

#[derive(Hash)]
pub struct WaterPipeline;

#[derive(Hash)]
struct WaterOverviewPipeline;

#[derive(Hash)]
struct WaterDepthPipeline;

impl PipelineKey for WaterDepthPipeline {
    fn build(
        &self,
        gfx: &GfxContext,
        mut mk_module: impl FnMut(&str, &[&str]) -> CompiledModule,
    ) -> RenderPipeline {
        let vert = mk_module("lit_mesh.vert", &[]);
        gfx.depth_pipeline(&[MeshVertex::desc()], &vert, None, false)
    }
}

impl Water {
    pub fn prepare(mesh: MeshBuilder<false>) -> Self {
        Self {
            n_indices: mesh.lods()[0].n_indices as u32,
            overview: None,
            mesh: Arc::new(WaterMesh {
                prepared: Mutex::new(Some(mesh)),
                uploaded: OnceLock::new(),
            }),
        }
    }

    pub fn with_overview(mut self, overview: Option<&Arc<WaterOverviewAtlas>>) -> Self {
        self.overview = overview.cloned();
        self
    }

    fn draw_mesh<'a>(&'a self, gfx: &'a GfxContext, rp: &mut RenderPass<'a>) -> bool {
        // The engine records passes after the game releases its simulation read lock.
        let mesh = self
            .mesh
            .uploaded
            .get_or_init(|| self.mesh.prepared.lock().unwrap().take()?.build(gfx));
        let Some(mesh) = mesh else { return false };
        rp.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
        rp.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        rp.draw_indexed(0..self.n_indices, 0, 0..1);
        true
    }
}

impl PipelineKey for WaterPipeline {
    fn build(
        &self,
        gfx: &GfxContext,
        mut mk_module: impl FnMut(&str, &[&str]) -> CompiledModule,
    ) -> RenderPipeline {
        build_water_pipeline(gfx, &mk_module("lit_mesh.vert", &[]), false)
    }
}

impl PipelineKey for WaterOverviewPipeline {
    fn build(
        &self,
        gfx: &GfxContext,
        mut mk_module: impl FnMut(&str, &[&str]) -> CompiledModule,
    ) -> RenderPipeline {
        build_water_pipeline(gfx, &mk_module("lit_mesh.vert", &[]), true)
    }
}

fn build_water_pipeline(gfx: &GfxContext, vert: &CompiledModule, overview: bool) -> RenderPipeline {
    let overview_shader = if overview {
        r#"
@group(2) @binding(0) var coverage: texture_2d<f32>;

fn apply_overview(color: vec3<f32>, texel: vec2<f32>) -> vec3<f32> {
    let pooled = textureLoad(coverage, vec2<i32>(floor(texel)), 0);
    let highlight = pooled.rgb * 1.65;
    return mix(color, highlight, pooled.a * 0.85);
}
"#
    } else {
        "fn apply_overview(color: vec3<f32>, _texel: vec2<f32>) -> vec3<f32> { return color; }"
    };
    let frag = gfx.device.create_shader_module(ShaderModuleDescriptor {
        label: Some("water surface"),
        source: ShaderSource::Wgsl(
            [
                include_str!("../../../assets/shaders/render_params.wgsl"),
                overview_shader,
                r#"
@group(0) @binding(0) var<uniform> params: RenderParams;

@fragment
fn frag(
    @location(0) tint: vec4<f32>,
    @location(1) _normal: vec3<f32>,
    @location(2) _tangent: vec4<f32>,
    @location(3) world: vec3<f32>,
    @location(4) texel: vec2<f32>,
) -> @location(0) vec4<f32> {
    let time = params.time_always;
    let a = sin(dot(world.xy, vec2(0.08, 0.12)) + time * 1.3);
    let b = sin(dot(world.xy, vec2(-0.13, 0.03)) - time * 0.8);
    let view = normalize(params.cam_pos.xyz - world);
    let reflection = pow(1.0 - clamp(view.z, 0.0, 1.0), 3.0);
    let daylight = 0.42 + 0.58 * smoothstep(-0.1, 0.35, params.sun.z);
    let ripple = 0.96 + 0.04 * a * b;
    let color = tint.rgb * daylight * ripple + reflection * daylight * vec3(0.07, 0.10, 0.14);
    return vec4(apply_overview(color, texel), tint.a);
}
"#,
            ]
            .concat()
            .into(),
        ),
    });

    // GfxContext rebuilds the fog bind group from slot 3 when its framebuffers change.
    let float_texture = Texture::bindgroup_layout(&gfx.device, [TL::Float]);
    let depth_texture = Texture::bindgroup_layout(
        &gfx.device,
        [if gfx.samples > 1 {
            TL::NonfilterableFloatMultisampled
        } else {
            TL::NonfilterableFloat
        }],
    );
    PipelineBuilder::color(
        "water",
        &[
            &gfx.render_params.layout,
            &depth_texture,
            &float_texture,
            &float_texture,
        ],
        &[MeshVertex::desc()],
        vert,
        &frag,
        gfx.sc_desc.format,
    )
    .with_samples(gfx.samples)
    .build(&gfx.device)
}

impl Drawable for Water {
    fn draw<'a>(&'a self, gfx: &'a GfxContext, rp: &mut RenderPass<'a>) {
        let atlas = self.overview.as_ref();
        if atlas.is_some() {
            rp.set_pipeline(gfx.get_pipeline(WaterOverviewPipeline));
        } else {
            rp.set_pipeline(gfx.get_pipeline(WaterPipeline));
        }
        rp.set_bind_group(0, &gfx.render_params.bg, &[]);
        rp.set_bind_group(1, &gfx.fbos.depth_bg, &[]);
        rp.set_bind_group(
            2,
            atlas.map_or(&gfx.water_bg, |atlas| atlas.bind_group(gfx)),
            &[],
        );
        rp.set_bind_group(3, &gfx.water_bg, &[]);
        if self.draw_mesh(gfx, rp) {
            gfx.perf.drawcall(self.n_indices / 3);
        }
    }

    fn draw_depth<'a>(
        &'a self,
        gfx: &'a GfxContext,
        rp: &mut RenderPass<'a>,
        shadow_cascade: Option<&Matrix4>,
    ) {
        if shadow_cascade.is_some() {
            return;
        }
        rp.set_pipeline(gfx.get_pipeline(WaterDepthPipeline));
        rp.set_bind_group(0, &gfx.render_params.bg, &[]);
        if self.draw_mesh(gfx, rp) {
            gfx.perf.depth_drawcall(self.n_indices / 3, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn water_w4_atlas_writes_only_the_latest_dirty_regions() {
        let atlas = WaterOverviewAtlas::new([640, 640], 8);
        atlas.stage([8, 0], vec![255; 256]);
        atlas.stage([16, 0], vec![255; 256]);
        atlas.stage([8, 0], vec![0; 256]);
        assert_eq!(atlas.pending_bytes(), 512);
        assert_eq!(atlas.pending.lock().unwrap()[&[8, 0]], vec![0; 256]);
        assert!(atlas.gpu.get().is_none());
    }
}
