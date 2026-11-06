use ironrdp::pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_viewer::monitor::{MonitorLayout, MonitorViewport};

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
            MonitorViewport { x: 4, y: 2, width: 4, height: 2, primary: true },
            MonitorViewport { x: 0, y: 2, width: 4, height: 2, primary: false },
            MonitorViewport { x: 4, y: 0, width: 4, height: 2, primary: false },
        ]
    );
}

fn monitor(left: i32, top: i32, right: i32, bottom: i32, primary: bool) -> Monitor {
    Monitor {
        left,
        top,
        right,
        bottom,
        flags: if primary { MonitorFlags::PRIMARY } else { MonitorFlags::empty() },
    }
}
