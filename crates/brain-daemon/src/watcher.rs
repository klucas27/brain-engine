//! File-system watcher with 500 ms debounce.
//!
//! Watches the project root recursively.  Events inside `.brain/` are filtered
//! out to avoid a feedback loop (every DB write would otherwise trigger a
//! reindex).  The remaining events are coalesced: only one reindex is triggered
//! per quiet period of [`DEBOUNCE_MS`] milliseconds.

use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use notify::{Event, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher};
use tokio::sync::oneshot;

use crate::worker::WorkerMsg;

const DEBOUNCE_MS: u64 = 500;

/// Start watching `root` using a dedicated debounce thread (no tokio required).
///
/// Returns the watcher handle — drop it to stop watching.
pub fn start_from_sync(
    root: &Path,
    worker_tx: std_mpsc::SyncSender<WorkerMsg>,
) -> notify::Result<RecommendedWatcher> {
    let (std_tx, std_rx) = std_mpsc::channel::<()>();

    // Spawn a plain thread that debounces events and sends Index requests.
    std::thread::spawn(move || {
        let debounce = Duration::from_millis(DEBOUNCE_MS);
        loop {
            // Block waiting for the first event.
            if std_rx.recv().is_err() {
                break; // watcher dropped
            }
            // Drain further events within the debounce window.
            let _ = std_rx.recv_timeout(debounce);
            // Drain any remaining queued events (bursty saves).
            while std_rx.try_recv().is_ok() {}

            let (reply_tx, reply_rx) = oneshot::channel();
            if worker_tx
                .send(WorkerMsg::Index {
                    reindex: false,
                    no_embed: false,
                    reply: reply_tx,
                })
                .is_err()
            {
                break; // worker gone
            }
            // Block until the index completes before accepting new watcher events,
            // preventing pile-ups during slow reindexes.
            match reply_rx.blocking_recv() {
                Ok(Ok(v)) => {
                    let indexed = v.get("indexed").and_then(|x| x.as_u64()).unwrap_or(0);
                    if indexed > 0 {
                        eprintln!("[brain-daemon] auto-reindexed ({indexed} file(s) changed)");
                    }
                }
                Ok(Err(e)) => eprintln!("[brain-daemon] reindex error: {e}"),
                Err(_) => break,
            }
        }
    });

    let mut watcher = notify::recommended_watcher(move |res: NotifyResult<Event>| {
        let Ok(event) = res else { return };
        let is_brain_internal = event
            .paths
            .iter()
            .any(|p| p.components().any(|c| c.as_os_str() == ".brain"));
        if !is_brain_internal {
            let _ = std_tx.send(());
        }
    })?;

    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}
