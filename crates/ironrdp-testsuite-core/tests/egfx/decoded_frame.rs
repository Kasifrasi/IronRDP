use ironrdp_egfx::decode::{DecodedFrame, Yuv420Frame};

#[test]
fn getters_return_constructor_inputs() {
    let data = vec![0u8; 2 * 3 * 4];
    let frame = DecodedFrame::new(data.clone(), 2, 3);
    assert_eq!(frame.data(), data.as_slice());
    assert_eq!(frame.width(), 2);
    assert_eq!(frame.height(), 3);
}

#[test]
fn into_data_yields_owned_buffer() {
    let data = vec![0xAAu8; 4 * 4 * 4];
    let frame = DecodedFrame::new(data.clone(), 4, 4);
    assert_eq!(frame.into_data(), data);
}

// oracle: mutant planes accepted that are narrower or shorter than the picture
#[test]
fn yuv420_planes_must_cover_the_picture() {
    // A 4x2 picture has 2x1 chroma planes.
    let planes = |luma: usize, chroma: usize| (vec![0u8; luma], vec![0u8; chroma], vec![0u8; chroma]);

    let (y, u, v) = planes(4 * 2, 2);
    assert!(Yuv420Frame::new(4, 2, y, 4, u, v, 2).is_ok());

    let (y, u, v) = planes(4 * 2, 2);
    assert!(
        Yuv420Frame::new(4, 2, y, 3, u, v, 2).is_err(),
        "luma stride narrower than the picture"
    );
    let (y, u, v) = planes(4 * 2, 2);
    assert!(
        Yuv420Frame::new(4, 2, y, 4, u, v, 1).is_err(),
        "chroma stride narrower than the picture"
    );
    let (y, u, v) = planes(4 * 2 - 1, 2);
    assert!(
        Yuv420Frame::new(4, 2, y, 4, u, v, 2).is_err(),
        "luma plane one byte short"
    );
    let (y, u, v) = planes(4 * 2, 1);
    assert!(
        Yuv420Frame::new(4, 2, y, 4, u, v, 2).is_err(),
        "chroma planes one byte short"
    );
}

// oracle: spec MS-RDPEGFX §3.3.8.3.1, reverse transform with 4:2:0 chroma sampling
#[rstest::rstest]
#[case::empty(0, 0, 0)]
#[case::no_columns(0, 3, 2)]
#[case::no_rows(3, 0, 2)]
#[case::one_pixel(1, 1, 0)]
#[case::one_pair(2, 2, 0)]
#[case::odd_edges(5, 3, 0)]
#[case::padded_odd_edges(5, 3, 7)]
#[case::padded_even_width(16, 9, 8)]
#[case::wide_odd(33, 17, 3)]
fn yuv420_conversion_preserves_pixels_across_pairs_and_padded_rows(
    #[case] width: u16,
    #[case] height: u16,
    #[case] padding: usize,
) {
    let (w, h) = (usize::from(width), usize::from(height));
    let luma_stride = w + padding;
    let chroma_stride = w.div_ceil(2) + padding;
    let byte = |index: usize, factor: usize| u8::try_from((index * factor + factor) % 256).unwrap();
    let y: Vec<u8> = (0..luma_stride * h).map(|i| byte(i, 37)).collect();
    let u: Vec<u8> = (0..chroma_stride * h.div_ceil(2)).map(|i| byte(i, 91)).collect();
    let v: Vec<u8> = (0..chroma_stride * h.div_ceil(2)).map(|i| byte(i, 53)).collect();
    let picture = Yuv420Frame::new(
        width,
        height,
        y.clone(),
        luma_stride,
        u.clone(),
        v.clone(),
        chroma_stride,
    )
    .expect("valid picture planes");

    let clamp = |value: i32| u8::try_from(value.clamp(0, 255)).unwrap();
    let mut expected = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        for col in 0..w {
            let luma = 256 * i32::from(y[row * luma_stride + col]);
            let cu = i32::from(u[row / 2 * chroma_stride + col / 2]) - 128;
            let cv = i32::from(v[row / 2 * chroma_stride + col / 2]) - 128;
            expected.extend_from_slice(&[
                clamp((luma + 403 * cv) >> 8),
                clamp((luma - 48 * cu - 120 * cv) >> 8),
                clamp((luma + 475 * cu) >> 8),
                0xFF,
            ]);
        }
    }
    assert_eq!(picture.to_rgba().data(), expected);
}
