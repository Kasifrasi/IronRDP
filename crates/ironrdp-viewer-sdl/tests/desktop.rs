#![expect(
    unused_crate_dependencies,
    reason = "dependencies are shared by the library, binary and integration tests"
)]

use core::num::NonZeroU16;

use ironrdp_client::rdp::DesktopUpdate;
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_viewer_sdl::desktop::{Desktop, Viewport};

#[test]
fn incomplete_desktop_batch_stays_hidden_beyond_sdl_event_limit() -> anyhow::Result<()> {
    // oracle: regression mixed old/new desktop when SDL presents after 64 output events.
    let mut desktop = Desktop::default();
    assert_eq!(
        desktop.push(update((4096, 1), (0, 0, 4095, 0), vec![0x00_FF_00_00; 4096]))?,
        Some(vec![Viewport {
            x: 0,
            y: 0,
            width: 4096,
            height: 1,
        }])
    );
    assert!(desktop.pixels().iter().all(|&pixel| pixel == 0x00_FF_00_00));

    for x in 0..65 {
        let patch = update((4096, 1), (x, 0, x, 0), vec![0x00_00_FF_00]).with_batch_end(false);
        assert_eq!(desktop.push(patch)?, None);
        assert!(
            desktop.pixels().iter().all(|&pixel| pixel == 0x00_FF_00_00),
            "incomplete update at x={x}"
        );
    }

    let changed = desktop.push(update((4096, 1), (64, 0, 64, 0), vec![0x00_00_00_FF]))?;
    let expected_damage = (0..65)
        .chain([64])
        .map(|x| Viewport {
            x,
            y: 0,
            width: 1,
            height: 1,
        })
        .collect();
    assert_eq!(changed, Some(expected_damage));
    assert!(desktop.pixels()[..64].iter().all(|&pixel| pixel == 0x00_00_FF_00));
    assert_eq!(desktop.pixels()[64], 0x00_00_00_FF);
    assert!(desktop.pixels()[65..].iter().all(|&pixel| pixel == 0x00_FF_00_00));
    Ok(())
}

#[test]
fn clearing_desktop_discards_incomplete_connection_pixels() -> anyhow::Result<()> {
    // oracle: regression incomplete desktop patches must not survive connection replacement.
    let mut desktop = Desktop::default();
    let _ = desktop.push(update((4096, 1), (0, 0, 4095, 0), vec![0x00_FF_00_00; 4096]))?;
    let patch = update((4096, 1), (0, 0, 0, 0), vec![0x00_00_FF_00]).with_batch_end(false);
    assert_eq!(desktop.push(patch)?, None);

    desktop.clear();

    assert_eq!(
        desktop.push(update((4096, 1), (1, 0, 1, 0), vec![0x00_00_00_FF]))?,
        Some(vec![Viewport {
            x: 1,
            y: 0,
            width: 1,
            height: 1,
        }])
    );
    assert_eq!(desktop.pixels()[1], 0x00_00_00_FF);
    Ok(())
}

#[test]
fn incomplete_desktop_resize_retains_visible_geometry() -> anyhow::Result<()> {
    // oracle: regression incomplete desktop batches must not resize the visible framebuffer.
    // oracle: regression a new extent must discard unfinished patches from the old extent.
    let mut desktop = Desktop::default();
    let _ = desktop.push(update((4096, 1), (0, 0, 4095, 0), vec![0x00_FF_00_00; 4096]))?;
    let stale = update((4096, 1), (0, 0, 0, 0), vec![0x00_00_FF_00]).with_batch_end(false);
    assert_eq!(desktop.push(stale)?, None);
    let patch = update((128, 32), (0, 0, 127, 0), vec![0x00_00_FF_00; 128]).with_batch_end(false);
    assert_eq!(desktop.push(patch)?, None);
    assert_eq!(desktop.size(), (4096, 1));
    assert!(desktop.pixels().iter().all(|&pixel| pixel == 0x00_FF_00_00));

    assert_eq!(
        desktop.push(update((128, 32), (0, 1, 127, 1), vec![0x00_00_00_FF; 128]))?,
        Some(vec![
            Viewport {
                x: 0,
                y: 0,
                width: 128,
                height: 1,
            },
            Viewport {
                x: 0,
                y: 1,
                width: 128,
                height: 1,
            },
        ])
    );
    assert_eq!(desktop.size(), (128, 32));
    assert!(desktop.pixels()[..128].iter().all(|&pixel| pixel == 0x00_00_FF_00));
    assert!(desktop.pixels()[128..256].iter().all(|&pixel| pixel == 0x00_00_00_FF));
    Ok(())
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
    .expect("valid test region")
}
