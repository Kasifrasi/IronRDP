use ironrdp_graphics::dwt_extrapolate;

#[test]
fn decode_matches_freerdp_for_signed_coefficients() {
    // oracle: reference FreeRDP/FreeRDP@97eff6ddfbbfaa8e42a126d362686bd001ff8bdc:libfreerdp/codec/progressive.c.
    // The fixture is the little-endian output of rfx_dwt_2d_extrapolate_decode for this input.
    // Nonuniform signed bands exercise rounding and the extrapolated right and bottom edges.
    let mut coefficients: [i16; 4096] =
        core::array::from_fn(|i| i16::try_from((i * 73 + (i / 64) * 29) % 127).unwrap() - 63);
    let mut scratch = [0; 4096];
    let expected: Vec<i16> = include_bytes!("../../test_data/graphics/dwt_extrapolate_freerdp.bin")
        .chunks_exact(2)
        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();

    dwt_extrapolate::decode(&mut coefficients, &mut scratch);

    assert_eq!(coefficients.as_slice(), expected);
}
