use core::num::NonZeroU16;
use core::time::Duration;

use ironrdp_client::output_channel::output_channel;
use ironrdp_client::rdp::{DesktopUpdate, DesktopUpdateBatch, DisplayResizeFallbackReason, RdpOutputEvent};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_viewer::events::{OutputEventBatch, forward_output};
use ironrdp_viewer::monitor::DesktopFrame;
use tokio::sync::mpsc;

// oracle: mutant forwarding desktop deltas into an unbounded GUI queue, dropping them or changing their order.
#[tokio::test(start_paused = true)]
async fn gui_forwarding_backpressures_without_losing_desktop_deltas() {
    let (sender, receiver) = output_channel(4);
    let (gui_sender, mut gui_receiver) = mpsc::unbounded_channel();
    let forwarder = tokio::spawn(forward_output(receiver, 2, move |event| gui_sender.send(event).is_ok()));
    for pixel in 1..=4 {
        sender
            .send(RdpOutputEvent::DesktopUpdate(
                DesktopUpdate::new(
                    vec![pixel],
                    NonZeroU16::MIN,
                    NonZeroU16::MIN,
                    InclusiveRectangle {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    },
                )
                .unwrap(),
            ))
            .await
            .unwrap();
    }
    drop(sender);

    let first = gui_receiver.recv().await.unwrap();
    let second = gui_receiver.recv().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), gui_receiver.recv())
            .await
            .is_err()
    );
    for (expected, event) in [(1, first), (2, second)] {
        let RdpOutputEvent::DesktopUpdate(update) = event.event() else {
            panic!("expected a desktop delta");
        };
        assert_eq!(update.buffer(), [expected]);
        drop(event);
        let next = gui_receiver.recv().await.unwrap();
        let RdpOutputEvent::DesktopUpdate(update) = next.event() else {
            panic!("expected a desktop delta");
        };
        assert_eq!(update.buffer(), [expected + 2]);
    }
    forwarder.await.unwrap();
    assert!(gui_receiver.recv().await.is_none());
}

// oracle: mutant retaining queue permits after the GUI closes, blocking the output forwarder.
#[tokio::test(start_paused = true)]
async fn gui_forwarding_stops_when_the_consumer_closes() {
    let (sender, receiver) = output_channel(2);
    let (gui_sender, mut gui_receiver) = mpsc::unbounded_channel();
    let forwarder = tokio::spawn(forward_output(receiver, 1, move |event| gui_sender.send(event).is_ok()));
    sender.send(RdpOutputEvent::Connected).await.unwrap();
    sender.send(RdpOutputEvent::MonitorLayoutRejected).await.unwrap();
    let pending = gui_receiver.recv().await.unwrap();
    drop(gui_receiver);
    drop(pending);
    tokio::time::timeout(Duration::from_secs(1), forwarder)
        .await
        .unwrap()
        .unwrap();
}

// oracle: regression user report of mixed old/new desktop rectangles; completed batches must become visible together.
#[tokio::test(start_paused = true)]
async fn desktop_batch_stays_invisible_while_more_than_64_patches_cross_a_capacity_one_bridge() {
    let width = NonZeroU16::new(4096).unwrap();
    let mut visible = DesktopFrame::default();
    visible
        .apply(
            &DesktopUpdate::new(
                vec![7; usize::from(width.get())],
                width,
                NonZeroU16::MIN,
                InclusiveRectangle {
                    left: 0,
                    top: 0,
                    right: width.get() - 1,
                    bottom: 0,
                },
            )
            .unwrap(),
        )
        .unwrap();
    let (sender, receiver) = output_channel(1);
    let (gui_sender, mut gui_receiver) = mpsc::unbounded_channel();
    let forwarder = tokio::spawn(forward_output(receiver, 1, move |event| gui_sender.send(event).is_ok()));
    let producer = tokio::spawn(async move {
        for x in 0..65 {
            sender
                .send(RdpOutputEvent::DesktopUpdate(
                    DesktopUpdate::new(
                        vec![100 + u32::from(x)],
                        width,
                        NonZeroU16::MIN,
                        InclusiveRectangle {
                            left: x,
                            top: 0,
                            right: x,
                            bottom: 0,
                        },
                    )
                    .unwrap()
                    .with_batch_end(false),
                ))
                .await
                .unwrap();
        }
        sender
            .send(RdpOutputEvent::DesktopUpdate(
                DesktopUpdate::new(
                    vec![900, 901, 902, 903],
                    width,
                    NonZeroU16::MIN,
                    InclusiveRectangle {
                        left: 63,
                        top: 0,
                        right: 66,
                        bottom: 0,
                    },
                )
                .unwrap(),
            ))
            .await
            .unwrap();
    });

    let mut pending = DesktopUpdateBatch::default();
    for _ in 0..65 {
        let event = tokio::time::timeout(Duration::from_secs(1), gui_receiver.recv())
            .await
            .expect("staging a payload must release the bridge permit")
            .unwrap();
        assert!(event.into_batch(&mut pending).is_none());
        assert!(visible.pixels().iter().all(|&pixel| pixel == 7));
    }
    let event = tokio::time::timeout(Duration::from_secs(1), gui_receiver.recv())
        .await
        .expect("the terminal patch must pass through the bridge")
        .unwrap();
    let Some(OutputEventBatch::DesktopUpdates(updates)) = event.into_batch(&mut pending) else {
        panic!("expected a completed desktop batch");
    };
    for update in updates {
        visible.apply(&update).unwrap();
    }

    assert_eq!(&visible.pixels()[..63], &(100..163).collect::<Vec<_>>());
    assert_eq!(&visible.pixels()[63..67], [900, 901, 902, 903]);
    assert!(visible.pixels()[67..].iter().all(|&pixel| pixel == 7));
    producer.await.unwrap();
    forwarder.await.unwrap();
    assert!(gui_receiver.recv().await.is_none());
}

// oracle: regression user report of mixed old/new desktop rectangles; incomplete batches cannot cross connections.
#[rstest::rstest]
#[case::connected(RdpOutputEvent::Connected)]
#[case::reconnected(RdpOutputEvent::AutoReconnected)]
#[case::reconnecting(RdpOutputEvent::AutoReconnecting {
    disconnect_reason: 0,
    attempt: 1,
    maximum_attempts: 1,
    response: tokio::sync::oneshot::channel().0,
})]
#[case::redirected(RdpOutputEvent::ServerRedirected { target: "replacement".to_owned() })]
#[case::resize_fallback(RdpOutputEvent::DisplayResizeFallback(DisplayResizeFallbackReason::DisplayControlUnavailable))]
#[case::failed(RdpOutputEvent::ConnectionFailure(ironrdp_connector::general_err!("test connection failure")))]
#[case::terminated(RdpOutputEvent::Terminated(Ok(ironrdp::session::GracefulDisconnectReason::UserInitiated)))]
#[case::image(RdpOutputEvent::Image {
    buffer: vec![7, 7],
    width: NonZeroU16::new(2).unwrap(),
    height: NonZeroU16::MIN,
})]
#[tokio::test(start_paused = true)]
async fn connection_and_snapshot_events_discard_incomplete_desktop_batches(#[case] event: RdpOutputEvent) {
    let (sender, receiver) = output_channel(4);
    let (gui_sender, mut gui_receiver) = mpsc::unbounded_channel();
    let forwarder = tokio::spawn(forward_output(receiver, 1, move |event| gui_sender.send(event).is_ok()));
    let mut pending = DesktopUpdateBatch::default();
    sender
        .send(RdpOutputEvent::DesktopUpdate(pixel_update(0, 8).with_batch_end(false)))
        .await
        .unwrap();
    assert!(gui_receiver.recv().await.unwrap().into_batch(&mut pending).is_none());
    sender.send(event).await.unwrap();
    assert!(matches!(
        gui_receiver.recv().await.unwrap().into_batch(&mut pending),
        Some(OutputEventBatch::Event(_))
    ));
    sender
        .send(RdpOutputEvent::DesktopUpdate(pixel_update(1, 9)))
        .await
        .unwrap();
    drop(sender);
    let Some(OutputEventBatch::DesktopUpdates(updates)) = gui_receiver.recv().await.unwrap().into_batch(&mut pending)
    else {
        panic!("a standalone update must complete immediately");
    };
    let mut visible = DesktopFrame::default();
    for update in updates {
        visible.apply(&update).unwrap();
    }
    assert_eq!(visible.pixels(), [0, 9]);
    forwarder.await.unwrap();
}

// oracle: regression user report of mixed old/new desktop rectangles; pointer events must preserve pending pixels.
#[tokio::test(start_paused = true)]
async fn pointer_events_remain_immediate_while_a_desktop_batch_is_pending() {
    let (sender, receiver) = output_channel(4);
    let (gui_sender, mut gui_receiver) = mpsc::unbounded_channel();
    let forwarder = tokio::spawn(forward_output(receiver, 1, move |event| gui_sender.send(event).is_ok()));
    let mut pending = DesktopUpdateBatch::default();
    sender
        .send(RdpOutputEvent::DesktopUpdate(pixel_update(0, 8).with_batch_end(false)))
        .await
        .unwrap();
    assert!(gui_receiver.recv().await.unwrap().into_batch(&mut pending).is_none());
    sender
        .send(RdpOutputEvent::PointerPosition { x: 1, y: 0 })
        .await
        .unwrap();
    assert!(matches!(
        gui_receiver.recv().await.unwrap().into_batch(&mut pending),
        Some(OutputEventBatch::Event(RdpOutputEvent::PointerPosition { x: 1, y: 0 }))
    ));
    sender
        .send(RdpOutputEvent::DesktopUpdate(pixel_update(1, 9)))
        .await
        .unwrap();
    drop(sender);
    let Some(OutputEventBatch::DesktopUpdates(updates)) = gui_receiver.recv().await.unwrap().into_batch(&mut pending)
    else {
        panic!("expected a completed desktop batch");
    };
    let mut visible = DesktopFrame::default();
    for update in updates {
        visible.apply(&update).unwrap();
    }
    assert_eq!(visible.pixels(), [8, 9]);
    forwarder.await.unwrap();
}

fn pixel_update(x: u16, pixel: u32) -> DesktopUpdate {
    DesktopUpdate::new(
        vec![pixel],
        NonZeroU16::new(2).unwrap(),
        NonZeroU16::MIN,
        InclusiveRectangle {
            left: x,
            top: 0,
            right: x,
            bottom: 0,
        },
    )
    .unwrap()
}
