//! Graphics capabilities requested by the SDL viewer.

use ironrdp_egfx::client::GraphicsPipelineHandler;
use ironrdp_egfx::pdu::{CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitySet};

/// Prefer AVC444 through the `AVC_THINCLIENT` flag in [MS-RDPEGFX] section 2.2.3.6,
/// "RDPGFX_CAPSET_VERSION103", and later capability sets.
/// The server still selects the codec and compression quality.
///
/// [MS-RDPEGFX]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/
pub fn capabilities() -> Vec<CapabilitySet> {
    struct Defaults;
    impl GraphicsPipelineHandler for Defaults {}

    let mut capabilities = Defaults.capabilities();
    for capability in &mut capabilities {
        match capability {
            CapabilitySet::V10_3 { flags } => {
                flags.set(
                    CapabilitiesV103Flags::AVC_THIN_CLIENT,
                    !flags.contains(CapabilitiesV103Flags::AVC_DISABLED),
                );
            }
            CapabilitySet::V10_4 { flags } | CapabilitySet::V10_5 { flags } | CapabilitySet::V10_6 { flags } => {
                flags.set(
                    CapabilitiesV104Flags::AVC_THIN_CLIENT,
                    !flags.contains(CapabilitiesV104Flags::AVC_DISABLED),
                );
            }
            CapabilitySet::V10_7 { flags } => {
                flags.set(
                    CapabilitiesV107Flags::AVC_THIN_CLIENT,
                    !flags.contains(CapabilitiesV107Flags::AVC_DISABLED),
                );
            }
            _ => {}
        }
    }
    capabilities
}
