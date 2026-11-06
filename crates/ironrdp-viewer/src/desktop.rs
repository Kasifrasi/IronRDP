use std::sync::Arc;

use anyhow::Context as _;
use ironrdp::client::rdp::DesktopUpdate;
use ironrdp::pdu::gcc::Monitor;
use tracing::{info, warn};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event_loop::{ActiveEventLoop, OwnedDisplayHandle};
use winit::window::{Cursor, Window, WindowAttributes, WindowId};

use crate::monitor::{DesktopFrame, MonitorLayout, MonitorViewport};
use crate::rail::bounded_window_size;

type Surface = softbuffer::Surface<OwnedDisplayHandle, Arc<Window>>;

struct DesktopWindow {
    window: Arc<Window>,
    surface: Surface,
    viewport: Option<MonitorViewport>,
}

/// Presents a shared desktop in windows using the server's monitor definitions.
pub(crate) struct MonitorWindows {
    windows: Vec<DesktopWindow>,
    layout: Option<MonitorLayout>,
    frame: DesktopFrame,
    awaiting_frame: bool,
    initial_size: PhysicalSize<u32>,
    cursor: Cursor,
    cursor_visible: bool,
}

impl MonitorWindows {
    pub(crate) fn new(initial_size: PhysicalSize<u32>) -> Self {
        Self {
            windows: Vec::new(),
            layout: None,
            frame: DesktopFrame::default(),
            awaiting_frame: true,
            initial_size,
            cursor: Cursor::from(winit::window::CursorIcon::default()),
            cursor_visible: true,
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
        view.viewport
            .as_ref()?
            .position((size.width, size.height), (local.x, local.y))
    }

    pub(crate) fn frame_size(&self) -> (u16, u16) {
        self.frame.size()
    }

    pub(crate) fn reset(&mut self) {
        self.layout = None;
        self.frame.clear();
        self.awaiting_frame = true;
        for view in &mut self.windows {
            view.viewport = None;
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
                Ok(layout) => {
                    info!(monitor_count = layout.monitors().len(), "Using server monitor layout");
                    Some(layout)
                }
                Err(error) => {
                    warn!(%error, "Invalid server monitor layout; showing the combined desktop");
                    None
                }
            }
        };
        self.sync_windows(event_loop, context)
    }

    pub(crate) fn update(
        &mut self,
        event_loop: &ActiveEventLoop,
        context: &softbuffer::Context<OwnedDisplayHandle>,
        update: &DesktopUpdate,
    ) -> anyhow::Result<()> {
        self.frame.apply(update)?;
        self.awaiting_frame = false;
        self.sync_windows(event_loop, context)?;
        for view in &self.windows {
            view.window.request_redraw();
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
        self.windows.truncate(viewports.len());
        for (index, viewport) in viewports.into_iter().enumerate() {
            let title = match viewport {
                Some(viewport)
                    if self
                        .layout
                        .as_ref()
                        .is_some_and(|layout| layout.size() == size || size == (0, 0)) =>
                {
                    format!(
                        "IronRDP - Monitor {}{}",
                        index + 1,
                        if viewport.primary { " (primary)" } else { "" }
                    )
                }
                Some(_) => "IronRDP - Combined desktop (monitor layout unavailable)".to_owned(),
                None => "IronRDP - Waiting for monitor layout".to_owned(),
            };
            if index == self.windows.len() {
                let size = viewport.map_or(self.initial_size, |view| {
                    PhysicalSize::new(u32::from(view.width), u32::from(view.height))
                });
                let (width, height) = bounded_window_size(size.width, size.height);
                let window = Arc::new(
                    event_loop.create_window(
                        WindowAttributes::default()
                            .with_title(&title)
                            .with_inner_size(PhysicalSize::new(width, height)),
                    )?,
                );
                window.set_cursor(self.cursor.clone());
                window.set_cursor_visible(self.cursor_visible);
                let surface = softbuffer::Surface::new(context, Arc::clone(&window))
                    .map_err(|error| anyhow::anyhow!("unable to create monitor surface: {error}"))?;
                self.windows.push(DesktopWindow {
                    window,
                    surface,
                    viewport,
                });
            } else {
                let view = &mut self.windows[index];
                view.viewport = viewport;
                view.window.set_title(&title);
                view.window.request_redraw();
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
        view.surface
            .resize(width, height)
            .map_err(|error| anyhow::anyhow!("unable to resize monitor surface: {error}"))?;
        let mut buffer = view
            .surface
            .buffer_mut()
            .map_err(|error| anyhow::anyhow!("unable to acquire monitor buffer: {error}"))?;
        buffer.fill(0);
        if let Some(viewport) = &view.viewport
            && !self.awaiting_frame
        {
            viewport.render(
                self.frame.pixels(),
                self.frame.size(),
                &mut buffer,
                (size.width, size.height),
            );
        }
        buffer
            .present()
            .map_err(|error| anyhow::anyhow!("unable to present monitor buffer: {error}"))
            .context("monitor redraw")
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
