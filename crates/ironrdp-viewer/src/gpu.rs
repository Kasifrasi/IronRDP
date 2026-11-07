//! Vulkan desktop presentation and an offscreen renderer check.

use std::sync::Arc;

use anyhow::Context as _;
use ironrdp::pdu::geometry::{ExclusiveRectangle, InclusiveRectangle};
use tracing::info;
use winit::window::Window;

use crate::monitor::{DesktopFrame, MonitorDamage, MonitorViewport};

/// A persistent desktop texture shared by all native monitor surfaces.
pub(crate) struct GpuDesktop {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    layout: wgpu::BindGroupLayout,
    shader: wgpu::ShaderModule,
    texture: wgpu::Texture,
    size: (u32, u32),
    generation: u64,
    damage: MonitorDamage,
}

pub(crate) struct GpuSurface {
    surface: wgpu::Surface<'static>,
    window: Arc<Window>,
    config: wgpu::SurfaceConfiguration,
    view: GpuView,
    reconfigure: bool,
}

struct GpuView {
    pipeline: wgpu::RenderPipeline,
    parameters: wgpu::Buffer,
    bindings: Option<wgpu::BindGroup>,
    generation: u64,
    srgb: bool,
}

impl GpuDesktop {
    pub(crate) fn new(window: Arc<Window>) -> anyhow::Result<(Self, GpuSurface)> {
        let (desktop, surface) = Self::create(Some(Arc::clone(&window)))?;
        let surface = desktop.attach(window, surface.context("missing GPU window surface")?)?;
        Ok((desktop, surface))
    }

    fn create(window: Option<Arc<Window>>) -> anyhow::Result<(Self, Option<wgpu::Surface<'static>>)> {
        anyhow::ensure!(
            cfg!(all(
                target_os = "linux",
                target_arch = "x86_64",
                target_endian = "little"
            )),
            "the Vulkan desktop renderer currently requires Linux x86_64"
        );
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(descriptor);
        let surface = window.map(|window| instance.create_surface(window)).transpose()?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: surface.as_ref(),
            force_fallback_adapter: false,
            ..Default::default()
        }))
        .context("no Vulkan adapter available; check the Vulkan loader and Mesa driver")?;
        let adapter_info = adapter.get_info();
        anyhow::ensure!(
            adapter_info.device_type != wgpu::DeviceType::Cpu,
            "only a software Vulkan adapter is available"
        );
        let limits = wgpu::Limits {
            max_texture_dimension_2d: adapter.limits().max_texture_dimension_2d,
            ..wgpu::Limits::default()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("IronRDP desktop"),
            required_limits: limits,
            ..Default::default()
        }))?;
        info!(
            adapter = %adapter_info.name,
            driver = %adapter_info.driver,
            driver_info = %adapter_info.driver_info,
            backend = ?adapter_info.backend,
            device_type = ?adapter_info.device_type,
            "Using GPU desktop renderer"
        );
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Desktop texture and viewport"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Desktop crop and scale"),
            source: wgpu::ShaderSource::Wgsl(include_str!("desktop.wgsl").into()),
        });
        let texture = Self::texture(&device, (1, 1));
        Ok((
            Self {
                instance,
                adapter,
                device,
                queue,
                layout,
                shader,
                texture,
                size: (1, 1),
                generation: 1,
                damage: MonitorDamage::default(),
            },
            surface,
        ))
    }

    pub(crate) fn create_surface(&self, window: Arc<Window>) -> anyhow::Result<GpuSurface> {
        let surface = self.instance.create_surface(Arc::clone(&window))?;
        self.attach(window, surface)
    }

    fn attach(&self, window: Arc<Window>, surface: wgpu::Surface<'static>) -> anyhow::Result<GpuSurface> {
        let size = window.inner_size();
        let caps = surface.get_capabilities(&self.adapter);
        let mut config = surface
            .get_default_config(&self.adapter, size.width.max(1), size.height.max(1))
            .context("GPU adapter cannot present this monitor surface")?;
        config.format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
            wgpu::TextureFormat::Bgra8UnormSrgb,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        ]
        .into_iter()
        .find(|format| caps.formats.contains(format))
        .context("surface has no supported 8-bit RGB format")?;
        config.alpha_mode = caps
            .alpha_modes
            .iter()
            .copied()
            .find(|mode| *mode == wgpu::CompositeAlphaMode::Opaque)
            .unwrap_or(config.alpha_mode);
        config.present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        config.desired_maximum_frame_latency = 2;
        let view = GpuView::new(self, config.format);
        info!(format = ?config.format, present_mode = ?config.present_mode, "Created GPU monitor surface");
        Ok(GpuSurface {
            surface,
            window,
            config,
            view,
            reconfigure: true,
        })
    }

    fn texture(device: &wgpu::Device, size: (u32, u32)) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Shared desktop pixels"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    pub(crate) fn mark_dirty(&mut self, region: InclusiveRectangle) {
        self.damage.add(ExclusiveRectangle {
            left: region.left,
            top: region.top,
            right: region.right + 1,
            bottom: region.bottom + 1,
        });
    }

    pub(crate) fn invalidate(&mut self) {
        self.damage.invalidate();
    }

    fn upload(&mut self, frame: &DesktopFrame) -> anyhow::Result<()> {
        let size = (u32::from(frame.size().0), u32::from(frame.size().1));
        if size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        let limit = self.device.limits().max_texture_dimension_2d;
        anyhow::ensure!(
            size.0 <= limit && size.1 <= limit,
            "desktop exceeds GPU texture dimensions"
        );
        if self.size != size {
            self.texture = Self::texture(&self.device, size);
            self.size = size;
            self.generation = self.generation.saturating_add(1);
            self.damage.invalidate();
        }
        // On little-endian x86_64, 0x00RRGGBB is BGRA in memory; the shader supplies opaque alpha.
        let bytes: &[u8] = bytemuck::cast_slice(frame.pixels());
        for region in self.damage.repair_regions(1, size) {
            let offset = (usize::from(region.top) * usize::try_from(size.0)? + usize::from(region.left)) * 4;
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: u32::from(region.left),
                        y: u32::from(region.top),
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                &bytes[offset..],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.0 * 4),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: u32::from(region.right - region.left),
                    height: u32::from(region.bottom - region.top),
                    depth_or_array_layers: 1,
                },
            );
        }
        self.damage.presented(size);
        Ok(())
    }
}

impl GpuView {
    fn new(gpu: &GpuDesktop, format: wgpu::TextureFormat) -> Self {
        let layout = gpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Desktop pipeline"),
            bind_group_layouts: &[Some(&gpu.layout)],
            immediate_size: 0,
        });
        let pipeline = gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Desktop pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &gpu.shader,
                entry_point: Some("vertex_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &gpu.shader,
                entry_point: Some("fragment_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let parameters = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Desktop viewport"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            parameters,
            bindings: None,
            generation: 0,
            srgb: format.is_srgb(),
        }
    }

    fn encode(
        &mut self,
        gpu: &GpuDesktop,
        target: &wgpu::TextureView,
        viewport: Option<MonitorViewport>,
        size: (u32, u32),
    ) -> wgpu::CommandBuffer {
        if self.generation != gpu.generation {
            self.bindings = Some(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Desktop view"),
                layout: &gpu.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&gpu.texture.create_view(&Default::default())),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.parameters.as_entire_binding(),
                    },
                ],
            }));
            self.generation = gpu.generation;
        }
        let crop = viewport.map_or([0; 4], |viewport| {
            [
                u32::from(viewport.x),
                u32::from(viewport.y),
                u32::from(viewport.width),
                u32::from(viewport.height),
            ]
        });
        let params = [
            crop[0],
            crop[1],
            crop[2],
            crop[3],
            size.0,
            size.1,
            u32::from(viewport.is_some()),
            u32::from(self.srgb),
        ];
        gpu.queue
            .write_buffer(&self.parameters, 0, bytemuck::cast_slice(&params));
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Desktop"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, self.bindings.as_ref().expect("desktop bindings initialized"), &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.finish()
    }
}

impl GpuSurface {
    pub(crate) fn present(
        &mut self,
        gpu: &mut GpuDesktop,
        frame: &DesktopFrame,
        viewport: Option<MonitorViewport>,
    ) -> anyhow::Result<bool> {
        let size = self.window.inner_size();
        let limit = gpu.device.limits().max_texture_dimension_2d;
        anyhow::ensure!(
            size.width <= limit && size.height <= limit,
            "window exceeds GPU texture dimensions"
        );
        if size.width == 0 || size.height == 0 {
            return Ok(false);
        }
        if self.reconfigure || self.config.width != size.width || self.config.height != size.height {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&gpu.device, &self.config);
            self.reconfigure = false;
        }
        let output = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => output,
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => {
                self.reconfigure = true;
                output
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.window.request_redraw();
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.reconfigure = true;
                self.window.request_redraw();
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(false),
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = gpu.instance.create_surface(Arc::clone(&self.window))?;
                self.reconfigure = true;
                self.window.request_redraw();
                return Ok(false);
            }
            wgpu::CurrentSurfaceTexture::Validation => anyhow::bail!("GPU surface validation failed"),
        };
        gpu.upload(frame)?;
        let command = self.view.encode(
            gpu,
            &output.texture.create_view(&Default::default()),
            viewport,
            (size.width, size.height),
        );
        gpu.queue.submit([command]);
        // UNTESTED: Wayland presentation must resume after occlusion and surface recovery.
        // Check playback on all monitors, resize, hide/restore and reconnect when changing this path.
        self.window.pre_present_notify();
        gpu.queue.present(output);
        Ok(true)
    }
}

/// Validates Vulkan uploads, colors, cropping, scaling and texture replacement without a window.
pub fn verify_renderer() -> anyhow::Result<()> {
    use core::num::NonZeroU16;
    use ironrdp::client::rdp::DesktopUpdate;

    let (mut gpu, _) = GpuDesktop::create(None)?;
    let mut view = GpuView::new(&gpu, wgpu::TextureFormat::Rgba8Unorm);
    let mut frame = DesktopFrame::default();
    let colors = [
        0xFF0000, 0x00FF00, 0x0000FF, 0x808080, 0xFFFFFF, 0xFF00FF, 0x00FFFF, 0x2080C0,
    ];
    frame.apply(
        &DesktopUpdate::new(
            colors.to_vec(),
            NonZeroU16::new(4).context("test width is zero")?,
            NonZeroU16::new(2).context("test height is zero")?,
            InclusiveRectangle {
                left: 0,
                top: 0,
                right: 3,
                bottom: 1,
            },
        )
        .context("invalid test frame")?,
    )?;
    gpu.upload(&frame)?;
    let viewport = MonitorViewport {
        x: 0,
        y: 0,
        width: 4,
        height: 2,
        primary: true,
    };
    // oracle: reference the supplied RGB pixels must retain their order and become opaque.
    verify_pixels(&gpu, &mut view, Some(viewport), (4, 2), &colors)?;
    let mut srgb_view = GpuView::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    verify_pixels(&gpu, &mut srgb_view, Some(viewport), (4, 2), &colors)?;
    // oracle: reference integer nearest-neighbor mapping selects columns 1,1,2,2,3 and rows 0,0,1.
    verify_pixels(
        &gpu,
        &mut view,
        Some(MonitorViewport {
            x: 1,
            width: 3,
            ..viewport
        }),
        (5, 3),
        &[
            0x00FF00, 0x00FF00, 0x0000FF, 0x0000FF, 0x808080, 0x00FF00, 0x00FF00, 0x0000FF, 0x0000FF, 0x808080,
            0xFF00FF, 0xFF00FF, 0x00FFFF, 0x00FFFF, 0x2080C0,
        ],
    )?;
    // oracle: reference source pixels outside the 4x2 image are opaque black.
    verify_pixels(
        &gpu,
        &mut view,
        Some(MonitorViewport {
            x: 3,
            y: 1,
            width: 2,
            height: 2,
            primary: true,
        }),
        (2, 2),
        &[0x2080C0, 0, 0, 0],
    )?;
    verify_pixels(&gpu, &mut view, None, (2, 2), &[0; 4])?;
    for (x, y, color) in [(0, 0, 0xFF8000), (3, 1, 0x101820)] {
        let region = InclusiveRectangle {
            left: x,
            top: y,
            right: x,
            bottom: y,
        };
        frame.apply(
            &DesktopUpdate::new(
                vec![color],
                NonZeroU16::new(4).context("test width is zero")?,
                NonZeroU16::new(2).context("test height is zero")?,
                region.clone(),
            )
            .context("invalid GPU test update")?,
        )?;
        gpu.mark_dirty(region);
    }
    gpu.upload(&frame)?;
    // oracle: reference two separate edits must preserve all six unchanged source pixels.
    verify_pixels(
        &gpu,
        &mut view,
        Some(viewport),
        (4, 2),
        &[
            0xFF8000, 0x00FF00, 0x0000FF, 0x808080, 0xFFFFFF, 0xFF00FF, 0x00FFFF, 0x101820,
        ],
    )?;
    frame.apply(
        &DesktopUpdate::new(
            vec![0x123456],
            NonZeroU16::MIN,
            NonZeroU16::MIN,
            InclusiveRectangle {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
        )
        .context("invalid GPU test update")?,
    )?;
    gpu.upload(&frame)?;
    // oracle: reference replacing the texture must also replace bindings in an existing view.
    verify_pixels(
        &gpu,
        &mut view,
        Some(MonitorViewport {
            width: 1,
            height: 1,
            ..viewport
        }),
        (2, 2),
        &[0x123456; 4],
    )?;
    info!(checks = 7, "GPU pixel verification passed");
    Ok(())
}

fn verify_pixels(
    gpu: &GpuDesktop,
    view: &mut GpuView,
    viewport: Option<MonitorViewport>,
    size: (u32, u32),
    expected: &[u32],
) -> anyhow::Result<()> {
    let format = if view.srgb {
        wgpu::TextureFormat::Rgba8UnormSrgb
    } else {
        wgpu::TextureFormat::Rgba8Unorm
    };
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("GPU pixel verification"),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let draw = view.encode(gpu, &target.create_view(&Default::default()), viewport, size);
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("GPU pixel readback"),
        size: 256 * u64::from(size.1),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut copy = gpu.device.create_command_encoder(&Default::default());
    copy.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([draw, copy.finish()]);
    let (sender, receiver) = std::sync::mpsc::channel();
    readback.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    gpu.device.poll(wgpu::PollType::wait_indefinitely())?;
    receiver.recv()??;
    let mapped = readback.slice(..).get_mapped_range()?;
    anyhow::ensure!(
        expected.len() == usize::try_from(size.0 * size.1)?,
        "incorrect expected pixel count"
    );
    for (index, expected) in expected.iter().enumerate() {
        let offset = (index / usize::try_from(size.0)?) * 256 + (index % usize::try_from(size.0)?) * 4;
        let reference = [
            expected.to_be_bytes()[1],
            expected.to_be_bytes()[2],
            expected.to_be_bytes()[3],
            255,
        ];
        for channel in 0..4 {
            let tolerance = u8::from(view.srgb && channel != 3);
            anyhow::ensure!(
                mapped[offset + channel].abs_diff(reference[channel]) <= tolerance,
                "GPU pixel {index} channel {channel} differs: got {}, expected {}",
                mapped[offset + channel],
                reference[channel]
            );
        }
    }
    Ok(())
}
