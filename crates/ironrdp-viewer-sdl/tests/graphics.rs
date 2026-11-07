#![expect(
    unused_crate_dependencies,
    reason = "dependencies are shared by the library, binary and integration tests"
)]

use ironrdp_egfx::pdu::{CapabilityVersion, RawCapabilitySet};

#[test]
fn sdl_requests_avc444_preference() {
    // oracle: spec MS-RDPEGFX §2.2.3.10 and §2.2.3.6: AVC_THINCLIENT is 0x40 and excludes AVC_DISABLED (0x20).
    let capabilities = ironrdp_viewer_sdl::graphics::capabilities();
    let capability = capabilities
        .iter()
        .find(|capability| capability.version() == CapabilityVersion::V10_7)
        .expect("version 10.7 capability");
    let raw = RawCapabilitySet::from(capability);
    let flags = u32::from_le_bytes(raw.data[..4].try_into().unwrap());
    assert_eq!(flags & 0x60, 0x40);
}
