use anyhow::Context as _;
use sdl3::pixels::{Color, PixelFormat};
use sdl3::rect::Rect;
use sdl3::render::{BlendMode, ScaleMode, Texture, TextureCreator, WindowCanvas};
use sdl3::video::WindowContext;

use crate::desktop::{Desktop, Viewport};

/// Persistent SDL textures belonging to one monitor's renderer.
/// The texture creator must come from the canvas passed to these methods.
/// Keep both on the SDL main thread.
pub struct MonitorRenderer<'a> {
    streaming: Texture<'a>,
    target: Texture<'a>,
    viewport: Viewport,
    dirty: bool,
    creator: &'a TextureCreator<WindowContext>,
}

impl<'a> MonitorRenderer<'a> {
    pub fn new(
        canvas: &mut WindowCanvas,
        creator: &'a TextureCreator<WindowContext>,
        viewport: Viewport,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            canvas.raw() == creator.raw(),
            "texture creator belongs to another SDL renderer"
        );
        let width = u32::from(viewport.width);
        let height = u32::from(viewport.height);
        let mut streaming = creator
            .create_texture_streaming(PixelFormat::XRGB8888, width, height)
            .context("failed to create SDL streaming texture")?;
        let mut target = creator
            .create_texture_target(PixelFormat::XRGB8888, width, height)
            .context("failed to create SDL render target")?;
        // Desktop pixels have an unused high byte, so alpha blending would hide valid video.
        streaming.set_blend_mode(BlendMode::None);
        target.set_blend_mode(BlendMode::None);
        streaming.set_scale_mode(ScaleMode::Nearest);
        target.set_scale_mode(ScaleMode::Linear);
        canvas
            .with_texture_canvas(&mut target, |canvas| {
                canvas.set_draw_color(Color::RGB(0, 0, 0));
                canvas.clear();
            })
            .context("failed to initialize SDL render target")?;
        Ok(Self {
            streaming,
            target,
            viewport,
            dirty: true,
            creator,
        })
    }

    /// Uploads only the changed pixels intersecting this monitor.
    pub fn upload(&mut self, canvas: &mut WindowCanvas, desktop: &Desktop, changed: Viewport) -> anyhow::Result<()> {
        anyhow::ensure!(
            canvas.raw() == self.creator.raw(),
            "texture belongs to another SDL renderer"
        );
        let Some(region) = self.viewport.intersection(changed) else {
            return Ok(());
        };
        let size = desktop.size();
        anyhow::ensure!(
            u32::from(region.x) + u32::from(region.width) <= u32::from(size.0)
                && u32::from(region.y) + u32::from(region.height) <= u32::from(size.1),
            "upload extends beyond desktop"
        );
        let offset = usize::from(region.y) * usize::from(size.0) + usize::from(region.x);
        let rect = Rect::new(
            i32::from(region.x - self.viewport.x),
            i32::from(region.y - self.viewport.y),
            u32::from(region.width),
            u32::from(region.height),
        );
        // Native-endian XRGB8888 matches the client's 0x00RRGGBB words without swizzling.
        self.streaming
            .update(
                rect,
                bytemuck::cast_slice(&desktop.pixels()[offset..]),
                usize::from(size.0) * 4,
            )
            .context("failed to upload SDL desktop pixels")?;
        let mut copied = Ok(());
        canvas
            .with_texture_canvas(&mut self.target, |canvas| {
                copied = canvas.copy(&self.streaming, rect, rect);
            })
            .context("failed to select SDL render target")?;
        copied.context("failed to copy dirty region to SDL render target")?;
        self.dirty = true;
        Ok(())
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Draws the retained image to the window backbuffer without presenting it.
    pub fn draw(&self, canvas: &mut WindowCanvas) -> anyhow::Result<()> {
        anyhow::ensure!(
            canvas.raw() == self.creator.raw(),
            "texture belongs to another SDL renderer"
        );
        canvas.set_draw_color(Color::RGB(0, 0, 0));
        canvas.clear();
        canvas
            .copy(&self.target, None, None)
            .context("failed to render SDL monitor")
    }

    /// Composes the complete retained target before presenting; SDL backbuffers are transient.
    pub fn present(&mut self, canvas: &mut WindowCanvas) -> anyhow::Result<()> {
        if self.dirty {
            self.draw(canvas)?;
            anyhow::ensure!(canvas.present(), "failed to present SDL monitor: {}", sdl3::get_error());
            self.dirty = false;
        }
        Ok(())
    }
}
