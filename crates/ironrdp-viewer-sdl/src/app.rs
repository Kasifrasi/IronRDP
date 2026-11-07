use core::time::Duration;
use std::time::Instant;

use anyhow::Context as _;
use ironrdp_client::rdp::{RdpInputEvent, RdpInputSender, RdpOutputEvent};
use ironrdp_viewer_sdl::desktop::{Desktop, Layout, Viewport};
use ironrdp_viewer_sdl::input::Input;
use ironrdp_viewer_sdl::renderer::MonitorRenderer;
use sdl3::event::{Event, WindowEvent};
use sdl3::keyboard::{Mod, Scancode};
use sdl3::mouse::{Cursor, SystemCursor};
use sdl3::pixels::PixelFormat;
use sdl3::render::{TextureCreator, WindowCanvas, create_renderer};
use sdl3::surface::Surface;
use sdl3::video::{FullscreenType, WindowContext};
use tokio::sync::mpsc;

pub(crate) fn create_windows(
    video: &sdl3::VideoSubsystem,
    host: &str,
    count: usize,
    size: (u16, u16),
    fullscreen: bool,
) -> anyhow::Result<Vec<WindowCanvas>> {
    let mut displays = video.displays()?;
    displays.sort_by_key(|display| {
        display
            .get_bounds()
            .map(|bounds| (bounds.x(), bounds.y()))
            .unwrap_or_default()
    });
    anyhow::ensure!(!displays.is_empty(), "SDL found no local displays");
    anyhow::ensure!(
        !fullscreen || count <= displays.len(),
        "fullscreen requires one local display per monitor"
    );
    let mut canvases = Vec::new();
    for index in 0..count {
        let display = &displays[index % displays.len()];
        let bounds = display.get_bounds()?;
        let (width, height) = if fullscreen {
            (bounds.width(), bounds.height())
        } else {
            (u32::from(size.0), u32::from(size.1))
        };
        let position = sdl3::sys::video::SDL_WINDOWPOS_CENTERED_DISPLAY(display.to_ll());
        let title = format!("{host} - Monitor {}", index + 1);
        let mut builder = video.window(&title, width, height);
        builder.resizable().high_pixel_density().position(position, position);
        if fullscreen {
            builder.fullscreen();
        }
        let window = builder.build().context("failed to create SDL monitor window")?;
        // UNTESTED: Native fullscreen must be confirmed on the selected display before negotiating pixels.
        // Check output assignment and fractional display scaling when changing SDL window creation.
        anyhow::ensure!(
            window.sync(),
            "SDL window configuration did not complete: {}",
            sdl3::get_error()
        );
        let canvas = create_renderer(window, None).context("failed to create SDL renderer")?;
        let display_name = display.get_name()?;
        tracing::info!(monitor = index, display = %display_name, pixels = ?canvas.window().size_in_pixels(), renderer = %canvas.renderer_name, "Created SDL monitor window");
        canvases.push(canvas);
    }
    Ok(canvases)
}

pub(crate) fn window_layout(canvases: &[WindowCanvas], primary: usize) -> anyhow::Result<Layout> {
    let sizes: Vec<_> = canvases.iter().map(|canvas| canvas.window().size_in_pixels()).collect();
    Layout::from_sizes(&sizes, primary)
}

pub(crate) fn run(
    sdl: &sdl3::Sdl,
    pump: &mut sdl3::EventPump,
    receiver: &mut mpsc::Receiver<RdpOutputEvent>,
    sender: &RdpInputSender,
    mut canvases: Vec<WindowCanvas>,
    mut layout: Layout,
) -> anyhow::Result<()> {
    let mut desktop = Desktop::default();
    // Creators outlive the textures; a server resize changes textures without destroying windows.
    let creators: Vec<_> = canvases.iter().map(WindowCanvas::texture_creator).collect();
    let mut renderers = create_textures(&mut canvases, &creators, &layout, &desktop)?;
    let mut input = Input::default();
    let mut connected = false;
    let mut server_monitor_mismatch = false;
    let mut focus_release = None;
    let mut resize_due = None;
    let mut last_requested = Some(layout.clone());
    let default_cursor = Cursor::from_system(SystemCursor::Arrow)?;
    default_cursor.set();
    let mut cursor: Option<Cursor> = None;
    loop {
        if let Some(event) = pump.wait_event_timeout(Duration::from_millis(10)) {
            for event in core::iter::once(event).chain(pump.poll_iter()) {
                let index = event
                    .get_window_id()
                    .and_then(|id| canvases.iter().position(|canvas| canvas.window().id() == id));
                match &event {
                    Event::Quit { .. }
                    | Event::Window {
                        win_event: WindowEvent::CloseRequested,
                        ..
                    } => return Ok(()),
                    Event::RenderDeviceReset { .. } | Event::RenderTargetsReset { .. } => {
                        // UNTESTED: GPU loss must permit texture recreation on the existing renderer.
                        // Exercise an actual device reset when changing recovery.
                        renderers.clear();
                        renderers = create_textures(&mut canvases, &creators, &layout, &desktop)?;
                    }
                    Event::Window {
                        win_event: WindowEvent::FocusLost,
                        ..
                    } => focus_release = Some(Instant::now() + Duration::from_millis(100)),
                    Event::Window {
                        win_event: WindowEvent::FocusGained,
                        ..
                    } => {
                        focus_release = None;
                        if let Some(cursor) = &cursor {
                            cursor.set();
                        } else {
                            default_cursor.set();
                        }
                        if connected {
                            input.synchronize(sdl.keyboard().mod_state());
                        }
                    }
                    Event::KeyDown {
                        scancode: Some(Scancode::Return),
                        keymod,
                        repeat,
                        ..
                    } if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD)
                        && keymod.intersects(Mod::LALTMOD | Mod::RALTMOD) =>
                    {
                        if !repeat {
                            input.release_all();
                            if let Some(index) = index {
                                let window = canvases[index].window_mut();
                                window.set_fullscreen(window.fullscreen_state() == FullscreenType::Off)?;
                                resize_due = Some(Instant::now() + Duration::from_millis(150));
                            }
                        }
                        continue;
                    }
                    Event::Window {
                        win_event:
                            WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..) | WindowEvent::DisplayChanged(..),
                        ..
                    } => {
                        resize_due = Some(Instant::now() + Duration::from_millis(150));
                    }
                    _ => {}
                }
                if let Some(index) = index {
                    if matches!(event, Event::Window { .. }) {
                        renderers[index].invalidate();
                    }
                    if connected {
                        input.event(&event, layout.monitors()[index], canvases[index].window().size())?;
                    }
                }
            }
        }
        if focus_release.is_some_and(|deadline| deadline <= Instant::now()) {
            input.release_all();
            focus_release = None;
        }
        if connected {
            input.flush(sender)?;
        }
        if connected
            && resize_due.is_some_and(|deadline| deadline <= Instant::now())
            && canvases.iter().all(|canvas| !canvas.window().is_minimized())
        {
            let desired = window_layout(&canvases, layout.primary())?;
            if last_requested.as_ref() == Some(&desired) {
                resize_due = None;
            } else if let Ok(permit) = sender.try_reserve() {
                permit.send(RdpInputEvent::MonitorLayout(desired.display_control()?));
                tracing::info!(size = ?desired.size(), monitors = ?desired.monitors(), "Requested native desktop resolution");
                last_requested = Some(desired);
                resize_due = None;
            }
        }
        for _ in 0..64 {
            let event = match receiver.try_recv() {
                Ok(event) => event,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => anyhow::bail!("RDP output channel closed unexpectedly"),
            };
            match event {
                RdpOutputEvent::Connected => {
                    connected = true;
                    input.reset();
                    input.synchronize(sdl.keyboard().mod_state());
                    desktop.clear();
                    renderers.clear();
                    renderers = create_textures(&mut canvases, &creators, &layout, &desktop)?;
                    let desired = window_layout(&canvases, layout.primary())?;
                    // UNTESTED: An already active layout must not trigger another resize request.
                    // Check startup without a reconnect when changing this server/window comparison.
                    last_requested = (!server_monitor_mismatch && layout == desired).then_some(layout.clone());
                    resize_due = Some(Instant::now() + Duration::from_millis(150));
                    tracing::info!("RDP desktop connected");
                }
                RdpOutputEvent::MonitorLayout(monitors) => {
                    let confirmed = Layout::from_server(&monitors).context("invalid remote monitor layout")?;
                    if confirmed.monitors().len() != canvases.len() {
                        server_monitor_mismatch = true;
                        tracing::warn!(
                            count = confirmed.monitors().len(),
                            "Server monitor count differs from the requested windows"
                        );
                        last_requested = None;
                        resize_due = Some(Instant::now() + Duration::from_millis(150));
                        continue;
                    }
                    server_monitor_mismatch = false;
                    if layout != confirmed {
                        layout = confirmed;
                        renderers.clear();
                        renderers = create_textures(&mut canvases, &creators, &layout, &desktop)?;
                    }
                }
                RdpOutputEvent::DesktopUpdate(update) => {
                    let previous_size = desktop.size();
                    let Some(changed) = desktop.push(update)? else {
                        continue;
                    };
                    if desktop.size() != previous_size {
                        tracing::info!(size = ?desktop.size(), "Received desktop framebuffer");
                    }
                    if desktop.size() != layout.size() {
                        if let Some(requested) = last_requested
                            .as_ref()
                            .filter(|requested| requested.size() == desktop.size())
                        {
                            layout = requested.clone();
                            renderers.clear();
                            renderers = create_textures(&mut canvases, &creators, &layout, &desktop)?;
                        } else {
                            continue;
                        }
                    }
                    for changed in changed {
                        tracing::trace!(region = ?changed, "Received desktop update");
                        for (renderer, canvas) in renderers.iter_mut().zip(&mut canvases) {
                            renderer.upload(canvas, &desktop, changed)?;
                        }
                    }
                }
                RdpOutputEvent::MonitorLayoutRejected => {
                    tracing::warn!("Server rejected the requested display resolution");
                }
                RdpOutputEvent::PointerDefault => {
                    default_cursor.set();
                    sdl.mouse().show_cursor(true);
                    cursor = None;
                }
                RdpOutputEvent::PointerHidden => sdl.mouse().show_cursor(false),
                RdpOutputEvent::PointerBitmap(pointer) => {
                    let mut pixels = pointer.bitmap_data.clone();
                    let surface = Surface::from_data(
                        &mut pixels,
                        u32::from(pointer.width),
                        u32::from(pointer.height),
                        u32::from(pointer.width) * 4,
                        PixelFormat::RGBA32,
                    )?;
                    match Cursor::from_surface(surface, i32::from(pointer.hotspot_x), i32::from(pointer.hotspot_y)) {
                        Ok(next) => {
                            next.set();
                            cursor = Some(next);
                            sdl.mouse().show_cursor(true);
                        }
                        Err(error) => tracing::warn!(%error, "Failed to create remote mouse cursor"),
                    }
                }
                RdpOutputEvent::ConnectionFailure(error) => {
                    desktop.clear();
                    return Err(error).context("RDP connection failed");
                }
                RdpOutputEvent::Terminated(result) => {
                    desktop.clear();
                    return result.map(|_| ()).context("RDP session failed");
                }
                _ => {}
            }
        }
        for (renderer, canvas) in renderers.iter_mut().zip(&mut canvases) {
            renderer.present(canvas)?;
        }
    }
}

fn create_textures<'a>(
    canvases: &mut [WindowCanvas],
    creators: &'a [TextureCreator<WindowContext>],
    layout: &Layout,
    desktop: &Desktop,
) -> anyhow::Result<Vec<MonitorRenderer<'a>>> {
    canvases
        .iter_mut()
        .zip(creators)
        .zip(layout.monitors())
        .map(|((canvas, creator), &viewport)| {
            let mut renderer = MonitorRenderer::new(canvas, creator, viewport)?;
            if desktop.size() == layout.size() {
                renderer.upload(
                    canvas,
                    desktop,
                    Viewport {
                        x: 0,
                        y: 0,
                        width: desktop.size().0,
                        height: desktop.size().1,
                    },
                )?;
            }
            Ok(renderer)
        })
        .collect()
}
