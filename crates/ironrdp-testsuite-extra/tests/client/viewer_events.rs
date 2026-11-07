use core::num::NonZeroU16;
use core::time::Duration;

use ironrdp_client::output_channel::output_channel;
use ironrdp_client::rdp::{DesktopUpdate, RdpOutputEvent};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_viewer::events::forward_output;
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
