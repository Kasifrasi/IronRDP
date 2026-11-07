#![expect(
    unused_crate_dependencies,
    reason = "dependencies are shared by the library, binary and integration tests"
)]

use core::ffi::CStr;
use core::num::NonZeroU16;

use ironrdp_client::rdp::DesktopUpdate;
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_viewer_sdl::desktop::{Desktop, Layout, Viewport};
use ironrdp_viewer_sdl::renderer::MonitorRenderer;
use sdl3::pixels::PixelFormat;
use sdl3::render::{WindowCanvas, create_renderer};

#[test]
fn non_first_primary_uses_primary_relative_coordinates() -> anyhow::Result<()> {
    // oracle: spec MS-RDPBCGR §2.2.1.3.6.1: primary origin and inclusive lower-right coordinates.
    let layout = Layout::horizontal(3, 1280, 800, 1)?;
    let monitors = layout.client_monitor_data().monitors;
    assert_eq!(
        monitors,
        vec![
            Monitor {
                left: -1280,
                top: 0,
                right: -1,
                bottom: 799,
                flags: MonitorFlags::empty()
            },
            Monitor {
                left: 0,
                top: 0,
                right: 1279,
                bottom: 799,
                flags: MonitorFlags::PRIMARY
            },
            Monitor {
                left: 1280,
                top: 0,
                right: 2559,
                bottom: 799,
                flags: MonitorFlags::empty()
            },
        ]
    );
    Ok(())
}

#[test]
fn server_monitors_above_and_left_of_primary_map_to_framebuffer() -> anyhow::Result<()> {
    // oracle: spec MS-RDPBCGR §2.2.12.1 and §2.2.1.3.6.1: signed primary-relative positions.
    let layout = Layout::from_server(&[
        Monitor {
            left: 0,
            top: 0,
            right: 799,
            bottom: 599,
            flags: MonitorFlags::PRIMARY,
        },
        Monitor {
            left: -640,
            top: -480,
            right: -1,
            bottom: -1,
            flags: MonitorFlags::empty(),
        },
    ])?;
    assert_eq!(layout.size(), (1440, 1080));
    assert_eq!(
        layout.monitors(),
        &[
            Viewport {
                x: 640,
                y: 480,
                width: 800,
                height: 600
            },
            Viewport {
                x: 0,
                y: 0,
                width: 640,
                height: 480
            },
        ]
    );
    Ok(())
}

#[test]
fn virtual_desktop_cannot_exceed_rdp_coordinate_limits() {
    // oracle: spec MS-RDPBCGR §2.2.1.3.6: virtual desktop extent is at most 32766 per axis.
    assert!(Layout::horizontal(4, 8192, 200, 0).is_err());
    assert!(Layout::horizontal(17, 200, 200, 0).is_err());
}

#[test]
fn two_sdl_windows_retain_dirty_pixels_and_restore_textures() -> anyhow::Result<()> {
    // oracle: spec SDL3 SDL_UpdateTexture: only the specified rectangle is updated.
    // oracle: spec SDL3 SDL_BLENDMODE_NONE: source RGB replaces destination RGB regardless of alpha.
    // All expected colors below are hand-selected inputs with a zero alpha byte.
    sdl3::hint::set("SDL_VIDEO_DRIVER", "dummy");
    let sdl = sdl3::init()?;
    let video = sdl.video()?;
    let mut left = canvas(&video, c"software", 4, 4)?;
    let mut right = canvas(&video, c"software", 4, 4)?;
    assert_ne!(left.window().id(), right.window().id());
    let left_creator = left.texture_creator();
    let right_creator = right.texture_creator();
    let left_view = Viewport {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    };
    let right_view = Viewport {
        x: 4,
        y: 0,
        width: 4,
        height: 4,
    };
    let mut left_renderer = MonitorRenderer::new(&mut left, &left_creator, left_view)?;
    let mut right_renderer = MonitorRenderer::new(&mut right, &right_creator, right_view)?;
    let mut desktop = Desktop::default();
    let full = update((8, 4), (0, 0, 7, 3), vec![0x00_FF_00_00; 32]);
    let region = desktop.apply(&full)?;
    left_renderer.upload(&mut left, &desktop, region)?;
    right_renderer.upload(&mut right, &desktop, region)?;
    left_renderer.present(&mut left)?;
    right_renderer.present(&mut right)?;

    left.window_mut().set_size(8, 8)?;
    left_renderer.invalidate();
    left_renderer.draw(&mut left)?;
    assert_eq!(pixel(&left, 7, 7)?, [255, 0, 0]);
    left.window_mut().set_size(4, 4)?;

    // This update crosses the monitor seam and uses the full desktop's stride on both renderers.
    let changed = update(
        (8, 4),
        (3, 1, 4, 2),
        vec![0x00_00_FF_00, 0x00_00_00_FF, 0x00_FF_FF_FF, 0x00_00_FF_FF],
    );
    let region = desktop.apply(&changed)?;
    left_renderer.upload(&mut left, &desktop, region)?;
    right_renderer.upload(&mut right, &desktop, region)?;
    left_renderer.draw(&mut left)?;
    right_renderer.draw(&mut right)?;
    assert_eq!(pixel(&left, 3, 1)?, [0, 255, 0]);
    assert_eq!(pixel(&left, 3, 2)?, [255, 255, 255]);
    assert_eq!(pixel(&right, 0, 1)?, [0, 0, 255]);
    assert_eq!(pixel(&right, 0, 2)?, [0, 255, 255]);
    assert_eq!(pixel(&left, 0, 0)?, [255, 0, 0]);
    assert_eq!(pixel(&right, 3, 3)?, [255, 0, 0]);
    left_renderer.present(&mut left)?;
    right_renderer.present(&mut right)?;

    // Recreating textures simulates renderer loss and must recover every retained pixel.
    left_renderer = MonitorRenderer::new(&mut left, &left_creator, left_view)?;
    left_renderer.upload(&mut left, &desktop, left_view)?;
    left_renderer.draw(&mut left)?;
    assert_eq!(pixel(&left, 3, 1)?, [0, 255, 0]);
    assert_eq!(pixel(&left, 0, 0)?, [255, 0, 0]);

    // A change confined to the right monitor must leave the left monitor's target alone.
    let outside = update((8, 4), (7, 3, 7, 3), vec![0x00_FF_FF_00]);
    let region = desktop.apply(&outside)?;
    left_renderer.upload(&mut left, &desktop, region)?;
    right_renderer.upload(&mut right, &desktop, region)?;
    left_renderer.draw(&mut left)?;
    right_renderer.draw(&mut right)?;
    assert_eq!(pixel(&left, 3, 1)?, [0, 255, 0]);
    assert_eq!(pixel(&right, 3, 3)?, [255, 255, 0]);
    Ok(())
}

fn canvas(video: &sdl3::VideoSubsystem, driver: &CStr, width: u32, height: u32) -> anyhow::Result<WindowCanvas> {
    let window = video.window("SDL video test", width, height).hidden().build()?;
    Ok(create_renderer(window, Some(driver))?)
}

fn update(size: (u16, u16), rect: (u16, u16, u16, u16), pixels: Vec<u32>) -> DesktopUpdate {
    DesktopUpdate::new(
        pixels,
        NonZeroU16::new(size.0).expect("nonzero test width"),
        NonZeroU16::new(size.1).expect("nonzero test height"),
        InclusiveRectangle {
            left: rect.0,
            top: rect.1,
            right: rect.2,
            bottom: rect.3,
        },
    )
    .expect("valid test region")
}

fn pixel(canvas: &WindowCanvas, x: usize, y: usize) -> anyhow::Result<[u8; 3]> {
    let surface = canvas.read_pixels(None)?.convert_format(PixelFormat::RGB24)?;
    let pitch = usize::try_from(surface.pitch())?;
    Ok(surface.with_lock(|bytes| {
        let offset = y * pitch + x * 3;
        [bytes[offset], bytes[offset + 1], bytes[offset + 2]]
    }))
}
