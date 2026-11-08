use ironrdp_client::config::{ConfigBuilder, Destination};
use ironrdp_connector::{ClientConnector, Sequence as _};
use ironrdp_core::{Encode as _, WriteBuf, decode, encode_vec};
use ironrdp_pdu::gcc::{ClientGccBlocks, ConnectionType, RedirectionVersion};
use ironrdp_pdu::mcs::ConnectInitial;
use ironrdp_pdu::nego::{ConnectionConfirm, ResponseFlags, SecurityProtocol};
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::x224::X224;
use ironrdp_viewer::cli::parse_config_from;
use rstest::rstest;

// oracle: regression UDP advertisement override omits only CS_MULTITRANSPORT while UDP remains eligible.
#[test]
fn udp_advertisement_override_omits_only_multitransport_block() {
    let enabled_config = complete_builder().build().expect("default UDP advertisement");
    let disabled_config = complete_builder()
        .with_udp_advertisement(false)
        .build()
        .expect("disabled UDP advertisement");
    assert!(enabled_config.udp_advertisement_enabled());
    assert!(!disabled_config.udp_advertisement_enabled());
    assert!(enabled_config.udp_transport_enabled());
    assert!(disabled_config.udp_transport_enabled());
    let enabled = encoded_gcc(ironrdp_client::__test_build_connector_with_udp(&enabled_config).unwrap());
    let disabled = encoded_gcc(ironrdp_client::__test_build_connector_with_udp(&disabled_config).unwrap());
    let enabled_blocks = raw_blocks(&enabled);
    let disabled_blocks = raw_blocks(&disabled);

    // oracle: spec MS-RDPBCGR §2.2.1.3.8; type 0xC00A, length 8, reliable UDP 0x1 and Soft-Sync 0x200.
    let multitransport = [0x0a, 0xc0, 0x08, 0x00, 0x01, 0x02, 0x00, 0x00];
    assert_eq!(
        enabled_blocks.iter().copied().find(|block| block[..2] == [0x0a, 0xc0]),
        Some(multitransport.as_slice())
    );
    assert!(disabled_blocks.iter().all(|block| block[..2] != [0x0a, 0xc0]));
    assert_eq!(enabled.len(), disabled.len() + multitransport.len());
    assert_eq!(
        enabled_blocks
            .into_iter()
            .filter(|block| block[..2] != [0x0a, 0xc0])
            .collect::<Vec<_>>(),
        disabled_blocks
    );
    for bytes in [&enabled, &disabled] {
        let gcc = decode::<ClientGccBlocks>(bytes).expect("decode encoded GCC blocks");
        assert_eq!(gcc.cluster.unwrap().redirection_version, RedirectionVersion::V6);
    }
}

// oracle: regression the advertisement option cannot enable UDP when its connector caller disables it.
#[test]
fn udp_advertisement_does_not_override_disabled_connector_udp() {
    let config = complete_builder()
        .with_udp_advertisement(true)
        .build()
        .expect("explicit UDP advertisement");
    let bytes = encoded_gcc(ironrdp_client::__test_build_connector(&config, None).unwrap());
    let gcc = decode::<ClientGccBlocks>(&bytes).expect("decode encoded GCC blocks");
    assert!(gcc.multi_transport_channel.is_none());
    assert_eq!(gcc.cluster.unwrap().redirection_version, RedirectionVersion::V5);
}

// oracle: regression the viewer defaults to advertising UDP and preserves explicit advertisement and UDP settings.
#[rstest]
#[case::default(&[], true, true)]
#[case::explicit_true(&["--udp=true", "--udp-advertise=true"], true, true)]
#[case::explicit_false(&["--udp=true", "--udp-advertise=false"], false, true)]
#[case::udp_disabled(&["--udp=false", "--udp-advertise=true"], true, false)]
fn viewer_udp_advertisement_settings(
    #[case] extra_args: &[&str],
    #[case] expected_advertisement: bool,
    #[case] expected_udp: bool,
) {
    let config = parse_config_from(
        ["ironrdp-viewer", "-u", "user", "-p", "password", "server.invalid"]
            .into_iter()
            .chain(extra_args.iter().copied()),
    )
    .expect("viewer UDP settings");
    assert_eq!(config.udp_advertisement_enabled(), expected_advertisement);
    assert_eq!(config.udp_transport_enabled(), expected_udp);
    assert_eq!(config.connector().connection_type, ConnectionType::Autodetect);
}

fn complete_builder() -> ConfigBuilder {
    ConfigBuilder::new()
        .with_destination(Destination::from_parts("server.invalid", 3389))
        .with_username("user")
        .with_password("")
        .with_client_build(1)
        .with_client_dir("tests")
        .with_platform(MajorPlatformType::UNIX)
        .with_client_name("udp-advertisement-test")
        .with_connection_type(ConnectionType::Autodetect)
        .with_udp_transport(true)
}

fn encoded_gcc(mut connector: ClientConnector) -> Vec<u8> {
    connector.step_no_input(&mut WriteBuf::new()).expect("send negotiation");
    let confirm = encode_vec(&X224(ConnectionConfirm::Response {
        flags: ResponseFlags::EXTENDED_CLIENT_DATA_SUPPORTED,
        protocol: SecurityProtocol::SSL,
    }))
    .unwrap();
    connector
        .step(&confirm, None, &mut WriteBuf::new())
        .expect("accept negotiation");
    connector.mark_security_upgrade_as_done();
    let mut output = WriteBuf::new();
    connector
        .step_no_input(&mut output)
        .expect("encode MCS Connect Initial");
    let packet = output.filled();
    // MS-RDPBCGR §2.2.1.3 places TPKT (4 bytes) and X.224 Data TPDU (3 bytes) before MCS.
    let initial = decode::<ConnectInitial>(&packet[7..]).expect("decode MCS Connect Initial");
    let gcc_size = initial.conference_create_request.gcc_blocks().size();
    packet[packet.len() - gcc_size..].to_vec()
}

fn raw_blocks(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut blocks = Vec::new();
    while !bytes.is_empty() {
        let length = usize::from(u16::from_le_bytes(bytes[2..4].try_into().unwrap()));
        assert!(4 <= length && length <= bytes.len(), "valid GCC block length");
        let (block, rest) = bytes.split_at(length);
        blocks.push(block);
        bytes = rest;
    }
    blocks
}
