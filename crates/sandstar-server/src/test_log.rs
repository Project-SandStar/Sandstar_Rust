//! Log capture for unit tests.
//!
//! One subscriber for the whole test binary, installed as the global
//! default the first time a test captures logs. It writes into a buffer
//! that belongs to the calling thread, and only while that thread is
//! capturing.
//!
//! Why not a subscriber per test (`tracing::subscriber::set_default`):
//! `tracing-core` keeps the interest of every callsite and the highest
//! enabled level in process-wide caches. With subscribers that come and go
//! on test threads, another thread can rebuild those caches from its own
//! point of view (no subscriber: nothing is enabled), and a `debug!` in the
//! capturing test is then skipped. The test that counted log lines failed
//! about once in several full runs (S-55, S-56). A single global subscriber
//! never changes, so the caches cannot go stale.
//!
//! Limits: only events emitted on the capturing thread are seen
//! (`#[tokio::test]` runs on the test's own thread, so its awaits are
//! covered; `spawn_blocking` and other threads are not).

use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    /// `Some` while this thread captures.
    static CAPTURED: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

struct ThreadWriter;

impl std::io::Write for ThreadWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        CAPTURED.with(|buf| {
            if let Some(buf) = buf.borrow_mut().as_mut() {
                buf.extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| ThreadWriter)
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("no other global tracing subscriber in the unit test binary");
    });
}

/// Captures the log lines of this thread until [`Capture::finish`].
pub(crate) struct Capture(());

/// Start capturing the log output of the calling thread (DEBUG and up).
pub(crate) fn capture() -> Capture {
    install();
    CAPTURED.with(|buf| *buf.borrow_mut() = Some(Vec::new()));
    Capture(())
}

impl Capture {
    /// Stop capturing and return what was logged.
    pub(crate) fn finish(self) -> String {
        let bytes = CAPTURED
            .with(|buf| buf.borrow_mut().take())
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        CAPTURED.with(|buf| *buf.borrow_mut() = None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_this_thread_only_and_only_while_capturing() {
        tracing::info!("before capture");
        let capture = capture();
        tracing::debug!("debug line");
        tracing::warn!("warn line");
        std::thread::spawn(|| tracing::error!("other thread"))
            .join()
            .unwrap();
        let out = capture.finish();
        tracing::info!("after capture");

        assert!(out.contains("DEBUG") && out.contains("debug line"), "{out}");
        assert!(out.contains("WARN") && out.contains("warn line"), "{out}");
        assert!(!out.contains("before capture"), "{out}");
        assert!(!out.contains("other thread"), "{out}");
        assert!(!out.contains("after capture"), "{out}");
    }

    /// The failure this module replaces: a `debug!` skipped in one thread
    /// while other threads log with and without capturing.
    #[test]
    fn every_line_is_captured_while_other_threads_log() {
        let noisy: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || {
                    for n in 0..200 {
                        if i % 2 == 0 {
                            let capture = capture();
                            tracing::debug!(n, "noise");
                            drop(capture);
                        } else {
                            tracing::debug!(n, "noise without capture");
                        }
                    }
                })
            })
            .collect();
        for round in 0..200 {
            let capture = capture();
            tracing::debug!(round, "counted line");
            tracing::debug!(round, "counted line");
            let out = capture.finish();
            assert_eq!(
                out.matches("counted line").count(),
                2,
                "round {round}: {out}"
            );
            assert!(!out.contains("noise"), "{out}");
        }
        for t in noisy {
            t.join().unwrap();
        }
    }
}
