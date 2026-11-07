use std::sync::Arc;

use anyhow::Context as _;
use ironrdp::client::rdp::DesktopUpdate;
use ironrdp::displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp::pdu::gcc::Monitor;
use smallvec::SmallVec;
use tracing::{info, warn};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::{ActiveEventLoop, OwnedDisplayHandle};
use winit::window::{Cursor, Fullscreen, Window, WindowAttributes, WindowId};

use crate::cli::MonitorLayoutMode;
use crate::monitor::{DesktopFrame, MonitorDamage, MonitorLayout, MonitorResizeState, MonitorViewport, OutputGeometry};
use crate::outputs::SelectedOutput;
use crate::rail::bounded_window_size;

type Surface = softbuffer::Surface<OwnedDisplayHandle, Arc<Window>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonitorId(usize);

struct DesktopWindow {
    monitor_id: MonitorId,
    remote_index: usize,
    window: Arc<Window>,
    surface: Surface,
    viewport: Option<MonitorViewport>,
    presented_size: Option<PhysicalSize<u32>>,
    damage: MonitorDamage,
    updates_applied: u64,
    updates_presented: u64,
}

/// Presents a shared desktop in windows using the server's monitor definitions.
pub(crate) struct MonitorWindows {
    windows: Vec<DesktopWindow>,
    preferred_primary: MonitorId,
    outputs: Vec<SelectedOutput>,
    output_geometries: Vec<OutputGeometry>,
    fullscreen: bool,
    layout_mode: MonitorLayoutMode,
    requested_layout: Option<MonitorLayout>,
    layout: Option<MonitorLayout>,
    frame: DesktopFrame,
    awaiting_frame: bool,
    initial_size: PhysicalSize<u32>,
    cursor: Cursor,
    cursor_visible: bool,
    resizes: MonitorResizeState,
}

impl MonitorWindows {
    pub(crate) fn set_outputs(
        &mut self,
        outputs: Vec<SelectedOutput>,
        fullscreen: bool,
        layout_mode: MonitorLayoutMode,
    ) {
        self.outputs = outputs;
        self.fullscreen = fullscreen;
        self.layout_mode = layout_mode;
    }

    pub(crate) fn prepare(&mut self, layout: MonitorLayout) {
        self.preferred_primary = MonitorId(
            layout
                .monitors()
                .iter()
                .position(|monitor| monitor.primary)
                .unwrap_or(0),
        );
        self.resizes.set_layout(&layout);
        self.requested_layout = Some(layout.clone());
        self.layout = Some(layout);
    }

    pub(crate) fn capture_output_geometries(&mut self) -> anyhow::Result<()> {
        self.output_geometries = self.outputs.iter().enumerate().map(|(index, output)| {
            let view = self.windows.iter().find(|view| view.monitor_id == MonitorId(index))
                .context("selected output has no monitor window")?;
            let scale = view.window.scale_factor();
            let output_scale = output.monitor.scale_factor();
            anyhow::ensure!(
                scale.is_finite() && 0.0 < scale && output_scale.is_finite() && 0.0 < output_scale,
                "output scale is invalid"
            );
            // winit 0.30's Wayland backend scales logical origins with the integer wl_output scale.
            // UNTESTED: undo that scale, rather than the fractional window scale; check when updating winit.
            let position = output.monitor.position().to_logical::<i32>(output_scale);
            let native = view.window.inner_size();
            let logical = native.to_logical::<u32>(scale);
            tracing::debug!(output = %output.name, ?position, ?logical, ?native, scale, "Resolved local output geometry");
            Ok(OutputGeometry {
                position: (position.x, position.y),
                logical_size: (logical.width, logical.height),
                native_size: (native.width, native.height),
            })
        }).collect::<anyhow::Result<Vec<_>>>()?;
        Ok(())
    }

    pub(crate) fn local_layout(&self) -> anyhow::Result<MonitorLayout> {
        MonitorLayout::from_local_outputs(&self.output_geometries, self.preferred_primary.0)
    }

    pub(crate) fn native_sizes(&self) -> Option<Vec<(u32, u32)>> {
        if self.windows.is_empty() {
            return None;
        }
        (0..self.windows.len())
            .map(|index| {
                let view = self.windows.iter().find(|view| view.remote_index == index)?;
                let size = view.window.inner_size();
                (size.width != 0 && size.height != 0 && view.presented_size == Some(size))
                    .then_some((size.width, size.height))
            })
            .collect()
    }

    pub(crate) fn fullscreen_ready(&self, event_loop: &ActiveEventLoop) -> anyhow::Result<bool> {
        if !self.fullscreen {
            return Ok(true);
        }
        let available: Vec<_> = event_loop.available_monitors().collect();
        for output in &self.outputs {
            anyhow::ensure!(
                available.contains(&output.monitor),
                "selected fullscreen output {:?} is no longer available",
                output.name
            );
        }
        // UNTESTED: connect only after fullscreen and the selected output are confirmed by the window system.
        // Check single and multiple outputs, including a non-first primary, when updating winit or this gate.
        Ok(self.windows.iter().all(|view| {
            self.outputs.get(view.monitor_id.0).is_some_and(|output| {
                matches!(view.window.fullscreen(), Some(Fullscreen::Borderless(_)))
                    && view.window.current_monitor().as_ref() == Some(&output.monitor)
            })
        }))
    }

    pub(crate) fn native_scale_factor(&self) -> u32 {
        let scale = self
            .windows
            .iter()
            .find(|view| view.viewport.is_some_and(|viewport| viewport.primary))
            .map_or(1.0, |view| view.window.scale_factor());
        #[expect(clippy::as_conversions, reason = "winit supplies a finite positive display scale")]
        let scale = (scale * 100.0).round() as u32;
        scale.clamp(100, 500)
    }

    pub(crate) fn set_scale_factor(&mut self, scale_factor: u32) -> anyhow::Result<()> {
        self.resizes = MonitorResizeState::new(scale_factor);
        if let Some(layout) = &self.layout {
            self.resizes.set_layout(layout);
        }
        for view in &self.windows {
            let size = view.window.inner_size();
            self.resizes.resized(view.remote_index, (size.width, size.height))?;
        }
        Ok(())
    }

    pub(crate) fn into_single_window(mut self) -> (Arc<Window>, Surface) {
        let view = self.windows.pop().expect("prepared desktop has one window");
        (view.window, view.surface)
    }

    pub(crate) fn new(initial_size: PhysicalSize<u32>, scale_factor: u32) -> Self {
        Self {
            windows: Vec::new(),
            preferred_primary: MonitorId(0),
            outputs: Vec::new(),
            output_geometries: Vec::new(),
            fullscreen: false,
            layout_mode: MonitorLayoutMode::Horizontal,
            requested_layout: None,
            layout: None,
            frame: DesktopFrame::default(),
            awaiting_frame: true,
            initial_size,
            cursor: Cursor::from(winit::window::CursorIcon::default()),
            cursor_visible: true,
            resizes: MonitorResizeState::new(scale_factor),
        }
    }

    pub(crate) fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows
            .iter()
            .find(|view| view.window.id() == id)
            .map(|view| view.window.as_ref())
    }

    pub(crate) fn position(&self, id: WindowId, local: PhysicalPosition<f64>) -> Option<ironrdp::input::MousePosition> {
        let view = self.windows.iter().find(|view| view.window.id() == id)?;
        let size = view.window.inner_size();
        if self.fullscreen
            && self.windows.len() == self.output_geometries.len()
            && let Some(layout) = &self.layout
        {
            // UNTESTED: Wayland keeps delivering drag motion to the pressed surface outside its bounds.
            // Preserve that motion across outputs; verify dragging in both directions at mixed scales.
            return layout.position_on_outputs(
                &self.output_geometries,
                view.remote_index,
                (size.width, size.height),
                (local.x, local.y),
            );
        }
        view.viewport
            .as_ref()?
            .position((size.width, size.height), (local.x, local.y))
    }

    pub(crate) fn has_focus(&self) -> bool {
        self.windows.iter().any(|view| view.window.has_focus())
    }

    pub(crate) fn frame_size(&self) -> (u16, u16) {
        self.frame.size()
    }

    pub(crate) fn resized(&mut self, id: WindowId, size: PhysicalSize<u32>) -> anyhow::Result<bool> {
        let Some(view) = self.windows.iter_mut().find(|view| view.window.id() == id) else {
            return Ok(false);
        };
        view.damage.resize((size.width, size.height));
        if self.layout_mode == MonitorLayoutMode::Local {
            return Ok(false);
        }
        self.resizes.resized(view.remote_index, (size.width, size.height))
    }

    pub(crate) fn pending_resize(&self) -> Option<&DisplayControlMonitorLayout> {
        if self.awaiting_frame {
            None
        } else {
            self.resizes.pending()
        }
    }

    pub(crate) fn resize_submitted(&mut self) {
        self.resizes.submitted();
    }

    pub(crate) fn resize_rejected(&mut self) -> anyhow::Result<bool> {
        self.resizes.rejected()
    }

    pub(crate) fn reset(&mut self) {
        self.layout = None;
        self.frame.clear();
        self.resizes.reset();
        self.awaiting_frame = true;
        for view in &mut self.windows {
            view.viewport = None;
            view.damage.invalidate();
            view.window.set_title("IronRDP - Waiting for monitor layout");
            view.window.request_redraw();
        }
    }

    pub(crate) fn set_layout(
        &mut self,
        event_loop: &ActiveEventLoop,
        context: &softbuffer::Context<OwnedDisplayHandle>,
        monitors: &[Monitor],
    ) -> anyhow::Result<()> {
        // Keep unchanged pixels for layout-only updates, but repaint after the next ordered frame update.
        self.awaiting_frame = true;
        self.layout = if monitors.is_empty() {
            warn!("Server supplied no monitor layout; showing the combined desktop");
            None
        } else {
            match MonitorLayout::new(monitors) {
                Ok(mut layout) => {
                    let known = match self.layout_mode {
                        MonitorLayoutMode::Local => self
                            .requested_layout
                            .as_ref()
                            .is_some_and(|reference| layout.reorder_to_match(reference)),
                        MonitorLayoutMode::Horizontal => {
                            layout.order_horizontal();
                            true
                        }
                    };
                    if known {
                        info!(monitor_count = layout.monitors().len(), "Using server monitor layout");
                        Some(layout)
                    } else {
                        warn!(
                            "Server monitor geometry differs from the requested local layout; showing the combined desktop"
                        );
                        None
                    }
                }
                Err(error) => {
                    warn!(%error, "Invalid server monitor layout; showing the combined desktop");
                    None
                }
            }
        };
        if let Some(layout) = &self.layout {
            self.resizes.set_layout(layout);
        } else {
            self.resizes.reset();
        }
        self.sync_windows(event_loop, context)
    }

    pub(crate) fn update(
        &mut self,
        event_loop: &ActiveEventLoop,
        context: &softbuffer::Context<OwnedDisplayHandle>,
        update: &DesktopUpdate,
    ) -> anyhow::Result<()> {
        let sync_windows = self.awaiting_frame || self.frame.size() != (update.width().get(), update.height().get());
        self.frame.apply(update)?;
        self.resizes.observe_frame(self.frame.size());
        self.awaiting_frame = false;
        // UNTESTED: pixel-only updates must retain window bindings and wake only affected outputs.
        // Check activity on one output, then a layout reset, when changing this gate.
        if sync_windows {
            self.sync_windows(event_loop, context)?;
            for view in &mut self.windows {
                view.updates_applied = view.updates_applied.saturating_add(1);
            }
        } else {
            let region = update.region();
            for view in &mut self.windows {
                let size = view.window.inner_size();
                view.damage.resize((size.width, size.height));
                if let Some(damage) = view
                    .viewport
                    .and_then(|viewport| viewport.damage_region(&region, (size.width, size.height)))
                {
                    view.damage.add(damage);
                    view.updates_applied = view.updates_applied.saturating_add(1);
                    view.window.request_redraw();
                }
            }
        }
        Ok(())
    }

    pub(crate) fn sync_windows(
        &mut self,
        event_loop: &ActiveEventLoop,
        context: &softbuffer::Context<OwnedDisplayHandle>,
    ) -> anyhow::Result<()> {
        let size = if self.awaiting_frame { (0, 0) } else { self.frame.size() };
        let viewports: Vec<_> = match &self.layout {
            Some(layout) if size == (0, 0) || layout.size() == size => {
                layout.monitors().iter().copied().map(Some).collect()
            }
            _ if size != (0, 0) => vec![Some(MonitorViewport {
                x: 0,
                y: 0,
                width: size.0,
                height: size.1,
                primary: true,
            })],
            _ => vec![None],
        };
        let ids: Vec<_> = (0..viewports.len())
            .map(|index| {
                if viewports.len() == 1 {
                    self.preferred_primary
                } else {
                    MonitorId(index)
                }
            })
            .collect();
        self.windows.retain(|view| ids.contains(&view.monitor_id));
        self.resizes.retain_windows(viewports.len());
        for (index, (monitor_id, viewport)) in ids.into_iter().zip(viewports).enumerate() {
            let title = match viewport {
                Some(viewport)
                    if self
                        .layout
                        .as_ref()
                        .is_some_and(|layout| layout.size() == size || size == (0, 0)) =>
                {
                    format!(
                        "IronRDP - Monitor {}{}{}",
                        monitor_id.0 + 1,
                        self.outputs
                            .get(monitor_id.0)
                            .map_or_else(String::new, |output| format!(" [{}]", output.name)),
                        if viewport.primary { " (primary)" } else { "" }
                    )
                }
                Some(_) => "IronRDP - Combined desktop (monitor layout unavailable)".to_owned(),
                None => "IronRDP - Waiting for monitor layout".to_owned(),
            };
            if let Some(view) = self.windows.iter_mut().find(|view| view.monitor_id == monitor_id) {
                view.remote_index = index;
                view.viewport = viewport;
                view.damage.invalidate();
                view.window.set_title(&title);
                view.window.request_redraw();
            } else {
                let size = viewport.map_or(self.initial_size, |view| {
                    PhysicalSize::new(u32::from(view.width), u32::from(view.height))
                });
                let (width, height) = bounded_window_size(size.width, size.height);
                let mut attributes = WindowAttributes::default()
                    .with_title(&title)
                    .with_inner_size(PhysicalSize::new(width, height));
                if self.fullscreen {
                    let output = self
                        .outputs
                        .get(monitor_id.0)
                        .context("monitor has no selected output")?;
                    attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(Some(output.monitor.clone()))));
                }
                let window = Arc::new(event_loop.create_window(attributes)?);
                window.request_redraw();
                window.set_cursor(self.cursor.clone());
                window.set_cursor_visible(self.cursor_visible);
                let surface = softbuffer::Surface::new(context, Arc::clone(&window))
                    .map_err(|error| anyhow::anyhow!("unable to create monitor surface: {error}"))?;
                self.windows.push(DesktopWindow {
                    monitor_id,
                    remote_index: index,
                    window,
                    surface,
                    viewport,
                    presented_size: None,
                    damage: MonitorDamage::default(),
                    updates_applied: 0,
                    updates_presented: 0,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn draw(&mut self, id: WindowId) -> anyhow::Result<()> {
        let Some(view) = self.windows.iter_mut().find(|view| view.window.id() == id) else {
            return Ok(());
        };
        let size = view.window.inner_size();
        let (Some(width), Some(height)) = (
            core::num::NonZeroU32::new(size.width),
            core::num::NonZeroU32::new(size.height),
        ) else {
            return Ok(());
        };
        // A compositor can give a window a larger area than requested.
        // Check before softbuffer allocates the native presentation buffer.
        if bounded_window_size(size.width, size.height) != (size.width, size.height) {
            return Ok(());
        }
        if view.presented_size != Some(size) {
            view.damage.invalidate();
            view.surface
                .resize(width, height)
                .map_err(|error| anyhow::anyhow!("unable to resize monitor surface: {error}"))?;
        }
        let mut buffer = view
            .surface
            .buffer_mut()
            .map_err(|error| anyhow::anyhow!("unable to acquire monitor buffer: {error}"))?;
        // UNTESTED: backend buffer ages must match successful presentations for partial repairs.
        // Check scrolling, occlusion, resize and reconnect on Wayland when changing this path.
        let regions = view.damage.repair_regions(buffer.age(), (size.width, size.height));
        let mut damage: SmallVec<[softbuffer::Rect; 8]> = SmallVec::new();
        for region in regions {
            if let Some(viewport) = &view.viewport
                && !self.awaiting_frame
            {
                anyhow::ensure!(
                    viewport.render_region(
                        self.frame.pixels(),
                        self.frame.size(),
                        &mut buffer,
                        (size.width, size.height),
                        region,
                    ),
                    "invalid monitor paint region"
                );
            } else {
                for row in u32::from(region.top)..u32::from(region.bottom) {
                    let start = usize::try_from(row * size.width + u32::from(region.left))?;
                    let end = start + usize::from(region.right - region.left);
                    buffer[start..end].fill(0);
                }
            }
            damage.push(softbuffer::Rect {
                x: u32::from(region.left),
                y: u32::from(region.top),
                width: core::num::NonZeroU32::new(u32::from(region.right - region.left))
                    .context("empty paint width")?,
                height: core::num::NonZeroU32::new(u32::from(region.bottom - region.top))
                    .context("empty paint height")?,
            });
        }
        // UNTESTED: Wayland frame callbacks must pace redraws; check live playback and resize.
        view.window.pre_present_notify();
        let result = if damage.is_empty() {
            buffer.present()
        } else {
            buffer.present_with_damage(&damage)
        };
        result
            .map_err(|error| anyhow::anyhow!("unable to present monitor buffer: {error}"))
            .context("monitor redraw")?;
        view.damage.presented((size.width, size.height));
        view.presented_size = Some(size);
        view.updates_presented = view.updates_applied;
        Ok(())
    }

    /// Immediately repaints one window from the stored desktop without contacting the server.
    pub(crate) fn repaint_locally(&mut self, id: WindowId) -> anyhow::Result<()> {
        anyhow::ensure!(!self.awaiting_frame, "desktop image is not ready for a local repaint");
        let view = self
            .windows
            .iter_mut()
            .find(|view| view.window.id() == id)
            .context("unknown monitor window")?;
        let size = view.window.inner_size();
        anyhow::ensure!(
            size.width != 0
                && size.height != 0
                && bounded_window_size(size.width, size.height) == (size.width, size.height),
            "monitor window size is not drawable"
        );
        let monitor = view.monitor_id.0 + 1;
        let updates_applied = view.updates_applied;
        let updates_presented = view.updates_presented;
        view.damage.invalidate();
        info!(
            monitor,
            updates_applied,
            updates_presented,
            frame_size = ?self.frame.size(),
            window_size = ?size,
            "Starting local full repaint"
        );
        // Bypass the redraw request to distinguish stored pixels from stalled redraw delivery.
        self.draw(id)?;
        info!(monitor, updates_applied, "Completed local full repaint");
        Ok(())
    }

    pub(crate) fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = cursor;
        self.cursor_visible = true;
        for view in &self.windows {
            view.window.set_cursor(self.cursor.clone());
            view.window.set_cursor_visible(true);
        }
    }

    pub(crate) fn hide_cursor(&mut self) {
        self.cursor_visible = false;
        for view in &self.windows {
            view.window.set_cursor_visible(false);
        }
    }

    pub(crate) fn move_cursor(&self, x: u16, y: u16) {
        for view in &self.windows {
            let Some(viewport) = &view.viewport else { continue };
            if viewport.x <= x && x - viewport.x < viewport.width && viewport.y <= y && y - viewport.y < viewport.height
            {
                let size = view.window.inner_size();
                let position = PhysicalPosition::new(
                    f64::from(x - viewport.x) * f64::from(size.width) / f64::from(viewport.width),
                    f64::from(y - viewport.y) * f64::from(size.height) / f64::from(viewport.height),
                );
                if let Err(error) = view.window.set_cursor_position(position) {
                    tracing::debug!(%error, "Compositor could not move the pointer");
                }
                break;
            }
        }
    }
}
