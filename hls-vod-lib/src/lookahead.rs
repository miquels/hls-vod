use std::sync::{Arc, OnceLock};

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender};

use crate::cache::segment_cache;
use crate::hlsvideo::PlaylistOrSegment;
use crate::media::StreamIndex;

/// Global sender channel for notifying the threadpool about lookahead work.
static LOOKAHEAD_QUEUE: OnceLock<Sender<Arc<StreamIndex>>> = OnceLock::new();

/// Initialize the look-ahead threadpool workers.
///
/// This should be called once at startup. It will spawn threadpool workers
/// to handle background pre-generation without creating unbound numbers of threads.
pub fn init_workers() {
    let num_workers = (num_cpus::get() / 2).max(1);

    // We use a bounded channel to provide backpressure if the threadpool is totally overwhelmed.
    // However, dropping a notification is fine because the player will request the segment
    // synchronously anyway, or future requests will re-trigger the lookahead.
    let (tx, rx) = crossbeam_channel::bounded::<Arc<StreamIndex>>(1000);

    if LOOKAHEAD_QUEUE.set(tx).is_err() {
        tracing::warn!("lookahead workers already initialized");
        return;
    }

    tracing::info!(
        "Initializing lookahead threadpool with {} workers",
        num_workers
    );

    for i in 0..num_workers {
        let rx = rx.clone();
        std::thread::Builder::new()
            .name(format!("hls-lookahead-{}", i))
            .spawn(move || worker_loop(rx))
            .expect("Failed to spawn lookahead worker");
    }
}

/// Send a notification to the threadpool to process the stream's lookahead queue.
pub(crate) fn notify_lookahead(stream: Arc<StreamIndex>) {
    if let Some(tx) = LOOKAHEAD_QUEUE.get() {
        // We use try_send so we never block the main request handler if the queue is full.
        let _ = tx.try_send(stream);
    }
}

/// The main loop for a lookahead worker thread.
fn worker_loop(rx: Receiver<Arc<StreamIndex>>) {
    // Wait for notifications
    for stream in rx {
        let stream_id = stream.stream_id.clone();

        loop {
            // Keep popping from this stream's queue until it's empty.
            let next_params = {
                let mut q = stream.lookahead_queue.lock().unwrap();
                match q.pop_front() {
                    Some(p) => p,
                    None => break, // Done with this stream for now.
                }
            };

            let segment_key = next_params.to_string();

            // Double-checked locking for dedup (fast path).
            if let Some(c) = segment_cache() {
                if c.get(&stream_id, &segment_key).is_some() {
                    continue; // already cached
                }
            }

            tracing::debug!(segment_key = %segment_key, "look-ahead: starting pre-generation (worker)");

            // Double-checked locking for dedup (locked path).
            if let Some(c) = segment_cache() {
                let lock = c.acquire_generation_lock(&stream_id, &segment_key);
                let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
                if c.get(&stream_id, &segment_key).is_some() {
                    c.cleanup_generation_lock(&stream_id, &segment_key);
                    continue; // completed by another thread
                }
            }

            let ps = PlaylistOrSegment {
                hls_params: next_params,
                index: stream.clone(),
            };

            match ps.do_generate() {
                Ok((data, _)) => {
                    if let Some(c) = segment_cache() {
                        c.insert(&stream_id, &segment_key, Bytes::from(data));
                        c.cleanup_generation_lock(&stream_id, &segment_key);
                    }
                    tracing::debug!(segment_key = %segment_key, "look-ahead: completed pre-generation (worker)");
                }
                Err(e) => {
                    if let Some(c) = segment_cache() {
                        c.cleanup_generation_lock(&stream_id, &segment_key);
                    }
                    tracing::warn!(segment_key = %segment_key, error = %e, "look-ahead: pre-generation failed (worker)");
                }
            }
        }
    }
}
