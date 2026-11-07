//! Bounded delivery of ordered client events to the native event loop.

use std::sync::Arc;

use ironrdp::client::output_channel::OutputEventReceiver;
use ironrdp::client::rdp::RdpOutputEvent;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A client event awaiting consumption by the native event loop.
pub struct OutputEvent {
    pub(crate) event: RdpOutputEvent,
    _permit: OwnedSemaphorePermit,
}

impl OutputEvent {
    /// Borrows the client event without releasing its queue capacity.
    pub fn event(&self) -> &RdpOutputEvent {
        &self.event
    }
}

/// Forwards events in order until either the client or the native event loop closes.
/// Backpressures while `capacity` events remain queued or in a handler, without discarding deltas.
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
        // The queued payload retains its permit until the GUI finishes handling it.
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .expect("the native event semaphore is never closed");
        if !deliver(OutputEvent { event, _permit: permit }) {
            break;
        }
    }
}
