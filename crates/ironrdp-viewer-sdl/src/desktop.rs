use ironrdp_client::rdp::DesktopUpdate;
use ironrdp_displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};
use ironrdp_input::MousePosition;
use ironrdp_pdu::gcc::{ClientMonitorData, MONITOR_COUNT_MAX, Monitor, MonitorFlags};

const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// A monitor rectangle in framebuffer coordinates, with exclusive right and bottom edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Viewport {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Viewport {
    /// Converts logical SDL coordinates to framebuffer pixels, including this monitor's offset.
    pub fn position(self, window: (u32, u32), point: (f32, f32)) -> Option<MousePosition> {
        if window.0 == 0
            || window.1 == 0
            || self.width == 0
            || self.height == 0
            || !point.0.is_finite()
            || !point.1.is_finite()
        {
            return None;
        }
        let coordinate = |value: f32, logical: u32, extent: u16, origin: u16| {
            let scaled = (f64::from(value) * f64::from(extent) / f64::from(logical)).clamp(0.0, f64::from(extent - 1));
            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the finite coordinate is clamped to a u16 monitor extent"
            )]
            let offset = scaled as u16;
            origin.saturating_add(offset)
        };
        Some(MousePosition {
            x: coordinate(point.0, window.0, self.width, self.x),
            y: coordinate(point.1, window.1, self.height, self.y),
        })
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (u32::from(self.x) + u32::from(self.width)).min(u32::from(other.x) + u32::from(other.width));
        let bottom = (u32::from(self.y) + u32::from(self.height)).min(u32::from(other.y) + u32::from(other.height));
        if u32::from(x) < right && u32::from(y) < bottom {
            Some(Self {
                x,
                y,
                width: u16::try_from(right - u32::from(x)).ok()?,
                height: u16::try_from(bottom - u32::from(y)).ok()?,
            })
        } else {
            None
        }
    }
}

/// Remote monitors normalized to the top-left corner of the shared framebuffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    monitors: Vec<Viewport>,
    primary: usize,
    size: (u16, u16),
}

impl Layout {
    /// Arranges monitor pixel sizes in a horizontal row, respecting Display Control size limits.
    pub fn from_sizes(sizes: &[(u32, u32)], primary: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=MONITOR_COUNT_MAX).contains(&sizes.len()) && primary < sizes.len(),
            "invalid monitor selection"
        );
        let mut x = 0u32;
        let mut height = 0;
        let monitors = sizes
            .iter()
            .map(|&(width, monitor_height)| {
                let (width, monitor_height) = MonitorLayoutEntry::adjust_display_size(width, monitor_height);
                let viewport = Viewport {
                    x: u16::try_from(x)?,
                    y: 0,
                    width: u16::try_from(width)?,
                    height: u16::try_from(monitor_height)?,
                };
                x += width;
                height = height.max(viewport.height);
                Ok(viewport)
            })
            .collect::<anyhow::Result<_>>()?;
        let size = (u16::try_from(x)?, height);
        Self::check_size(size)?;
        Ok(Self {
            monitors,
            primary,
            size,
        })
    }

    pub fn display_control(&self) -> anyhow::Result<DisplayControlMonitorLayout> {
        let origin = self.monitors[self.primary];
        let entries = self
            .monitors
            .iter()
            .enumerate()
            .map(|(index, viewport)| {
                let size = (u32::from(viewport.width), u32::from(viewport.height));
                let entry = if index == self.primary {
                    MonitorLayoutEntry::new_primary(size.0, size.1)?
                } else {
                    MonitorLayoutEntry::new_secondary(size.0, size.1)?
                };
                Ok(entry.with_position(
                    i32::from(viewport.x) - i32::from(origin.x),
                    i32::from(viewport.y) - i32::from(origin.y),
                )?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(DisplayControlMonitorLayout::new(&entries)?)
    }

    pub fn horizontal(count: usize, width: u16, height: u16, primary: usize) -> anyhow::Result<Self> {
        anyhow::ensure!((1..=MONITOR_COUNT_MAX).contains(&count), "invalid monitor count");
        anyhow::ensure!(primary < count, "primary monitor index is out of range");
        anyhow::ensure!(
            (200..=8192).contains(&width) && width.is_multiple_of(2),
            "monitor width must be even and between 200 and 8192"
        );
        anyhow::ensure!(
            (200..=8192).contains(&height),
            "monitor height must be between 200 and 8192"
        );
        let total_width = u16::try_from(usize::from(width) * count)?;
        Self::check_size((total_width, height))?;
        Ok(Self {
            monitors: (0..count)
                .map(|index| {
                    Ok(Viewport {
                        x: u16::try_from(index * usize::from(width))?,
                        y: 0,
                        width,
                        height,
                    })
                })
                .collect::<anyhow::Result<_>>()?,
            primary,
            size: (total_width, height),
        })
    }

    pub fn single(width: u16, height: u16) -> anyhow::Result<Self> {
        Self::check_size((width, height))?;
        Ok(Self {
            monitors: vec![Viewport {
                x: 0,
                y: 0,
                width,
                height,
            }],
            primary: 0,
            size: (width, height),
        })
    }

    pub fn from_server(monitors: &[Monitor]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !monitors.is_empty() && monitors.len() <= MONITOR_COUNT_MAX,
            "invalid server monitor count"
        );
        let left = monitors.iter().map(|monitor| monitor.left).min().unwrap_or(0);
        let top = monitors.iter().map(|monitor| monitor.top).min().unwrap_or(0);
        let right = monitors.iter().map(|monitor| monitor.right).max().unwrap_or(0);
        let bottom = monitors.iter().map(|monitor| monitor.bottom).max().unwrap_or(0);
        let size = (
            u16::try_from(i64::from(right) - i64::from(left) + 1)?,
            u16::try_from(i64::from(bottom) - i64::from(top) + 1)?,
        );
        Self::check_size(size)?;
        let primary = monitors
            .iter()
            .position(|monitor| monitor.flags.contains(MonitorFlags::PRIMARY))
            .ok_or_else(|| anyhow::anyhow!("server monitor layout has no primary monitor"))?;
        anyhow::ensure!(
            monitors
                .iter()
                .filter(|monitor| monitor.flags.contains(MonitorFlags::PRIMARY))
                .count()
                == 1,
            "server monitor layout has multiple primary monitors"
        );
        anyhow::ensure!(
            (monitors[primary].left, monitors[primary].top) == (0, 0),
            "server primary monitor must start at the origin"
        );
        let mut viewports: Vec<_> = monitors
            .iter()
            .map(|monitor| {
                let viewport = Viewport {
                    x: u16::try_from(i64::from(monitor.left) - i64::from(left))?,
                    y: u16::try_from(i64::from(monitor.top) - i64::from(top))?,
                    width: u16::try_from(i64::from(monitor.right) - i64::from(monitor.left) + 1)?,
                    height: u16::try_from(i64::from(monitor.bottom) - i64::from(monitor.top) + 1)?,
                };
                anyhow::ensure!(viewport.width != 0 && viewport.height != 0, "server monitor is empty");
                Ok(viewport)
            })
            .collect::<anyhow::Result<_>>()?;
        let primary_viewport = viewports[primary];
        if viewports.iter().all(|viewport| viewport.y == 0) {
            viewports.sort_by_key(|viewport| viewport.x);
        }
        let primary = viewports
            .iter()
            .position(|viewport| *viewport == primary_viewport)
            .ok_or_else(|| anyhow::anyhow!("primary monitor is missing"))?;
        Ok(Self {
            monitors: viewports,
            primary,
            size,
        })
    }

    pub fn client_monitor_data(&self) -> ClientMonitorData {
        let primary = self.monitors[self.primary];
        ClientMonitorData {
            monitors: self
                .monitors
                .iter()
                .enumerate()
                .map(|(index, monitor)| {
                    let left = i32::from(monitor.x) - i32::from(primary.x);
                    let top = i32::from(monitor.y) - i32::from(primary.y);
                    Monitor {
                        left,
                        top,
                        right: left + i32::from(monitor.width) - 1,
                        bottom: top + i32::from(monitor.height) - 1,
                        flags: if index == self.primary {
                            MonitorFlags::PRIMARY
                        } else {
                            MonitorFlags::empty()
                        },
                    }
                })
                .collect(),
        }
    }

    pub fn monitors(&self) -> &[Viewport] {
        &self.monitors
    }
    pub fn primary(&self) -> usize {
        self.primary
    }
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    fn check_size((width, height): (u16, u16)) -> anyhow::Result<()> {
        anyhow::ensure!(width != 0 && height != 0, "desktop is empty");
        anyhow::ensure!(
            width <= 32766 && height <= 32766,
            "desktop exceeds RDP coordinate limits"
        );
        anyhow::ensure!(
            usize::from(width) * usize::from(height) <= MAX_FRAME_BYTES / 4,
            "desktop exceeds framebuffer memory limit"
        );
        Ok(())
    }
}

/// Retains desktop pixels for expose events and recovery after SDL texture loss.
#[derive(Default)]
pub struct Desktop {
    pixels: Vec<u32>,
    size: (u16, u16),
}

impl Desktop {
    pub fn apply(&mut self, update: &DesktopUpdate) -> anyhow::Result<Viewport> {
        let size = (update.width().get(), update.height().get());
        Layout::check_size(size)?;
        if self.size != size {
            let count = usize::from(size.0) * usize::from(size.1);
            let mut pixels = Vec::new();
            pixels.try_reserve_exact(count)?;
            pixels.resize(count, 0);
            self.pixels = pixels;
            self.size = size;
        }
        let region = update.region();
        let width = region.right - region.left + 1;
        for (row, source) in update.buffer().chunks_exact(usize::from(width)).enumerate() {
            let offset = (usize::from(region.top) + row) * usize::from(size.0) + usize::from(region.left);
            self.pixels[offset..offset + source.len()].copy_from_slice(source);
        }
        Ok(Viewport {
            x: region.left,
            y: region.top,
            width,
            height: region.bottom - region.top + 1,
        })
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}
