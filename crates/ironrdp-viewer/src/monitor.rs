//! Monitor rectangles in the shared desktop framebuffer.

use ironrdp::client::rdp::DesktopUpdate;
use ironrdp::input::MousePosition;
use ironrdp::pdu::gcc::{MONITOR_COUNT_MAX, Monitor, MonitorFlags};

/// Maximum memory retained for a shared desktop, matching the graphics compositor's limit.
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// A monitor's rectangle relative to the framebuffer's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorViewport {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
}

impl MonitorViewport {
    /// Maps a physical window position to the monitor's position in the shared framebuffer.
    /// Positions outside the window clamp to the nearest monitor pixel.
    pub fn position(&self, window_size: (u32, u32), local: (f64, f64)) -> Option<MousePosition> {
        if window_size.0 == 0 || window_size.1 == 0 || self.width == 0 || self.height == 0 {
            return None;
        }
        if !local.0.is_finite() || !local.1.is_finite() {
            return None;
        }
        let scale = |position: f64, window: u32, extent: u16, origin: u16| {
            let scaled = (position / f64::from(window) * f64::from(extent)).clamp(0.0, f64::from(extent - 1));
            #[expect(
                clippy::as_conversions,
                reason = "the finite coordinate is clamped to the monitor extent"
            )]
            let scaled = scaled as u16;
            origin.saturating_add(scaled)
        };
        Some(MousePosition {
            x: scale(local.0, window_size.0, self.width, self.x),
            y: scale(local.1, window_size.1, self.height, self.y),
        })
    }

    /// Paints this monitor into an existing window buffer using nearest-neighbor scaling.
    /// Pixels outside the available framebuffer are black.
    /// Returns false for zero dimensions or a target buffer with the wrong length.
    pub fn render(&self, frame: &[u32], frame_size: (u16, u16), target: &mut [u32], target_size: (u32, u32)) -> bool {
        if target_size.0 == 0 || target_size.1 == 0 || self.width == 0 || self.height == 0 {
            return false;
        }
        let Ok(target_width) = usize::try_from(target_size.0) else {
            return false;
        };
        let pixel_count = u64::from(target_size.0) * u64::from(target_size.1);
        if usize::try_from(pixel_count).ok() != Some(target.len()) {
            return false;
        }
        for (row, output) in (0..target_size.1).zip(target.chunks_exact_mut(target_width)) {
            let y = u64::from(self.y) + u64::from(row) * u64::from(self.height) / u64::from(target_size.1);
            for (column, pixel) in (0..target_size.0).zip(output.iter_mut()) {
                let x = u64::from(self.x) + u64::from(column) * u64::from(self.width) / u64::from(target_size.0);
                *pixel = if x < u64::from(frame_size.0) && y < u64::from(frame_size.1) {
                    usize::try_from(y * u64::from(frame_size.0) + x)
                        .ok()
                        .and_then(|index| frame.get(index))
                        .copied()
                        .unwrap_or(0)
                } else {
                    0
                };
            }
        }
        true
    }
}

/// A server monitor layout normalized to nonnegative framebuffer coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorLayout {
    monitors: Vec<MonitorViewport>,
    size: (u16, u16),
}

impl MonitorLayout {
    /// Converts inclusive primary-relative monitor rectangles into framebuffer rectangles.
    ///
    /// See ["Monitor Definition (TS_MONITOR_DEF)"] for the server coordinate system.
    ///
    /// ["Monitor Definition (TS_MONITOR_DEF)"]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/c3964b39-3d54-4ae1-a84a-ceaed311e0f6
    pub fn new(monitors: &[Monitor]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=MONITOR_COUNT_MAX).contains(&monitors.len()),
            "invalid monitor count"
        );
        anyhow::ensure!(
            monitors
                .iter()
                .filter(|monitor| monitor.flags.contains(MonitorFlags::PRIMARY))
                .count()
                == 1,
            "monitor layout must have one primary monitor"
        );
        let left = monitors.iter().map(|monitor| monitor.left).min().unwrap_or(0);
        let top = monitors.iter().map(|monitor| monitor.top).min().unwrap_or(0);
        let right = monitors.iter().map(|monitor| monitor.right).max().unwrap_or(0);
        let bottom = monitors.iter().map(|monitor| monitor.bottom).max().unwrap_or(0);
        let width = i64::from(right) - i64::from(left) + 1;
        let height = i64::from(bottom) - i64::from(top) + 1;
        anyhow::ensure!(
            (1..=32_766).contains(&width) && (1..=32_766).contains(&height),
            "monitor layout exceeds desktop dimension limits"
        );
        let viewports = monitors
            .iter()
            .map(|monitor| {
                anyhow::ensure!(
                    monitor.left <= monitor.right && monitor.top <= monitor.bottom,
                    "invalid monitor rectangle"
                );
                let primary = monitor.flags.contains(MonitorFlags::PRIMARY);
                anyhow::ensure!(
                    !primary || (monitor.left, monitor.top) == (0, 0),
                    "primary monitor must start at the origin"
                );
                Ok(MonitorViewport {
                    x: u16::try_from(i64::from(monitor.left) - i64::from(left))?,
                    y: u16::try_from(i64::from(monitor.top) - i64::from(top))?,
                    width: u16::try_from(i64::from(monitor.right) - i64::from(monitor.left) + 1)?,
                    height: u16::try_from(i64::from(monitor.bottom) - i64::from(monitor.top) + 1)?,
                    primary,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Self {
            monitors: viewports,
            size: (u16::try_from(width)?, u16::try_from(height)?),
        })
    }

    /// Returns monitor rectangles in the server's order.
    pub fn monitors(&self) -> &[MonitorViewport] {
        &self.monitors
    }

    /// Returns the bounding framebuffer dimensions.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }
}

/// Reconstructs the shared framebuffer from ordered desktop updates.
#[derive(Debug, Default)]
pub struct DesktopFrame {
    pixels: Vec<u32>,
    size: (u16, u16),
}

impl DesktopFrame {
    /// Applies a validated packed region, clearing old pixels when the desktop extent changes.
    pub fn apply(&mut self, update: &DesktopUpdate) -> anyhow::Result<()> {
        let size = (update.width().get(), update.height().get());
        let pixel_count = usize::from(size.0) * usize::from(size.1);
        anyhow::ensure!(
            pixel_count <= MAX_FRAME_BYTES / 4,
            "desktop exceeds viewer framebuffer memory limit"
        );
        if self.size != size {
            let mut pixels = Vec::new();
            pixels.try_reserve_exact(pixel_count)?;
            pixels.resize(pixel_count, 0);
            self.pixels = pixels;
            self.size = size;
        }
        let region = update.region();
        let row_width = usize::from(region.right - region.left) + 1;
        for (row, source) in update.buffer().chunks_exact(row_width).enumerate() {
            let offset = (usize::from(region.top) + row) * usize::from(size.0) + usize::from(region.left);
            self.pixels[offset..offset + row_width].copy_from_slice(source);
        }
        Ok(())
    }

    /// Returns the shared pixels in row-major `0x00RRGGBB` format.
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }

    /// Returns the current framebuffer dimensions, or zero before the first update.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Discards pixels and geometry from the previous connection or graphics layout.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}
