//! Bounded delivery of ordered client events to the native event loop.

use std::sync::Arc;

use ironrdp::client::output_channel::OutputEventReceiver;
use ironrdp::client::rdp::{DesktopUpdate, DesktopUpdateBatch, RdpOutputEvent};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A client event awaiting consumption by the native event loop.
pub struct OutputEvent {
    pub(crate) event: RdpOutputEvent,
    _permit: OwnedSemaphorePermit,
}

/// Client output ready for application in one native event handler.
pub enum OutputEventBatch {
    DesktopUpdates(Vec<DesktopUpdate>),
    Event(RdpOutputEvent),
}

impl OutputEvent {
    /// Borrows the client event without releasing its queue capacity.
    pub fn event(&self) -> &RdpOutputEvent {
        &self.event
    }

    /// Releases queue capacity and returns complete desktop batches or standalone events.
    pub fn into_batch(self, pending: &mut DesktopUpdateBatch) -> Option<OutputEventBatch> {
        let Self { event, _permit: permit } = self;
        drop(permit);
        match event {
            RdpOutputEvent::DesktopUpdate(update) => pending.push(update).map(OutputEventBatch::DesktopUpdates),
            event => {
                if matches!(
                    event,
                    RdpOutputEvent::Connected
                        | RdpOutputEvent::AutoReconnected
                        | RdpOutputEvent::AutoReconnecting { .. }
                        | RdpOutputEvent::ServerRedirected { .. }
                        | RdpOutputEvent::DisplayResizeFallback(_)
                        | RdpOutputEvent::ConnectionFailure(_)
                        | RdpOutputEvent::Terminated(_)
                        | RdpOutputEvent::Image { .. }
                ) {
                    pending.clear();
                }
                Some(OutputEventBatch::Event(event))
            }
        }
    }
}

/// Forwards events in order until either the client or the native event loop closes.
/// Backpressures while `capacity` events await consumption, without discarding deltas.
///
/// # Panics
///
/// Panics if `capacity` is zero or exceeds [`Semaphore::MAX_PERMITS`].
pub async fn forward_output(
    mut receiver: OutputEventReceiver,
    capacity: usize,
    mut deliver: impl FnMut(OutputEvent) -> bool,
) {
    assert!(capacity != 0, "native event queue capacity must be nonzero");
    let permits = Arc::new(Semaphore::new(capacity));
    while let Some(event) = receiver.recv().await {
        // The queued payload retains its permit until the native event loop consumes it.
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .expect("the native event semaphore is never closed");
        if !deliver(OutputEvent { event, _permit: permit }) {
            break;
        }
    }
}
