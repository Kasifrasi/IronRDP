//! Monitor rectangles in the shared desktop framebuffer.

use ironrdp::client::rdp::DesktopUpdate;
use ironrdp::displaycontrol::pdu::{DeviceScaleFactor, DisplayControlMonitorLayout, MonitorLayoutEntry};
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

    /// Resizes one monitor in a horizontal row and keeps its neighbors adjacent.
    pub fn resize_horizontal(&self, index: usize, size: (u32, u32)) -> anyhow::Result<Self> {
        anyhow::ensure!(index < self.monitors.len(), "monitor index is out of range");
        let mut monitors = self.monitors.clone();
        let (width, height) = MonitorLayoutEntry::adjust_display_size(size.0, size.1);
        monitors[index].width = u16::try_from(width)?;
        monitors[index].height = u16::try_from(height)?;
        Self::arrange_horizontal(monitors)
    }

    fn resize_horizontal_all(&self, sizes: &[(u32, u32)]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            sizes.len() == self.monitors.len(),
            "window count does not match monitor layout"
        );
        let mut monitors = self.monitors.clone();
        for (monitor, size) in monitors.iter_mut().zip(sizes) {
            let (width, height) = MonitorLayoutEntry::adjust_display_size(size.0, size.1);
            monitor.width = u16::try_from(width)?;
            monitor.height = u16::try_from(height)?;
        }
        Self::arrange_horizontal(monitors)
    }

    fn arrange_horizontal(mut monitors: Vec<MonitorViewport>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            monitors.iter().all(|monitor| monitor.y == 0),
            "dynamic resizing requires a horizontal monitor row"
        );
        let mut order: Vec<_> = (0..monitors.len()).collect();
        order.sort_by_key(|&index| monitors[index].x);
        let mut width = 0u32;
        for index in order {
            monitors[index].x = u16::try_from(width)?;
            width += u32::from(monitors[index].width);
        }
        anyhow::ensure!(width <= 32_766, "monitor layout exceeds desktop dimension limits");
        let width = u16::try_from(width)?;
        let height = monitors.iter().map(|monitor| monitor.height).max().unwrap_or(0);
        anyhow::ensure!(
            usize::from(width) * usize::from(height) <= MAX_FRAME_BYTES / 4,
            "monitor layout exceeds viewer framebuffer memory limit"
        );
        Ok(Self {
            monitors,
            size: (width, height),
        })
    }

    /// Builds a complete Display Control request in primary-relative coordinates.
    pub fn to_display_control(&self, scale_factor: u32) -> anyhow::Result<DisplayControlMonitorLayout> {
        let primary = self
            .monitors
            .iter()
            .find(|monitor| monitor.primary)
            .ok_or_else(|| anyhow::anyhow!("monitor layout has no primary monitor"))?;
        let scale = MonitorLayoutEntry::adjust_desktop_scale_factor(scale_factor);
        let entries = self
            .monitors
            .iter()
            .map(|monitor| {
                let size = (u32::from(monitor.width), u32::from(monitor.height));
                anyhow::ensure!(
                    MonitorLayoutEntry::adjust_display_size(size.0, size.1) == size,
                    "monitor dimensions cannot be preserved in a Display Control request"
                );
                let entry = if monitor.primary {
                    MonitorLayoutEntry::new_primary(size.0, size.1)?
                } else {
                    MonitorLayoutEntry::new_secondary(size.0, size.1)?
                };
                Ok(entry
                    .with_position(
                        i32::from(monitor.x) - i32::from(primary.x),
                        i32::from(monitor.y) - i32::from(primary.y),
                    )?
                    .with_desktop_scale_factor(scale)?
                    .with_device_scale_factor(DeviceScaleFactor::from_desktop_scale_factor(scale)))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(DisplayControlMonitorLayout::new(&entries)?)
    }
}

/// Keeps local resize intent separate from the server's applied monitor layout.
pub struct MonitorResizeState {
    applied: Option<MonitorLayout>,
    desired: Option<MonitorLayout>,
    submitted: Option<MonitorLayout>,
    local_sizes: Vec<Option<(u32, u32)>>,
    pending: Option<DisplayControlMonitorLayout>,
    has_frame: bool,
    logged_in: bool,
    has_post_logon_frame: bool,
    initial_sizes_synced: bool,
    scale_factor: u32,
}

impl MonitorResizeState {
    /// Starts a connection without discarding monitor metadata received during activation.
    pub fn connected(&mut self) {
        self.logged_in = false;
        self.has_post_logon_frame = false;
        self.initial_sizes_synced = false;
        self.has_frame = false;
        self.desired = self.applied.clone();
        self.submitted = None;
        self.pending = None;
    }

    /// Records successful remote logon.
    pub fn login_completed(&mut self) {
        if !self.logged_in {
            self.logged_in = true;
            self.has_post_logon_frame = false;
        }
    }

    /// Returns whether native window sizes can be fitted for this connection.
    pub fn needs_initial_sizes(&self) -> bool {
        !self.initial_sizes_synced && self.has_post_logon_frame
    }

    /// Requests the initial native window sizes after logon and a matching desktop frame.
    pub fn synchronize_initial_sizes(&mut self, sizes: &[(u32, u32)]) -> anyhow::Result<bool> {
        if !self.needs_initial_sizes() {
            return Ok(false);
        }
        let Some(desired) = &self.desired else {
            return Ok(false);
        };
        if sizes.len() != desired.monitors.len() || sizes.iter().any(|&(width, height)| width == 0 || height == 0) {
            return Ok(false);
        }
        // Try once per connection, so a rejected or unsupported layout cannot cause a resize loop.
        self.initial_sizes_synced = true;
        let resized = desired.resize_horizontal_all(sizes)?;
        let pending = resized.to_display_control(self.scale_factor)?;
        self.local_sizes = sizes.iter().copied().map(Some).collect();
        if &resized == desired {
            return Ok(false);
        }
        self.desired = Some(resized);
        self.pending = Some(pending);
        Ok(true)
    }

    #[must_use]
    pub fn new(scale_factor: u32) -> Self {
        Self {
            applied: None,
            desired: None,
            submitted: None,
            local_sizes: Vec::new(),
            pending: None,
            has_frame: false,
            logged_in: false,
            has_post_logon_frame: false,
            initial_sizes_synced: false,
            scale_factor,
        }
    }

    /// Adopts a new monitor count or primary selection, preserving queued local sizes otherwise.
    pub fn set_layout(&mut self, layout: &MonitorLayout) {
        if self.desired.as_ref().is_none_or(|desired| {
            desired.monitors.len() != layout.monitors.len()
                || desired
                    .monitors
                    .iter()
                    .zip(&layout.monitors)
                    .any(|(a, b)| a.primary != b.primary)
        }) {
            self.desired = Some(layout.clone());
            self.pending = None;
            self.submitted = None;
        } else {
            if self.submitted.as_ref() == Some(layout) {
                self.submitted = None;
            }
            if self.submitted.is_none() && self.pending.is_none() {
                self.desired = Some(layout.clone());
            }
        }
        self.applied = Some(layout.clone());
    }

    /// Enables resize requests only for an image matching the confirmed monitor layout.
    pub fn observe_frame(&mut self, size: (u16, u16)) {
        if self.applied.as_ref().is_some_and(|layout| layout.size() == size) {
            self.has_frame = true;
            self.has_post_logon_frame |= self.logged_in;
        } else {
            self.reset();
        }
    }

    /// Records native window geometry and queues changes after its initial configuration.
    pub fn resized(&mut self, index: usize, size: (u32, u32)) -> anyhow::Result<bool> {
        anyhow::ensure!(index < MONITOR_COUNT_MAX, "monitor index is out of range");
        if size.0 == 0 || size.1 == 0 {
            return Ok(false);
        }
        self.local_sizes.resize(self.local_sizes.len().max(index + 1), None);
        let previous = self.local_sizes[index].replace(size);
        if previous.is_none() || previous == Some(size) || !self.has_frame {
            return Ok(false);
        }
        let Some(desired) = self.desired.as_ref() else {
            return Ok(false);
        };
        let resized = desired.resize_horizontal(index, size)?;
        if &resized == desired {
            return Ok(false);
        }
        let pending = resized.to_display_control(self.scale_factor)?;
        self.desired = Some(resized);
        self.pending = Some(pending);
        Ok(true)
    }

    pub fn pending(&self) -> Option<&DisplayControlMonitorLayout> {
        self.pending.as_ref()
    }

    /// Marks the current request as delivered to the client input queue.
    pub fn submitted(&mut self) {
        self.submitted = self.desired.clone();
        self.pending = None;
    }

    /// Removes a rejected request while retaining newer unsent monitor changes.
    pub fn rejected(&mut self) -> anyhow::Result<bool> {
        let Some(submitted) = self.submitted.take() else {
            return Ok(self.pending.is_some());
        };
        let newer = self.pending.take().and_then(|_| self.desired.take());
        self.desired = self.applied.clone();
        let Some(applied) = &self.applied else {
            return Ok(false);
        };
        let mut resized = applied.clone();
        if let Some(newer) = newer {
            for (index, (wanted, sent)) in newer.monitors.iter().zip(&submitted.monitors).enumerate() {
                if (wanted.width, wanted.height) != (sent.width, sent.height) {
                    resized = resized.resize_horizontal(index, (u32::from(wanted.width), u32::from(wanted.height)))?;
                }
            }
        }
        if &resized != applied {
            self.pending = Some(resized.to_display_control(self.scale_factor)?);
            self.desired = Some(resized);
        }
        Ok(self.pending.is_some())
    }

    /// Forgets observations for windows that were closed.
    pub fn retain_windows(&mut self, count: usize) {
        self.local_sizes.truncate(count);
    }

    /// Clears layout state while retaining logon state and surviving native window geometry.
    pub fn reset(&mut self) {
        self.applied = None;
        self.desired = None;
        self.submitted = None;
        self.pending = None;
        self.has_frame = false;
        self.has_post_logon_frame = false;
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
