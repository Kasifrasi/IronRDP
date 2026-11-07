#![expect(
    unused_crate_dependencies,
    reason = "dependencies are shared by the binary, library and tests"
)]

use ironrdp_client::rdp::{RdpInputEvent, RdpInputSender};
use ironrdp_input::MousePosition;
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp_pdu::input::mouse::PointerFlags;
use ironrdp_viewer_sdl::desktop::{Layout, Viewport};
use ironrdp_viewer_sdl::input::Input;
use sdl3::event::Event;
use sdl3::keyboard::{Mod, Scancode};
use sdl3::mouse::{MouseButton, MouseState, MouseWheelDirection};

#[test]
fn hidpi_pointer_uses_logical_size_and_remote_monitor_offset() {
    // oracle: regression user report: mouse must operate the displayed monitor, including fractional scaling.
    let monitor = Viewport {
        x: 3840,
        y: 0,
        width: 1920,
        height: 1200,
    };
    assert_eq!(
        monitor.position((1536, 960), (768.0, 480.0)),
        Some(MousePosition { x: 4800, y: 600 })
    );
    assert_eq!(
        monitor.position((1536, 960), (-10.0, 1000.0)),
        Some(MousePosition { x: 3840, y: 1199 })
    );
    assert_eq!(monitor.position((0, 960), (0.0, 0.0)), None);
}

#[test]
fn native_monitor_sizes_survive_resize_and_primary_translation() -> anyhow::Result<()> {
    // oracle: regression user report: use the actual monitor resolution instead of stretching 1280x800.
    // oracle: spec MS-RDPEDISP §2.2.2.2.1: primary at (0,0), even width and 200..8192 size limits.
    let layout = Layout::from_sizes(&[(1920, 1080), (1920, 1080), (1920, 1200)], 1)?;
    assert_eq!(layout.size(), (5760, 1200));
    let monitors = layout.client_monitor_data().monitors;
    assert_eq!((monitors[0].left, monitors[0].right), (-1920, -1));
    assert_eq!((monitors[2].right, monitors[2].bottom), (3839, 1199));
    let request = layout.display_control()?;
    assert_eq!(request.monitors()[0].position(), Some((-1920, 0)));
    assert_eq!(request.monitors()[1].dimensions(), (1920, 1080));
    assert_eq!(request.monitors()[2].dimensions(), (1920, 1200));
    let resized = Layout::from_sizes(&[(1920, 1080), (1537, 960), (1920, 1200)], 1)?;
    assert_eq!(resized.size(), (5376, 1200));
    assert_eq!(resized.monitors()[1].width, 1536);
    assert_eq!(resized.monitors()[2].x, 3456);
    Ok(())
}

#[test]
fn key_release_survives_full_client_queue() -> anyhow::Result<()> {
    // oracle: spec MS-RDPBCGR §2.2.8.1.2.2.1: make and release events describe the same key.
    let (sender, mut receiver) = RdpInputSender::channel(1);
    let mut input = Input::default();
    input.event(&key(true, Scancode::A), viewport(), (1920, 1080))?;
    input.flush(&sender)?;
    input.event(&key(false, Scancode::A), viewport(), (1920, 1080))?;
    input.flush(&sender)?;
    assert_key(receiver.try_recv()?, KeyboardFlags::empty(), 0x1E);
    input.flush(&sender)?;
    assert_key(receiver.try_recv()?, KeyboardFlags::RELEASE, 0x1E);
    Ok(())
}

#[test]
fn focus_loss_releases_modifiers_and_drag_buttons() -> anyhow::Result<()> {
    // oracle: regression user report: remote controls must remain usable when switching monitor windows.
    // oracle: spec MS-RDPBCGR §2.2.8.1.2.2.1: right Ctrl uses the extended scancode flag.
    let (sender, mut receiver) = RdpInputSender::channel(8);
    let mut input = Input::default();
    input.event(&key(true, Scancode::RCtrl), viewport(), (1920, 1080))?;
    input.event(
        &Event::MouseButtonDown {
            timestamp: 0,
            window_id: 1,
            which: 0,
            mouse_btn: MouseButton::Left,
            clicks: 1,
            x: 0.0,
            y: 0.0,
        },
        viewport(),
        (1920, 1080),
    )?;
    input.release_all();
    input.flush(&sender)?;
    assert_key(receiver.try_recv()?, KeyboardFlags::EXTENDED, 0x1D);
    let RdpInputEvent::FastPath(pressed) = receiver.try_recv()? else {
        anyhow::bail!("expected mouse press")
    };
    assert!(
        matches!(&pressed[0], FastPathInputEvent::MouseEvent(mouse) if mouse.flags == PointerFlags::LEFT_BUTTON | PointerFlags::DOWN)
    );
    let RdpInputEvent::FastPath(released) = receiver.try_recv()? else {
        anyhow::bail!("expected input release")
    };
    assert!(matches!(&released[0], FastPathInputEvent::MouseEvent(mouse) if mouse.flags == PointerFlags::LEFT_BUTTON));
    assert_eq!(
        released[1],
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED | KeyboardFlags::RELEASE, 0x1D)
    );
    Ok(())
}

#[test]
fn click_uses_its_own_position_after_motion_coalescing() -> anyhow::Result<()> {
    // oracle: regression user report: clicking on another monitor must not hit a stale cursor position.
    let (sender, mut receiver) = RdpInputSender::channel(8);
    let mut input = Input::default();
    let viewport = Viewport {
        x: 1920,
        y: 0,
        width: 1920,
        height: 1080,
    };
    input.event(
        &Event::MouseMotion {
            timestamp: 0,
            window_id: 1,
            which: 0,
            mousestate: MouseState::from_sdl_state(0),
            x: 50.0,
            y: 50.0,
            xrel: 0.0,
            yrel: 0.0,
        },
        viewport,
        (960, 540),
    )?;
    input.event(
        &Event::MouseButtonDown {
            timestamp: 0,
            window_id: 1,
            which: 0,
            mouse_btn: MouseButton::Right,
            clicks: 1,
            x: 480.0,
            y: 270.0,
        },
        viewport,
        (960, 540),
    )?;
    input.flush(&sender)?;
    let RdpInputEvent::FastPath(motion) = receiver.try_recv()? else {
        anyhow::bail!("expected motion")
    };
    assert!(
        matches!(&motion[0], FastPathInputEvent::MouseEvent(mouse) if (mouse.x_position, mouse.y_position) == (2880, 540))
    );
    let RdpInputEvent::FastPath(click) = receiver.try_recv()? else {
        anyhow::bail!("expected click")
    };
    assert!(
        matches!(&click[0], FastPathInputEvent::MouseEvent(mouse) if mouse.flags == PointerFlags::RIGHT_BUTTON | PointerFlags::DOWN && (mouse.x_position, mouse.y_position) == (2880, 540))
    );
    Ok(())
}

#[test]
fn wheel_follows_sdl_scroll_direction() -> anyhow::Result<()> {
    // oracle: spec MS-RDPBCGR §2.2.8.1.1.3.1.1.3: positive wheel rotation is forward.
    let (sender, mut receiver) = RdpInputSender::channel(4);
    let mut input = Input::default();
    input.event(
        &Event::MouseWheel {
            timestamp: 0,
            window_id: 1,
            which: 0,
            x: 0.0,
            y: 1.0,
            direction: MouseWheelDirection::Flipped,
            mouse_x: 0.0,
            mouse_y: 0.0,
            integer_x: 0,
            integer_y: 1,
        },
        viewport(),
        (1920, 1080),
    )?;
    input.flush(&sender)?;
    let RdpInputEvent::FastPath(events) = receiver.try_recv()? else {
        anyhow::bail!("expected wheel")
    };
    assert!(
        matches!(&events[0], FastPathInputEvent::MouseEvent(mouse) if mouse.flags == PointerFlags::VERTICAL_WHEEL && mouse.number_of_wheel_rotation_units == -120)
    );
    Ok(())
}

#[test]
fn pause_uses_the_required_four_event_sequence() -> anyhow::Result<()> {
    // oracle: spec MS-RDPBCGR §2.2.8.1.2.2.1: Pause is E1 Ctrl make, NumLock make, E1 Ctrl break, NumLock break.
    let (sender, mut receiver) = RdpInputSender::channel(1);
    let mut input = Input::default();
    input.event(&key(true, Scancode::Pause), viewport(), (1920, 1080))?;
    input.flush(&sender)?;
    let RdpInputEvent::FastPath(events) = receiver.try_recv()? else {
        anyhow::bail!("expected Pause")
    };
    assert_eq!(
        events.as_slice(),
        &[
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED1, 0x1D),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 0x45),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::EXTENDED1 | KeyboardFlags::RELEASE, 0x1D),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 0x45),
        ]
    );
    Ok(())
}

fn viewport() -> Viewport {
    Viewport {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    }
}
fn key(down: bool, scancode: Scancode) -> Event {
    if down {
        Event::KeyDown {
            timestamp: 0,
            window_id: 1,
            which: 0,
            keycode: None,
            scancode: Some(scancode),
            keymod: Mod::NOMOD,
            repeat: false,
            raw: 0,
        }
    } else {
        Event::KeyUp {
            timestamp: 0,
            window_id: 1,
            which: 0,
            keycode: None,
            scancode: Some(scancode),
            keymod: Mod::NOMOD,
            repeat: false,
            raw: 0,
        }
    }
}
fn assert_key(event: RdpInputEvent, flags: KeyboardFlags, scancode: u8) {
    assert!(
        matches!(event, RdpInputEvent::FastPath(events) if events.as_slice() == [FastPathInputEvent::KeyboardEvent(flags, scancode)])
    );
}
