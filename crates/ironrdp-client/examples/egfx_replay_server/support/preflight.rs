//! Validate a connection-start graphics recording before opening a replay listener.
//!
//! Codec payloads remain opaque and byte-identical; this checks framing and graphics command state.
//! It does not establish that compressed bitmap payloads are decodable or contain every reference
//! picture or progressive tile they require: the receiving FreeRDP decoder performs that check.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use ironrdp_client::graphics_capture::{CaptureEnd, CaptureReader};
use ironrdp_core::{Decode as _, ReadCursor};
use ironrdp_egfx::pdu::{CapabilitiesAdvertisePdu, CapabilityVersion, Codec1Type, GfxPdu, RawCapabilitySet};
use ironrdp_pdu::geometry::ExclusiveRectangle;

/// Maximum acknowledgements that one indivisible captured message may require.
pub(crate) const MAX_IN_FLIGHT_BATCH: usize = 32;
const MAX_GRAPHICS_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GRAPHICS_DIMENSION: u32 = 32766;

pub(crate) struct Step {
    pub(crate) elapsed: Duration,
    /// The entire original post-ZGFX message, without re-encoding any PDU.
    pub(crate) bytes: Vec<u8>,
    pub(crate) frame_ends: Vec<u32>,
}

pub(crate) struct Replay {
    pub(crate) size: (u16, u16),
    pub(crate) capability: RawCapabilitySet,
    pub(crate) steps: Vec<Step>,
    pub(crate) codecs: Vec<&'static str>,
    /// Limit and checkpoint endings describe usable prefixes, not a complete session.
    pub(crate) end: CaptureEnd,
}

impl Replay {
    pub(crate) fn load(path: &Path) -> anyhow::Result<Self> {
        let mut reader = CaptureReader::open(path).context("cannot open graphics recording")?;
        let mut state = State::default();
        let mut steps: Vec<Step> = Vec::new();
        while let Some(message) = reader
            .next_message()
            .context("incomplete or invalid graphics recording")?
        {
            ensure!(
                message.elapsed <= Duration::from_secs(60),
                "recording timestamp exceeds the 60-second capture limit"
            );
            if let Some(previous) = steps.last() {
                ensure!(
                    previous.elapsed <= message.elapsed,
                    "recording timestamps move backwards"
                );
            }
            let mut frame_ends = Vec::new();
            state
                .message(&message.data, &mut frame_ends)
                .with_context(|| format!("invalid graphics message {}", steps.len() + 1))?;
            steps.push(Step {
                elapsed: message.elapsed,
                bytes: message.data,
                frame_ends,
            });
        }
        ensure!(state.frame.is_none(), "recording ends inside a graphics frame");
        ensure!(
            state.visible_frame,
            "recording contains no complete frame with an output-mapped surface"
        );
        Ok(Self {
            size: state
                .first_size
                .context("recording has no ResetGraphics initialization")?,
            capability: state
                .capability
                .context("recording has no CapabilitiesConfirm initialization")?,
            steps,
            codecs: state.codecs.into_iter().collect(),
            end: reader.end().context("recording has no completion marker")?,
        })
    }

    pub(crate) fn check_capabilities(&self, advertised: &CapabilitiesAdvertisePdu) -> anyhow::Result<()> {
        ensure!(
            advertised.0.iter().any(|cap| cap == &self.capability),
            "FreeRDP did not advertise the recording's exact capability set ({}); advertised: {}; use a FreeRDP build and /gfx mode that offer the recorded version and flags",
            describe_capability(&self.capability),
            advertised
                .0
                .iter()
                .map(describe_capability)
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Surface {
    size: (u16, u16),
    mapped: bool,
}

#[derive(Default)]
struct State {
    capability: Option<RawCapabilitySet>,
    first_size: Option<(u16, u16)>,
    output: Option<(u16, u16)>,
    surfaces: BTreeMap<u16, Surface>,
    cache: BTreeMap<u16, (u16, u16)>,
    contexts: BTreeSet<(u16, u32)>,
    frames: BTreeSet<u32>,
    frame: Option<u32>,
    visible_frame: bool,
    codecs: BTreeSet<&'static str>,
}

impl State {
    fn message(&mut self, mut bytes: &[u8], frame_ends: &mut Vec<u32>) -> anyhow::Result<()> {
        ensure!(!bytes.is_empty(), "empty graphics message");
        while !bytes.is_empty() {
            ensure!(8 <= bytes.len(), "truncated graphics PDU header");
            ensure!(bytes[2..4] == [0, 0], "nonzero graphics PDU flags are unsupported");
            let length = usize::try_from(u32::from_le_bytes(bytes[4..8].try_into()?))?;
            ensure!(
                8 <= length && length <= bytes.len(),
                "graphics PDU length exceeds its message boundary"
            );
            // GfxPdu::decode does not itself constrain reads to the header's pduLength.
            let mut cursor = ReadCursor::new(&bytes[..length]);
            let pdu = GfxPdu::decode(&mut cursor).context("cannot decode graphics PDU")?;
            ensure!(
                cursor.is_empty(),
                "graphics PDU length does not match its decoded fields"
            );
            self.pdu(pdu, frame_ends)?;
            bytes = &bytes[length..];
        }
        Ok(())
    }

    fn pdu(&mut self, pdu: GfxPdu, frame_ends: &mut Vec<u32>) -> anyhow::Result<()> {
        if let GfxPdu::CapabilitiesConfirm(confirm) = pdu {
            ensure!(
                self.capability.is_none(),
                "capability renegotiation requires unrecorded client state and is unsupported"
            );
            validate_capability(&confirm.0)?;
            self.capability = Some(confirm.0);
            return Ok(());
        }
        let cap = self
            .capability
            .as_ref()
            .context("recording must begin with CapabilitiesConfirm")?;
        let flags = capability_flags(cap);
        let legacy = matches!(cap.version, CapabilityVersion::V8 | CapabilityVersion::V8_1);
        let small_cache = cap.version == CapabilityVersion::V10_3 || (legacy && flags & 1 != 0) || flags & 2 != 0;
        match pdu {
            GfxPdu::ResetGraphics(reset) => {
                ensure!(self.frame.is_none(), "ResetGraphics inside a frame is unsupported");
                ensure!(0 < reset.width && 0 < reset.height, "empty graphics output");
                ensure!(
                    !reset.monitors.is_empty(),
                    "ResetGraphics requires at least one monitor for FreeRDP replay"
                );
                let size = (u16::try_from(reset.width)?, u16::try_from(reset.height)?);
                let mut primary_count = 0;
                for monitor in reset.monitors {
                    ensure!(
                        monitor.left <= monitor.right && monitor.top <= monitor.bottom,
                        "invalid graphics monitor rectangle"
                    );
                    if monitor.flags.bits() & 1 != 0 {
                        primary_count += 1;
                        ensure!(
                            monitor.left == 0 && monitor.top == 0,
                            "primary graphics monitor must start at (0,0)"
                        );
                    }
                }
                ensure!(
                    primary_count == 1,
                    "graphics monitor layout requires exactly one PRIMARY monitor"
                );
                self.first_size.get_or_insert(size);
                self.output = Some(size);
                // ResetGraphics does not delete surface IDs, cache slots, or their mappings.
            }
            GfxPdu::CacheImportReply(reply) => {
                ensure!(
                    reply.cache_slots.is_empty(),
                    "persistent cache imports need the original client's bitmap data and are unsupported"
                );
            }
            GfxPdu::CreateSurface(surface) => {
                ensure!(
                    self.output.is_some(),
                    "CreateSurface precedes ResetGraphics initialization"
                );
                ensure!(0 < surface.width && 0 < surface.height, "empty graphics surface");
                ensure!(
                    surface.width <= 32766 && surface.height <= 32766,
                    "graphics surface dimensions exceed FreeRDP's 32766-pixel limit"
                );
                ensure!(
                    !self.surfaces.contains_key(&surface.surface_id),
                    "surface {} is created twice without deletion",
                    surface.surface_id
                );
                self.surfaces.insert(
                    surface.surface_id,
                    Surface {
                        size: (surface.width, surface.height),
                        mapped: false,
                    },
                );
            }
            GfxPdu::DeleteSurface(surface) => {
                ensure!(
                    self.surfaces.remove(&surface.surface_id).is_some(),
                    "deletion references an unknown surface {}",
                    surface.surface_id
                );
                self.contexts
                    .retain(|(surface_id, _)| *surface_id != surface.surface_id);
            }
            GfxPdu::MapSurfaceToOutput(mapping) => {
                let size = self.surface(mapping.surface_id)?.size;
                self.mapping(
                    mapping.output_origin_x,
                    mapping.output_origin_y,
                    u32::from(size.0),
                    u32::from(size.1),
                )?;
                self.surfaces
                    .get_mut(&mapping.surface_id)
                    .context("unknown mapped surface")?
                    .mapped = true;
            }
            GfxPdu::MapSurfaceToScaledOutput(mapping) => {
                ensure!(
                    matches!(
                        cap.version,
                        CapabilityVersion::V10_5 | CapabilityVersion::V10_6 | CapabilityVersion::V10_6_ERR
                    ) || (cap.version == CapabilityVersion::V10_7 && flags & 0x80 == 0),
                    "recorded capability set does not support scaled output mappings"
                );
                self.surface(mapping.surface_id)?;
                self.mapping(
                    mapping.output_origin_x,
                    mapping.output_origin_y,
                    mapping.target_width,
                    mapping.target_height,
                )?;
                self.surfaces
                    .get_mut(&mapping.surface_id)
                    .context("unknown mapped surface")?
                    .mapped = true;
            }
            GfxPdu::StartFrame(start) => {
                ensure!(
                    self.output.is_some(),
                    "StartFrame precedes ResetGraphics initialization"
                );
                ensure!(self.frame.is_none(), "nested graphics frames are unsupported");
                ensure!(
                    self.frames.insert(start.frame_id),
                    "repeated frame ID {} cannot be acknowledged unambiguously",
                    start.frame_id
                );
                self.frame = Some(start.frame_id);
            }
            GfxPdu::EndFrame(end) => {
                ensure!(
                    self.frame == Some(end.frame_id),
                    "EndFrame does not match the active StartFrame"
                );
                ensure!(
                    frame_ends.len() < MAX_IN_FLIGHT_BATCH,
                    "one captured message exceeds the supported batch of {MAX_IN_FLIGHT_BATCH} frame acknowledgements"
                );
                self.frame = None;
                frame_ends.push(end.frame_id);
                self.visible_frame |= self.surfaces.values().any(|surface| surface.mapped);
            }
            GfxPdu::WireToSurface1(wire) => {
                self.drawing()?;
                rectangle(self.surface(wire.surface_id)?.size, &wire.destination_rectangle)?;
                let avc_enabled = !legacy && flags & 0x20 == 0;
                self.codecs.insert(match wire.codec_id {
                    Codec1Type::Uncompressed => "Uncompressed",
                    Codec1Type::RemoteFx => "RemoteFX",
                    Codec1Type::ClearCodec => "ClearCodec",
                    Codec1Type::Planar => "Planar",
                    Codec1Type::Avc420 => "AVC420",
                    Codec1Type::Alpha => "Alpha",
                    Codec1Type::Avc444 => "AVC444",
                    Codec1Type::Avc444v2 => "AVC444v2",
                });
                match wire.codec_id {
                    Codec1Type::Avc420 => ensure!(
                        avc_enabled || (cap.version == CapabilityVersion::V8_1 && flags & 0x10 != 0),
                        "recorded capability set disables AVC420"
                    ),
                    Codec1Type::Avc444 | Codec1Type::Avc444v2 => {
                        ensure!(avc_enabled, "recorded capability set disables AVC444")
                    }
                    Codec1Type::Uncompressed => {
                        let size = rectangle_size(&wire.destination_rectangle);
                        ensure!(
                            u64::try_from(wire.bitmap_data.len())? == pixel_bytes(size),
                            "uncompressed bitmap length does not match its rectangle"
                        );
                    }
                    _ => {}
                }
                ensure!(!wire.bitmap_data.is_empty(), "empty encoded bitmap payload");
            }
            GfxPdu::WireToSurface2(wire) => {
                self.drawing()?;
                self.surface(wire.surface_id)?;
                ensure!(
                    !(legacy && flags & 1 != 0),
                    "THINCLIENT forbids the RemoteFX Progressive codec"
                );
                ensure!(!wire.bitmap_data.is_empty(), "empty Progressive bitmap payload");
                self.contexts.insert((wire.surface_id, wire.codec_context_id));
                self.codecs.insert("Progressive");
            }
            GfxPdu::DeleteEncodingContext(context) => {
                self.surface(context.surface_id)?;
                ensure!(
                    self.contexts.remove(&(context.surface_id, context.codec_context_id)),
                    "deletion references an unknown encoding context"
                );
            }
            GfxPdu::SolidFill(fill) => {
                self.drawing()?;
                let size = self.surface(fill.surface_id)?.size;
                for rect in fill.rectangles {
                    rectangle(size, &rect)?;
                }
            }
            GfxPdu::SurfaceToSurface(copy) => {
                self.drawing()?;
                rectangle(self.surface(copy.source_surface_id)?.size, &copy.source_rectangle)?;
                let target = self.surface(copy.destination_surface_id)?.size;
                let size = rectangle_size(&copy.source_rectangle);
                for point in copy.destination_points {
                    destination(target, point.x, point.y, size)?;
                }
            }
            GfxPdu::SurfaceToCache(copy) => {
                self.drawing()?;
                rectangle(self.surface(copy.surface_id)?.size, &copy.source_rectangle)?;
                let capacity = if small_cache { 4096 } else { 25600 };
                ensure!(
                    1 <= copy.cache_slot && copy.cache_slot <= capacity,
                    "cache slot {} is outside the negotiated range 1..={capacity}",
                    copy.cache_slot
                );
                self.cache
                    .insert(copy.cache_slot, rectangle_size(&copy.source_rectangle));
            }
            GfxPdu::CacheToSurface(copy) => {
                self.drawing()?;
                let size = *self
                    .cache
                    .get(&copy.cache_slot)
                    .context("bitmap copy references an uninitialized cache slot")?;
                let target = self.surface(copy.surface_id)?.size;
                for point in copy.destination_points {
                    destination(target, point.x, point.y, size)?;
                }
            }
            GfxPdu::EvictCacheEntry(evict) => {
                ensure!(
                    self.cache.remove(&evict.cache_slot).is_some(),
                    "eviction references an uninitialized cache slot"
                );
            }
            GfxPdu::MapSurfaceToWindow(_) | GfxPdu::MapSurfaceToScaledWindow(_) => {
                bail!("RemoteApp window mappings require unrecorded RAIL state and are unsupported")
            }
            GfxPdu::CapabilitiesAdvertise(_)
            | GfxPdu::CacheImportOffer(_)
            | GfxPdu::FrameAcknowledge(_)
            | GfxPdu::QoeFrameAcknowledge(_) => bail!("client-to-server graphics PDU in a server recording"),
            GfxPdu::CapabilitiesConfirm(_) => unreachable!("handled above"),
            _ => bail!("unsupported graphics command in recording"),
        }
        let cache_bytes: u64 = self.cache.values().copied().map(pixel_bytes).sum();
        ensure!(
            cache_bytes
                <= if small_cache {
                    16 * 1024 * 1024
                } else {
                    100 * 1024 * 1024
                },
            "bitmap cache exceeds the recorded capability limit"
        );
        let surface_bytes: u64 = self.surfaces.values().map(|surface| pixel_bytes(surface.size)).sum();
        ensure!(
            surface_bytes + cache_bytes + self.output.map_or(0, pixel_bytes) <= MAX_GRAPHICS_BYTES,
            "graphics state exceeds the replay limit of 256 MiB"
        );
        Ok(())
    }

    fn surface(&self, id: u16) -> anyhow::Result<&Surface> {
        self.surfaces
            .get(&id)
            .with_context(|| format!("graphics command references an unknown surface {id}"))
    }

    fn drawing(&self) -> anyhow::Result<()> {
        ensure!(
            self.frame.is_some(),
            "drawing outside a StartFrame/EndFrame pair is unsupported"
        );
        Ok(())
    }

    fn mapping(&self, x: u32, y: u32, width: u32, height: u32) -> anyhow::Result<()> {
        ensure!(self.output.is_some(), "output mapping precedes ResetGraphics");
        ensure!(0 < width && 0 < height, "empty output mapping");
        // FreeRDP clips partially visible surfaces in gdi_OutputUpdate.
        // Bound individual fields as its GFX receiver does, without adding them in u32.
        ensure!(
            x <= MAX_GRAPHICS_DIMENSION
                && y <= MAX_GRAPHICS_DIMENSION
                && width <= MAX_GRAPHICS_DIMENSION
                && height <= MAX_GRAPHICS_DIMENSION,
            "output mapping exceeds FreeRDP's 32766-pixel coordinate or size limit"
        );
        Ok(())
    }
}

fn pixel_bytes(size: (u16, u16)) -> u64 {
    u64::from(size.0) * u64::from(size.1) * 4
}

fn rectangle_size(rect: &ExclusiveRectangle) -> (u16, u16) {
    (rect.right - rect.left, rect.bottom - rect.top)
}

fn rectangle(size: (u16, u16), rect: &ExclusiveRectangle) -> anyhow::Result<()> {
    ensure!(
        rect.left < rect.right && rect.top < rect.bottom && rect.right <= size.0 && rect.bottom <= size.1,
        "bitmap rectangle is empty, inverted, or exceeds its surface"
    );
    Ok(())
}

fn destination(target: (u16, u16), x: u16, y: u16, size: (u16, u16)) -> anyhow::Result<()> {
    ensure!(
        u32::from(x) + u32::from(size.0) <= u32::from(target.0)
            && u32::from(y) + u32::from(size.1) <= u32::from(target.1),
        "bitmap destination exceeds its surface"
    );
    Ok(())
}

fn capability_flags(cap: &RawCapabilitySet) -> u32 {
    cap.data
        .get(..4)
        .and_then(|data| data.try_into().ok())
        .map_or(0, u32::from_le_bytes)
}

fn describe_capability(cap: &RawCapabilitySet) -> String {
    let flags = capability_flags(cap);
    format!(
        "version {:#010x}, flags {flags:#04x} (AVC420_ENABLED=0x10, AVC_DISABLED=0x20, SCALEDMAP_DISABLE=0x80)",
        cap.version.0
    )
}

fn validate_capability(cap: &RawCapabilitySet) -> anyhow::Result<()> {
    let mask = match cap.version {
        CapabilityVersion::V8 => 0x03,
        CapabilityVersion::V8_1 => 0x13,
        CapabilityVersion::V10 | CapabilityVersion::V10_2 => 0x22,
        CapabilityVersion::V10_1 => {
            ensure!(cap.data == [0; 16], "invalid reserved version 10.1 capability body");
            return Ok(());
        }
        CapabilityVersion::V10_3 => 0x60,
        CapabilityVersion::V10_4
        | CapabilityVersion::V10_5
        | CapabilityVersion::V10_6
        | CapabilityVersion::V10_6_ERR => 0x62,
        CapabilityVersion::V10_7 => 0xE2,
        _ => bail!("unknown recorded graphics capability version {:#010x}", cap.version.0),
    };
    ensure!(cap.data.len() == 4, "recorded capability body has the wrong length");
    let flags = capability_flags(cap);
    ensure!(
        flags & !mask == 0,
        "unknown flags in recorded capability set: {}",
        describe_capability(cap)
    );
    ensure!(
        flags & 0x60 != 0x60,
        "AVC_THINCLIENT and AVC_DISABLED cannot both be set"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    struct Recording(std::path::PathBuf);

    impl Recording {
        fn new(messages: &[(u64, &[u8])], end: CaptureEnd) -> anyhow::Result<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ironrdp-preflight-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut bytes = b"IRGFX001".to_vec();
            for (elapsed, message) in messages {
                bytes.extend_from_slice(&u32::try_from(message.len())?.to_le_bytes());
                bytes.extend_from_slice(&elapsed.to_le_bytes());
                bytes.extend_from_slice(message);
            }
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&messages.last().map_or(0, |(time, _)| *time).to_le_bytes());
            bytes.push(match end {
                CaptureEnd::Closed => 0,
                CaptureEnd::Limit => 1,
                CaptureEnd::Checkpoint => 2,
            });
            std::fs::write(&path, bytes)?;
            Ok(Self(path))
        }
    }

    impl Drop for Recording {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn pdu(command: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = command.to_le_bytes().to_vec();
        bytes.extend_from_slice(&[0; 2]);
        bytes.extend_from_slice(&u32::try_from(8 + payload.len()).expect("small test PDU").to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn initialization() -> Vec<u8> {
        // MS-RDPEGFX 2.2.2.19: version 8, four flag bytes, SMALL_CACHE.
        let mut bytes = pdu(0x13, &[4, 0, 8, 0, 4, 0, 0, 0, 2, 0, 0, 0]);
        // MS-RDPEGFX 2.2.2.14: 64x64 output, one primary monitor, ignored nonzero padding.
        let mut reset = vec![0xA5; 332];
        reset[..12].copy_from_slice(&[64, 0, 0, 0, 64, 0, 0, 0, 1, 0, 0, 0]);
        reset[12..32].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 63, 0, 0, 0, 63, 0, 0, 0, 1, 0, 0, 0]);
        bytes.extend(pdu(0x0E, &reset));
        // MS-RDPEGFX 2.2.2.9 and 2.2.2.15: XRGB surface 0, mapped at (0,0).
        bytes.extend(pdu(9, &[0, 0, 64, 0, 64, 0, 0x20]));
        bytes.extend(pdu(0x0F, &[0; 12]));
        bytes
    }

    fn frame(id: u32) -> Vec<u8> {
        let mut start = vec![0; 4];
        start.extend_from_slice(&id.to_le_bytes());
        let mut bytes = pdu(0x0B, &start);
        // MS-RDPEGFX 2.2.2.4: one opaque red 64x64 fill on surface 0.
        bytes.extend(pdu(4, &[0, 0, 0, 0, 255, 255, 1, 0, 0, 0, 0, 0, 64, 0, 64, 0]));
        bytes.extend(pdu(0x0C, &id.to_le_bytes()));
        bytes
    }

    #[test]
    fn retains_whole_messages_padding_timing_and_all_frame_ends() -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §2.2.2.14 permits arbitrary ignored pad bytes.
        // oracle: reference hand-built command bytes above, retained exactly as selected by the caller.
        let first = initialization();
        let mut second = frame(7);
        second.extend(frame(8));
        let recording = Recording::new(&[(11, &first), (42, &second)], CaptureEnd::Closed)?;
        let replay = Replay::load(&recording.0)?;
        assert_eq!(replay.size, (64, 64));
        assert_eq!(replay.steps.len(), 2);
        assert_eq!(replay.steps[0].bytes, first);
        assert_eq!(replay.steps[1].bytes, second);
        assert_eq!(replay.steps[0].elapsed, Duration::from_micros(11));
        assert_eq!(replay.steps[1].elapsed, Duration::from_micros(42));
        assert_eq!(replay.steps[1].frame_ends, [7, 8]);
        Ok(())
    }

    #[test]
    fn distinguishes_usable_checkpoint_and_limit_prefixes() -> anyhow::Result<()> {
        // oracle: reference CaptureEnd::Limit and CaptureEnd::Checkpoint identify complete message prefixes.
        let mut bytes = initialization();
        bytes.extend(frame(1));
        let checkpoint = Recording::new(&[(0, &bytes)], CaptureEnd::Checkpoint)?;
        assert_eq!(Replay::load(&checkpoint.0)?.end, CaptureEnd::Checkpoint);
        let limited = Recording::new(&[(0, &bytes)], CaptureEnd::Limit)?;
        assert_eq!(Replay::load(&limited.0)?.end, CaptureEnd::Limit);
        Ok(())
    }

    #[test]
    fn requires_the_recorded_flags_as_well_as_the_capability_version() -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §3.2.5.19 selects a capability set present in CapsAdvertise.
        let mut bytes = initialization();
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        let replay = Replay::load(&recording.0)?;
        replay.check_capabilities(&CapabilitiesAdvertisePdu(vec![replay.capability.clone()]))?;
        let mut different = replay.capability.clone();
        different.data[0] = 0;
        let error = replay
            .check_capabilities(&CapabilitiesAdvertisePdu(vec![different]))
            .expect_err("flags must be offered unchanged");
        assert!(error.to_string().contains("exact capability set"));
        Ok(())
    }

    #[test]
    fn rejects_repeated_frame_ids_that_would_make_acknowledgements_ambiguous() -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §2.2.2.11 defines frameId as a unique frame identifier.
        let mut bytes = initialization();
        bytes.extend(frame(7));
        bytes.extend(frame(7));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        let error = Replay::load(&recording.0).err().expect("duplicate frame ID");
        assert!(format!("{error:#}").contains("repeated frame ID 7"));
        Ok(())
    }

    #[test]
    fn rejects_an_indivisible_message_with_too_many_acknowledgements() -> anyhow::Result<()> {
        // oracle: mutant exceeding the replay sender's explicit 32-frame acknowledgement batch.
        let mut bytes = initialization();
        for id in 0..33 {
            bytes.extend(frame(id));
        }
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        let error = Replay::load(&recording.0).err().expect("oversized frame batch");
        assert!(format!("{error:#}").contains("batch of 32 frame acknowledgements"));
        Ok(())
    }

    #[test]
    fn rejects_backwards_recording_timestamps() -> anyhow::Result<()> {
        // oracle: property monotonic.
        let first = initialization();
        let second = frame(1);
        let recording = Recording::new(&[(2, &first), (1, &second)], CaptureEnd::Closed)?;
        let error = Replay::load(&recording.0).err().expect("backwards elapsed time");
        assert!(format!("{error:#}").contains("timestamps move backwards"));
        Ok(())
    }

    #[test]
    fn rejects_a_length_that_overlaps_the_next_pdu() -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §2.2.1.5 pduLength includes exactly one PDU and its header.
        let mut bytes = initialization();
        bytes[4..8].copy_from_slice(&21u32.to_le_bytes());
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        assert!(Replay::load(&recording.0).is_err());
        Ok(())
    }

    #[test]
    fn rejects_a_missing_capture_footer() -> anyhow::Result<()> {
        // oracle: spec Rust std::io::Read::read_exact reports UnexpectedEof on incomplete input.
        let mut bytes = initialization();
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        let file = std::fs::OpenOptions::new().write(true).open(&recording.0)?;
        file.set_len(file.metadata()?.len() - 13)?;
        assert!(Replay::load(&recording.0).is_err());
        Ok(())
    }

    #[test]
    fn rejects_a_prefix_ending_inside_a_frame() -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §2.2.2.11 and §2.2.2.12 bracket a complete graphics frame.
        let mut bytes = initialization();
        bytes.extend(pdu(0x0B, &[0, 0, 0, 0, 1, 0, 0, 0]));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Checkpoint)?;
        assert!(Replay::load(&recording.0).is_err());
        Ok(())
    }

    #[test]
    fn rejects_persistent_cache_imports_without_original_client_bitmaps() -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §2.2.2.17 imports entries from the original client's cache offer.
        let mut bytes = initialization();
        bytes.extend(pdu(0x11, &[1, 0, 3, 0]));
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        assert!(format!("{:#}", Replay::load(&recording.0).err().expect("missing cache data")).contains("persistent"));
        Ok(())
    }

    #[test]
    fn rejects_empty_monitor_layouts_before_freerdp_closes_the_channel() -> anyhow::Result<()> {
        // oracle: reference FreeRDP/FreeRDP@8a312188d093:channels/rdpgfx/client/rdpgfx_main.c rdpgfx_recv_reset_graphics_pdu rejects monitorCount=0.
        let mut bytes = initialization();
        let count_offset = 20 /* CapsConfirm */ + 8 /* ResetGraphics header */ + 8 /* Width, Height */;
        bytes[count_offset..count_offset + 4].fill(0);
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        let error = Replay::load(&recording.0).err().expect("missing monitor layout");
        assert!(format!("{error:#}").contains("at least one monitor"));
        Ok(())
    }

    #[rstest::rstest]
    #[case::missing_primary(0, 63, 0)]
    #[case::misplaced_primary(1, 63, 1)]
    #[case::inverted_rectangle(64, 63, 1)]
    fn rejects_invalid_primary_monitor_definitions(
        #[case] left: i32,
        #[case] right: i32,
        #[case] flags: u32,
    ) -> anyhow::Result<()> {
        // oracle: spec MS-RDPBCGR §2.2.1.3.6.1 defines inclusive monitor rectangles relative to PRIMARY at (0,0).
        let mut bytes = initialization();
        let monitor_offset = 20 /* CapsConfirm */ + 8 /* ResetGraphics header */ + 12 /* Width, Height, Count */;
        bytes[monitor_offset..monitor_offset + 4].copy_from_slice(&left.to_le_bytes());
        bytes[monitor_offset + 8..monitor_offset + 12].copy_from_slice(&right.to_le_bytes());
        bytes[monitor_offset + 16..monitor_offset + 20].copy_from_slice(&flags.to_le_bytes());
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        assert!(Replay::load(&recording.0).is_err());
        Ok(())
    }

    #[test]
    fn preserves_a_surface_mapping_that_is_partially_outside_the_output() -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §2.2.2.15 and §3.3.5.15 map the surface's origin without requiring containment.
        // oracle: reference FreeRDP/FreeRDP@8a312188d093:libfreerdp/gdi/gfx.c gdi_OutputUpdate clips to the output.
        let mut bytes = initialization();
        let origin_offset = bytes.len() - 20 /* MapSurfaceToOutput */ + 12 /* Header, SurfaceId, Reserved */;
        bytes[origin_offset..origin_offset + 4].copy_from_slice(&32u32.to_le_bytes());
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        let replay = Replay::load(&recording.0)?;
        assert_eq!(replay.steps[0].bytes, bytes);
        Ok(())
    }

    #[rstest::rstest]
    #[case::maximum_width(32766, 1, true)]
    #[case::excessive_width(32767, 1, false)]
    #[case::maximum_height(1, 32766, true)]
    #[case::excessive_height(1, 32767, false)]
    fn validates_unmapped_surface_dimensions(
        #[case] width: u16,
        #[case] height: u16,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        // oracle: reference FreeRDP/FreeRDP@8a312188d093:channels/rdpgfx/client/rdpgfx_main.c rejects surface dimensions above MAX_SURFACE_SIZE=32766.
        let mut bytes = initialization();
        // An additional unmapped surface isolates CreateSurface validation from mapping bounds.
        let mut surface = 1u16.to_le_bytes().to_vec();
        surface.extend_from_slice(&width.to_le_bytes());
        surface.extend_from_slice(&height.to_le_bytes());
        surface.push(0x20);
        bytes.extend(pdu(9, &surface));
        bytes.extend(frame(1));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        assert_eq!(Replay::load(&recording.0).is_ok(), accepted);
        Ok(())
    }

    #[rstest::rstest]
    #[case::small_zero(2, 0, false)]
    #[case::small_first(2, 1, true)]
    #[case::small_last(2, 4096, true)]
    #[case::small_past_end(2, 4097, false)]
    #[case::full_zero(0, 0, false)]
    #[case::full_first(0, 1, true)]
    #[case::full_last(0, 25600, true)]
    #[case::full_past_end(0, 25601, false)]
    fn validates_one_based_cache_slots(
        #[case] flags: u8,
        #[case] slot: u16,
        #[case] accepted: bool,
    ) -> anyhow::Result<()> {
        // oracle: spec MS-RDPEGFX §3.3.1.4 slots use one-based indices and capacities 4,096 or 25,600.
        // oracle: reference FreeRDP/FreeRDP@8a312188d093:channels/rdpgfx/client/rdpgfx_main.c rdpgfx_set_cache_slot_data.
        let mut bytes = initialization();
        bytes[16] = flags;
        bytes.extend(pdu(0x0B, &[0, 0, 0, 0, 1, 0, 0, 0]));
        // MS-RDPEGFX 2.2.2.6: surface 0, cache key 0, chosen slot, 1x1 source rectangle.
        let mut cache = [0; 20];
        cache[10..12].copy_from_slice(&slot.to_le_bytes());
        cache[16..18].copy_from_slice(&1u16.to_le_bytes());
        cache[18..20].copy_from_slice(&1u16.to_le_bytes());
        bytes.extend(pdu(6, &cache));
        // MS-RDPEGFX 2.2.2.7: paste the chosen slot into surface 0 at (0,0).
        let mut paste = [0; 10];
        paste[..2].copy_from_slice(&slot.to_le_bytes());
        paste[4..6].copy_from_slice(&1u16.to_le_bytes());
        bytes.extend(pdu(7, &paste));
        bytes.extend(pdu(0x0C, &1u32.to_le_bytes()));
        let recording = Recording::new(&[(0, &bytes)], CaptureEnd::Closed)?;
        assert_eq!(Replay::load(&recording.0).is_ok(), accepted);
        Ok(())
    }
}
