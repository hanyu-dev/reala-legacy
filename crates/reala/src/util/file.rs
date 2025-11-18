//! File related utilities.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crossfire::stream::AsyncStream;
use notify::event::ModifyKind;
use notify::{Event, EventHandler, EventKind, RecommendedWatcher, Watcher as _};
use quanta::Instant;

#[must_use = "futures do nothing unless polled"]
/// A future that resolves when the given file is changed.
pub(crate) struct Changing {
    _watcher: Option<RecommendedWatcher>,
    rx: Option<AsyncStream<Event>>,
}

impl Changing {
    /// Creates a new [`Changing`] future that watches the given file.
    pub(crate) fn new<P: AsRef<Path>>(file: P) -> io::Result<Self> {
        let (tx, rx) = crossfire::mpsc::bounded_async(1);

        let mut watcher = notify::recommended_watcher(IoEventTx {
            inner: tx,
            threshold: None,
        })
        .map_err(io::Error::other)?;

        watcher
            .watch(file.as_ref(), notify::RecursiveMode::NonRecursive)
            .map_err(io::Error::other)?;

        Ok(Self {
            _watcher: Some(watcher),
            rx: Some(rx.into_stream()),
        })
    }

    /// Creates a no-op [`Changing`] future that never resolves.
    pub(crate) fn new_noop() -> Self {
        Self {
            _watcher: None,
            rx: None,
        }
    }

    /// Creates a new [`Changing`] future that watches the given file,
    /// or a no-op future if watcher creation fails.
    pub(crate) fn new_or_noop<P: AsRef<Path>>(file: P) -> Self {
        match Self::new(file) {
            Ok(watcher) => watcher,
            Err(e) => {
                tracing::error!("Failed to create file watcher, auto-reload disabled: {e:?}");

                Self {
                    _watcher: None,
                    rx: None,
                }
            }
        }
    }
}

impl Future for Changing {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(rx) = &mut self.rx else {
            return Poll::Pending;
        };

        match rx.poll_item(cx) {
            Poll::Ready(Some(_)) => Poll::Ready(Ok(())),
            Poll::Ready(None) => Poll::Ready(Err(io::Error::other("File watch channel closed"))),
            Poll::Pending => Poll::Pending,
        }
    }
}

wrapper_lite::wrapper!(
    struct IoEventTx {
        inner: crossfire::MAsyncTx<notify::Event>,
        threshold: Option<Instant>,
    }
);

impl EventHandler for IoEventTx {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        match event {
            Ok(event) if matches!(event.kind, EventKind::Modify(ModifyKind::Data(_))) => {
                // Throttle events to at most one per 5s.
                let threshold = self.threshold.replace(Instant::now());

                if let Some(threshold) = threshold
                    && threshold.elapsed() > Duration::from_secs(5)
                {
                    let _ = self.inner.try_send(event);
                } else {
                    // Ignore.
                    tracing::trace!("File change event throttled: {event:?}");
                }
            }
            Ok(event) => {
                // Ignore.
                tracing::trace!("File change event ignored: {event:?}");
            }
            Err(e) => {
                tracing::error!("File watch event error: {:?}", e);
            }
        }
    }
}
