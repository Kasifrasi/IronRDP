#![expect(
    unused_crate_dependencies,
    reason = "the package also builds native channel integrations"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use ironrdp_client::graphics_capture::CaptureReader;
use ironrdp_client::h264::H264DecoderFactory;
use ironrdp_core::{ReadCursor, decode, decode_cursor, encode_vec};
use ironrdp_dvc::DvcProcessor as _;
use ironrdp_egfx::client::{BitmapUpdate, GraphicsPipelineClient, GraphicsPipelineHandler};
use ironrdp_egfx::pdu::{self, Avc420BitmapStream, Codec1Type, GfxPdu};
use ironrdp_graphics::zgfx::wrap_uncompressed;
use ironrdp_pdu::geometry::ExclusiveRectangle;

type Patch = (ExclusiveRectangle, Vec<u8>);
#[derive(Default)]
struct Observed {
    patches: Vec<Patch>,
    message: Vec<u8>,
}
struct Handler(Arc<Mutex<Observed>>);
impl GraphicsPipelineHandler for Handler {
    fn on_message_received(&mut self, message: &[u8]) {
        self.0.lock().expect("observer lock").message = message.to_vec();
    }
    fn on_bitmap_updated(&mut self, update: &BitmapUpdate) {
        self.0
            .lock()
            .expect("observer lock")
            .patches
            .push((update.destination_rectangle.clone(), update.data.clone()));
    }
}

struct Worker {
    child: Child,
    input: Option<BufWriter<ChildStdin>>,
    output: BufReader<ChildStdout>,
}
impl Worker {
    fn start(path: &Path) -> anyhow::Result<Self> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start FreeRDP codec worker")?;
        let input = Some(BufWriter::new(child.stdin.take().context("worker stdin")?));
        let output = BufReader::new(child.stdout.take().context("worker stdout")?);
        Ok(Self { child, input, output })
    }
    fn request(&mut self, op: u32, surface: u16, tail: &[u8], size: (u16, u16)) -> anyhow::Result<(i32, Vec<Patch>)> {
        let input = self.input.as_mut().context("worker input closed")?;
        input.write_all(&op.to_le_bytes())?;
        input.write_all(&u32::from(surface).to_le_bytes())?;
        input.write_all(tail)?;
        input.flush()?;
        let mut header = [0; 8];
        self.output.read_exact(&mut header).context("read FreeRDP status")?;
        let status = i32::from_le_bytes(header[..4].try_into()?);
        let count = u32::from_le_bytes(header[4..].try_into()?);
        anyhow::ensure!(count <= 65535, "too many FreeRDP rectangles");
        let mut patches = Vec::new();
        let mut bytes = 0usize;
        for _ in 0..count {
            let mut rectangle = [0; 8];
            self.output.read_exact(&mut rectangle)?;
            let edges: Vec<_> = rectangle
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            let rect = ExclusiveRectangle {
                left: edges[0],
                top: edges[1],
                right: edges[2],
                bottom: edges[3],
            };
            anyhow::ensure!(
                rect.left < rect.right && rect.top < rect.bottom && rect.right <= size.0 && rect.bottom <= size.1,
                "invalid FreeRDP rectangle"
            );
            let length = usize::from(rect.right - rect.left) * usize::from(rect.bottom - rect.top) * 4;
            bytes = bytes.checked_add(length).context("codec output size overflow")?;
            anyhow::ensure!(bytes <= 256 * 1024 * 1024, "codec output exceeds memory limit");
            let mut data = vec![0; length];
            self.output.read_exact(&mut data)?;
            patches.push((rect, data));
        }
        Ok((status, patches))
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        self.input.take();
        anyhow::ensure!(self.child.wait()?.success(), "FreeRDP codec worker failed");
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Metrics {
    updates: u64,
    pixels: u64,
    different: u64,
    error: u64,
    maximum: u8,
    over16: u64,
    over64: u64,
    iron_only: u64,
    free_only: u64,
    errors: u64,
}
impl Metrics {
    fn add(&mut self, other: &Self) {
        self.updates += other.updates;
        self.pixels += other.pixels;
        self.different += other.different;
        self.error += other.error;
        self.maximum = self.maximum.max(other.maximum);
        self.over16 += other.over16;
        self.over64 += other.over64;
        self.iron_only += other.iron_only;
        self.free_only += other.free_only;
        self.errors += other.errors;
    }
    #[expect(
        clippy::as_conversions,
        reason = "diagnostic mean errors are approximate floating-point ratios of bounded counts"
    )]
    fn mean(&self) -> f64 {
        if self.pixels == 0 {
            0.0
        } else {
            self.error as f64 / (self.pixels as f64 * 3.0)
        }
    }
    fn json(&self) -> String {
        format!(
            "{{\"updates\":{},\"compared_pixels\":{},\"different_pixels\":{},\"mean_absolute_channel_error\":{:.6},\"maximum_channel_error\":{},\"pixels_over_16\":{},\"pixels_over_64\":{},\"iron_only_pixels\":{},\"free_only_pixels\":{},\"decode_errors\":{}}}",
            self.updates,
            self.pixels,
            self.different,
            self.mean(),
            self.maximum,
            self.over16,
            self.over64,
            self.iron_only,
            self.free_only,
            self.errors
        )
    }
}
struct Canvas {
    size: (u16, u16),
    iron: Vec<u8>,
    free: Vec<u8>,
    iron_mask: Vec<bool>,
    free_mask: Vec<bool>,
    avc_gap: bool,
}
impl Canvas {
    fn new(size: (u16, u16)) -> anyhow::Result<Self> {
        let count = usize::from(size.0) * usize::from(size.1);
        anyhow::ensure!(
            size.0 != 0 && size.1 != 0 && count <= 32 * 1024 * 1024,
            "comparison surface exceeds memory limit"
        );
        Ok(Self {
            size,
            iron: vec![0; count * 4],
            free: vec![0; count * 4],
            iron_mask: vec![false; count],
            free_mask: vec![false; count],
            avc_gap: false,
        })
    }
    fn compare(&mut self, iron: &[Patch], free: &[Patch]) -> anyhow::Result<Metrics> {
        self.iron_mask.fill(false);
        self.free_mask.fill(false);
        apply(self.size, &mut self.iron, &mut self.iron_mask, iron)?;
        apply(self.size, &mut self.free, &mut self.free_mask, free)?;
        let mut result = Metrics {
            updates: 1,
            ..Metrics::default()
        };
        for (index, (&a, &b)) in self.iron_mask.iter().zip(&self.free_mask).enumerate() {
            match (a, b) {
                (true, true) => {
                    result.pixels += 1;
                    let mut maximum = 0;
                    for channel in 0..3 {
                        let difference = self.iron[index * 4 + channel].abs_diff(self.free[index * 4 + channel]);
                        result.error += u64::from(difference);
                        maximum = maximum.max(difference);
                    }
                    result.different += u64::from(maximum != 0);
                    result.over16 += u64::from(16 < maximum);
                    result.over64 += u64::from(64 < maximum);
                    result.maximum = result.maximum.max(maximum);
                }
                (true, false) => result.iron_only += 1,
                (false, true) => result.free_only += 1,
                (false, false) => {}
            }
        }
        Ok(result)
    }
    fn dump(&self, directory: &Path, codec: &str, ordinal: u64) -> anyhow::Result<()> {
        for (name, pixels, mask) in [
            ("ironrdp", &self.iron, &self.iron_mask),
            ("freerdp", &self.free, &self.free_mask),
        ] {
            let path = directory.join(format!("{codec}-{ordinal}-{name}.ppm"));
            let mut output = BufWriter::new(File::create(path)?);
            write!(output, "P6\n{} {}\n255\n", self.size.0, self.size.1)?;
            for (pixel, &covered) in pixels.chunks_exact(4).zip(mask) {
                output.write_all(if covered { &pixel[..3] } else { &[0, 0, 0] })?;
            }
            output.flush()?;
        }
        Ok(())
    }
}
fn apply(size: (u16, u16), pixels: &mut [u8], mask: &mut [bool], patches: &[Patch]) -> anyhow::Result<()> {
    for (rect, data) in patches {
        anyhow::ensure!(
            rect.left < rect.right && rect.top < rect.bottom && rect.right <= size.0 && rect.bottom <= size.1,
            "invalid decoded rectangle"
        );
        let width = usize::from(rect.right - rect.left);
        anyhow::ensure!(
            data.len() == width * usize::from(rect.bottom - rect.top) * 4,
            "decoded pixel length mismatch"
        );
        for (row, source) in data.chunks_exact(width * 4).enumerate() {
            let offset = (usize::from(rect.top) + row) * usize::from(size.0) + usize::from(rect.left);
            pixels[offset * 4..(offset + width) * 4].copy_from_slice(source);
            mask[offset..offset + width].fill(true);
        }
    }
    Ok(())
}
fn blob(tail: &mut Vec<u8>, data: &[u8]) -> anyhow::Result<()> {
    tail.extend_from_slice(&u32::try_from(data.len())?.to_le_bytes());
    tail.extend_from_slice(data);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--fixture") {
        return fixture(&args[2..], false);
    }
    if args.get(1).is_some_and(|arg| arg == "--avc-fixture") {
        return fixture(&args[2..], true);
    }
    anyhow::ensure!(
        args.len() == 4,
        "usage: egfx_compare CAPTURE FREERDP_WORKER OUTPUT_DIRECTORY"
    );
    let directory = Path::new(&args[3]);
    fs::create_dir_all(directory)?;
    let mut reader = CaptureReader::open(Path::new(&args[1]))?;
    let mut worker = Worker::start(Path::new(&args[2]))?;
    let observed = Arc::new(Mutex::new(Observed::default()));
    let factory = H264DecoderFactory::platform_default().context("build with the openh264 feature")?;
    let initial = factory.create().context("create H.264 decoder")?;
    let mut pipeline = GraphicsPipelineClient::new(Box::new(Handler(Arc::clone(&observed))), Some(initial))
        .with_h264_decoder_factory(Box::new(move || factory.create()));
    pipeline.start(0)?;
    let mut surfaces: BTreeMap<u16, Canvas> = BTreeMap::new();
    let mut metrics: BTreeMap<&str, Metrics> = BTreeMap::new();
    let mut dumped = BTreeSet::new();
    let mut csv = BufWriter::new(File::create(directory.join("updates.csv"))?);
    writeln!(
        csv,
        "ordinal,elapsed_us,frame,surface,codec,compared_pixels,mean_absolute_channel_error,maximum_channel_error,pixels_over_16,pixels_over_64,iron_only_pixels,free_only_pixels,decode_errors,freerdp_status"
    )?;
    let (mut ordinal, mut frame, mut unsupported, mut messages) = (0u64, 0u32, 0u64, 0u64);
    let mut truncated = false;
    loop {
        let message = match reader.next_message() {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                writeln!(
                    io::stderr(),
                    "Capture ended without a complete footer; comparing its complete message prefix"
                )?;
                truncated = true;
                break;
            }
            Err(error) => return Err(error.into()),
        };
        messages += 1;
        let mut cursor = ReadCursor::new(&message.data);
        while !cursor.is_empty() {
            let remaining = cursor.remaining();
            let pdu: GfxPdu = decode_cursor(&mut cursor)?;
            ordinal += 1;
            let encoded = &remaining[..remaining.len() - cursor.remaining().len()];
            let mut comparison = None;
            match &pdu {
                GfxPdu::CreateSurface(create) => {
                    let size = (create.width, create.height);
                    let resident: usize = surfaces
                        .iter()
                        .filter(|(id, _)| **id != create.surface_id)
                        .map(|(_, s)| s.iron.len() + s.free.len())
                        .sum();
                    anyhow::ensure!(
                        resident + usize::from(size.0) * usize::from(size.1) * 8 <= 256 * 1024 * 1024,
                        "comparison surfaces exceed memory limit"
                    );
                    surfaces.insert(create.surface_id, Canvas::new(size)?);
                    let mut tail = u32::from(size.0).to_le_bytes().to_vec();
                    tail.extend_from_slice(&u32::from(size.1).to_le_bytes());
                    anyhow::ensure!(
                        0 <= worker.request(0, create.surface_id, &tail, size)?.0,
                        "FreeRDP surface creation failed"
                    );
                }
                GfxPdu::DeleteSurface(delete) => {
                    surfaces.remove(&delete.surface_id);
                    worker.request(1, delete.surface_id, &[], (0, 0))?;
                }
                GfxPdu::ResetGraphics(_) => {
                    surfaces.clear();
                    worker.request(2, 0, &[], (0, 0))?;
                }
                GfxPdu::StartFrame(start) => frame = start.frame_id,
                GfxPdu::WireToSurface2(update) => {
                    let surface = surfaces
                        .get(&update.surface_id)
                        .context("Progressive surface missing")?;
                    let mut tail = frame.to_le_bytes().to_vec();
                    blob(&mut tail, &update.bitmap_data)?;
                    comparison = Some((
                        "Progressive",
                        update.surface_id,
                        worker.request(3, update.surface_id, &tail, surface.size)?,
                    ));
                }
                GfxPdu::WireToSurface1(update) if update.codec_id == Codec1Type::Avc420 => {
                    let surface = surfaces.get(&update.surface_id).context("AVC420 surface missing")?;
                    let stream: Avc420BitmapStream<'_> = decode(&update.bitmap_data)?;
                    if stream.rectangles.is_empty() || surface.avc_gap {
                        unsupported += 1;
                    } else {
                        let mut tail = u32::try_from(stream.rectangles.len())?.to_le_bytes().to_vec();
                        for rect in &stream.rectangles {
                            for edge in [rect.left, rect.top, rect.right, rect.bottom] {
                                tail.extend_from_slice(&edge.to_le_bytes());
                            }
                        }
                        blob(&mut tail, stream.data)?;
                        comparison = Some((
                            "AVC420",
                            update.surface_id,
                            worker.request(4, update.surface_id, &tail, surface.size)?,
                        ));
                    }
                }
                GfxPdu::WireToSurface1(update)
                    if matches!(update.codec_id, Codec1Type::Avc444 | Codec1Type::Avc444v2) =>
                {
                    unsupported += 1;
                    if let Some(surface) = surfaces.get_mut(&update.surface_id) {
                        surface.avc_gap = true;
                    }
                }
                _ => {}
            }
            observed.lock().expect("observer lock").patches.clear();
            let dropped_before = pipeline.dropped_updates();
            pipeline
                .process(0, &wrap_uncompressed(encoded))
                .with_context(|| format!("IronRDP PDU {ordinal}"))?;
            let _ = pipeline.drain_output();
            let _ = pipeline.drain_window_updates();
            anyhow::ensure!(
                observed.lock().expect("observer lock").message == encoded,
                "recording observer changed message bytes"
            );
            if let Some((codec, id, (status, free))) = comparison {
                let observed = observed.lock().expect("observer lock");
                let surface = surfaces.get_mut(&id).context("comparison surface missing")?;
                let mut result = surface.compare(&observed.patches, &free)?;
                result.errors = u64::from(status < 0 || dropped_before != pipeline.dropped_updates());
                if result.errors != 0 {
                    writeln!(
                        io::stderr(),
                        "Decoder failure: PDU {ordinal}, surface {id}, codec {codec}, FreeRDP status {status}, IronRDP dropped delta {}",
                        pipeline.dropped_updates() - dropped_before
                    )?;
                }
                if (32 < result.maximum || result.iron_only != 0 || result.free_only != 0) && dumped.insert(codec) {
                    surface.dump(directory, codec, ordinal)?;
                }
                writeln!(
                    csv,
                    "{ordinal},{},{frame},{id},{codec},{},{:.6},{},{},{},{},{},{},{status}",
                    message.elapsed.as_micros(),
                    result.pixels,
                    result.mean(),
                    result.maximum,
                    result.over16,
                    result.over64,
                    result.iron_only,
                    result.free_only,
                    result.errors
                )?;
                metrics.entry(codec).or_default().add(&result);
            }
        }
    }
    csv.flush()?;
    worker.finish()?;
    let capture_end = match reader.end() {
        Some(ironrdp_client::graphics_capture::CaptureEnd::Closed) => "closed",
        Some(ironrdp_client::graphics_capture::CaptureEnd::Limit) => "limit",
        Some(ironrdp_client::graphics_capture::CaptureEnd::Checkpoint) => "checkpoint",
        None => "truncated",
    };
    let decoder_errors = metrics.values().any(|m| m.errors != 0) || pipeline.dropped_updates() != 0;
    let compared_anything = metrics
        .values()
        .any(|m| m.pixels != 0 || m.iron_only != 0 || m.free_only != 0);
    let comparison_valid = !truncated && unsupported == 0 && !decoder_errors && compared_anything;
    let mut report = format!(
        "{{\n  \"messages\":{messages},\n  \"pdus\":{ordinal},\n  \"scope\":\"Progressive and AVC420 codec output before session compositing\",\n  \"comparison_valid\":{comparison_valid},\n  \"capture_end\":\"{capture_end}\",\n  \"truncated\":{truncated},\n  \"unsupported_avc_updates\":{unsupported},\n  \"ironrdp_dropped_updates\":{},\n  \"codecs\":{{",
        pipeline.dropped_updates()
    );
    for (index, (codec, result)) in metrics.iter().enumerate() {
        if index != 0 {
            report.push(',');
        }
        report.push_str(&format!("\n    \"{codec}\":{}", result.json()));
    }
    report.push_str("\n  }\n}\n");
    fs::write(directory.join("summary.json"), &report)?;
    io::stdout().write_all(report.as_bytes())?;
    Ok(())
}

fn fixture(args: &[std::ffi::OsString], avc: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.len() >= 2,
        "usage: egfx_compare --fixture CAPTURE PROGRESSIVE_STREAM..."
    );
    let mut output = BufWriter::new(File::create(&args[0])?);
    output.write_all(b"IRGFX001")?;
    let (width, height) = if avc { (64, 64) } else { (1280, 800) };
    let mut pdus = vec![
        GfxPdu::CapabilitiesConfirm(pdu::CapabilitiesConfirmPdu::from_typed(&pdu::CapabilitySet::V10_7 {
            flags: pdu::CapabilitiesV107Flags::SMALL_CACHE,
        })),
        GfxPdu::ResetGraphics(pdu::ResetGraphicsPdu {
            width: u32::from(width),
            height: u32::from(height),
            monitors: vec![],
        }),
        GfxPdu::CreateSurface(pdu::CreateSurfacePdu {
            surface_id: 0,
            width,
            height,
            pixel_format: pdu::PixelFormat::XRgb,
        }),
    ];
    for (index, path) in args[1..].iter().enumerate() {
        let id = u32::try_from(index + 1)?;
        pdus.push(GfxPdu::StartFrame(pdu::StartFramePdu {
            frame_id: id,
            timestamp: pdu::Timestamp {
                milliseconds: 0,
                seconds: 0,
                minutes: 0,
                hours: 0,
            },
        }));
        let data = fs::read(path)?;
        if avc {
            let stream = Avc420BitmapStream {
                rectangles: vec![ExclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                }],
                quant_qual_vals: vec![pdu::QuantQuality {
                    quantization_parameter: 0,
                    progressive: false,
                    quality: 100,
                }],
                data: &data,
            };
            pdus.push(GfxPdu::WireToSurface1(pdu::WireToSurface1Pdu {
                surface_id: 0,
                codec_id: Codec1Type::Avc420,
                pixel_format: pdu::PixelFormat::XRgb,
                destination_rectangle: ExclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                },
                bitmap_data: encode_vec(&stream)?,
            }));
        } else {
            pdus.push(GfxPdu::WireToSurface2(pdu::WireToSurface2Pdu {
                surface_id: 0,
                codec_id: pdu::Codec2Type::RemoteFxProgressive,
                codec_context_id: 3,
                pixel_format: pdu::PixelFormat::XRgb,
                bitmap_data: data,
            }));
        }
        pdus.push(GfxPdu::EndFrame(pdu::EndFramePdu { frame_id: id }));
    }
    for pdu in pdus {
        let data = encode_vec(&pdu)?;
        output.write_all(&u32::try_from(data.len())?.to_le_bytes())?;
        output.write_all(&0u64.to_le_bytes())?;
        output.write_all(&data)?;
    }
    output.write_all(&[0; 13])?;
    output.flush()?;
    Ok(())
}
