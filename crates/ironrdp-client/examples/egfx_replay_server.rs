//! Replay a validated graphics capture through an ordinary loopback RDP connection.

#![expect(
    unused_crate_dependencies,
    reason = "examples share the client's optional integrations"
)]
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the diagnostic CLI reports connection and replay status"
)]

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use ironrdp_core::{Decode as _, Encode, EncodeResult, ReadCursor, WriteCursor, encode_vec, impl_as_any};
use ironrdp_dvc::{DvcEncode, DvcMessage, DvcMessageBatch, DvcProcessor, DvcServerProcessor, encode_dvc_messages};
use ironrdp_egfx::pdu::{self, GfxPdu, QueueDepth};
use ironrdp_graphics::zgfx::wrap_uncompressed;
use ironrdp_pdu::geometry::ExclusiveRectangle;
use ironrdp_pdu::{PduResult, pdu_other_err};
use ironrdp_server::{
    DesktopSize, DisplayContext, DisplayUpdate, DynamicChannelFactory, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, ServerEvent, ServerResult,
};
use ironrdp_svc::ChannelFlags;
use tokio::net::TcpListener;
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_rustls::{TlsAcceptor, rustls};

#[path = "egfx_replay_server/support/preflight.rs"]
mod preflight;

const IO_TIMEOUT: Duration = Duration::from_secs(15);
const CLIENT_MESSAGE_LIMIT: usize = 1024 * 1024;

struct Wrapped(Vec<u8>);

impl Encode for Wrapped {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.0.len());
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "replayed graphics message"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for Wrapped {}

#[derive(Default)]
struct FrameAcks {
    pending: BTreeSet<u32>,
}

impl FrameAcks {
    fn begin(&mut self, frames: &[u32]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.pending.is_empty(),
            "previous graphics message still awaits acknowledgement"
        );
        anyhow::ensure!(
            frames.len() <= preflight::MAX_IN_FLIGHT_BATCH,
            "too many outstanding graphics frames"
        );
        for frame in frames {
            anyhow::ensure!(self.pending.insert(*frame), "duplicate outstanding frame ID");
        }
        Ok(())
    }

    fn acknowledge(&mut self, frame: u32, depth: QueueDepth) -> anyhow::Result<()> {
        anyhow::ensure!(
            depth != QueueDepth::Suspend,
            "replay requires frame acknowledgements to remain enabled"
        );
        anyhow::ensure!(
            self.pending.remove(&frame),
            "acknowledgement does not identify an outstanding frame"
        );
        Ok(())
    }
}

struct Session {
    live: AtomicBool,
    acks: Mutex<FrameAcks>,
    changed: Notify,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            live: AtomicBool::new(true),
            acks: Mutex::new(FrameAcks::default()),
            changed: Notify::new(),
        }
    }
}

type EventSender = mpsc::UnboundedSender<ServerEvent>;

struct ReplayChannel {
    replay: Arc<preflight::Replay>,
    sender: Arc<Mutex<Option<EventSender>>>,
    failed: mpsc::UnboundedSender<String>,
    session: Arc<Session>,
    task: Option<JoinHandle<()>>,
}

impl ReplayChannel {
    fn stop(&mut self) {
        self.session.live.store(false, Ordering::Release);
        self.session.changed.notify_waiters();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    fn receive(&mut self, channel: u32, pdu: GfxPdu) -> anyhow::Result<Vec<DvcMessage>> {
        match pdu {
            GfxPdu::CapabilitiesAdvertise(caps) => {
                anyhow::ensure!(
                    self.task.is_none(),
                    "graphics capabilities were advertised more than once"
                );
                self.replay.check_capabilities(&caps)?;
                let sender = self
                    .sender
                    .lock()
                    .expect("event sender lock")
                    .clone()
                    .context("server event sender missing")?;
                let replay = Arc::clone(&self.replay);
                let session = Arc::clone(&self.session);
                let failed = self.failed.clone();
                self.task = Some(tokio::task::spawn_local(async move {
                    if let Err(error) = send_capture(&replay, channel, &sender, &session).await {
                        let _ = failed.send(format!("{error:#}"));
                    }
                }));
                Ok(Vec::new())
            }
            GfxPdu::FrameAcknowledge(ack) => {
                self.session
                    .acks
                    .lock()
                    .expect("frame acknowledgement lock")
                    .acknowledge(ack.frame_id, ack.queue_depth)?;
                self.session.changed.notify_one();
                Ok(Vec::new())
            }
            GfxPdu::QoeFrameAcknowledge(_) => Ok(Vec::new()),
            // Replay never imports pixels from a different connection's persistent cache.
            GfxPdu::CacheImportOffer(_) => {
                let reply = GfxPdu::CacheImportReply(pdu::CacheImportReplyPdu {
                    cache_slots: Vec::new(),
                });
                Ok(vec![Box::new(Wrapped(wrap_uncompressed(&encode_vec(&reply)?)))])
            }
            _ => anyhow::bail!("unexpected client graphics message"),
        }
    }
}

impl_as_any!(ReplayChannel);

impl DvcProcessor for ReplayChannel {
    fn channel_name(&self) -> &str {
        ironrdp_egfx::CHANNEL_NAME
    }

    fn max_message_size(&self) -> Option<usize> {
        Some(CLIENT_MESSAGE_LIMIT)
    }

    fn start(&mut self, _channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        self.stop();
        self.session = Arc::new(Session::default());
        Ok(Vec::new())
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        let result = (|| -> anyhow::Result<Vec<DvcMessage>> {
            let mut output = Vec::new();
            let mut offset = 0;
            while offset < payload.len() {
                let tail = &payload[offset..];
                anyhow::ensure!(8 <= tail.len(), "short client graphics header");
                anyhow::ensure!(tail[2..4] == [0, 0], "unsupported client graphics flags");
                let length = usize::try_from(u32::from_le_bytes(tail[4..8].try_into()?))?;
                anyhow::ensure!(8 <= length && length <= tail.len(), "invalid client graphics length");
                let mut cursor = ReadCursor::new(&tail[..length]);
                let pdu = GfxPdu::decode(&mut cursor)?;
                anyhow::ensure!(cursor.is_empty(), "trailing client graphics bytes");
                output.extend(self.receive(channel_id, pdu)?);
                offset += length;
            }
            Ok(output)
        })();
        if let Err(error) = &result {
            // Some channel drivers log processor errors without ending the connection.
            // Wake the outer connection owner so rejected negotiation cannot leave a black window open.
            let _ = self.failed.send(format!("{error:#}"));
        }
        result.map_err(|error| pdu_other_err!("replay client message", source: std::io::Error::other(error)))
    }

    fn close(&mut self, _channel_id: u32) {
        self.stop();
    }
}

impl DvcServerProcessor for ReplayChannel {}

impl Drop for ReplayChannel {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Factory {
    replay: Arc<preflight::Replay>,
    sender: Arc<Mutex<Option<EventSender>>>,
    failed: mpsc::UnboundedSender<String>,
}

impl DynamicChannelFactory for Factory {
    fn attach(&self, channels: ironrdp_dvc::DrdynvcServer) -> ironrdp_dvc::DrdynvcServer {
        channels.with_dynamic_channel(ReplayChannel {
            replay: Arc::clone(&self.replay),
            sender: Arc::clone(&self.sender),
            failed: self.failed.clone(),
            session: Arc::new(Session::default()),
            task: None,
        })
    }
}

async fn send_capture(
    replay: &preflight::Replay,
    channel: u32,
    sender: &EventSender,
    session: &Arc<Session>,
) -> anyhow::Result<()> {
    let origin = replay.steps.first().context("empty replay")?.elapsed;
    let started = tokio::time::Instant::now();
    let mut maximum_lateness = Duration::ZERO;
    let semaphore = Arc::new(Semaphore::new(1));
    println!("REPLAY_STARTED");
    for step in &replay.steps {
        let scheduled = started + step.elapsed.saturating_sub(origin);
        tokio::time::sleep_until(scheduled).await;
        maximum_lateness = maximum_lateness.max(tokio::time::Instant::now().saturating_duration_since(scheduled));
        anyhow::ensure!(session.live.load(Ordering::Acquire), "graphics channel closed");
        let permit = Arc::clone(&semaphore).acquire_owned().await?;
        let messages = encode_dvc_messages(
            channel,
            vec![Box::new(Wrapped(wrap_uncompressed(&step.bytes)))],
            ChannelFlags::empty(),
        )?;
        let batch = DvcMessageBatch::try_new(channel, messages)?;
        // Register before writing, because a fast client can acknowledge before write completion is delivered.
        session
            .acks
            .lock()
            .expect("frame acknowledgement lock")
            .begin(&step.frame_ends)?;
        let (completion, completed) = oneshot::channel();
        let current = Arc::clone(session);
        sender
            .send(ServerEvent::DynamicChannelWithCompletion {
                batch,
                completion,
                permit,
                is_current: Box::new(move || current.live.load(Ordering::Acquire)),
            })
            .map_err(|_| anyhow::anyhow!("server event receiver closed"))?;
        tokio::time::timeout(IO_TIMEOUT, completed)
            .await
            .context("graphics transport write timed out")?
            .context("graphics transport write failed")?;
        tokio::time::timeout(IO_TIMEOUT, async {
            loop {
                let changed = session.changed.notified();
                if session
                    .acks
                    .lock()
                    .expect("frame acknowledgement lock")
                    .pending
                    .is_empty()
                {
                    break;
                }
                anyhow::ensure!(
                    session.live.load(Ordering::Acquire),
                    "graphics channel closed while awaiting acknowledgement"
                );
                changed.await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("graphics frame acknowledgement timed out")??;
    }
    // Keep the connection and its last output alive until the user disconnects.
    println!("REPLAY_COMPLETE max_lateness_us={}", maximum_lateness.as_micros());
    Ok(())
}

struct Display((u16, u16));
struct NoUpdates;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for NoUpdates {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for Display {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.0.0,
            height: self.0.1,
        }
    }
    async fn updates(&mut self, _: DisplayContext) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(NoUpdates))
    }
    async fn offers_display_control(&mut self) -> bool {
        false
    }
}

fn tls_acceptor() -> anyhow::Result<TlsAcceptor> {
    let identity = rcgen::generate_simple_self_signed(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])?;
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(identity.signing_key.serialize_der());
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![identity.cert.der().clone()], key.into())?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

async fn serve(replay: Arc<preflight::Replay>, port: u16) -> anyhow::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let address = listener.local_addr()?;
    let acceptor = tls_acceptor()?;
    println!("REPLAY_CAPTURE_END={:?}", replay.end);
    if matches!(
        replay.end,
        ironrdp_client::graphics_capture::CaptureEnd::Limit | ironrdp_client::graphics_capture::CaptureEnd::Checkpoint
    ) {
        eprintln!("REPLAY_WARNING: usable prefix, not complete session");
    }
    println!(
        "REPLAY_CAPTURE size={}x{} capability={:?} codecs={:?}",
        replay.size.0, replay.size.1, replay.capability, replay.codecs
    );
    println!("REPLAY_LISTEN={address}");
    println!(
        "FreeRDP: /v:{address} /sec:tls /cert:ignore /u:replay /p:replay /gfx:AVC420,AVC444,small-cache:on,progressive:on"
    );
    loop {
        let (stream, peer) = listener.accept().await?;
        anyhow::ensure!(peer.ip().is_loopback(), "replay only accepts loopback clients");
        stream.set_nodelay(true)?;
        let sender = Arc::new(Mutex::new(None));
        let (failed, mut failures) = mpsc::unbounded_channel();
        let mut server = RdpServer::builder()
            .with_addr(address)
            .with_tls(acceptor.clone())
            .with_no_input()
            .with_display_handler(Display(replay.size))
            .with_dynamic_channel_factory(Box::new(Factory {
                replay: Arc::clone(&replay),
                sender: Arc::clone(&sender),
                failed,
            }))
            .build();
        *sender.lock().expect("event sender lock") = Some(server.event_sender().clone());
        // Dropping the connection future after a write/ACK timeout closes this session; partial writes are never retried.
        tokio::select! {
            result = server.run_connection(stream) => {
                if let Err(error) = result {
                    let reason = failures.try_recv().unwrap_or_else(|_| error.to_string());
                    eprintln!("REPLAY_CONNECTION_ENDED: {reason}");
                }
            }
            Some(error) = failures.recv() => { eprintln!("REPLAY_FAILED: {error}"); }
        }
    }
}

fn fixture(path: &Path, units: &[PathBuf]) -> anyhow::Result<()> {
    let avc = !units.is_empty();
    let cap = if avc {
        pdu::CapabilitySet::V8_1 {
            flags: pdu::CapabilitiesV81Flags::SMALL_CACHE | pdu::CapabilitiesV81Flags::AVC420_ENABLED,
        }
    } else {
        pdu::CapabilitySet::V8 {
            flags: pdu::CapabilitiesV8Flags::SMALL_CACHE,
        }
    };
    let rectangle = ExclusiveRectangle {
        left: 0,
        top: 0,
        right: 64,
        bottom: 64,
    };
    let initial = [
        GfxPdu::CapabilitiesConfirm(pdu::CapabilitiesConfirmPdu::from_typed(&cap)),
        GfxPdu::ResetGraphics(pdu::ResetGraphicsPdu {
            width: 64,
            height: 64,
            monitors: vec![ironrdp_pdu::gcc::Monitor {
                left: 0,
                top: 0,
                right: 63,
                bottom: 63,
                flags: ironrdp_pdu::gcc::MonitorFlags::PRIMARY,
            }],
        }),
        GfxPdu::CreateSurface(pdu::CreateSurfacePdu {
            surface_id: 0,
            width: 64,
            height: 64,
            pixel_format: pdu::PixelFormat::XRgb,
        }),
        GfxPdu::MapSurfaceToOutput(pdu::MapSurfaceToOutputPdu {
            surface_id: 0,
            output_origin_x: 0,
            output_origin_y: 0,
        }),
    ];
    let mut messages: Vec<(u64, Vec<GfxPdu>)> = vec![(0, initial.to_vec())];
    let count = if avc { units.len() } else { 2 };
    for index in 0..count {
        let frame_id = u32::try_from(index + 1)?;
        let timestamp = pdu::Timestamp {
            milliseconds: 0,
            seconds: 0,
            minutes: 0,
            hours: 0,
        };
        let data = if avc {
            let data = fs::read(&units[index])?;
            encode_vec(&pdu::Avc420BitmapStream {
                rectangles: vec![rectangle.clone()],
                quant_qual_vals: vec![pdu::QuantQuality {
                    quantization_parameter: 0,
                    progressive: false,
                    quality: 100,
                }],
                data: &data,
            })?
        } else {
            let color = if index == 0 { [0, 0, 255, 255] } else { [0, 255, 0, 255] };
            color.repeat(64 * 64)
        };
        messages.push((
            u64::try_from(index)? * if avc { 33_333 } else { 500_000 },
            vec![
                GfxPdu::StartFrame(pdu::StartFramePdu { frame_id, timestamp }),
                GfxPdu::WireToSurface1(pdu::WireToSurface1Pdu {
                    surface_id: 0,
                    codec_id: if avc {
                        pdu::Codec1Type::Avc420
                    } else {
                        pdu::Codec1Type::Uncompressed
                    },
                    pixel_format: pdu::PixelFormat::XRgb,
                    destination_rectangle: rectangle.clone(),
                    bitmap_data: data,
                }),
                GfxPdu::EndFrame(pdu::EndFramePdu { frame_id }),
            ],
        ));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut output = BufWriter::new(options.open(path)?);
    output.write_all(b"IRGFX001")?;
    for (elapsed, pdus) in messages {
        let mut bytes = Vec::new();
        for pdu in pdus {
            bytes.extend(encode_vec(&pdu)?);
        }
        output.write_all(&u32::try_from(bytes.len())?.to_le_bytes())?;
        output.write_all(&elapsed.to_le_bytes())?;
        output.write_all(&bytes)?;
    }
    output.write_all(&[0; 13])?;
    output.flush()?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--fixture") && args.len() == 2 {
        return fixture(Path::new(&args[1]), &[]);
    }
    if args.first().is_some_and(|arg| arg == "--avc-fixture") && 3 <= args.len() {
        return fixture(
            Path::new(&args[1]),
            &args[2..].iter().map(PathBuf::from).collect::<Vec<_>>(),
        );
    }
    anyhow::ensure!(
        args.len() == 2 || args.len() == 4,
        "usage: egfx_replay_server --capture PATH [--port 3390]; --fixture PATH; --avc-fixture PATH ACCESS_UNIT..."
    );
    anyhow::ensure!(args[0] == "--capture", "expected --capture PATH");
    let port = if args.len() == 4 {
        anyhow::ensure!(args[2] == "--port", "expected --port NUMBER");
        args[3].to_str().context("port is not UTF-8")?.parse::<u16>()?
    } else {
        3390
    };
    let replay = Arc::new(preflight::Replay::load(Path::new(&args[1]))?);
    tokio::task::LocalSet::new().run_until(serve(replay, port)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledgements_cannot_advance_unsent_frames() {
        // oracle: spec MS-RDPEGFX 2.2.2.13: frameId identifies the frame being acknowledged.
        let mut acks = FrameAcks::default();
        acks.begin(&[9, 11]).unwrap();
        assert!(acks.acknowledge(10, QueueDepth::Unavailable).is_err());
        assert_eq!(acks.pending, BTreeSet::from([9, 11]));
        acks.acknowledge(11, QueueDepth::Unavailable).unwrap();
        assert_eq!(acks.pending, BTreeSet::from([9]));
        acks.acknowledge(9, QueueDepth::Unavailable).unwrap();
        assert!(acks.pending.is_empty());
    }

    #[test]
    fn wrapping_preserves_original_graphics_message() {
        // oracle: property round-trip.
        let original: Vec<_> = (0u8..=250).cycle().take(65_536).collect();
        let encoded = encode_vec(&Wrapped(wrap_uncompressed(&original))).unwrap();
        // oracle: spec MS-RDPEGFX 3.1.9.1 limits each segment to 65535 decompressed bytes.
        assert_eq!(encoded[0], 0xe1);
        assert_eq!(u16::from_le_bytes(encoded[1..3].try_into().unwrap()), 2);
        let mut decompressed = Vec::new();
        ironrdp_graphics::zgfx::Decompressor::new()
            .decompress(&encoded, &mut decompressed)
            .unwrap();
        assert_eq!(decompressed, original);
    }
}
