//! Monitor rectangles in the shared desktop framebuffer.

use core::time::Duration;
use std::collections::VecDeque;
use std::time::Instant;

use ironrdp::client::rdp::DesktopUpdate;
use ironrdp::displaycontrol::pdu::{DeviceScaleFactor, DisplayControlMonitorLayout, MonitorLayoutEntry};
use ironrdp::input::MousePosition;
use ironrdp::pdu::gcc::{ClientMonitorData, MONITOR_COUNT_MAX, Monitor, MonitorFlags};
use ironrdp::pdu::geometry::{ExclusiveRectangle, InclusiveRectangle, Rectangle as _};
use smallvec::SmallVec;

/// Maximum memory retained for a shared desktop, matching the graphics compositor's limit.
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

/// Bounds fragmented repaint work before falling back to a bounding rectangle.
const MAX_DAMAGE_REGIONS: usize = 32;

type DamageRegions = SmallVec<[ExclusiveRectangle; 8]>;

/// Defers input release briefly so focus can transfer between monitor windows.
#[derive(Default)]
pub struct MonitorFocusState {
    deadline: Option<Instant>,
}

impl MonitorFocusState {
    pub fn set_focused(&mut self, now: Instant, focused: bool) {
        // Focus loss and gain can arrive in separate dispatches; keep external release bounded.
        self.deadline = (!focused).then_some(now + Duration::from_millis(100));
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn release_due(&mut self, now: Instant) -> bool {
        if self.deadline.is_some_and(|deadline| deadline <= now) {
            self.deadline = None;
            true
        } else {
            false
        }
    }
}

/// Accumulates target damage and repairs previously presented software buffers.
pub struct MonitorDamage {
    pending: DamageRegions,
    history: VecDeque<DamageRegions>,
    invalid: bool,
    size: Option<(u32, u32)>,
    repaired: DamageRegions,
}

impl Default for MonitorDamage {
    fn default() -> Self {
        Self {
            pending: DamageRegions::new(),
            history: VecDeque::new(),
            invalid: true,
            size: None,
            repaired: SmallVec::new(),
        }
    }
}

impl MonitorDamage {
    /// Returns the target rectangles needed to repair this presentation buffer.
    /// Keeps separated changes sparse and includes earlier presentations according to `age`.
    pub fn repair_regions(&mut self, age: u8, size: (u32, u32)) -> &[ExclusiveRectangle] {
        self.resize(size);
        self.repaired.clear();
        if self.invalid || age == 0 || self.history.len() < usize::from(age) - 1 {
            self.repaired.extend(Self::full_region(size));
        } else {
            self.repaired.clone_from(&self.pending);
            for region in self.history.iter().take(usize::from(age) - 1).flatten() {
                add_damage(&mut self.repaired, region.clone());
            }
        }
        &self.repaired
    }

    /// Observes target size changes, including intermediate sizes before a redraw.
    pub fn resize(&mut self, size: (u32, u32)) {
        if self.size != Some(size) {
            self.invalidate();
            self.size = Some(size);
        }
    }

    pub fn add(&mut self, region: ExclusiveRectangle) {
        add_damage(&mut self.pending, region);
    }

    pub fn invalidate(&mut self) {
        self.pending.clear();
        self.repaired.clear();
        self.history.clear();
        self.invalid = true;
    }

    /// Returns the area needed to make a buffer of this age match the current image.
    /// Age zero or missing history requires a full repaint.
    /// See [softbuffer's buffer age] for the presentation count.
    ///
    /// [softbuffer's buffer age]: https://docs.rs/softbuffer/0.4.8/softbuffer/struct.Buffer.html#method.age
    pub fn repair_region(&mut self, age: u8, size: (u32, u32)) -> Option<ExclusiveRectangle> {
        self.repair_regions(age, size)
            .iter()
            .cloned()
            .reduce(|pending, region| pending.union(&region))
    }

    /// Records visible changes only after a successful presentation.
    pub fn presented(&mut self, size: (u32, u32)) {
        self.resize(size);
        let changed = if self.invalid {
            Self::full_region(size).into_iter().collect()
        } else {
            core::mem::take(&mut self.pending)
        };
        self.history.push_front(changed);
        self.history.truncate(8);
        self.pending.clear();
        self.invalid = false;
    }

    fn full_region(size: (u32, u32)) -> Option<ExclusiveRectangle> {
        let right = u16::try_from(size.0).ok()?;
        let bottom = u16::try_from(size.1).ok()?;
        (right != 0 && bottom != 0).then_some(ExclusiveRectangle {
            left: 0,
            top: 0,
            right,
            bottom,
        })
    }
}

fn add_damage(regions: &mut DamageRegions, mut region: ExclusiveRectangle) {
    if !(region.left < region.right && region.top < region.bottom) {
        return;
    }
    let area = |rect: &ExclusiveRectangle| u64::from(rect.right - rect.left) * u64::from(rect.bottom - rect.top);
    let mut index = 0;
    while index < regions.len() {
        let merged = regions[index].union(&region);
        // Merge only when painting the bounding box costs no more than both rectangles.
        if area(&merged) <= area(&regions[index]) + area(&region) {
            region = merged;
            regions.swap_remove(index);
            index = 0;
        } else {
            index += 1;
        }
    }
    if regions.len() == MAX_DAMAGE_REGIONS {
        region = regions.iter().fold(region, |region, pending| region.union(pending));
        regions.clear();
    }
    regions.push(region);
}

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
    /// Maps changed framebuffer pixels to the exclusive target area that samples them.
    pub fn damage_region(&self, region: &InclusiveRectangle, target_size: (u32, u32)) -> Option<ExclusiveRectangle> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        let left = u32::from(region.left).max(u32::from(self.x));
        let top = u32::from(region.top).max(u32::from(self.y));
        let right = (u32::from(region.right) + 1).min(u32::from(self.x) + u32::from(self.width));
        let bottom = (u32::from(region.bottom) + 1).min(u32::from(self.y) + u32::from(self.height));
        if !(left < right && top < bottom) {
            return None;
        }
        // A source interval [a,b) affects target pixels [ceil(a*T/S),ceil(b*T/S)).
        let project = |position: u32, origin: u16, source: u16, target: u32| {
            u16::try_from((u64::from(position - u32::from(origin)) * u64::from(target)).div_ceil(u64::from(source)))
                .ok()
        };
        let damage = ExclusiveRectangle {
            left: project(left, self.x, self.width, target_size.0)?,
            top: project(top, self.y, self.height, target_size.1)?,
            right: project(right, self.x, self.width, target_size.0)?,
            bottom: project(bottom, self.y, self.height, target_size.1)?,
        };
        (damage.left < damage.right && damage.top < damage.bottom).then_some(damage)
    }

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
        self.render_rect(
            frame,
            frame_size,
            target,
            target_size,
            (0, 0, target_size.0, target_size.1),
        )
    }

    /// Paints only an exclusive target region, retaining pixels outside it.
    pub fn render_region(
        &self,
        frame: &[u32],
        frame_size: (u16, u16),
        target: &mut [u32],
        target_size: (u32, u32),
        region: &ExclusiveRectangle,
    ) -> bool {
        self.render_rect(
            frame,
            frame_size,
            target,
            target_size,
            (
                u32::from(region.left),
                u32::from(region.top),
                u32::from(region.right),
                u32::from(region.bottom),
            ),
        )
    }

    fn render_rect(
        &self,
        frame: &[u32],
        frame_size: (u16, u16),
        target: &mut [u32],
        target_size: (u32, u32),
        (left, top, right, bottom): (u32, u32, u32, u32),
    ) -> bool {
        if target_size.0 == 0 || target_size.1 == 0 || self.width == 0 || self.height == 0 {
            return false;
        }
        if !(left < right && top < bottom && right <= target_size.0 && bottom <= target_size.1) {
            return false;
        }
        let Ok(target_width) = usize::try_from(target_size.0) else {
            return false;
        };
        let pixel_count = u64::from(target_size.0) * u64::from(target_size.1);
        if usize::try_from(pixel_count).ok() != Some(target.len()) {
            return false;
        }
        let (Ok(left_index), Ok(right_index), Ok(top_index), Ok(height)) = (
            usize::try_from(left),
            usize::try_from(right),
            usize::try_from(top),
            usize::try_from(bottom - top),
        ) else {
            return false;
        };
        let native = (u32::from(self.width), u32::from(self.height)) == target_size;
        for (row, output) in (0..target_size.1)
            .zip(target.chunks_exact_mut(target_width))
            .skip(top_index)
            .take(height)
        {
            let output = &mut output[left_index..right_index];
            let y = u64::from(self.y) + u64::from(row) * u64::from(self.height) / u64::from(target_size.1);
            if native
                && y < u64::from(frame_size.1)
                && u64::from(self.x) + u64::from(right) <= u64::from(frame_size.0)
                && let Ok(offset) = usize::try_from(y * u64::from(frame_size.0) + u64::from(self.x) + u64::from(left))
                && let Some(source) = frame.get(offset..offset + output.len())
            {
                output.copy_from_slice(source);
                continue;
            }
            for (column, pixel) in (left..right).zip(output.iter_mut()) {
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

/// A local output in logical desktop coordinates with its native window size in pixels.
#[derive(Debug, Clone, Copy)]
pub struct OutputGeometry {
    pub position: (i32, i32),
    pub logical_size: (u32, u32),
    pub native_size: (u32, u32),
}

impl OutputGeometry {
    fn edges(self) -> (i64, i64, i64, i64) {
        let left = i64::from(self.position.0);
        let top = i64::from(self.position.1);
        (
            left,
            top,
            left + i64::from(self.logical_size.0),
            top + i64::from(self.logical_size.1),
        )
    }

    fn overlaps(self, other: Self) -> bool {
        let (left, top, right, bottom) = self.edges();
        let (other_left, other_top, other_right, other_bottom) = other.edges();
        left < other_right && other_left < right && top < other_bottom && other_top < bottom
    }

    fn neighbor_at(self, position: (i64, i64), other: Self) -> Option<(i64, i64)> {
        let (left, top, right, bottom) = self.edges();
        let (other_left, other_top, other_right, other_bottom) = other.edges();
        let horizontal = (right == other_left || other_right == left) && top <= other_bottom && other_top <= bottom;
        let vertical = (bottom == other_top || other_bottom == top) && left <= other_right && other_left <= right;
        if !horizontal && !vertical {
            return None;
        }
        let scaled = |delta: i64, native: u32, logical: u32| {
            let numerator = delta * i64::from(native);
            (numerator + numerator.signum() * (i64::from(logical) / 2)) / i64::from(logical)
        };
        let x = if horizontal {
            if other_left < left {
                position.0 - i64::from(other.native_size.0)
            } else {
                position.0 + i64::from(self.native_size.0)
            }
        } else {
            position.0 + scaled(other_left - left, self.native_size.0, self.logical_size.0)
        };
        let y = if vertical {
            if other_top < top {
                position.1 - i64::from(other.native_size.1)
            } else {
                position.1 + i64::from(self.native_size.1)
            }
        } else {
            position.1 + scaled(other_top - top, self.native_size.1, self.logical_size.1)
        };
        let horizontal_contact = horizontal
            && position.1 <= y + i64::from(other.native_size.1)
            && y <= position.1 + i64::from(self.native_size.1);
        let vertical_contact = vertical
            && position.0 <= x + i64::from(other.native_size.0)
            && x <= position.0 + i64::from(self.native_size.0);
        (horizontal_contact || vertical_contact).then_some((x, y))
    }
}

impl MonitorLayout {
    /// Maps a fullscreen pointer through logical outputs into the corresponding remote monitor.
    /// Positions outside the selected outputs clamp to the nearest selected output.
    pub fn position_on_outputs(
        &self,
        outputs: &[OutputGeometry],
        source: usize,
        window_size: (u32, u32),
        local: (f64, f64),
    ) -> Option<MousePosition> {
        if outputs.len() != self.monitors.len()
            || window_size.0 == 0
            || window_size.1 == 0
            || !local.0.is_finite()
            || !local.1.is_finite()
        {
            return None;
        }
        let origin = outputs.get(source)?;
        let position = (
            f64::from(origin.position.0) + local.0 / f64::from(window_size.0) * f64::from(origin.logical_size.0),
            f64::from(origin.position.1) + local.1 / f64::from(window_size.1) * f64::from(origin.logical_size.1),
        );
        if !position.0.is_finite() || !position.1.is_finite() {
            return None;
        }
        let mut nearest = None;
        let mut distance = f64::INFINITY;
        for (index, output) in outputs.iter().enumerate() {
            if output.logical_size.0 == 0 || output.logical_size.1 == 0 {
                return None;
            }
            let left = f64::from(output.position.0);
            let top = f64::from(output.position.1);
            let right = left + f64::from(output.logical_size.0);
            let bottom = top + f64::from(output.logical_size.1);
            if left <= position.0 && position.0 < right && top <= position.1 && position.1 < bottom {
                nearest = Some(index);
                break;
            }
            let candidate =
                (position.0 - position.0.clamp(left, right)).hypot(position.1 - position.1.clamp(top, bottom));
            if nearest.is_none() || candidate < distance {
                nearest = Some(index);
                distance = candidate;
            }
        }
        let target = nearest?;
        let output = &outputs[target];
        self.monitors[target].position(
            output.logical_size,
            (
                position.0 - f64::from(output.position.0),
                position.1 - f64::from(output.position.1),
            ),
        )
    }

    /// Reconstructs local output adjacency while preserving native remote resolutions.
    /// Output rectangles must form a connected, nonoverlapping logical layout.
    /// Native rectangles are placed through logical neighbors, so mixed scales can change edge alignment.
    pub fn from_local_outputs(outputs: &[OutputGeometry], primary: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=MONITOR_COUNT_MAX).contains(&outputs.len()) && primary < outputs.len(),
            "invalid monitor selection"
        );
        for output in outputs {
            anyhow::ensure!(
                output.logical_size.0 != 0
                    && output.logical_size.1 != 0
                    && output.native_size.0 != 0
                    && output.native_size.1 != 0,
                "output geometry is empty"
            );
            anyhow::ensure!(
                MonitorLayoutEntry::adjust_display_size(output.native_size.0, output.native_size.1)
                    == output.native_size,
                "native output resolution is unsupported by RDP; use a horizontal monitor layout"
            );
        }
        for (index, output) in outputs.iter().enumerate() {
            anyhow::ensure!(
                !outputs[..index].iter().any(|other| output.overlaps(*other)),
                "local output rectangles overlap; use a horizontal monitor layout"
            );
        }
        // A spatial anchor keeps native geometry independent of selection order and primary choice.
        let mut order: Vec<_> = (0..outputs.len()).collect();
        order.sort_by_key(|&index| outputs[index].position);
        let anchor = order[0];
        let mut positions = vec![None; outputs.len()];
        positions[anchor] = Some((0, 0));
        let mut queue = vec![(anchor, (0, 0))];
        let mut cursor = 0;
        while let Some(&(reference, position)) = queue.get(cursor) {
            for &index in &order {
                if positions[index].is_none()
                    && let Some(position) = outputs[reference].neighbor_at(position, outputs[index])
                {
                    positions[index] = Some(position);
                    queue.push((index, position));
                }
            }
            cursor += 1;
        }
        let positions = positions.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| {
            anyhow::anyhow!(
                "selected outputs are not connected; select adjacent outputs or use a horizontal monitor layout"
            )
        })?;
        let origin = positions[primary];
        let monitors = outputs
            .iter()
            .zip(&positions)
            .enumerate()
            .map(|(index, (output, position))| {
                let left = i32::try_from(position.0 - origin.0)?;
                let top = i32::try_from(position.1 - origin.1)?;
                Ok(Monitor {
                    left,
                    top,
                    right: left
                        .checked_add(i32::try_from(output.native_size.0)? - 1)
                        .ok_or_else(|| anyhow::anyhow!("monitor position is out of range"))?,
                    bottom: top
                        .checked_add(i32::try_from(output.native_size.1)? - 1)
                        .ok_or_else(|| anyhow::anyhow!("monitor position is out of range"))?,
                    flags: if index == primary {
                        MonitorFlags::PRIMARY
                    } else {
                        MonitorFlags::empty()
                    },
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let layout = Self::new(&monitors)?;
        anyhow::ensure!(
            usize::from(layout.size.0) * usize::from(layout.size.1) <= MAX_FRAME_BYTES / 4,
            "monitor layout exceeds viewer framebuffer memory limit"
        );
        layout.to_display_control(100)?;
        Ok(layout)
    }

    pub(crate) fn horizontal(sizes: &[(u32, u32)], primary: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=MONITOR_COUNT_MAX).contains(&sizes.len()) && primary < sizes.len(),
            "invalid monitor selection"
        );
        let monitors = sizes
            .iter()
            .enumerate()
            .map(|(index, &(width, height))| {
                anyhow::ensure!(width != 0 && height != 0, "selected output has no usable resolution");
                let (width, height) = MonitorLayoutEntry::adjust_display_size(width, height);
                Ok(MonitorViewport {
                    x: u16::try_from(index)?,
                    y: 0,
                    width: u16::try_from(width)?,
                    height: u16::try_from(height)?,
                    primary: index == primary,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Self::arrange_horizontal(monitors)
    }

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

    pub(crate) fn order_horizontal(&mut self) {
        if self.monitors.iter().all(|monitor| monitor.y == 0) {
            // UNTESTED: server array order must not swap windows in a horizontal monitor row.
            // Verify a non-first primary monitor and resize both windows when changing this ordering.
            self.monitors.sort_by_key(|monitor| monitor.x);
        }
    }

    /// Restores output identity when a server reports the same rectangles in another order.
    pub fn reorder_to_match(&mut self, reference: &Self) -> bool {
        if self.monitors.len() != reference.monitors.len() {
            return false;
        }
        let ordered = reference
            .monitors
            .iter()
            .map(|wanted| {
                self.monitors
                    .iter()
                    .find(|monitor| {
                        (monitor.x, monitor.y, monitor.width, monitor.height)
                            == (wanted.x, wanted.y, wanted.width, wanted.height)
                    })
                    .copied()
            })
            .collect::<Option<Vec<_>>>();
        let Some(ordered) = ordered else { return false };
        self.monitors = ordered;
        true
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

    /// Builds the initial monitor topology in primary-relative coordinates.
    pub fn to_client_monitor_data(&self) -> anyhow::Result<ClientMonitorData> {
        let primary = self
            .monitors
            .iter()
            .find(|monitor| monitor.primary)
            .ok_or_else(|| anyhow::anyhow!("monitor layout has no primary monitor"))?;
        Ok(ClientMonitorData {
            monitors: self
                .monitors
                .iter()
                .map(|monitor| {
                    let left = i32::from(monitor.x) - i32::from(primary.x);
                    let top = i32::from(monitor.y) - i32::from(primary.y);
                    Monitor {
                        left,
                        top,
                        right: left + i32::from(monitor.width) - 1,
                        bottom: top + i32::from(monitor.height) - 1,
                        flags: if monitor.primary {
                            MonitorFlags::PRIMARY
                        } else {
                            MonitorFlags::empty()
                        },
                    }
                })
                .collect(),
        })
    }

    /// Fits a horizontal monitor row to the native window contents before connecting.
    pub fn fit_window_sizes(&self, sizes: &[(u32, u32)]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            sizes.len() == self.monitors.len(),
            "window count does not match monitor layout"
        );
        anyhow::ensure!(
            sizes.iter().all(|&(width, height)| width != 0 && height != 0),
            "window contents are empty"
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
    scale_factor: u32,
}

impl MonitorResizeState {
    #[must_use]
    pub fn new(scale_factor: u32) -> Self {
        Self {
            applied: None,
            desired: None,
            submitted: None,
            local_sizes: Vec::new(),
            pending: None,
            has_frame: false,
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
        if previous.is_none() || previous == Some(size) {
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
        Ok(self.has_frame)
    }

    pub fn pending(&self) -> Option<&DisplayControlMonitorLayout> {
        self.pending.as_ref().filter(|_| self.has_frame)
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

    /// Clears layout state while retaining surviving native window geometry.
    pub fn reset(&mut self) {
        self.applied = None;
        self.desired = None;
        self.submitted = None;
        self.pending = None;
        self.has_frame = false;
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
