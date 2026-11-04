use wgpu::{Extent3d, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureFormat};

use crate::context::GpuContext;
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;

/// 环形槽数(K)与回读延迟(LAG,帧)。帧 N 写槽 N%K、回读槽 (N-LAG)%K → 读写不同槽,同槽复用
/// 间隔 K 帧(早已完成)。LAG 给 GPU LAG 帧(~33ms@30fps)完成那点 trivial 渲染绰绰有余,
/// 回读 poll 对 LAG 帧前提交的槽秒回——把同步回读每帧 ~10ms 的 GPU 等待墙消到 ~0
/// (与录制 gpu_composite 的 Stage B 同款设计;录制用 K=3/LAG=2,beauty 链更轻且 LAG 直接
/// 加在预览延迟上,取 K=2/LAG=1 折中:回读只落后 1 帧,同槽复用间隔仍有 2 帧余量)。
pub const RING_SLOTS: usize = 2;
pub const READBACK_LAG: usize = 1;

/// 跨槽共享的回读资源(passthrough pipeline / bind layout),K 槽各自持独立 readback 纹理与
/// staging buffer(每帧 write 的目标槽与本帧 readback 的来源槽不同,同槽写读不重叠)。
struct ReadbackShared {
    pipeline: wgpu::RenderPipeline,
    bg_layout: wgpu::BindGroupLayout,
    padded_bytes_per_row: u32,
    width: u32,
    height: u32,
    /// Bind group 缓存:keyed on 输入 TextureView(帧间输入纹理稳定,避免每帧分配)。
    cached_bg: Option<(wgpu::TextureView, wgpu::BindGroup)>,
}

/// 单个环形槽:passthrough 渲染目标 + readback buffer + 最近一次提交的 index + 该帧元数据。
struct RingSlot {
    readback_texture: wgpu::Texture,
    readback_view: wgpu::TextureView,
    staging_buffer: wgpu::Buffer,
    submission: Option<wgpu::SubmissionIndex>,
    /// 该槽帧的元数据(调用方 opaque,如 ts + landmarks):回读时与像素同槽返回——异步回读
    /// LAG 帧后,回传的像素与其元数据必须同帧一致(前端 overlay 用同帧 landmark 贴同帧像素)。
    meta: Option<Vec<u8>>,
}

pub struct SinkRawData {
    input_view: Option<wgpu::TextureView>,
    width: u32,
    height: u32,
    shared: Option<ReadbackShared>,
    ring: Vec<RingSlot>,
    frame_counter: u64,
}

impl SinkRawData {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            input_view: None,
            width,
            height,
            shared: None,
            ring: Vec::new(),
            frame_counter: 0,
        }
    }

    fn ensure_cache(&mut self, ctx: &GpuContext) {
        let needs_new = match &self.shared {
            Some(s) => s.width != self.width || s.height != self.height,
            None => true,
        };
        if !needs_new {
            return;
        }

        let size = Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        };

        let bytes_per_pixel = 4u32;
        let unpadded_bytes_per_row = self.width * bytes_per_pixel;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = (unpadded_bytes_per_row + align - 1) / align * align;

        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("passthrough copy shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/passthrough.wgsl").into()),
        });

        let bg_layout = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("passthrough bg layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("passthrough layout"),
            bind_group_layouts: &[Some(&bg_layout)],
            immediate_size: 0,
        });

        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("passthrough copy pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let buffer_size = (padded_bytes_per_row * self.height) as u64;
        let mk_slot = |i: usize| {
            // Create a texture we can copy from (need COPY_SRC usage)
            let readback_texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(&format!("sink readback texture{i}")),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let readback_view = readback_texture.create_view(&wgpu::TextureViewDescriptor::default());
            let staging_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(&format!("sink readback staging buffer{i}")),
                size: buffer_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            RingSlot {
                readback_texture,
                readback_view,
                staging_buffer,
                submission: None,
                meta: None,
            }
        };

        self.shared = Some(ReadbackShared {
            pipeline,
            bg_layout,
            padded_bytes_per_row,
            width: self.width,
            height: self.height,
            cached_bg: None,
        });
        self.ring = (0..RING_SLOTS).map(mk_slot).collect();
        // 尺寸变化重建:新 ring 从零开始,重新走 priming(前 LAG 帧同步回读)。
        self.frame_counter = 0;
    }

    /// 环形槽异步回读:提交本帧(自建 encoder:passthrough 渲染入本槽 + copy 到本槽 staging,
    /// 独立 submit)→ 回读 LAG 帧前的槽(其提交早已完成 → 有界 poll 秒回)。返回 (像素, 元数据),
    /// 元数据为提交本槽时存入的那份(与像素同帧)。前 LAG 帧(priming)回读刚提交的本槽(同步)。
    /// 失败返回 Err(设备停顿/丢失),调用方直通。
    pub fn read_pixels(
        &mut self,
        ctx: &GpuContext,
        meta: Vec<u8>,
    ) -> Result<(Vec<u8>, Vec<u8>), String> {
        self.ensure_cache(ctx);
        let input_view = self.input_view.clone().expect("No input framebuffer set");

        // Bind group 缓存:输入纹理不变时复用(输入纹理来自链尾 framebuffer,会话内稳定)。
        let bg = {
            let shared = self.shared.as_mut().expect("ensure_cache builds shared");
            if shared.cached_bg.as_ref().map(|(v, _)| v == &input_view).unwrap_or(false) {
                shared.cached_bg.as_ref().unwrap().1.clone()
            } else {
                let bg = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    layout: &shared.bg_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&input_view),
                        },
                    ],
                    label: Some("sink copy bg"),
                });
                shared.cached_bg = Some((input_view.clone(), bg.clone()));
                bg
            }
        };

        let slot_idx = (self.frame_counter % RING_SLOTS as u64) as usize;
        self.frame_counter += 1;
        // 回读槽:当前帧号 N=frame_counter-1,N>=LAG 时回读 (N-LAG)%K;priming(N<LAG)回读本槽。
        let read_idx = if self.frame_counter > READBACK_LAG as u64 {
            ((self.frame_counter - 1 - READBACK_LAG as u64) % RING_SLOTS as u64) as usize
        } else {
            slot_idx
        };

        // 1. 提交本帧入槽
        {
            let slot = &mut self.ring[slot_idx];
            slot.meta = Some(meta);
            let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("sink copy encoder"),
            });
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("sink copy pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &slot.readback_view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(&self.shared.as_ref().expect("ensure_cache builds shared").pipeline);
                pass.set_bind_group(0, &bg, &[]);
                pass.draw(0..4, 0..1);
            }
            encoder.copy_texture_to_buffer(
                TexelCopyTextureInfo {
                    texture: &slot.readback_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                TexelCopyBufferInfo {
                    buffer: &slot.staging_buffer,
                    layout: TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(self.shared.as_ref().expect("ensure_cache builds shared").padded_bytes_per_row),
                        rows_per_image: Some(self.height),
                    },
                },
                Extent3d {
                    width: self.width,
                    height: self.height,
                    depth_or_array_layers: 1,
                },
            );
            let index = ctx.queue.submit(std::iter::once(encoder.finish()));
            slot.submission = Some(index);
        }

        // 2. 回读 LAG 帧前的槽(LAG 帧前提交已完成 → poll 秒回)。有界等待(3s):正常秒回,
        // 仅 GPU 停顿/丢失才到超时,超时报错由调用方直通,worker 得以响应"停止"。
        let read_slot = &mut self.ring[read_idx];
        let read_submission = read_slot
            .submission
            .as_ref()
            .ok_or_else(|| "sink readback: slot never submitted".to_string())?
            .clone();
        let (tx, rx) = std::sync::mpsc::channel();
        read_slot.staging_buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let poll_ok = ctx
            .device
            .poll(wgpu::PollType::Wait {
                timeout: Some(std::time::Duration::from_secs(3)),
                submission_index: Some(read_submission),
            })
            .is_ok();
        let mapped = poll_ok
            && matches!(rx.recv_timeout(std::time::Duration::from_millis(500)), Ok(Ok(())));
        if !mapped {
            return Err("sink readback timeout (device stalled or lost)".into());
        }
        let padded = self.shared.as_ref().expect("ensure_cache builds shared").padded_bytes_per_row;
        let mut result = Vec::with_capacity((self.width as usize) * (self.height as usize) * 4);
        {
            let mapped_range = read_slot.staging_buffer.slice(..).get_mapped_range().expect("mapped in");
            for chunk in mapped_range.chunks(padded as usize) {
                result.extend_from_slice(&chunk[..self.width as usize * 4]);
            }
        }
        read_slot.staging_buffer.unmap();
        let meta_back = read_slot
            .meta
            .take()
            .ok_or_else(|| "sink readback: slot meta missing".to_string())?;
        Ok((result, meta_back))
    }
}

impl Sink for SinkRawData {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, _slot: u32, _ctx: &GpuContext) {
        self.input_view = Some(fb.view.clone());
        self.width = fb.width;
        self.height = fb.height;
    }

    fn render(&mut self, _ctx: &GpuContext) {
        // SinkRawData doesn't render; call read_pixels() explicitly
    }
}
