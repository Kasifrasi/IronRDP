use core::num::NonZeroU16;
use core::time::Duration;
use std::time::Instant;

use ironrdp::client::rdp::DesktopUpdate;
use ironrdp::input::MousePosition;
use ironrdp::pdu::gcc::{Monitor, MonitorFlags};
use ironrdp::pdu::geometry::InclusiveRectangle;
use ironrdp_viewer::monitor::{
    DesktopFrame, MonitorFocusState, MonitorLayout, MonitorResizeState, MonitorViewport, OutputGeometry,
};

// oracle: mutant releasing held input between focus loss and a later dispatch that focuses another monitor window.
#[test]
fn drag_survives_focus_transfer_in_separate_dispatches() {
    let now = Instant::now();
    let mut focus = MonitorFocusState::default();
    focus.set_focused(now, false);
    assert!(!focus.release_due(now));

    focus.set_focused(now + Duration::from_millis(20), true);
    assert!(!focus.release_due(now + Duration::from_secs(1)));
    assert_eq!(focus.deadline(), None);
}

// oracle: mutant retaining held input indefinitely after focus leaves every monitor window.
#[test]
fn drag_releases_held_input_after_external_focus_loss() {
    let now = Instant::now();
    let mut focus = MonitorFocusState::default();
    focus.set_focused(now, false);

    assert!(!focus.release_due(now + Duration::from_millis(20)));
    assert!(focus.release_due(now + Duration::from_secs(1)));
    assert!(!focus.release_due(now + Duration::from_secs(2)));
    assert_eq!(focus.deadline(), None);
}

// oracle: reference reversed local outputs with a horizontal remote row; logical x=1800 is x=3720 on remote monitor 1.
#[test]
fn drag_follows_local_outputs_when_the_remote_row_has_a_different_order() {
    let outputs = [
        output((1920, 0), (1920, 1080), (1920, 1080)),
        output((0, 0), (1920, 1080), (1920, 1080)),
    ];
    let layout = MonitorLayout::new(&[monitor(0, 0, 1919, 1079, true), monitor(1920, 0, 3839, 1079, false)]).unwrap();

    assert_eq!(
        layout.position_on_outputs(&outputs, 0, (1920, 1080), (-120.0, 540.0)),
        Some(MousePosition { x: 3720, y: 540 })
    );
}

// oracle: reference two adjacent 1920-pixel outputs; a grabbed pointer 180 pixels past the first window is at desktop x=2100.
#[test]
fn drag_crosses_the_source_monitor_boundary() {
    let outputs = [
        output((0, 0), (1920, 1080), (1920, 1080)),
        output((1920, 0), (1920, 1080), (1920, 1080)),
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 0).unwrap();

    assert_eq!(
        layout.position_on_outputs(&outputs, 0, (1920, 1080), (2100.0, 540.0)),
        Some(MousePosition { x: 2100, y: 540 })
    );
}

// oracle: reference captured Wayland layout; laptop x=3840+(180*1.25), y=120+(480*1.25), regardless of the event's source window.
#[rstest::rstest]
#[case::external_to_laptop(1, (1920, 1080), (2100.0, 600.0), (4065, 720))]
#[case::laptop_to_external(2, (1920, 1200), (-225.0, 600.0), (3660, 600))]
#[case::laptop_own_window(2, (1920, 1200), (225.0, 600.0), (4065, 720))]
#[case::laptop_left_edge(1, (1920, 1080), (1920.0, 600.0), (3840, 720))]
fn drag_uses_the_destination_output_scale(
    #[case] source: usize,
    #[case] window_size: (u32, u32),
    #[case] local: (f64, f64),
    #[case] expected: (u16, u16),
) {
    let outputs = [
        output((0, 0), (1920, 1080), (1920, 1080)),
        output((1920, 0), (1920, 1080), (1920, 1080)),
        output((3840, 120), (1536, 960), (1920, 1200)),
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 2).unwrap();

    assert_eq!(
        layout.position_on_outputs(&outputs, source, window_size, local),
        Some(MousePosition {
            x: expected.0,
            y: expected.1
        })
    );
}

// oracle: reference scale-2 desktop with negative logical origins; native top is (0,0) and native bottom is (800,2000).
#[rstest::rstest]
#[case::up(1, (1600, 1200), (200.0, -200.0), (1000, 1800))]
#[case::down(0, (2400, 2000), (1000.0, 2200.0), (1000, 2200))]
fn drag_crosses_vertical_monitor_boundaries(
    #[case] source: usize,
    #[case] window_size: (u32, u32),
    #[case] local: (f64, f64),
    #[case] expected: (u16, u16),
) {
    let outputs = [
        output((-600, -1000), (1200, 1000), (2400, 2000)),
        output((-200, 0), (800, 600), (1600, 1200)),
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 1).unwrap();

    assert_eq!(
        layout.position_on_outputs(&outputs, source, window_size, local),
        Some(MousePosition {
            x: expected.0,
            y: expected.1
        })
    );
}

// oracle: reference selected desktop boundaries; out-of-desktop input stays on the nearest selected output's last valid pixel.
#[rstest::rstest]
#[case::top_left((-100.0, -100.0), (0, 0))]
#[case::bottom_right((6000.0, 1500.0), (5759, 1319))]
fn drag_clamps_at_the_selected_desktop_edge(#[case] local: (f64, f64), #[case] expected: (u16, u16)) {
    let outputs = [
        output((0, 0), (1920, 1080), (1920, 1080)),
        output((1920, 0), (1920, 1080), (1920, 1080)),
        output((3840, 120), (1536, 960), (1920, 1200)),
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 0).unwrap();

    assert_eq!(
        layout.position_on_outputs(&outputs, 0, (1920, 1080), local),
        Some(MousePosition {
            x: expected.0,
            y: expected.1
        })
    );
}

// oracle: reference finite fullscreen-coordinate model; a missing source, zero content extent or NaN has no desktop position.
#[rstest::rstest]
#[case::missing_source(1, (1920, 1080), (1.0, 1.0))]
#[case::minimized(0, (0, 1080), (1.0, 1.0))]
#[case::nan(0, (1920, 1080), (f64::NAN, 1.0))]
fn drag_rejects_unmappable_input(#[case] source: usize, #[case] window_size: (u32, u32), #[case] local: (f64, f64)) {
    let outputs = [output((0, 0), (1920, 1080), (1920, 1080))];
    let layout = MonitorLayout::from_local_outputs(&outputs, 0).unwrap();

    assert_eq!(layout.position_on_outputs(&outputs, source, window_size, local), None);
}

// oracle: reference captured Wayland output layout; HDMI and DP are adjacent, and the laptop starts 120 logical pixels lower.
#[test]
fn local_layout_preserves_offsets_and_native_mixed_scale_resolutions() {
    let outputs = [
        OutputGeometry {
            position: (0, 0),
            logical_size: (1920, 1080),
            native_size: (1920, 1080),
        },
        OutputGeometry {
            position: (1920, 0),
            logical_size: (1920, 1080),
            native_size: (1920, 1080),
        },
        OutputGeometry {
            position: (3840, 120),
            logical_size: (1536, 960),
            native_size: (1920, 1200),
        },
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 0).unwrap();
    assert_eq!(layout.size(), (5760, 1320));
    assert_eq!(
        layout.to_client_monitor_data().unwrap().monitors,
        vec![
            monitor(0, 0, 1919, 1079, true),
            monitor(1920, 0, 3839, 1079, false),
            monitor(3840, 120, 5759, 1319, false),
        ]
    );
}

// oracle: reference captured Wayland output layout translated to the laptop origin; changing primary does not move a physical output.
#[test]
fn local_layout_is_independent_of_selection_order_and_primary() {
    let outputs = [
        output((3840, 120), (1536, 960), (1920, 1200)),
        output((0, 0), (1920, 1080), (1920, 1080)),
        output((1920, 0), (1920, 1080), (1920, 1080)),
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 0).unwrap();
    assert_eq!(layout.size(), (5760, 1320));
    assert_eq!(
        layout.to_client_monitor_data().unwrap().monitors,
        vec![
            monitor(0, 0, 1919, 1199, true),
            monitor(-3840, -120, -1921, 959, false),
            monitor(-1920, -120, -1, 959, false),
        ]
    );
}

// oracle: reference uniform scale 2 model doubles the logical cross-axis offset of 400 and the 1000-pixel vertical edge.
#[test]
fn local_layout_supports_vertical_neighbors_and_negative_coordinates() {
    let outputs = [
        output((-600, -1000), (1200, 1000), (2400, 2000)),
        output((-200, 0), (800, 600), (1600, 1200)),
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 1).unwrap();
    assert_eq!(layout.size(), (2400, 3200));
    assert_eq!(
        layout.to_client_monitor_data().unwrap().monitors,
        vec![monitor(-800, -2000, 1599, -1, false), monitor(0, 0, 1599, 1199, true),]
    );
}

// oracle: spec MS-RDPEDISP §3.1.5.2 permits monitors adjacent at a single point.
#[test]
fn local_layout_accepts_corner_neighbors() {
    let outputs = [
        output((0, 0), (800, 600), (800, 600)),
        output((800, 600), (640, 480), (640, 480)),
    ];
    let layout = MonitorLayout::from_local_outputs(&outputs, 0).unwrap();
    assert_eq!(
        layout.to_client_monitor_data().unwrap().monitors,
        vec![monitor(0, 0, 799, 599, true), monitor(800, 600, 1439, 1079, false),]
    );
}

// oracle: spec MS-RDPEDISP §§3.1.5.2 and 2.2.2.2.1 require nonoverlapping adjacent rectangles and supported dimensions.
#[rstest::rstest]
#[case::empty_logical(vec![output((0, 0), (0, 1080), (1920, 1080))])]
#[case::odd_native_width(vec![output((0, 0), (1921, 1080), (1921, 1080))])]
#[case::oversized_native(vec![output((0, 0), (9000, 1080), (9000, 1080))])]
#[case::overlapping_logical(vec![
    output((0, 0), (1920, 1080), (1920, 1080)),
    output((100, 100), (1920, 1080), (1920, 1080)),
])]
#[case::disconnected(vec![
    output((0, 0), (1920, 1080), (1920, 1080)),
    output((4000, 0), (1920, 1080), (1920, 1080)),
])]
#[case::overlapping_native(vec![
    output((0, 0), (1000, 1000), (1000, 1000)),
    output((1000, 0), (1000, 1000), (2000, 2000)),
    output((0, 1000), (1000, 1000), (1000, 1000)),
    output((1000, 1000), (1000, 1000), (1000, 1000)),
])]
// oracle: reference connected native desktop model; these logical neighbors would become two separated native pairs.
#[case::disconnected_native(vec![
    output((0, 0), (1000, 1000), (2000, 2000)),
    output((0, 1000), (1000, 1000), (2000, 2000)),
    output((1000, -800), (1000, 1000), (1000, 1000)),
    output((1000, -1800), (1000, 1000), (1000, 1000)),
])]
fn local_layout_rejects_invalid_geometry(#[case] outputs: Vec<OutputGeometry>) {
    assert!(MonitorLayout::from_local_outputs(&outputs, 0).is_err());
}

// oracle: reference advertised-rectangle identity model, independent of the server's array order.
#[test]
fn local_layout_restores_window_order_from_server_rectangles() {
    let expected = MonitorLayout::new(&[
        monitor(3840, 120, 5759, 1319, false),
        monitor(0, 0, 1919, 1079, true),
        monitor(1920, 0, 3839, 1079, false),
    ])
    .unwrap();
    let mut reported = MonitorLayout::new(&[
        monitor(0, 0, 1919, 1079, true),
        monitor(1920, 0, 3839, 1079, false),
        monitor(3840, 120, 5759, 1319, false),
    ])
    .unwrap();
    assert!(reported.reorder_to_match(&expected));
    assert_eq!(reported, expected);
}

// oracle: reference horizontal native-window model; 1888 + 1400 + 1600 = 4888 pixels wide, 1004 pixels high.
#[test]
fn native_windows_define_the_initial_monitor_topology() {
    let initial = MonitorLayout::new(&[
        monitor(0, 0, 1279, 799, true),
        monitor(1280, 0, 2559, 799, false),
        monitor(2560, 0, 3839, 799, false),
    ])
    .unwrap();
    let fitted = initial
        .fit_window_sizes(&[(1888, 1004), (1400, 900), (1600, 1000)])
        .unwrap();
    assert_eq!(fitted.size(), (4888, 1004));
    assert_eq!(
        fitted.to_client_monitor_data().unwrap().monitors,
        vec![
            monitor(0, 0, 1887, 1003, true),
            monitor(1888, 0, 3287, 899, false),
            monitor(3288, 0, 4887, 999, false),
        ]
    );
}

// oracle: reference horizontal tiling model; 8000 + 200 + 200 = 8400 pixels wide and 3000 pixels high.
#[test]
fn initial_sizes_are_validated_as_one_complete_layout() {
    let layout = MonitorLayout::new(&[
        monitor(0, 0, 8191, 999, true),
        monitor(8192, 0, 16383, 999, false),
        monitor(16384, 0, 24575, 999, false),
    ])
    .unwrap();
    let fitted = layout
        .fit_window_sizes(&[(8000, 3000), (200, 3000), (200, 3000)])
        .unwrap();
    assert_eq!(fitted.to_display_control(100).unwrap().desktop_size(), (8400, 3000));
}

// oracle: reference native-window model; every monitor requires a nonempty native content rectangle.
#[rstest::rstest]
#[case::missing_window(vec![(1888, 1004)])]
#[case::empty_width(vec![(1888, 1004), (0, 900)])]
#[case::empty_height(vec![(1888, 1004), (1400, 0)])]
fn initial_layout_requires_all_nonempty_windows(#[case] sizes: Vec<(u32, u32)>) {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1279, 799, true), monitor(1280, 0, 2559, 799, false)]).unwrap();
    assert!(layout.fit_window_sizes(&sizes).is_err());
}

// oracle: reference native-window resize continuity; a change after the startup snapshot remains requested when the first frame arrives.
#[test]
fn resize_during_connection_waits_for_the_first_matching_frame() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1887, 1003, true), monitor(1888, 0, 3775, 1003, false)]).unwrap();
    let mut state = MonitorResizeState::new(100);
    state.set_layout(&layout);
    state.resized(0, (1888, 1004)).unwrap();
    state.resized(1, (1888, 1004)).unwrap();
    assert!(!state.resized(1, (1400, 900)).unwrap());
    assert!(state.pending().is_none());
    state.observe_frame(layout.size());
    assert_eq!(state.pending().unwrap().desktop_size(), (3288, 1004));
    assert_eq!(state.pending().unwrap().monitors()[1].dimensions(), (1400, 900));
}

// oracle: fallback must not infer monitor topology from framebuffer dimensions alone
#[test]
fn fallback_clears_resize_intent_until_a_matching_layout_is_confirmed() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1279, 799, true), monitor(1280, 0, 2559, 799, false)]).unwrap();
    let mut state = MonitorResizeState::new(100);
    state.set_layout(&layout);
    state.resized(0, (1888, 1004)).unwrap();
    state.observe_frame(layout.size());
    assert!(state.resized(0, (1400, 900)).unwrap());
    state.observe_frame((2000, 900));
    assert!(state.pending().is_none());
    assert!(!state.resized(0, (1600, 900)).unwrap());
    state.observe_frame(layout.size());
    assert!(!state.resized(0, (1800, 900)).unwrap());
    state.set_layout(&layout);
    state.observe_frame(layout.size());
    assert!(state.resized(0, (2000, 900)).unwrap());
}

// oracle: MonitorLayoutRejected keeps the applied layout; newer unsent changes remain local intent
#[test]
fn rejected_layout_preserves_only_newer_unsent_monitor_changes() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1279, 799, true), monitor(1280, 0, 2559, 799, false)]).unwrap();
    let mut state = MonitorResizeState::new(100);
    state.set_layout(&layout);
    state.resized(0, (1888, 1004)).unwrap();
    state.resized(1, (1888, 1004)).unwrap();
    state.observe_frame(layout.size());
    state.resized(0, (1400, 1004)).unwrap();
    state.submitted();
    state.resized(1, (1600, 900)).unwrap();
    state.rejected().unwrap();
    let request = state.pending().unwrap();
    assert_eq!(request.monitors()[0].dimensions(), (1280, 800));
    assert_eq!(request.monitors()[1].dimensions(), (1600, 900));
    assert_eq!(request.monitors()[1].position(), Some((1280, 0)));
}

// oracle: resizing one monitor must preserve all untouched monitor resolutions
#[test]
fn odd_sibling_size_is_preserved_instead_of_silently_shrunk() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1280, 799, true), monitor(1281, 0, 2561, 799, false)]).unwrap();
    let resized = layout.resize_horizontal(0, (1400, 800)).unwrap();
    assert_eq!(resized.monitors()[1].width, 1281);
    assert!(resized.to_display_control(100).is_err());
}

// oracle: signed primary-relative monitor coordinates and the requested horizontal resize behavior
#[test]
fn resizing_a_monitor_left_of_primary_keeps_primary_at_zero() {
    let layout = MonitorLayout::new(&[
        monitor(0, 0, 1279, 799, true),
        monitor(-1280, 0, -1, 799, false),
        monitor(1280, 0, 2559, 799, false),
    ])
    .unwrap();
    let request = layout
        .resize_horizontal(1, (1000, 800))
        .unwrap()
        .to_display_control(100)
        .unwrap();
    assert_eq!(
        request
            .monitors()
            .iter()
            .map(|entry| entry.position())
            .collect::<Vec<_>>(),
        vec![Some((0, 0)), Some((-1000, 0)), Some((1280, 0))]
    );
    assert_eq!(request.desktop_size(), (3560, 800));
}

// oracle: a later monitor resize must not discard a preceding request when the server repeats the old layout
#[test]
fn resize_intent_survives_old_layouts_and_coalesces_across_monitors() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1279, 799, true), monitor(1280, 0, 2559, 799, false)]).unwrap();
    let mut state = MonitorResizeState::new(100);
    state.set_layout(&layout);
    assert!(!state.resized(0, (1888, 1004)).unwrap());
    assert!(!state.resized(1, (1888, 1004)).unwrap());
    state.observe_frame(layout.size());
    assert!(state.resized(0, (1400, 1004)).unwrap());
    state.submitted();
    state.set_layout(&layout);
    assert!(state.resized(1, (1600, 900)).unwrap());
    let request = state.pending().unwrap();
    assert_eq!(request.monitors()[0].dimensions(), (1400, 1004));
    assert_eq!(request.monitors()[1].dimensions(), (1600, 900));
    assert_eq!(request.monitors()[1].position(), Some((1400, 0)));
    state.submitted();
    assert!(!state.resized(1, (1601, 900)).unwrap());
    assert!(state.pending().is_none());
}

// oracle: compositor-created windows must not request another startup layout even after an early desktop frame
#[test]
fn first_native_sizes_after_a_frame_remain_local_configuration() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1279, 799, true), monitor(1280, 0, 2559, 799, false)]).unwrap();
    let mut state = MonitorResizeState::new(100);
    state.set_layout(&layout);
    state.observe_frame(layout.size());
    assert!(!state.resized(0, (1888, 1004)).unwrap());
    assert!(!state.resized(1, (1888, 1004)).unwrap());
    assert!(state.pending().is_none());
}

// oracle: dynamic requests currently support the horizontal virtual layout selected by --monitor-count
#[test]
fn resizing_a_vertical_server_layout_keeps_local_scaling() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1279, 799, true), monitor(0, 800, 1279, 1599, false)]).unwrap();
    assert!(layout.resize_horizontal(0, (1400, 800)).is_err());
}

// oracle: the existing viewer/compositor framebuffer limit is 256 MiB
#[test]
fn resizing_a_monitor_cannot_exceed_the_framebuffer_budget() {
    let layout = MonitorLayout::new(&[
        monitor(0, 0, 1279, 799, true),
        monitor(1280, 0, 2559, 799, false),
        monitor(2560, 0, 3839, 799, false),
    ])
    .unwrap();
    assert!(layout.resize_horizontal(0, (8192, 8192)).is_err());
}

// oracle: requested behavior and startup trace, initial compositor sizing must not trigger a remote resize
#[test]
fn initial_window_sizing_waits_for_a_later_local_resize() {
    let layout = MonitorLayout::new(&[monitor(0, 0, 1279, 799, true), monitor(1280, 0, 2559, 799, false)]).unwrap();
    let mut state = MonitorResizeState::new(100);
    assert!(!state.resized(0, (1888, 1004)).unwrap());
    state.set_layout(&layout);
    state.observe_frame(layout.size());
    assert!(state.resized(0, (1400, 1004)).unwrap());
    let request = state.pending().unwrap();
    assert_eq!(request.monitors()[0].dimensions(), (1400, 1004));
    assert_eq!(request.monitors()[1].dimensions(), (1280, 800));
    assert_eq!(request.monitors()[1].position(), Some((1400, 0)));
}

// oracle: requested behavior, resizing one horizontal monitor preserves its siblings and their adjacency
#[test]
fn resizing_one_monitor_updates_the_complete_horizontal_layout() {
    let layout = MonitorLayout::new(&[
        monitor(0, 0, 1279, 799, true),
        monitor(1280, 0, 2559, 799, false),
        monitor(2560, 0, 3839, 799, false),
    ])
    .unwrap();
    let resized = layout.resize_horizontal(1, (1401, 900)).unwrap();
    let request = resized.to_display_control(150).unwrap();
    assert_eq!(resized.size(), (3960, 900));
    assert_eq!(request.desktop_size(), (3960, 900));
    assert_eq!(
        request
            .monitors()
            .iter()
            .map(|entry| (
                entry.is_primary(),
                entry.position(),
                entry.dimensions(),
                entry.desktop_scale_factor()
            ))
            .collect::<Vec<_>>(),
        vec![
            (true, Some((0, 0)), (1280, 800), Some(150)),
            (false, Some((1280, 0)), (1400, 900), Some(150)),
            (false, Some((2680, 0)), (1280, 800), Some(150)),
        ]
    );
}

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

fn output(position: (i32, i32), logical_size: (u32, u32), native_size: (u32, u32)) -> OutputGeometry {
    OutputGeometry {
        position,
        logical_size,
        native_size,
    }
}
