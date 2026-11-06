use core::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use ironrdp::client::rdp::{RdpInputSender, RdpOutputEvent};
use tracing::{debug, error};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::window::WindowId;

use crate::app::{App, create_context};
use crate::cli::DesktopOptions;
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
        let primary = options
            .layout
            .monitors()
            .iter()
            .find(|monitor| monitor.primary)
            .context("initial monitor layout has no primary monitor")?;
        let mut windows = MonitorWindows::new(
            PhysicalSize::new(u32::from(primary.width), u32::from(primary.height)),
            options.scale_factor.unwrap_or(100),
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
        let layout = desktop.options.layout.fit_window_sizes(&sizes)?;
        let scale_factor = desktop
            .options
            .scale_factor
            .unwrap_or_else(|| desktop.windows.native_scale_factor());
        // Validate the same complete layout that subsequent Display Control requests will use.
        layout.to_display_control(scale_factor)?;
        desktop.windows.prepare(layout.clone());
        desktop.windows.set_scale_factor(scale_factor)?;
        debug!(?sizes, desktop_size = ?layout.size(), "Native desktop windows are ready; starting connection");
        let sender = (desktop.connect)(&layout, scale_factor)?;
        self.state = State::Running(Box::new(App::from_prepared_desktop(
            desktop.context,
            desktop.windows,
            sender,
            PhysicalSize::new(u32::from(layout.size().0), u32::from(layout.size().1)),
            1 < layout.monitors().len(),
        )));
        Ok(())
    }
}

impl ApplicationHandler<RdpOutputEvent> for WindowedApp<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match &mut self.state {
            State::Preparing(desktop) => {
                if let Some(selection) = desktop.options.selection.take() {
                    match selection.resolve(event_loop) {
                        Ok((layout, names)) => {
                            desktop.windows.set_output_names(names);
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
                return;
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = desktop.windows.draw(window_id) {
                    self.fail(event_loop, error);
                    return;
                }
            }
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                desktop.ready_at = None;
                if let Some(window) = desktop.windows.window(window_id) {
                    window.request_redraw();
                }
            }
            _ => return,
        }
        if desktop.windows.native_sizes().is_some() {
            // Let mapping and tiling changes across all newly created windows settle together.
            desktop
                .ready_at
                .get_or_insert_with(|| Instant::now() + Duration::from_millis(100));
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let State::Preparing(desktop) = &self.state else {
            if let State::Running(app) = &mut self.state {
                app.about_to_wait(event_loop);
            }
            return;
        };
        if desktop.expires_at <= Instant::now() {
            self.fail(event_loop, anyhow::anyhow!("native window configuration timed out"));
        } else if desktop.ready_at.is_some_and(|deadline| deadline <= Instant::now()) {
            if let Err(error) = self.connect() {
                self.fail(event_loop, error);
            }
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(desktop.ready_at.unwrap_or(desktop.expires_at)));
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RdpOutputEvent) {
        if let State::Running(app) = &mut self.state {
            app.user_event(event_loop, event);
        }
    }
}
