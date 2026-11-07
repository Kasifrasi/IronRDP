use core::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use ironrdp::client::rdp::{RdpInputSender, RdpOutputEvent};
use raw_window_handle::{HasDisplayHandle as _, RawDisplayHandle};
use tracing::{debug, error};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::window::WindowId;

use crate::app::{App, create_context};
use crate::cli::{DesktopOptions, MonitorLayoutMode};
use crate::desktop::MonitorWindows;
use crate::monitor::MonitorLayout;

type Connect<'a> = Box<dyn FnOnce(&MonitorLayout, u32) -> anyhow::Result<RdpInputSender> + 'a>;

/// Creates native desktop windows before handing their dimensions to the connection owner.
pub struct WindowedApp<'a> {
    state: State<'a>,
    error: Option<anyhow::Error>,
}

enum State<'a> {
    Preparing(Box<PreparingDesktop<'a>>),
    Running(Box<App>),
    Closed,
}

struct PreparingDesktop<'a> {
    context: softbuffer::Context<OwnedDisplayHandle>,
    windows: MonitorWindows,
    options: DesktopOptions,
    connect: Connect<'a>,
    ready_at: Option<Instant>,
    expires_at: Instant,
}

impl<'a> WindowedApp<'a> {
    pub fn new(
        event_loop: &EventLoop<RdpOutputEvent>,
        options: DesktopOptions,
        connect: Connect<'a>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !options.fullscreen || options.selection.is_some(),
            "fullscreen requires selected local outputs"
        );
        anyhow::ensure!(
            options.monitor_layout != MonitorLayoutMode::Local || options.fullscreen,
            "local monitor layout requires fullscreen"
        );
        let primary = options
            .layout
            .monitors()
            .iter()
            .find(|monitor| monitor.primary)
            .context("initial monitor layout has no primary monitor")?;
        let mut windows = MonitorWindows::new(
            PhysicalSize::new(u32::from(primary.width), u32::from(primary.height)),
            options.scale_factor.unwrap_or(100),
            options.renderer,
        );
        windows.prepare(options.layout.clone());
        Ok(Self {
            state: State::Preparing(Box::new(PreparingDesktop {
                context: create_context(event_loop.owned_display_handle())?,
                windows,
                options,
                connect,
                ready_at: None,
                expires_at: Instant::now() + Duration::from_secs(10),
            })),
            error: None,
        })
    }

    pub fn take_error(&mut self) -> Option<anyhow::Error> {
        self.error.take()
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        error!(%error, "Unable to prepare desktop windows");
        self.error = Some(error);
        self.state = State::Closed;
        event_loop.exit();
    }

    fn connect(&mut self) -> anyhow::Result<()> {
        let State::Preparing(mut desktop) = core::mem::replace(&mut self.state, State::Closed) else {
            return Ok(());
        };
        let sizes = desktop
            .windows
            .native_sizes()
            .context("native windows are not drawable")?;
        if desktop.options.fullscreen {
            desktop.windows.capture_output_geometries()?;
        }
        let layout = match desktop.options.monitor_layout {
            MonitorLayoutMode::Local => desktop.windows.local_layout()?,
            MonitorLayoutMode::Horizontal => desktop.options.layout.fit_window_sizes(&sizes)?,
        };
        let scale_factor = desktop
            .options
            .scale_factor
            .unwrap_or_else(|| desktop.windows.native_scale_factor());
        // Validate the same complete layout that subsequent Display Control requests will use.
        layout.to_display_control(scale_factor)?;
        desktop.windows.prepare(layout.clone());
        desktop.windows.set_scale_factor(scale_factor)?;
        debug!(?sizes, desktop_size = ?layout.size(), fullscreen = desktop.options.fullscreen, "Native desktop windows are ready; starting connection");
        let sender = (desktop.connect)(&layout, scale_factor)?;
        self.state = State::Running(Box::new(App::from_prepared_desktop(
            desktop.context,
            desktop.windows,
            sender,
            PhysicalSize::new(u32::from(layout.size().0), u32::from(layout.size().1)),
        )));
        Ok(())
    }
}

impl ApplicationHandler<RdpOutputEvent> for WindowedApp<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.state {
            State::Preparing(desktop) => {
                // Other winit backends can return the requested state before fullscreen is applied.
                if desktop.options.fullscreen
                    && !event_loop
                        .display_handle()
                        .is_ok_and(|handle| matches!(handle.as_raw(), RawDisplayHandle::Wayland(_)))
                {
                    self.fail(
                        event_loop,
                        anyhow::anyhow!("confirmed fullscreen startup requires Wayland"),
                    );
                    return;
                }
                if let Some(selection) = desktop.options.selection.take() {
                    match selection.resolve(event_loop) {
                        Ok((layout, outputs)) => {
                            desktop.windows.set_outputs(
                                outputs,
                                desktop.options.fullscreen,
                                desktop.options.monitor_layout,
                            );
                            desktop.windows.prepare(layout.clone());
                            desktop.options.layout = layout;
                        }
                        Err(error) => {
                            self.fail(event_loop, error);
                            return;
                        }
                    }
                }
                if let Err(error) = desktop.windows.sync_windows(event_loop, &desktop.context) {
                    self.fail(event_loop, error);
                }
            }
            State::Running(app) => app.resumed(event_loop),
            State::Closed => {}
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        let State::Preparing(desktop) = &mut self.state else {
            if let State::Running(app) = &mut self.state {
                app.window_event(event_loop, window_id, event);
            }
            return;
        };
        if desktop.windows.window(window_id).is_none() {
            return;
        }
        match event {
            WindowEvent::CloseRequested => {
                self.state = State::Closed;
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = desktop.windows.draw(window_id) {
                    self.fail(event_loop, error);
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                desktop.ready_at = None;
                if let Some(window) = desktop.windows.window(window_id) {
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let State::Preparing(desktop) = &mut self.state else {
            if let State::Running(app) = &mut self.state {
                app.about_to_wait(event_loop);
            }
            return;
        };
        let fullscreen_ready = match desktop.windows.fullscreen_ready(event_loop) {
            Ok(ready) => ready,
            Err(error) => {
                self.fail(event_loop, error);
                return;
            }
        };
        let now = Instant::now();
        if fullscreen_ready && desktop.windows.native_sizes().is_some() {
            // Let mapping and tiling changes across all newly created windows settle together.
            desktop.ready_at.get_or_insert(now + Duration::from_millis(100));
        } else {
            desktop.ready_at = None;
        }
        if desktop.expires_at <= now {
            let message = if desktop.options.fullscreen {
                "fullscreen windows were not ready on their selected outputs within 10 seconds"
            } else {
                "native window configuration timed out"
            };
            self.fail(event_loop, anyhow::anyhow!(message));
        } else if desktop.ready_at.is_some_and(|deadline| deadline <= now) {
            if let Err(error) = self.connect() {
                self.fail(event_loop, error);
            }
        } else {
            // Output-enter notifications can update the backend without a public window event.
            let pending = if desktop.options.fullscreen {
                now + Duration::from_millis(50)
            } else {
                desktop.expires_at
            };
            let deadline = desktop.ready_at.unwrap_or(pending).min(desktop.expires_at);
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RdpOutputEvent) {
        if let State::Running(app) = &mut self.state {
            app.user_event(event_loop, event);
        }
    }
}
