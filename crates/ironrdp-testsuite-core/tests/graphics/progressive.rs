use std::collections::BTreeSet;

use ironrdp_graphics::progressive::{DecodedTile, ProgressiveDecodeError, ProgressiveDecoder, encode_first_pass};
use ironrdp_pdu::codecs::rfx::RfxRectangle;
use ironrdp_pdu::codecs::rfx::progressive::{
    ComponentCodecQuant, ProgressiveBlock, ProgressiveContextPdu, ProgressiveFrameBeginPdu, ProgressiveFrameEndPdu,
    ProgressiveRegion, ProgressiveSyncPdu, ProgressiveTile, TILE_FLAG_DIFFERENCE, TileSimple, TileUpgrade,
    encode_progressive_stream,
};

#[test]
fn region_error_preserves_references_for_difference_tiles() {
    // oracle: mutant losing retained references when another tile in the region fails.
    // oracle: spec MS-RDPEGFX §3.3.8.2.1.1 (a zero difference preserves prior coefficients).
    let (mut decoder, original) = decoder_after_region_error();
    let zero = encode_component(0);
    let stream = tile_stream(simple_tiles(&zero, TILE_FLAG_DIFFERENCE));
    let recovered = decoder
        .decode_bitmap(1, 1, 512, 64, &stream)
        .expect("difference tiles retain their references after a region error");

    assert_eq!(tile_pixels(recovered), tile_pixels(original));
}

#[test]
fn region_error_preserves_tile_state_for_upgrades() {
    // oracle: mutant losing tile state when another tile in the region fails.
    // oracle: spec MS-RDPEGFX §3.3.8.2.1.2 (an upgrade with no remaining bits leaves coefficients unchanged).
    let (mut decoder, original) = decoder_after_region_error();
    let stream = tile_stream(
        (0..8)
            .map(|x_idx| {
                ProgressiveTile::Upgrade(TileUpgrade {
                    quant_idx_y: 0,
                    quant_idx_cb: 0,
                    quant_idx_cr: 0,
                    x_idx,
                    y_idx: 0,
                    quality: 0xFF,
                    y_srl_data: &[],
                    y_raw_data: &[],
                    cb_srl_data: &[],
                    cb_raw_data: &[],
                    cr_srl_data: &[],
                    cr_raw_data: &[],
                })
            })
            .collect(),
    );
    let recovered = decoder
        .decode_bitmap(1, 1, 512, 64, &stream)
        .expect("upgrades retain their tile state after a region error");

    assert_eq!(tile_pixels(recovered), tile_pixels(original));
}

#[test]
fn one_pixel_region_rows_survive_clipping() {
    // oracle: spec MS-RDPEGFX §2.2.4.2.1.5: rects defines the updated region, including one-pixel rows.
    let component = encode_component(0);
    let stream = tile_stream_with_rectangles(
        simple_tiles(&component, 0),
        vec![
            RfxRectangle {
                x: 0,
                y: 0,
                width: 6,
                height: 2,
            },
            RfxRectangle {
                x: 0,
                y: 3,
                width: 2,
                height: 1,
            },
            RfxRectangle {
                x: 4,
                y: 3,
                width: 2,
                height: 1,
            },
        ],
    );
    let tiles = ProgressiveDecoder::new()
        .decode_bitmap(1, 1, 512, 64, &stream)
        .expect("decode thin regions");
    let expected = (0..6)
        .flat_map(|x| [(x, 0), (x, 1)])
        .chain([(0, 3), (1, 3), (4, 3), (5, 3)])
        .collect::<BTreeSet<_>>();
    assert_eq!(updated_pixels(&tiles), expected);
}

#[test]
fn one_pixel_region_columns_survive_clipping() {
    // oracle: spec MS-RDPEGFX §2.2.4.2.1.5: rects defines the updated region, including one-pixel columns.
    let component = encode_component(0);
    let stream = tile_stream_with_rectangles(
        simple_tiles(&component, 0),
        vec![
            RfxRectangle {
                x: 0,
                y: 0,
                width: 2,
                height: 6,
            },
            RfxRectangle {
                x: 3,
                y: 0,
                width: 1,
                height: 2,
            },
            RfxRectangle {
                x: 3,
                y: 4,
                width: 1,
                height: 2,
            },
        ],
    );
    let tiles = ProgressiveDecoder::new()
        .decode_bitmap(1, 1, 512, 64, &stream)
        .expect("decode thin columns");
    let expected = (0..6)
        .flat_map(|y| [(0, y), (1, y)])
        .chain([(3, 0), (3, 1), (3, 4), (3, 5)])
        .collect::<BTreeSet<_>>();
    assert_eq!(updated_pixels(&tiles), expected);
}

#[test]
fn thin_region_is_clipped_at_tile_and_surface_edges() {
    // oracle: spec MS-RDPEGFX §2.2.4.2.1.5 and §3.3.1.6: only visible REGION pixels update the surface.
    let component = encode_component(0);
    let tiles = simple_tiles(&component, 0).into_iter().take(2).collect();
    let stream = tile_stream_with_rectangles(
        tiles,
        vec![RfxRectangle {
            x: 64,
            y: 62,
            width: 1,
            height: 2,
        }],
    );
    let decoded = ProgressiveDecoder::new()
        .decode_bitmap(1, 1, 65, 63, &stream)
        .expect("decode clipped surface edge");
    assert_eq!(updated_pixels(&decoded), BTreeSet::from([(64, 62)]));
    assert!(
        decoded
            .iter()
            .flat_map(|tile| &tile.update_rectangles)
            .all(|rect| rect.left < rect.right && rect.top < rect.bottom)
    );
}

fn updated_pixels(tiles: &[DecodedTile]) -> BTreeSet<(u16, u16)> {
    tiles
        .iter()
        .flat_map(|tile| &tile.update_rectangles)
        .flat_map(|rect| (rect.top..rect.bottom).flat_map(move |y| (rect.left..rect.right).map(move |x| (x, y))))
        .collect()
}

fn tile_pixels(tiles: Vec<DecodedTile>) -> Vec<(u16, u16, Vec<u8>)> {
    tiles
        .into_iter()
        .map(|tile| (tile.x_idx, tile.y_idx, tile.pixels))
        .collect()
}

fn decoder_after_region_error() -> (ProgressiveDecoder, Vec<DecodedTile>) {
    let components: Vec<_> = (1..=8).map(|value| encode_component(value * 8)).collect();
    let mut initial = simple_tiles(&components[0], 0);
    for (tile, component) in initial.iter_mut().zip(&components) {
        let ProgressiveTile::Simple(tile) = tile else {
            unreachable!("simple_tiles creates only TILE_SIMPLE blocks");
        };
        tile.y_data = component;
        tile.cb_data = component;
        tile.cr_data = component;
    }
    let mut decoder = ProgressiveDecoder::new();
    let original = decoder
        .decode_bitmap(1, 1, 512, 64, &tile_stream(initial))
        .expect("initial tiles decode");

    let zero = encode_component(0);
    let mut invalid = simple_tiles(&zero, TILE_FLAG_DIFFERENCE);
    let ProgressiveTile::Simple(tile) = &mut invalid[3] else {
        unreachable!("simple_tiles creates only TILE_SIMPLE blocks");
    };
    tile.y_data = &[];
    let error = decoder
        .decode_bitmap(1, 1, 512, 64, &tile_stream(invalid))
        .err()
        .expect("the empty component stream is rejected");
    assert!(
        matches!(error, ProgressiveDecodeError::Rlgr(_)),
        "unexpected region error: {error}"
    );

    (decoder, original)
}

fn encode_component(value: i16) -> Vec<u8> {
    let mut coefficients = [0; 4096];
    coefficients[4032..].fill(value);
    let mut encoded = vec![0; 8192];
    let len = encode_first_pass(
        &mut coefficients,
        &mut encoded,
        &ComponentCodecQuant::LOSSLESS,
        &ComponentCodecQuant::LOSSLESS,
        false,
    )
    .expect("encode synthetic full-quality coefficients");
    encoded.truncate(len);
    encoded
}

fn simple_tiles(component: &[u8], flags: u8) -> Vec<ProgressiveTile<'_>> {
    (0..8)
        .map(|x_idx| {
            ProgressiveTile::Simple(TileSimple {
                quant_idx_y: 0,
                quant_idx_cb: 0,
                quant_idx_cr: 0,
                x_idx,
                y_idx: 0,
                flags,
                y_data: component,
                cb_data: component,
                cr_data: component,
                tail_data: &[],
            })
        })
        .collect()
}

fn tile_stream(tiles: Vec<ProgressiveTile<'_>>) -> Vec<u8> {
    tile_stream_with_rectangles(
        tiles,
        vec![RfxRectangle {
            x: 0,
            y: 0,
            width: 512,
            height: 64,
        }],
    )
}

fn tile_stream_with_rectangles(tiles: Vec<ProgressiveTile<'_>>, rects: Vec<RfxRectangle>) -> Vec<u8> {
    encode_progressive_stream(&[
        ProgressiveBlock::Sync(ProgressiveSyncPdu),
        ProgressiveBlock::Context(ProgressiveContextPdu {
            context_id: 0,
            tile_size: 0x40,
            flags: 0,
        }),
        ProgressiveBlock::FrameBegin(ProgressiveFrameBeginPdu {
            frame_index: 0,
            region_count: 1,
        }),
        ProgressiveBlock::Region(ProgressiveRegion {
            tile_size: 0x40,
            rects,
            quant_vals: vec![ComponentCodecQuant::LOSSLESS],
            quant_prog_vals: vec![],
            flags: 0,
            tiles,
        }),
        ProgressiveBlock::FrameEnd(ProgressiveFrameEndPdu),
    ])
    .expect("encode synthetic progressive region")
}
