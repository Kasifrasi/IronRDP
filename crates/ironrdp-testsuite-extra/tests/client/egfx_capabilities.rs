use ironrdp_client::config::{ConfigBuilder, Destination};
use ironrdp_client::h264::H264DecoderFactory;
use ironrdp_core::{decode, encode_vec};
use ironrdp_dvc::DrdynvcClient;
use ironrdp_dvc::pdu::{CreateRequestPdu, DrdynvcClientPdu, DrdynvcDataPdu, DrdynvcServerPdu};
use ironrdp_egfx::decode::{DecodedFrame, DecoderError, DecoderResult, H264Decoder};
use ironrdp_egfx::pdu::{
    CapabilitiesV8Flags, CapabilitiesV81Flags, CapabilitiesV107Flags, CapabilitySet, CapabilityVersion, GfxPdu,
    RawCapabilitySet,
};
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_svc::SvcProcessor as _;
use rstest::rstest;

#[rstest]
#[case::version_103(CapabilityVersion::V10_3)]
#[case::version_104(CapabilityVersion::V10_4)]
#[case::version_105(CapabilityVersion::V10_5)]
#[case::version_106(CapabilityVersion::V10_6)]
#[case::version_107(CapabilityVersion::V10_7)]
fn custom_capability_is_advertised_on_the_wire(#[case] version: CapabilityVersion) {
    // oracle: spec MS-RDPEGFX §2.2.3.6–2.2.3.10: AVC_THINCLIENT is 0x40 and cannot accompany AVC_DISABLED (0x20).
    let requested = RawCapabilitySet {
        version,
        data: 0x40u32.to_le_bytes().to_vec(),
    }
    .parsed()
    .unwrap()
    .expect("known capability version");
    let caps = capabilities(Some(vec![requested]), Some(true));
    assert_eq!(caps.len(), 1);
    let cap = caps
        .iter()
        .find(|cap| cap.version == version)
        .expect("advertised capability version");
    let flags = u32::from_le_bytes(cap.data[..4].try_into().unwrap());
    assert_eq!(flags & 0x60, 0x40);
}

#[test]
fn default_configuration_does_not_request_avc_thinclient() {
    // oracle: regression AVC444 preference is scoped to the SDL viewer, preserving other embedders' defaults.
    let caps = capabilities(None, Some(true));
    let cap = caps.iter().find(|cap| cap.version == CapabilityVersion::V10_7).unwrap();
    let flags = u32::from_le_bytes(cap.data[..4].try_into().unwrap());
    assert_eq!(flags & 0x40, 0);
}

#[test]
fn rgba_only_decoder_keeps_avc420_fallback() {
    // oracle: spec MS-RDPEGFX §2.2.3.6: without AVC_DISABLED the client must decode YUV444.
    let caps = capabilities(Some(custom_capabilities()), Some(false));
    assert!(
        caps.iter()
            .filter(|cap| matches!(
                cap.version,
                CapabilityVersion::V10_3
                    | CapabilityVersion::V10_4
                    | CapabilityVersion::V10_5
                    | CapabilityVersion::V10_6
                    | CapabilityVersion::V10_7
            ))
            .all(|cap| u32::from_le_bytes(cap.data[..4].try_into().unwrap()) & 0x60 == 0x20)
    );
    assert!(caps.iter().any(|cap| cap.version == CapabilityVersion::V8_1));
}

#[test]
fn missing_decoder_keeps_non_avc_fallback() {
    // oracle: spec MS-RDPEGFX §2.2.3.1: version 8 does not advertise AVC support.
    let caps = capabilities(Some(custom_capabilities()), None);
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0].version, CapabilityVersion::V8);
}

fn custom_capabilities() -> Vec<CapabilitySet> {
    vec![
        CapabilitySet::V10_7 {
            flags: CapabilitiesV107Flags::AVC_THIN_CLIENT,
        },
        CapabilitySet::V8_1 {
            flags: CapabilitiesV81Flags::AVC420_ENABLED,
        },
        CapabilitySet::V8 {
            flags: CapabilitiesV8Flags::SMALL_CACHE,
        },
    ]
}

fn capabilities(requested: Option<Vec<CapabilitySet>>, yuv_decoder: Option<bool>) -> Vec<RawCapabilitySet> {
    let decoder = yuv_decoder.map(|yuv| H264DecoderFactory::new(move || Some(Box::new(Decoder { yuv }))));
    let builder = ConfigBuilder::new()
        .with_destination(Destination::new("127.0.0.1").unwrap())
        .with_username("user")
        .with_password("")
        .with_client_build(0)
        .with_client_dir("tests")
        .with_client_name("graphics-test")
        .with_platform(MajorPlatformType::UNIX)
        .with_graphics_pipeline(true)
        .with_h264_decoder(decoder);
    let builder = match requested {
        Some(requested) => builder.with_graphics_capabilities(requested),
        None => builder,
    };
    let config = builder.build().unwrap();
    let mut connector = ironrdp_client::__test_build_connector(&config, None).unwrap();
    let drdynvc = connector.get_static_channel_processor_mut::<DrdynvcClient>().unwrap();
    let request = encode_vec(&DrdynvcServerPdu::Create(CreateRequestPdu::new(
        1,
        "Microsoft::Windows::RDS::Graphics".to_owned(),
    )))
    .unwrap();
    let responses = drdynvc.process(&request).unwrap();
    responses
        .iter()
        .find_map(|message| {
            let bytes = message.encode_unframed_pdu().unwrap();
            let pdu = decode::<DrdynvcClientPdu>(&bytes).unwrap();
            let DrdynvcClientPdu::Data(DrdynvcDataPdu::Data(data)) = pdu else {
                return None;
            };
            let GfxPdu::CapabilitiesAdvertise(caps) = decode::<GfxPdu>(data.data()).unwrap() else {
                return None;
            };
            Some(caps.0)
        })
        .expect("graphics capabilities advertisement")
}

struct Decoder {
    yuv: bool,
}
impl H264Decoder for Decoder {
    fn decode(&mut self, _: &[u8]) -> DecoderResult<DecodedFrame> {
        Err(DecoderError::msg("capability tests do not decode pictures"))
    }
    fn supports_yuv420(&self) -> bool {
        self.yuv
    }
}
