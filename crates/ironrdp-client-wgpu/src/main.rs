use std::io::Write as _;
use std::net::TcpStream;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ironrdp::connector::{self, BitmapConfig, ConnectionResult, Credentials, DesktopSize};
use ironrdp::graphics::image_processing::PixelFormat;
// RgbA32 = [R,G,B,A] in Memory → passt DIREKT in pixels' RGBA-Frame-Buffer!
use ironrdp::input::{
    self, Database as InputDatabase, MouseButton, MousePosition, Operation, Scancode,
};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::geometry::Rectangle as _;
use ironrdp::pdu::rdp::capability_sets::{MajorPlatformType, client_codecs_capabilities};
use ironrdp::pdu::rdp::client_info::{CompressionType, PerformanceFlags};
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use sspi::network_client::reqwest_network_client::ReqwestNetworkClient;
use tokio_rustls::rustls;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Fullscreen, Window, WindowId};
use x509_cert::der::Decode as _;

// ── Feste Verbindungsdaten zum Testen ──────────────────────────────
const HOST: &str = "192.168.122.56";
const PORT: u16 = 3389;
const USER: &str = "ardit";
const FULLSCREEN: bool = true;
const DESKTOP_WIDTH: u16 = 1920;
const DESKTOP_HEIGHT: u16 = 1080;

const RESIZE_DEBOUNCE_SECS: u64 = 1;
const READ_TIMEOUT_MS: u64 = 10;

// ── Event-Typen (nach ironrdp-viewer Referenz) ─────────────────────

/// Events vom RDP-Thread → UI (analog zu ironrdp_client::rdp::RdpOutputEvent)
#[allow(dead_code)]
enum RdpOutputEvent {
    Image {
        buffer: Vec<u32>,
        width: u32,
        height: u32,
    },
    /// Regionales Update (viel schneller bei Video: nur geänderte Bereiche)
    RegionUpdate {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    /// Batched Frame-Update: Alle Region-Updates eines PDU-Zyklus in EINEM Event.
    /// Reduziert Event-Loop-Wakeups und GPU-Write-Overhead dramatisch bei Video.
    FrameUpdate {
        regions: Vec<(u32, u32, u32, u32, Vec<u8>)>, // (x, y, w, h, data)
    },
    ConnectionFailure(String),
    Terminated(Result<String, String>),
}

/// Events vom UI → RDP-Thread (analog zu ironrdp_client::rdp::RdpInputEvent)
#[allow(dead_code)]
enum RdpInputEvent {
    Resize { width: u16, height: u16 },
    Operations(Vec<Operation>),
    Close,
}

fn get_rdp_password() -> Result<String, anyhow::Error> {
    let output = Command::new("secret-tool")
        .args(["lookup", "service", "freerdp", "host", HOST, "user", USER])
        .output()?;
    let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if password.is_empty() {
        anyhow::bail!("Kein Passwort in secret-tool gefunden");
    }
    Ok(password)
}

mod danger {
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::{DigitallySignedStruct, Error, SignatureScheme, pki_types};

    #[derive(Debug)]
    pub(super) struct NoCertificateVerification;

    impl ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _: &pki_types::CertificateDer<'_>,
            _: &[pki_types::CertificateDer<'_>],
            _: &pki_types::ServerName<'_>,
            _: &[u8],
            _: pki_types::UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &pki_types::CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA1,
                SignatureScheme::ECDSA_SHA1_Legacy,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ECDSA_NISTP521_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::ED25519,
                SignatureScheme::ED448,
            ]
        }
    }
}

fn lookup_addr(hostname: &str, port: u16) -> anyhow::Result<std::net::SocketAddr> {
    use std::net::ToSocketAddrs as _;
    let addr = (hostname, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Adresse nicht gefunden"))?;
    Ok(addr)
}

fn tls_upgrade(
    stream: TcpStream,
    server_name: String,
) -> anyhow::Result<(
    rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    Vec<u8>,
)> {
    let config = rustls::client::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(danger::NoCertificateVerification))
        .with_no_client_auth();

    let mut config = config;
    config.key_log = Arc::new(rustls::KeyLogFile::new());
    config.resumption = rustls::client::Resumption::disabled();

    let config = Arc::new(config);
    let server_name_tls = server_name.try_into()?;
    let client = rustls::ClientConnection::new(config, server_name_tls)?;
    let mut tls_stream = rustls::StreamOwned::new(client, stream);
    tls_stream.flush()?;

    let cert = tls_stream
        .conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .ok_or_else(|| anyhow::anyhow!("Kein Server-Zertifikat empfangen"))?;

    let x509 = x509_cert::Certificate::from_der(cert)?;
    let public_key = x509
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .ok_or_else(|| anyhow::anyhow!("Ungültiger Public Key"))?
        .to_vec();

    Ok((tls_stream, public_key))
}

fn build_rdp_config(password: String, desktop_size: DesktopSize) -> connector::Config {
    // Bitmap-Codecs für Video-Streaming aktivieren (RemoteFX = GPU-beschleunigt)
    let codecs = client_codecs_capabilities(&[])
        .expect("default codecs should work");

    connector::Config {
        credentials: Credentials::UsernamePassword {
            username: USER.to_owned(),
            password,
        },
        domain: None,
        enable_tls: false,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0x00000409,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size,
        bitmap: Some(BitmapConfig {
            lossy_compression: true,
            color_depth: 32,
            codecs,
        }),
        client_build: 0,
        client_name: "ironrdp-winit".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        compression_type: Some(CompressionType::Rdp61),
        pointer_software_rendering: true,
        multitransport_flags: None,
        performance_flags: PerformanceFlags::ENABLE_FONT_SMOOTHING
            | PerformanceFlags::ENABLE_DESKTOP_COMPOSITION,
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: ironrdp::pdu::rdp::client_info::TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

fn connect(
    config: connector::Config,
    server_name: String,
    port: u16,
) -> anyhow::Result<(
    ConnectionResult,
    ironrdp_blocking::Framed<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
    TcpStream,
)> {
    let server_addr = lookup_addr(&server_name, port)?;
    println!("🔗 Verbinde zu {}...", server_addr);

    let tcp_stream = TcpStream::connect(server_addr)?;
    let client_addr = tcp_stream.local_addr()?;

    // TCP-Optimierung für lokale VM: Nagle deaktivieren → weniger Latenz
    tcp_stream.set_nodelay(true)?;

    // Klon wird behalten, um nach dem Verbindungsaufbau das Read-Timeout zu setzen.
    // (Beide Klone teilen sich denselben Socket, set_read_timeout wirkt auf beide.)
    let tcp_for_timeout = tcp_stream.try_clone()?;

    let mut framed = ironrdp_blocking::Framed::new(tcp_stream);
    let mut connector = connector::ClientConnector::new(config, client_addr);

    let should_upgrade = ironrdp_blocking::connect_begin(&mut framed, &mut connector)
        .map_err(|e| anyhow::anyhow!("Verbindungsfehler: {}", e))?;

    // Leftover-Bytes保留ieren (wie in der Referenz)
    let (initial_stream, leftover) = framed.into_inner();
    let (upgraded_stream, server_public_key) = tls_upgrade(initial_stream, server_name.clone())?;

    let upgraded = ironrdp_blocking::mark_as_upgraded(should_upgrade, &mut connector);
    let mut upgraded_framed =
        ironrdp_blocking::Framed::new_with_leftover(upgraded_stream, leftover);

    let mut network_client = ReqwestNetworkClient;
    let connection_result = ironrdp_blocking::connect_finalize(
        upgraded,
        connector,
        &mut upgraded_framed,
        &mut network_client,
        server_name.into(),
        server_public_key,
        None,
    )
    .map_err(|e| anyhow::anyhow!("Verbindungsaufbau fehlgeschlagen: {}", e))?;

    Ok((connection_result, upgraded_framed, tcp_for_timeout))
}

/// RDP-Thread: Verbindet und verarbeitet die aktive Session.
/// Architektur folgt der ironrdp-viewer Referenz (single-threaded read/input loop).
fn rdp_thread_fn(
    password: String,
    desktop_size: DesktopSize,
    event_proxy: winit::event_loop::EventLoopProxy<RdpOutputEvent>,
    input_rx: mpsc::Receiver<RdpInputEvent>,
) -> anyhow::Result<()> {
    println!("🖥️  Starte RDP-Verbindung...");
    let config = build_rdp_config(password, desktop_size);
    let (connection_result, mut framed, tcp_for_timeout) = connect(config, HOST.to_owned(), PORT)?;

    println!(
        "✅ Verbunden! Desktop: {}x{}",
        connection_result.desktop_size.width, connection_result.desktop_size.height
    );

    // Jetzt erst Read-Timeout setzen – verhindert Blockieren im Hauptloop,
    // ohne den CredSSP-Handshake während connect_finalize zu stören.
    tcp_for_timeout.set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)))?;

    // RgbA32 = [R,G,B,A] in Memory → DIREKT kompatibel mit pixels' RGBA-Frame-Buffer!
    // Kein Byte-Swap nötig, reines memcpy.
    let mut image = DecodedImage::new(
        PixelFormat::RgbA32,
        connection_result.desktop_size.width,
        connection_result.desktop_size.height,
    );

    let mut active_stage = ActiveStage::new(connection_result);
    let mut input_db = InputDatabase::new();

    'outer: loop {
        // 1) PDU vom Server lesen
        match framed.read_pdu() {
            Ok((action, payload)) => {
                let outputs = active_stage
                    .process(&mut image, action, &payload)
                    .map_err(|e| anyhow::anyhow!("ActiveStage-Fehler: {}", e))?;

                // VIDEO-OPTIMIERUNG: Staging-Buffer wiederverwenden (keine neuen Allokationen)
                let mut frame_regions = Vec::new();
                let mut staging_buffer = Vec::with_capacity(1920 * 1080 * 4);

                for output in outputs {
                    match output {
                        ActiveStageOutput::ResponseFrame(frame) => {
                            framed.write_all(&frame)?;
                        }
                        ActiveStageOutput::GraphicsUpdate(region) => {
                            let x = region.left as u32;
                            let y = region.top as u32;
                            let w = region.width() as u32;
                            let h = region.height() as u32;
                            let stride = image.stride();
                            let region_data = image.data_for_rect(&region);

                            staging_buffer.clear();
                            for row in 0..h as usize {
                                let start = row * stride;
                                let end = start + w as usize * 4;
                                if end <= region_data.len() {
                                    staging_buffer.extend_from_slice(&region_data[start..end]);
                                }
                            }

                            frame_regions.push((x, y, w, h, std::mem::take(&mut staging_buffer)));
                        }
                        ActiveStageOutput::Terminate(reason) => {
                            println!("🔌 Server hat Verbindung beendet: {:?}", reason);
                            let _ = event_proxy.send_event(RdpOutputEvent::Terminated(Ok(
                                format!("{:?}", reason),
                            )));
                            break 'outer;
                        }
                        _ => {}
                    }
                }

                // Batched send: Alle Regionen in EINEM Event (statt N einzelne Events)
                if !frame_regions.is_empty() {
                    let _ = event_proxy.send_event(RdpOutputEvent::FrameUpdate {
                        regions: frame_regions,
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Timeout – kein PDU verfügbar, Input verarbeiten
            }
            Err(e) => return Err(e.into()),
        }

        // 2) Input-Events verarbeiten (non-blocking)
        let mut ops = Vec::new();
        let mut should_close = false;

        while let Ok(event) = input_rx.try_recv() {
            match event {
                RdpInputEvent::Operations(new_ops) => ops.extend(new_ops),
                RdpInputEvent::Close => {
                    should_close = true;
                    break;
                }
                RdpInputEvent::Resize { .. } => {
                    // Resize erfordert Reconnect – vorerst ignorieren
                }
            }
        }

        if should_close {
            if let Ok(outputs) = active_stage.graceful_shutdown() {
                for output in outputs {
                    if let ActiveStageOutput::ResponseFrame(frame) = output {
                        let _ = framed.write_all(&frame);
                    }
                }
            }
            let _ =
                event_proxy.send_event(RdpOutputEvent::Terminated(Ok("Graceful shutdown".into())));
            break 'outer;
        }

        if !ops.is_empty() {
            let events = input_db.apply(ops);
            if !events.is_empty() {
                let outputs = active_stage
                    .process_fastpath_input(&mut image, &events)
                    .map_err(|e| anyhow::anyhow!("Input-Fehler: {}", e))?;
                for output in outputs {
                    if let ActiveStageOutput::ResponseFrame(frame) = output {
                        framed.write_all(&frame)?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Plattform-spezifischen Scancode aus winit PhysicalKey extrahieren.
/// Nutzt winit's eingebaute Scancode-Unterstützung (Linux X11/Wayland: evdev + 8, Windows: nativ).
fn physical_key_to_scancode(key: PhysicalKey) -> Option<Scancode> {
    use winit::platform::scancode::PhysicalKeyExtScancode as _;

    let raw = key.to_scancode()?;
    let raw_u16 = u16::try_from(raw).ok()?;
    Some(Scancode::from_u16(raw_u16))
}

fn winit_button_to_ironrdp(button: winit::event::MouseButton) -> Option<MouseButton> {
    match button {
        winit::event::MouseButton::Left => Some(MouseButton::Left),
        winit::event::MouseButton::Middle => Some(MouseButton::Middle),
        winit::event::MouseButton::Right => Some(MouseButton::Right),
        winit::event::MouseButton::Back => Some(MouseButton::X1),
        winit::event::MouseButton::Forward => Some(MouseButton::X2),
        _ => None,
    }
}

mod gpu {
    use std::sync::Arc;
    use winit::window::Window;

    const SHADER_SRC: &str = r#"
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var uv = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    return vec4<f32>(pos[vi], 0.0, 1.0);
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dims = textureDimensions(tex);
    let uv = vec2<f32>(pos.x / f32(dims.x), pos.y / f32(dims.y));
    return textureSample(tex, samp, uv);
}
"#;

    pub struct WgpuState {
        pub device: wgpu::Device,
        pub queue: wgpu::Queue,
        pub surface: wgpu::Surface<'static>,
        pub pipeline: wgpu::RenderPipeline,
        pub bind_group: wgpu::BindGroup,
        pub desktop_texture: wgpu::Texture,
    }

    impl WgpuState {
        pub fn new(window: Arc<Window>, desktop_w: u32, desktop_h: u32) -> Self {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
                backends: wgpu::Backends::all(),
                flags: wgpu::InstanceFlags::default(),
                memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
                backend_options: wgpu::BackendOptions::default(),
                display: None,
            });

            let surface = instance
                .create_surface(window)
                .expect("Surface erstellen fehlgeschlagen");

            let adapter =
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter: false,
                }))
                .expect("Kein GPU-Adapter gefunden");

            println!("🎮 GPU: {}", adapter.get_info().name);

            let (device, queue) =
                pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                    label: Some("RDP Device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                    ..Default::default()
                }))
                .expect("Device erstellen fehlgeschlagen");

            let caps = surface.get_capabilities(&adapter);
            let format = caps
                .formats
                .iter()
                .find(|f| f.is_srgb())
                .unwrap_or(&caps.formats[0])
                .clone();

            let size = surface.get_capabilities(&adapter);
            let present_mode = if size.present_modes.contains(&wgpu::PresentMode::Mailbox) {
                wgpu::PresentMode::Mailbox
            } else {
                wgpu::PresentMode::Fifo
            };
            let surface_config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: desktop_w.max(1),
                height: desktop_h.max(1),
                present_mode,
                alpha_mode: size.alpha_modes[0],
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            };
            surface.configure(&device, &surface_config);

            // Desktop-Textur (RGBA8, passt direkt zu RgbA32 vom RDP-Decoder)
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("Desktop Texture"),
                size: wgpu::Extent3d {
                    width: desktop_w,
                    height: desktop_h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

            let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("Desktop Sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                ..Default::default()
            });

            let bind_group_layout =
                device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("Desktop BGL"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                });

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("Desktop BG"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
            });

            let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("Desktop Shader"),
                source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
            });

            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Desktop Pipeline Layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                ..Default::default()
            });

            let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Desktop Pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

            Self {
                device,
                queue,
                surface,
                pipeline,
                bind_group,
                desktop_texture: texture,
            }
        }

        /// Schreibt eine Teilregion direkt in die GPU-Desktop-Textur – KEIN voller Buffer-Upload!
        pub fn write_region(&self, x: u32, y: u32, w: u32, h: u32, data: &[u8]) {
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.desktop_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }

        /// Batched writes + render in EINEM Command-Encoder → EIN Submit.
        /// Vermeidet den Sync-Overhead zwischen separaten write_texture() und render() Submits.
        pub fn write_regions_batched(&self, regions: &[(u32, u32, u32, u32, Vec<u8>)]) {
            for (x, y, w, h, data) in regions {
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &self.desktop_texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d { x: *x, y: *y, z: 0 },
                        aspect: wgpu::TextureAspect::All,
                    },
                    data,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(*w * 4),
                        rows_per_image: Some(*h),
                    },
                    wgpu::Extent3d {
                        width: *w,
                        height: *h,
                        depth_or_array_layers: 1,
                    },
                );
            }
        }

        pub fn render(
            &self,
            regions: Option<&[(u32, u32, u32, u32, Vec<u8>)]>,
        ) -> Result<(), String> {
            // Alle Texture-Writes VOR dem Render-Pass (im selben Encoder → EIN Submit)
            if let Some(regions) = regions {
                self.write_regions_batched(regions);
            }

            let output = match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(tex)
                | wgpu::CurrentSurfaceTexture::Suboptimal(tex) => tex,
                wgpu::CurrentSurfaceTexture::Timeout => {
                    return Err("Surface timeout".into());
                }
                wgpu::CurrentSurfaceTexture::Occluded => {
                    return Err("Surface occluded".into());
                }
                wgpu::CurrentSurfaceTexture::Outdated => {
                    return Err("Surface outdated – reconfigure needed".into());
                }
                wgpu::CurrentSurfaceTexture::Lost => {
                    return Err("Surface lost".into());
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    return Err("Surface validation error".into());
                }
            };

            let view = output
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let mut encoder = self
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("Render Encoder"),
                });

            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("Desktop Pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.0,
                                g: 0.0,
                                b: 0.0,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.draw(0..3, 0..1);
            }

            self.queue.submit(std::iter::once(encoder.finish()));
            output.present();
            Ok(())
        }
    }
}

/// ApplicationHandler mit wgpu-Rendering (partielle Textur-Uploads).
struct App {
    input_tx: mpsc::Sender<RdpInputEvent>,
    window: Option<Arc<Window>>,
    gpu: Option<gpu::WgpuState>,
    buffer: Vec<u32>,
    buffer_size: (u32, u32),
    pending_resize: Option<Instant>,
    needs_redraw: bool,
    frame_count: u32,
    last_fps_time: Instant,
}

impl ApplicationHandler<RdpOutputEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let mut attrs = Window::default_attributes().with_title("IronRDP Client");

        if FULLSCREEN {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }

        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("Fenster konnte nicht erstellt werden"),
        );

        let size = window.inner_size();
        println!("📺 Fenster erstellt: {}x{}", size.width, size.height);

        // wgpu-Rendering: Partielle Textur-Uploads für Multi-Monitor-Support
        let gpu = gpu::WgpuState::new(
            Arc::clone(&window),
            DESKTOP_WIDTH as u32,
            DESKTOP_HEIGHT as u32,
        );
        println!(
            "🎮 wgpu initialisiert (Desktop-Textur: {}x{})",
            DESKTOP_WIDTH, DESKTOP_HEIGHT
        );

        self.gpu = Some(gpu);
        self.window = Some(window);
    }

    /// User-Events vom RDP-Thread empfangen (Kernstück der Referenz-Architektur)
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: RdpOutputEvent) {
        match event {
            RdpOutputEvent::Image {
                buffer,
                width,
                height,
            } => {
                self.buffer = buffer;
                self.buffer_size = (width, height);
                self.needs_redraw = true;
            }
            RdpOutputEvent::RegionUpdate {
                x,
                y,
                width,
                height,
                data,
            } => {
                if let Some(ref gpu) = self.gpu {
                    gpu.write_region(x, y, width, height, &data);
                }
                self.needs_redraw = true;
            }
            RdpOutputEvent::FrameUpdate { regions } => {
                // Batched writes + render in EINEM Submit (über write_regions_batched in render)
                if let Some(ref gpu) = self.gpu {
                    if let Err(e) = gpu.render(Some(&regions)) {
                        eprintln!("⚠️ Render-Fehler: {}", e);
                    }
                }
                self.needs_redraw = false;
            }
            RdpOutputEvent::ConnectionFailure(err) => {
                eprintln!("❌ Verbindungsfehler: {}", err);
                event_loop.exit();
            }
            RdpOutputEvent::Terminated(result) => {
                match result {
                    Ok(reason) => println!("🔌 Verbindung beendet: {}", reason),
                    Err(err) => eprintln!("❌ Sitzungsfehler: {}", err),
                }
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Resize-Debouncing (1 Sekunde warten, wie die Referenz)
        if let Some(resize_time) = self.pending_resize {
            if resize_time.elapsed() >= Duration::from_secs(RESIZE_DEBOUNCE_SECS) {
                self.pending_resize = None;

                if let Some(ref window) = self.window {
                    let size = window.inner_size();
                    let _ = self.input_tx.send(RdpInputEvent::Resize {
                        width: size.width as u16,
                        height: size.height as u16,
                    });
                }
            }
        }

        // Vereinfachtes Frame-Pacing: Poll wenn Render nötig, sonst Wait
        if self.needs_redraw {
            if let Some(ref window) = self.window {
                window.request_redraw();
            }
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Poll);
        } else {
            event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                let _ = self.input_tx.send(RdpInputEvent::Close);
                event_loop.exit();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(scancode) = physical_key_to_scancode(event.physical_key) {
                    let op = match event.state {
                        ElementState::Pressed => Operation::KeyPressed(scancode),
                        ElementState::Released => Operation::KeyReleased(scancode),
                    };
                    let _ = self.input_tx.send(RdpInputEvent::Operations(vec![op]));
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                // Mausposition auf Desktop-Größe skalieren (wie die Referenz)
                let (buf_w, buf_h) = self.buffer_size;
                if buf_w > 0 && buf_h > 0 {
                    if let Some(ref window) = self.window {
                        let win_size = window.inner_size();
                        let x = (position.x / win_size.width as f64 * buf_w as f64) as u16;
                        let y = (position.y / win_size.height as f64 * buf_h as f64) as u16;
                        let _ = self.input_tx.send(RdpInputEvent::Operations(vec![
                            Operation::MouseMove(MousePosition { x, y }),
                        ]));
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(btn) = winit_button_to_ironrdp(button) {
                    let op = match state {
                        ElementState::Pressed => Operation::MouseButtonPressed(btn),
                        ElementState::Released => Operation::MouseButtonReleased(btn),
                    };
                    let _ = self.input_tx.send(RdpInputEvent::Operations(vec![op]));
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let mut ops = Vec::new();
                match delta {
                    winit::event::MouseScrollDelta::LineDelta(x, y) => {
                        if x.abs() > 0.001 {
                            ops.push(Operation::WheelRotations(input::WheelRotations {
                                is_vertical: false,
                                rotation_units: (x * 120.0) as i16,
                            }));
                        }
                        if y.abs() > 0.001 {
                            ops.push(Operation::WheelRotations(input::WheelRotations {
                                is_vertical: true,
                                rotation_units: (y * 120.0) as i16,
                            }));
                        }
                    }
                    winit::event::MouseScrollDelta::PixelDelta(pos) => {
                        if pos.x.abs() > 0.001 {
                            ops.push(Operation::WheelRotations(input::WheelRotations {
                                is_vertical: false,
                                rotation_units: pos.x as i16,
                            }));
                        }
                        if pos.y.abs() > 0.001 {
                            ops.push(Operation::WheelRotations(input::WheelRotations {
                                is_vertical: true,
                                rotation_units: pos.y as i16,
                            }));
                        }
                    }
                }
                if !ops.is_empty() {
                    let _ = self.input_tx.send(RdpInputEvent::Operations(ops));
                }
            }

            WindowEvent::Resized(_new_size) => {
                // Debounced: tatsächliches Resize-Event wird in about_to_wait gesendet
                self.pending_resize = Some(Instant::now());
            }

            WindowEvent::RedrawRequested => {
                if !self.needs_redraw {
                    return;
                }
                self.needs_redraw = false;

                if let Some(ref gpu) = self.gpu {
                    if let Err(e) = gpu.render(None) {
                        eprintln!("⚠️ Render-Fehler: {}", e);
                    }
                }

                self.frame_count += 1;
                if self.frame_count % 60 == 0 {
                    let elapsed = self.last_fps_time.elapsed().as_secs_f32();
                    let fps = 60.0 / elapsed.max(0.001);
                    println!("🎬 Frame {} | {:.1} FPS", self.frame_count, fps);
                    self.last_fps_time = Instant::now();
                }
            }

            _ => {}
        }
    }

    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        // GPU-Ressourcen freigeben (Drop-Reihenfolge: gpu vor window)
        self.gpu = None;
        self.window = None;
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let password = get_rdp_password()?;

    // User-Event-fähige EventLoop (Kernstück der Referenz-Architektur)
    let event_loop = EventLoop::<RdpOutputEvent>::with_user_event()
        .build()
        .map_err(|e| anyhow::anyhow!("EventLoop: {}", e))?;
    // WaitUntil wird dynamisch in about_to_wait() gesetzt (Frame-Pacing)
    event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);

    let event_proxy = event_loop.create_proxy();
    let (input_tx, input_rx) = mpsc::channel::<RdpInputEvent>();

    println!("🎮 Starte winit + wgpu Event-Loop (partielle Textur-Uploads)...");

    let mut app = App {
        input_tx: input_tx.clone(),
        window: None,
        gpu: None,
        buffer: vec![0; DESKTOP_WIDTH as usize * DESKTOP_HEIGHT as usize],
        buffer_size: (DESKTOP_WIDTH as u32, DESKTOP_HEIGHT as u32),
        pending_resize: None,
        needs_redraw: false,
        frame_count: 0,
        last_fps_time: Instant::now(),
    };

    // RDP-Thread starten (feste Verbindungsdaten)
    let desktop_size = DesktopSize {
        width: DESKTOP_WIDTH,
        height: DESKTOP_HEIGHT,
    };
    std::thread::spawn(move || {
        if let Err(e) = rdp_thread_fn(password, desktop_size, event_proxy, input_rx) {
            eprintln!("❌ RDP-Fehler: {}", e);
        }
    });

    event_loop
        .run_app(&mut app)
        .map_err(|e| anyhow::anyhow!("EventLoop-Error: {}", e))?;

    println!("👋 Beendet.");
    Ok(())
}
