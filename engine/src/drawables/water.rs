use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    CompiledModule, Drawable, GfxContext, Mesh, MeshBuilder, MeshVertex, PipelineBuilder,
    PipelineKey, Texture, TL,
};
use geom::Matrix4;
use wgpu::{RenderPass, RenderPipeline, ShaderModuleDescriptor, ShaderSource};

#[derive(Clone)]
pub struct Water {
    mesh: Arc<WaterMesh>,
    n_indices: u32,
}

struct WaterMesh {
    prepared: Mutex<Option<MeshBuilder<false>>>,
    uploaded: OnceLock<Option<Mesh>>,
}

#[derive(Hash)]
pub struct WaterPipeline;

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
            mesh: Arc::new(WaterMesh {
                prepared: Mutex::new(Some(mesh)),
                uploaded: OnceLock::new(),
            }),
        }
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
        let vert = &mk_module("lit_mesh.vert", &[]);
        let frag = gfx.device.create_shader_module(ShaderModuleDescriptor {
            label: Some("water surface"),
            source: ShaderSource::Wgsl(
                concat!(
                    include_str!("../../../assets/shaders/render_params.wgsl"),
                    r#"
@group(0) @binding(0) var<uniform> params: RenderParams;

@fragment
fn frag(
    @location(0) tint: vec4<f32>,
    @location(1) _normal: vec3<f32>,
    @location(2) _tangent: vec4<f32>,
    @location(3) world: vec3<f32>,
    @location(4) _uv: vec2<f32>,
) -> @location(0) vec4<f32> {
    let time = params.time_always;
    let a = sin(dot(world.xy, vec2(0.08, 0.12)) + time * 1.3);
    let b = sin(dot(world.xy, vec2(-0.13, 0.03)) - time * 0.8);
    let view = normalize(params.cam_pos.xyz - world);
    let reflection = pow(1.0 - clamp(view.z, 0.0, 1.0), 3.0);
    let daylight = 0.42 + 0.58 * smoothstep(-0.1, 0.35, params.sun.z);
    let ripple = 0.96 + 0.04 * a * b;
    let color = tint.rgb * daylight * ripple + reflection * daylight * vec3(0.07, 0.10, 0.14);
    return vec4(color, tint.a);
}
"#,
                )
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
}

impl Drawable for Water {
    fn draw<'a>(&'a self, gfx: &'a GfxContext, rp: &mut RenderPass<'a>) {
        rp.set_pipeline(gfx.get_pipeline(WaterPipeline));
        rp.set_bind_group(0, &gfx.render_params.bg, &[]);
        rp.set_bind_group(1, &gfx.fbos.depth_bg, &[]);
        rp.set_bind_group(2, &gfx.water_bg, &[]);
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
