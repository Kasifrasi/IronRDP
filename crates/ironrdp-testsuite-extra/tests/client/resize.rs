use core::time::Duration;
use std::path::Path;

use ironrdp::displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_client::config::{ConfigBuilder, Destination};
use ironrdp_client::output_channel::{OutputEventReceiver, output_channel};
use ironrdp_client::rdp::{
    DesktopUpdate, DesktopUpdateBatch, DisplayResizeFallbackReason, RdpClient, RdpInputEvent, RdpInputSender,
    RdpOutputEvent,
};
use ironrdp_egfx::pdu::{
    CapabilitiesAdvertisePdu, CapabilitySet, Color, CreateSurfacePdu, EndFramePdu, GfxPdu, MapSurfaceToOutputPdu,
    PixelFormat, ResetGraphicsPdu, SolidFillPdu, StartFramePdu, Timestamp,
};
use ironrdp_egfx::server::GraphicsPipelineHandler;
use ironrdp_pdu::geometry::{ExclusiveRectangle, InclusiveRectangle};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_server::{
    DesktopSize, DisplayContext, DisplayUpdate, EgfxServerMessage, GfxContext, GfxServerFactory, GfxServerHandle,
    KeyboardEvent, MouseEvent, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates, RdpServerInputHandler,
    ServerEvent, ServerEventSender, ServerResult, TlsIdentityCtx,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

struct Display {
    layouts: UnboundedSender<DisplayControlMonitorLayout>,
    graphics: UnboundedSender<GfxServerHandle>,
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: 800,
            height: 600,
        }
    }

    async fn updates(&mut self, ctx: DisplayContext) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        self.graphics.send(ctx.gfx_handle.unwrap()).unwrap();
        Ok(Box::new(IdleDisplay))
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        self.layouts.send(layout).unwrap();
    }
}

struct IdleDisplay;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for IdleDisplay {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        core::future::pending().await
    }
}

struct GraphicsFactory(UnboundedSender<()>);

struct InputBarrier(UnboundedSender<()>);

impl RdpServerInputHandler for InputBarrier {
    fn keyboard(&mut self, event: KeyboardEvent) {
        if matches!(event, KeyboardEvent::UnicodePressed(0x7A)) {
            self.0.send(()).unwrap();
        }
    }

    fn mouse(&mut self, _: MouseEvent) {}
}

impl ServerEventSender for GraphicsFactory {
    fn set_sender(&mut self, _: UnboundedSender<ServerEvent>) {}
}

impl GfxServerFactory for GraphicsFactory {
    fn build_gfx_handler(&self, _: GfxContext) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(GraphicsFactory(self.0.clone()))
    }
}

impl GraphicsPipelineHandler for GraphicsFactory {
    fn capabilities_advertise(&mut self, _: &CapabilitiesAdvertisePdu) {}

    fn on_ready(&mut self, _: &CapabilitySet) {
        self.0.send(()).unwrap();
    }
}

struct Session {
    input: RdpInputSender,
    output: OutputEventReceiver,
    layouts: UnboundedReceiver<DisplayControlMonitorLayout>,
    graphics: GfxServerHandle,
    events: UnboundedSender<ServerEvent>,
    input_barrier: UnboundedReceiver<()>,
    client: JoinHandle<()>,
    server: JoinHandle<ServerResult<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.client.abort();
        self.server.abort();
    }
}

impl Session {
    async fn connect() -> Self {
        Self::connect_with_desktop_updates(false).await
    }

    async fn connect_with_desktop_updates(desktop_updates: bool) -> Self {
        let certs = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/certs");
        let identity =
            TlsIdentityCtx::init_from_paths(&certs.join("server-cert.pem"), &certs.join("server-key.pem")).unwrap();
        let (layouts_tx, layouts) = mpsc::unbounded_channel();
        let (graphics_tx, mut graphics_rx) = mpsc::unbounded_channel();
        let (ready_tx, mut ready_rx) = mpsc::unbounded_channel();
        let (input_barrier_tx, input_barrier) = mpsc::unbounded_channel();
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_tls(identity.make_acceptor().unwrap())
            .with_input_handler(InputBarrier(input_barrier_tx))
            .with_display_handler(Display {
                layouts: layouts_tx,
                graphics: graphics_tx,
            })
            .with_gfx_factory(Some(Box::new(GraphicsFactory(ready_tx))))
            .build();
        let events = server.event_sender().clone();
        let handle = server.handle().clone();
        let server = tokio::task::spawn_local(async move { server.run().await });
        let address = handle.local_addr().await.unwrap().unwrap();
        let config = ConfigBuilder::new()
            .with_destination(Destination::from_parts("127.0.0.1", address.port()))
            .with_username("")
            .with_password("")
            .with_client_build(0)
            .with_client_dir("tests")
            .with_client_name("resize-test")
            .with_platform(MajorPlatformType::UNIX)
            .with_desktop_width(800)
            .with_desktop_height(600)
            .with_credssp(false)
            .with_graphics_pipeline(true)
            .build()
            .unwrap();
        let (sender, mut output) = output_channel(64);
        let client = RdpClient::new(config, sender);
        let client = if desktop_updates {
            client.with_desktop_updates()
        } else {
            client
        };
        let input = client.input_sender();
        let client = tokio::task::spawn_local(client.run());
        loop {
            let event = output.recv().await.unwrap();
            assert!(
                !matches!(event, RdpOutputEvent::ConnectionFailure(_)),
                "connection failed: {event:?}"
            );
            if matches!(event, RdpOutputEvent::Connected) {
                break;
            }
        }
        ready_rx.recv().await.unwrap();
        let graphics = graphics_rx.recv().await.unwrap();
        Self {
            input,
            output,
            layouts,
            graphics,
            events,
            input_barrier,
            client,
            server,
        }
    }

    async fn resize(&mut self, width: u16, height: u16) {
        self.input
            .try_send(RdpInputEvent::Resize {
                width,
                height,
                scale_factor: 100,
                physical_size: None,
            })
            .unwrap();
        let layout = self.layouts.recv().await.unwrap();
        assert_eq!(layout.desktop_size(), (u32::from(width), u32::from(height)));
    }

    fn reset_graphics(&self, width: u16, height: u16) {
        let mut graphics = self.graphics.lock().unwrap();
        graphics.resize(width, height);
        let messages = ironrdp_dvc::encode_dvc_messages(
            graphics.channel_id().unwrap(),
            graphics.drain_output(),
            ironrdp_svc::ChannelFlags::empty(),
        )
        .unwrap();
        self.events
            .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages { messages }))
            .unwrap();
    }

    fn send_graphics(&self, pdus: &[GfxPdu]) {
        use ironrdp_dvc::pdu::{DataPdu, DrdynvcDataPdu, DrdynvcServerPdu};

        let channel_id = self.graphics.lock().unwrap().channel_id().unwrap();
        let mut payload = Vec::new();
        for pdu in pdus {
            payload.extend(ironrdp_core::encode_vec(pdu).unwrap());
        }
        let message = DrdynvcServerPdu::Data(DrdynvcDataPdu::Data(DataPdu::new(
            channel_id,
            ironrdp_graphics::zgfx::wrap_uncompressed(&payload),
        )));
        self.events
            .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                messages: vec![ironrdp_svc::SvcMessage::from(message)],
            }))
            .unwrap();
    }

    async fn initialize_desktop(&mut self, width: u16, height: u16) {
        self.send_graphics(&[
            GfxPdu::ResetGraphics(ResetGraphicsPdu {
                width: u32::from(width),
                height: u32::from(height),
                monitors: Vec::new(),
            }),
            GfxPdu::CreateSurface(CreateSurfacePdu {
                surface_id: 1,
                width,
                height,
                pixel_format: PixelFormat::XRgb,
            }),
            GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                surface_id: 1,
                output_origin_x: 0,
                output_origin_y: 0,
            }),
            GfxPdu::StartFrame(StartFramePdu {
                timestamp: Timestamp {
                    milliseconds: 0,
                    seconds: 0,
                    minutes: 0,
                    hours: 0,
                },
                frame_id: 1,
            }),
            GfxPdu::SolidFill(SolidFillPdu {
                surface_id: 1,
                fill_pixel: Color {
                    b: 0,
                    g: 0,
                    r: 0,
                    xa: 0xFF,
                },
                rectangles: vec![ExclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                }],
            }),
            GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }),
        ]);
        let _ = self.desktop_update().await;
    }

    async fn desktop_update(&mut self) -> DesktopUpdate {
        loop {
            let event = self.output.recv().await.expect("client remains active");
            match event {
                RdpOutputEvent::DesktopUpdate(update) => return update,
                event => assert!(
                    !matches!(
                        event,
                        RdpOutputEvent::ConnectionFailure(_) | RdpOutputEvent::Terminated(_)
                    ),
                    "unexpected session failure: {event:?}"
                ),
            }
        }
    }

    fn fill_frame(&self, frame_id: u32, fill_pixel: Color, rectangles: Vec<ExclusiveRectangle>) {
        self.send_graphics(&[
            GfxPdu::StartFrame(StartFramePdu {
                timestamp: Timestamp {
                    milliseconds: 0,
                    seconds: 0,
                    minutes: 0,
                    hours: 0,
                },
                frame_id,
            }),
            GfxPdu::SolidFill(SolidFillPdu {
                surface_id: 1,
                fill_pixel,
                rectangles,
            }),
            GfxPdu::EndFrame(EndFramePdu { frame_id }),
        ]);
    }

    async fn stays_connected(&mut self) {
        let observation = async {
            loop {
                let event = self.output.recv().await.expect("client remains active");
                assert!(
                    !matches!(
                        event,
                        RdpOutputEvent::DisplayResizeFallback(_)
                            | RdpOutputEvent::ConnectionFailure(_)
                            | RdpOutputEvent::Connected
                    ),
                    "unexpected connection event: {event:?}"
                );
            }
        };
        assert!(tokio::time::timeout(Duration::from_secs(4), observation).await.is_err());
    }

    async fn resize_fallback(&mut self) -> DisplayResizeFallbackReason {
        loop {
            let event = self.output.recv().await.expect("client reports the fallback");
            assert!(
                !matches!(event, RdpOutputEvent::ConnectionFailure(_)),
                "connection failed: {event:?}"
            );
            if let RdpOutputEvent::DisplayResizeFallback(reason) = event {
                return reason;
            }
        }
    }

    async fn wait_for_input(&mut self) {
        self.input
            .try_send(RdpInputEvent::FastPath(
                vec![FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), 0x7A)].into(),
            ))
            .unwrap();
        self.input_barrier.recv().await.unwrap();
    }
}

// oracle: regression completed overlapping desktop damage must fit one snapshot without losing final pixels.
#[tokio::test]
async fn overlapping_egfx_damage_delivers_one_complete_snapshot() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect_with_desktop_updates(true).await;
            session.initialize_desktop(800, 600).await;
            session.send_graphics(&[
                GfxPdu::StartFrame(StartFramePdu {
                    timestamp: Timestamp {
                        milliseconds: 0,
                        seconds: 0,
                        minutes: 0,
                        hours: 0,
                    },
                    frame_id: 2,
                }),
                GfxPdu::SolidFill(SolidFillPdu {
                    surface_id: 1,
                    fill_pixel: Color {
                        b: 0x30,
                        g: 0x20,
                        r: 0x10,
                        xa: 0xFF,
                    },
                    rectangles: vec![ExclusiveRectangle {
                        left: 0,
                        top: 0,
                        right: 800,
                        bottom: 400,
                    }],
                }),
                GfxPdu::SolidFill(SolidFillPdu {
                    surface_id: 1,
                    fill_pixel: Color {
                        b: 0x60,
                        g: 0x50,
                        r: 0x40,
                        xa: 0xFF,
                    },
                    rectangles: vec![ExclusiveRectangle {
                        left: 0,
                        top: 200,
                        right: 800,
                        bottom: 600,
                    }],
                }),
                GfxPdu::EndFrame(EndFramePdu { frame_id: 2 }),
            ]);
            let update = session.desktop_update().await;
            assert_eq!(
                update.region(),
                InclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: 799,
                    bottom: 599,
                }
            );
            assert_eq!(update.buffer().len(), 800 * 600);
            assert!(update.is_batch_end());
            assert!(update.buffer()[..800 * 200].iter().all(|pixel| *pixel == 0x0010_2030));
            assert!(update.buffer()[800 * 200..].iter().all(|pixel| *pixel == 0x0040_5060));
        }),
    )
    .await
    .expect("overlapping desktop damage finishes");
}

// oracle: regression user report of mixed old/new desktop rectangles; completed sparse damage has one final boundary.
#[tokio::test]
async fn completed_sparse_egfx_damage_marks_only_its_final_region() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect_with_desktop_updates(true).await;
            session.initialize_desktop(800, 600).await;
            session.fill_frame(
                2,
                Color {
                    b: 0x30,
                    g: 0x20,
                    r: 0x10,
                    xa: 0xFF,
                },
                (0..65)
                    .map(|index| ExclusiveRectangle {
                        left: index * 2,
                        top: 3,
                        right: index * 2 + 1,
                        bottom: 4,
                    })
                    .collect(),
            );
            let mut pending = DesktopUpdateBatch::default();
            for index in 0..64 {
                let update = session.desktop_update().await;
                assert!(!update.is_batch_end());
                assert_eq!(
                    update.region(),
                    InclusiveRectangle {
                        left: index * 2,
                        top: 3,
                        right: index * 2,
                        bottom: 3,
                    }
                );
                assert_eq!(update.buffer(), [0x0010_2030]);
                assert!(pending.push(update).is_none());
            }
            let last = session.desktop_update().await;
            assert!(last.is_batch_end());
            assert_eq!(
                last.region(),
                InclusiveRectangle {
                    left: 128,
                    top: 3,
                    right: 128,
                    bottom: 3,
                }
            );
            assert_eq!(last.buffer(), [0x0010_2030]);
            let completed = pending.push(last).expect("the last region releases the batch");
            assert_eq!(completed.len(), 65);
            let metadata_bytes = completed.capacity() * size_of::<DesktopUpdate>();
            let pixel_bytes: usize = completed
                .into_iter()
                .map(|update| update.into_parts().0.capacity() * size_of::<u32>())
                .sum();
            assert!(pixel_bytes + metadata_bytes <= 800 * 600 * 4 + size_of::<DesktopUpdate>());

            session.fill_frame(
                3,
                Color {
                    b: 0xC0,
                    g: 0xB0,
                    r: 0xA0,
                    xa: 0xFF,
                },
                vec![ExclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: 1,
                    bottom: 1,
                }],
            );
            let next = session.desktop_update().await;
            assert!(next.is_batch_end());
            assert_eq!(next.buffer(), [0x00A0_B0C0]);
            assert_eq!(pending.push(next).expect("a standalone batch is immediate").len(), 1);
        }),
    )
    .await
    .expect("sparse desktop damage finishes");
}

// oracle: regression desktop staging includes metadata and spare vector capacity, even for tiny damage regions.
#[tokio::test]
async fn tiny_egfx_regions_fall_back_before_staging_metadata_exceeds_a_snapshot() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect_with_desktop_updates(true).await;
            session.initialize_desktop(1000, 1).await;
            session.fill_frame(
                2,
                Color {
                    b: 0x30,
                    g: 0x20,
                    r: 0x10,
                    xa: 0xFF,
                },
                (0..65)
                    .map(|index| ExclusiveRectangle {
                        left: index,
                        top: 0,
                        right: index + 1,
                        bottom: 1,
                    })
                    .collect(),
            );
            let update = session.desktop_update().await;
            assert!(update.is_batch_end());
            assert_eq!(
                update.region(),
                InclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: 999,
                    bottom: 0,
                }
            );
            assert_eq!(update.buffer().len(), 1000);
            assert!(update.buffer()[..65].iter().all(|pixel| *pixel == 0x0010_2030));
            assert!(update.buffer()[65..].iter().all(|pixel| *pixel == 0));
        }),
    )
    .await
    .expect("metadata-bounded desktop damage finishes");
}

// oracle: spec MS-RDPEGFX §2.2.2.14 (ResetGraphics applies monitor definitions before the following frame)
#[tokio::test]
async fn egfx_monitor_layout_precedes_its_desktop_update_at_the_same_extent() {
    use ironrdp_egfx::pdu::{
        Color, CreateSurfacePdu, EndFramePdu, GfxPdu, MapSurfaceToOutputPdu, PixelFormat, ResetGraphicsPdu,
        SolidFillPdu, StartFramePdu, Timestamp,
    };
    use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
    use ironrdp_pdu::geometry::ExclusiveRectangle;

    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect_with_desktop_updates(true).await;
            for secondary_left in [400, -400] {
                let monitors = vec![
                    Monitor {
                        left: 0,
                        top: 0,
                        right: 399,
                        bottom: 599,
                        flags: MonitorFlags::PRIMARY,
                    },
                    Monitor {
                        left: secondary_left,
                        top: 0,
                        right: secondary_left + 399,
                        bottom: 599,
                        flags: MonitorFlags::empty(),
                    },
                ];
                session.send_graphics(&[
                    GfxPdu::ResetGraphics(ResetGraphicsPdu {
                        width: 800,
                        height: 600,
                        monitors: monitors.clone(),
                    }),
                    GfxPdu::CreateSurface(CreateSurfacePdu {
                        surface_id: 1,
                        width: 800,
                        height: 600,
                        pixel_format: PixelFormat::XRgb,
                    }),
                    GfxPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
                        surface_id: 1,
                        output_origin_x: 0,
                        output_origin_y: 0,
                    }),
                    GfxPdu::StartFrame(StartFramePdu {
                        timestamp: Timestamp {
                            milliseconds: 0,
                            seconds: 0,
                            minutes: 0,
                            hours: 0,
                        },
                        frame_id: 1,
                    }),
                    GfxPdu::SolidFill(SolidFillPdu {
                        surface_id: 1,
                        fill_pixel: Color {
                            b: 0x30,
                            g: 0x20,
                            r: 0x10,
                            xa: 0xFF,
                        },
                        rectangles: vec![ExclusiveRectangle {
                            left: 0,
                            top: 0,
                            right: 800,
                            bottom: 600,
                        }],
                    }),
                    GfxPdu::EndFrame(EndFramePdu { frame_id: 1 }),
                ]);
                let mut latest_layout = None;
                loop {
                    let event = session.output.recv().await.expect("client remains active");
                    match event {
                        RdpOutputEvent::MonitorLayout(actual) => latest_layout = Some(actual),
                        RdpOutputEvent::DesktopUpdate(update) => {
                            assert_eq!(latest_layout, Some(monitors));
                            assert_eq!((update.width().get(), update.height().get()), (800, 600));
                            assert_eq!(update.buffer().len(), 800 * 600);
                            assert!(update.buffer().iter().all(|pixel| *pixel == 0x0010_2030));
                            break;
                        }
                        event => assert!(
                            !matches!(
                                event,
                                RdpOutputEvent::ConnectionFailure(_) | RdpOutputEvent::Terminated(_)
                            ),
                            "unexpected session failure: {event:?}"
                        ),
                    }
                }
            }
        }),
    )
    .await
    .expect("monitor layout test finishes");
}

// oracle: spec MS-RDPEDISP v10.0 §1.3 (EGFX resizes without reactivation)
#[tokio::test]
async fn egfx_output_reset_completes_a_resize_without_reconnecting() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect().await;
            session.resize(1024, 768).await;
            session.reset_graphics(1024, 768);
            session.stays_connected().await;
        }),
    )
    .await
    .expect("resize test finishes");
}

// oracle: regression Devolutions/IronRDP#2014 (servers may adjust the requested size)
#[tokio::test]
async fn an_adjusted_egfx_size_ends_the_resize_attempt() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect().await;
            session.resize(1024, 768).await;
            session.reset_graphics(1000, 750);
            session.stays_connected().await;
            session.resize(1024, 768).await;
        }),
    )
    .await
    .expect("resize test finishes");
}

// oracle: regression Devolutions/IronRDP#2014 (an unrelated reset must not disable resize recovery)
#[tokio::test]
async fn a_reset_at_the_starting_size_keeps_the_resize_timeout() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect().await;
            session.resize(1024, 768).await;
            session.reset_graphics(800, 600);
            assert_eq!(
                session.resize_fallback().await,
                DisplayResizeFallbackReason::ReactivationTimedOut
            );
        }),
    )
    .await
    .expect("resize test finishes");
}

// oracle: regression Devolutions/IronRDP#2014 (a completed resize must release the queued request)
#[tokio::test]
async fn an_egfx_reset_releases_the_newest_pending_resize() {
    tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::LocalSet::new().run_until(async {
            let mut session = Session::connect().await;
            session.resize(1024, 768).await;
            session
                .input
                .try_send(RdpInputEvent::Resize {
                    width: 1200,
                    height: 800,
                    scale_factor: 100,
                    physical_size: None,
                })
                .unwrap();
            session
                .input
                .try_send(RdpInputEvent::Resize {
                    width: 1280,
                    height: 800,
                    scale_factor: 100,
                    physical_size: None,
                })
                .unwrap();
            // The server seeing the key proves the client consumed both preceding resize events.
            session.wait_for_input().await;
            assert!(matches!(
                session.layouts.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            session.reset_graphics(1024, 768);
            let layout = tokio::time::timeout(Duration::from_secs(1), session.layouts.recv())
                .await
                .expect("next resize is sent before the fallback timeout")
                .unwrap();
            assert_eq!(layout.desktop_size(), (1280, 800));
            session.reset_graphics(1280, 800);
            session.stays_connected().await;
        }),
    )
    .await
    .expect("resize test finishes");
}
