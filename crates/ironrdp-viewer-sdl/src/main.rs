#![expect(
    unused_crate_dependencies,
    reason = "dependencies are shared by the library and binary targets"
)]

use core::time::Duration;

use anyhow::Context as _;
use clap::Parser;
use ironrdp_client::config::{ConfigBuilder, Destination};
use ironrdp_client::output_channel::output_channel;
use ironrdp_client::rdp::RdpClient;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_tls::CertificateValidation;
use ironrdp_viewer_sdl::desktop::Layout;
use tokio::sync::{mpsc, oneshot};

mod app;

#[derive(Parser)]
#[command(about = "SDL3 RDP video viewer with one window per remote monitor")]
struct Options {
    /// RDP server, optionally followed by :port.
    host: String,
    #[arg(short = 'u', long)]
    username: String,
    #[arg(short = 'd', long)]
    domain: Option<String>,
    /// Read from the environment or prompt when absent.
    #[arg(long, env = "RDP_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    #[arg(long, default_value_t = 1280)]
    width: u16,
    #[arg(long, default_value_t = 800)]
    height: u16,
    /// Use one local display per monitor at its native pixel resolution.
    #[arg(long)]
    fullscreen: bool,
    /// Windows keyboard layout ID (1031: German, 1033: US).
    #[arg(long, default_value_t = 1031)]
    keyboard_layout: u32,
    #[arg(long, default_value_t = 1)]
    monitors: usize,
    /// Zero-based index in the horizontal monitor row.
    #[arg(long, default_value_t = 0)]
    primary_monitor: usize,
    /// Accept an untrusted server certificate.
    #[arg(long)]
    insecure: bool,
    /// Keep video on the graphics pipeline instead of separate video remoting channels.
    #[arg(long)]
    no_video_optimization: bool,
}

struct Wake;

fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("IRONRDP_LOG").unwrap_or_else(|_| "warn".into()))
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))?;
    Layout::horizontal(options.monitors, options.width, options.height, options.primary_monitor)?;
    let password = match options.password {
        Some(password) => password,
        None => inquire::Password::new("Password:")
            .without_confirmation()
            .prompt()
            .context("password prompt failed")?,
    };
    // UNTESTED: The click that focuses a monitor window must also reach the remote desktop.
    // Check first clicks between native windows when updating SDL.
    sdl3::hint::set("SDL_MOUSE_FOCUS_CLICKTHROUGH", "1");
    let sdl = sdl3::init().context("failed to initialize SDL")?;
    let video = sdl.video().context("failed to initialize SDL video")?;
    let events = sdl.event().context("failed to initialize SDL events")?;
    events
        .register_custom_event::<Wake>()
        .context("failed to register SDL wake event")?;
    let wake = events.event_sender();
    let mut pump = sdl.event_pump().context("failed to create SDL event pump")?;
    let canvases = app::create_windows(
        &video,
        &options.host,
        options.monitors,
        (options.width, options.height),
        options.fullscreen,
    )?;
    let layout = app::window_layout(&canvases, options.primary_monitor)?;
    tracing::info!(size = ?layout.size(), monitors = ?layout.monitors(), "Negotiating native desktop resolution");
    let mut builder = ConfigBuilder::new()
        .with_destination(Destination::new(options.host.clone())?)
        .with_username(options.username)
        .with_password(password)
        // UNTESTED: SDL must keep playback disabled when Cargo unifies the sound feature.
        // Check standalone and combined viewer builds when changing this setting.
        .with_sound(false)
        .with_keyboard_layout(options.keyboard_layout)
        .with_connection_type(ironrdp_pdu::gcc::ConnectionType::Lan)
        .with_graphics_pipeline(true)
        .with_graphics_capabilities(ironrdp_viewer_sdl::graphics::capabilities())
        .with_client_build(0)
        .with_client_dir("C:\\Windows\\System32\\mstscax.dll")
        .with_client_name("ironrdp-sdl")
        .with_platform(if cfg!(windows) {
            MajorPlatformType::WINDOWS
        } else if cfg!(target_os = "macos") {
            MajorPlatformType::MACINTOSH
        } else {
            MajorPlatformType::UNIX
        })
        .with_certificate_validation(CertificateValidation::Strict)
        .with_desktop_width(layout.size().0)
        .with_desktop_height(layout.size().1)
        .with_monitor_layout(layout.client_monitor_data());
    if let Some(domain) = options.domain {
        builder = builder.with_domain(domain);
    }
    if options.insecure {
        builder = builder.with_certificate_validation(CertificateValidation::DangerouslyAcceptInvalidCertificate);
    }
    if options.no_video_optimization {
        builder = builder.with_video_optimized_remoting(false);
    }
    let config = builder.build().context("invalid RDP configuration")?;
    let (output_sender, mut output_receiver) = output_channel(8);
    let client = RdpClient::new(config, output_sender)
        .with_desktop_updates()
        .with_auto_reconnect(0);
    let input = client.input_sender();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create RDP runtime")?;
    let (sender, mut receiver) = mpsc::channel(8);
    let forwarder = runtime.spawn(async move {
        while let Some(event) = output_receiver.recv().await {
            if sender.send(event).await.is_err() {
                break;
            }
            // The payload stays in the bounded channel; the SDL event only wakes the main thread.
            // A full SDL event queue is harmless because the event loop also has a bounded wait.
            let _ = wake.push_custom_event(Wake);
        }
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let connection = std::thread::Builder::new()
        .name("ironrdp-sdl-connection".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                tokio::select! {
                    () = async move {
                        Box::pin(client.run()).await;
                        let _ = forwarder.await;
                    } => {},
                    _ = stopped => {},
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(2));
        })
        .context("failed to start RDP connection thread")?;
    let result = app::run(&sdl, &mut pump, &mut receiver, &input, canvases, layout);
    input.request_close();
    drop(receiver);
    let _ = stop.send(());
    connection
        .join()
        .map_err(|_| anyhow::anyhow!("RDP connection thread panicked"))?;
    result
}
