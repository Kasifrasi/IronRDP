use core::num::NonZeroU16;

use ironrdp::client::rdp::DesktopUpdate;
use ironrdp::input::MousePosition;
use ironrdp::pdu::gcc::{Monitor, MonitorFlags};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp_viewer::monitor::{DesktopFrame, MonitorLayout, MonitorViewport};

// oracle: spec MS-RDPBCGR §2.2.1.3.6.1 (inclusive coordinates relative to the primary monitor)
#[test]
fn monitors_left_and_above_primary_have_nonnegative_framebuffer_rectangles() {
    let layout = MonitorLayout::new(&[
        monitor(0, 0, 3, 1, true),
        monitor(-4, 0, -1, 1, false),
        monitor(0, -2, 3, -1, false),
    ])
    .unwrap();

    assert_eq!(layout.size(), (8, 4));
    assert_eq!(
        layout.monitors(),
        &[
            MonitorViewport {
                x: 4,
                y: 2,
                width: 4,
                height: 2,
                primary: true
            },
            MonitorViewport {
                x: 0,
                y: 2,
                width: 4,
                height: 2,
                primary: false
            },
            MonitorViewport {
                x: 4,
                y: 0,
                width: 4,
                height: 2,
                primary: false
            },
        ]
    );
}

// oracle: mutant using the entire desktop extent instead of the receiving monitor's scaled rectangle
#[test]
fn window_positions_scale_into_the_receiving_monitor() {
    let viewport = MonitorViewport {
        x: 4,
        y: 2,
        width: 4,
        height: 2,
        primary: false,
    };

    assert_eq!(
        viewport.position((8, 4), (4.0, 2.0)),
        Some(MousePosition { x: 6, y: 3 })
    );
    assert_eq!(
        viewport.position((8, 4), (-1.0, -1.0)),
        Some(MousePosition { x: 4, y: 2 })
    );
    assert_eq!(
        viewport.position((8, 4), (99.0, 99.0)),
        Some(MousePosition { x: 7, y: 3 })
    );
}

// oracle: mutant a nonfinite pointer coordinate or minimized window producing an arbitrary click
#[rstest::rstest]
#[case::minimized((0, 4), (0.0, 0.0))]
#[case::nan((8, 4), (f64::NAN, 0.0))]
#[case::infinite((8, 4), (0.0, f64::INFINITY))]
fn invalid_window_positions_do_not_produce_input(#[case] size: (u32, u32), #[case] position: (f64, f64)) {
    let viewport = MonitorViewport {
        x: 4,
        y: 2,
        width: 4,
        height: 2,
        primary: false,
    };
    assert_eq!(viewport.position(size, position), None);
}

// oracle: mutant painting the complete shared desktop into every monitor window
#[test]
fn monitor_views_crop_and_scale_distinct_regions() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1, 1, true), monitor(2, 0, 3, 1, false)]).unwrap();
    let frame = [1, 2, 3, 4, 5, 6, 7, 8];
    let mut left = [0; 16];
    let mut right = [0; 4];

    assert!(layout.monitors()[0].render(&frame, (4, 2), &mut left, (4, 4)));
    assert!(layout.monitors()[1].render(&frame, (4, 2), &mut right, (2, 2)));
    assert_eq!(left, [1, 1, 2, 2, 1, 1, 2, 2, 5, 5, 6, 6, 5, 5, 6, 6]);
    assert_eq!(right, [3, 4, 7, 8]);
}

// oracle: mutant dropping the vertical crop origin or using the horizontal extent for vertical scaling
#[test]
fn scaled_monitor_views_keep_both_axis_extents() {
    let viewport = MonitorViewport {
        x: 1,
        y: 1,
        width: 4,
        height: 2,
        primary: false,
    };
    let frame: Vec<u32> = (0..24).collect();
    let mut target = [0; 8];

    assert!(viewport.render(&frame, (6, 4), &mut target, (2, 4)));
    assert_eq!(target, [7, 9, 7, 9, 13, 15, 13, 15]);
}

// oracle: mutant an out-of-frame x coordinate wrapping into the next framebuffer row
#[test]
fn pixels_outside_the_available_frame_are_black() {
    let viewport = MonitorViewport {
        x: 1,
        y: 0,
        width: 2,
        height: 2,
        primary: false,
    };
    let mut target = [99; 4];
    assert!(viewport.render(&[1, 2, 3, 4], (2, 2), &mut target, (2, 2)));
    assert_eq!(target, [2, 0, 4, 0]);
}

// oracle: mutant ignoring the packed region offset, or replacing a complete frame with one delta
#[test]
fn dirty_region_crossing_a_monitor_boundary_updates_both_views() {
    let mut frame = DesktopFrame::default();
    frame
        .apply(&update((4, 2), (0, 0, 3, 1), vec![1, 2, 3, 4, 5, 6, 7, 8]))
        .unwrap();
    frame
        .apply(&update((4, 2), (1, 0, 2, 1), vec![20, 30, 60, 70]))
        .unwrap();
    let layout = MonitorLayout::new(&[monitor(0, 0, 1, 1, true), monitor(2, 0, 3, 1, false)]).unwrap();
    let mut left = [0; 4];
    let mut right = [0; 4];

    layout.monitors()[0].render(frame.pixels(), frame.size(), &mut left, (2, 2));
    layout.monitors()[1].render(frame.pixels(), frame.size(), &mut right, (2, 2));
    assert_eq!(left, [1, 20, 5, 60]);
    assert_eq!(right, [30, 4, 70, 8]);
}

// oracle: mutant ignoring the vertical origin of a packed dirty rectangle
#[test]
fn dirty_regions_keep_their_vertical_origin() {
    let mut frame = DesktopFrame::default();
    frame.apply(&update((3, 4), (1, 2, 2, 3), vec![7, 8, 9, 10])).unwrap();

    assert_eq!(frame.pixels(), [0, 0, 0, 0, 0, 0, 0, 7, 8, 0, 9, 10]);
}

// oracle: mutant retaining old pixels when a desktop update announces a different extent
#[test]
fn new_frame_extents_clear_pixels_outside_the_first_update() {
    let mut frame = DesktopFrame::default();
    frame.apply(&update((2, 2), (0, 0, 1, 1), vec![1, 2, 3, 4])).unwrap();
    frame.apply(&update((4, 2), (1, 0, 2, 0), vec![9, 10])).unwrap();

    assert_eq!(frame.size(), (4, 2));
    assert_eq!(frame.pixels(), [0, 9, 10, 0, 0, 0, 0, 0]);
}

// oracle: mutant carrying framebuffer pixels from a previous connection with the same dimensions
#[test]
fn clearing_the_frame_discards_previous_connection_pixels() {
    let mut frame = DesktopFrame::default();
    frame.apply(&update((2, 2), (0, 0, 1, 1), vec![1, 2, 3, 4])).unwrap();
    frame.clear();
    assert_eq!(frame.size(), (0, 0));
    assert!(frame.pixels().is_empty());
    frame.apply(&update((2, 2), (1, 0, 1, 0), vec![9])).unwrap();
    assert_eq!(frame.pixels(), [0, 9, 0, 0]);
}

// oracle: mutant identifying a topology solely by its desktop width and height
#[test]
fn layouts_with_equal_extents_keep_their_different_monitor_rectangles() {
    let first = MonitorLayout::new(&[monitor(0, 0, 1, 1, true), monitor(2, 0, 3, 1, false)]).unwrap();
    let second = MonitorLayout::new(&[monitor(0, 0, 1, 1, true), monitor(-2, 0, -1, 1, false)]).unwrap();
    assert_eq!(first.size(), second.size());
    assert_eq!((first.monitors()[0].x, second.monitors()[0].x), (0, 2));
    assert_eq!((first.monitors()[1].x, second.monitors()[1].x), (2, 0));
}

// oracle: spec MS-RDPBCGR §§2.2.1.3.6 and 2.2.1.3.6.1 (monitor count, extent and primary origin)
#[rstest::rstest]
#[case::empty(vec![])]
#[case::too_many(vec![monitor(0, 0, 1, 1, true); 17])]
#[case::no_primary(vec![monitor(0, 0, 1, 1, false)])]
#[case::two_primaries(vec![monitor(0, 0, 1, 1, true), monitor(0, 0, 1, 1, true)])]
#[case::invalid_origin(vec![monitor(1, 0, 2, 1, true)])]
#[case::inverted_rectangle(vec![monitor(0, 0, -1, 1, true)])]
#[case::oversized_extent(vec![monitor(0, 0, 32_766, 1, true)])]
#[case::overflowing_signed_coordinates(vec![monitor(0, 0, 1, 1, true), monitor(i32::MIN, 0, i32::MAX, 1, false)])]
fn malformed_layouts_are_rejected(#[case] monitors: Vec<Monitor>) {
    assert!(MonitorLayout::new(&monitors).is_err());
}

// oracle: mutant a server-provided framebuffer extent allocating beyond the viewer memory budget
#[test]
fn oversized_frame_does_not_replace_the_current_frame() {
    let mut frame = DesktopFrame::default();
    frame.apply(&update((2, 2), (0, 0, 1, 1), vec![1, 2, 3, 4])).unwrap();

    assert!(frame.apply(&update((32_766, 32_766), (0, 0, 0, 0), vec![9])).is_err());
    assert_eq!(frame.size(), (2, 2));
    assert_eq!(frame.pixels(), [1, 2, 3, 4]);
}

fn update(size: (u16, u16), rect: (u16, u16, u16, u16), pixels: Vec<u32>) -> DesktopUpdate {
    DesktopUpdate::new(
        pixels,
        NonZeroU16::new(size.0).unwrap(),
        NonZeroU16::new(size.1).unwrap(),
        InclusiveRectangle {
            left: rect.0,
            top: rect.1,
            right: rect.2,
            bottom: rect.3,
        },
    )
    .unwrap()
}

fn monitor(left: i32, top: i32, right: i32, bottom: i32, primary: bool) -> Monitor {
    Monitor {
        left,
        top,
        right,
        bottom,
        flags: if primary {
            MonitorFlags::PRIMARY
        } else {
            MonitorFlags::empty()
        },
    }
}
