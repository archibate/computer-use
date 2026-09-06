use std::{
    fs,
    future::Future,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use serde_json::Value;
use tokio::sync::oneshot;

use super::*;

struct Finished(Option<oneshot::Sender<()>>);

impl Drop for Finished {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn manager(root: &Path) -> AsyncWaits {
    AsyncWaits::new(root.join("computer-use"), Uuid::new_v4())
}

fn register<F, Fut>(waits: &AsyncWaits, baseline: &str, work: F) -> (PathBuf, oneshot::Receiver<()>)
where
    F: FnOnce(WorkerGuard) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut control = lock(&waits.control);
    assert!(!control.closed);
    assert!(control.pending.is_none());
    let signal = control.store.prepare().unwrap();
    let path = signal.path().to_owned();
    let generation = Uuid::new_v4();
    let guard = WorkerGuard {
        control: Arc::downgrade(&waits.control),
        generation,
    };
    let (done, finished) = oneshot::channel();
    let done = Finished(Some(done));
    let task = tokio::spawn(async move {
        let _done = done;
        work(guard).await;
    });
    control.pending = Some(Pending {
        generation,
        baseline: baseline.to_owned(),
        started: Instant::now(),
        signal,
        task,
    });
    (path, finished)
}

fn late_worker(waits: &AsyncWaits) -> WorkerGuard {
    WorkerGuard {
        control: Arc::downgrade(&waits.control),
        generation: lock(&waits.control).pending.as_ref().unwrap().generation,
    }
}

async fn received(receiver: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(2), receiver)
        .await
        .expect("worker did not reach the synchronization point")
        .expect("worker dropped the synchronization channel");
}

fn result(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[tokio::test]
async fn completion_before_cancellation_preserves_the_only_terminal_result() {
    let root = tempfile::tempdir().unwrap();
    let waits = manager(root.path());
    let owner = waits.owner();
    let (release, ready) = oneshot::channel();
    let (path, finished) = register(&waits, "baseline", |guard| async move {
        received(ready).await;
        guard.finish(&SignalResult::Timeout { elapsed_ms: 42 });
    });
    let late = late_worker(&waits);
    release.send(()).unwrap();
    received(finished).await;
    let published = fs::read(&path).unwrap();
    waits.cancel().await;
    late.finish(&SignalResult::Cancelled { elapsed_ms: 43 });
    drop(late);
    owner.shutdown().await;
    drop(owner);
    assert_eq!(result(&path)["status"], "timeout");
    assert_eq!(fs::read(&path).unwrap(), published);
    assert!(lock(&waits.control).pending.is_none());
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[tokio::test]
async fn cancellation_before_completion_stays_visible_and_is_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let waits = manager(root.path());
    let _owner = waits.owner();
    let (started, active) = oneshot::channel();
    let (path, finished) = register(&waits, "baseline", |guard| async move {
        let _guard = guard;
        started.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let late = late_worker(&waits);
    received(active).await;
    waits.cancel().await;
    received(finished).await;
    let published = fs::read(&path).unwrap();
    late.finish(&SignalResult::Timeout { elapsed_ms: 20 });
    drop(late);
    assert_eq!(result(&path)["status"], "cancelled");
    assert_eq!(fs::read(&path).unwrap(), published);
    assert!(lock(&waits.control).pending.is_none());
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[tokio::test]
async fn old_worker_completion_and_drop_cannot_finish_a_new_generation() {
    let root = tempfile::tempdir().unwrap();
    let waits = manager(root.path());
    let _owner = waits.owner();
    let (old_path, old_finished) = register(&waits, "baseline", |guard| async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    let late = late_worker(&waits);
    waits.cancel().await;
    received(old_finished).await;
    assert_eq!(result(&old_path)["status"], "cancelled");

    let (release, ready) = oneshot::channel();
    let (path, finished) = register(&waits, "baseline", |guard| async move {
        received(ready).await;
        guard.finish(&SignalResult::Timeout { elapsed_ms: 45 });
    });
    let generation = lock(&waits.control).pending.as_ref().unwrap().generation;
    late.finish(&SignalResult::Timeout { elapsed_ms: 1 });
    drop(late);
    assert_eq!(
        lock(&waits.control).pending.as_ref().unwrap().generation,
        generation
    );
    assert!(!path.exists());
    assert_eq!(result(&old_path)["status"], "cancelled");

    release.send(()).unwrap();
    received(finished).await;
    assert_eq!(result(&path)["status"], "timeout");
    assert_eq!(result(&path)["elapsed_ms"], 45);
    assert_eq!(result(&old_path)["status"], "cancelled");
    assert!(lock(&waits.control).pending.is_none());
}

#[tokio::test]
async fn shutdown_and_drop_publish_cancellation_even_while_the_frame_mutex_is_held() {
    for explicit_shutdown in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let waits = manager(root.path());
        let owner = waits.owner();
        let state = Arc::new(tokio::sync::Mutex::new(McpSessionState::default()));
        let held = state.lock().await;
        let worker_state = Arc::clone(&state);
        let (waiting, blocked) = oneshot::channel();
        let (path, finished) = register(&waits, "baseline", |guard| async move {
            waiting.send(()).unwrap();
            let _state = worker_state.lock().await;
            guard.finish(&SignalResult::Timeout { elapsed_ms: 2 });
        });
        let late = late_worker(&waits);
        let session = path.parent().unwrap().to_owned();
        received(blocked).await;
        if explicit_shutdown {
            tokio::time::timeout(Duration::from_secs(2), owner.shutdown())
                .await
                .expect("shutdown waited on the frame mutex");
        }
        drop(owner);
        received(finished).await;
        assert_eq!(result(&path)["status"], "cancelled");
        let published = fs::read(&path).unwrap();
        drop(held);
        late.finish(&SignalResult::Timeout { elapsed_ms: 3 });
        drop(late);
        assert_eq!(fs::read(&path).unwrap(), published);
        assert_eq!(fs::read_dir(&session).unwrap().count(), 1);
        let control = lock(&waits.control);
        assert!(control.closed);
        assert!(control.pending.is_none());
    }
}

#[tokio::test]
async fn panicking_worker_publishes_an_error_and_releases_the_pending_slot() {
    let root = tempfile::tempdir().unwrap();
    let waits = manager(root.path());
    let _owner = waits.owner();
    let (path, finished) = register(&waits, "baseline", |guard| async move {
        let _guard = guard;
        panic!("injected worker failure");
    });
    received(finished).await;
    let failed = result(&path);
    assert_eq!(failed["status"], "error");
    assert_eq!(failed["code"], "internal");
    assert!(lock(&waits.control).pending.is_none());

    let (next_path, next_finished) = register(&waits, "baseline", |guard| async move {
        guard.finish(&SignalResult::Timeout { elapsed_ms: 0 });
    });
    received(next_finished).await;
    assert_eq!(result(&path), failed);
    assert_eq!(result(&next_path)["status"], "timeout");
}

#[tokio::test]
async fn cancelling_or_stopping_an_unpolled_worker_publishes_without_a_false_error() {
    for shutdown in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let waits = manager(root.path());
        let owner = waits.owner();
        let was_polled = Arc::new(AtomicBool::new(false));
        let worker_polled = Arc::clone(&was_polled);
        let (path, finished) = register(&waits, "baseline", |guard| async move {
            worker_polled.store(true, Ordering::SeqCst);
            guard.finish(&SignalResult::error(0, "internal", "should never run"));
        });
        // The single-thread runtime has not yielded since spawning the worker.
        if shutdown {
            owner.shutdown().await;
        } else {
            waits.cancel().await;
        }
        received(finished).await;
        assert!(!was_polled.load(Ordering::SeqCst));
        assert_eq!(result(&path)["status"], "cancelled");
        assert!(lock(&waits.control).pending.is_none());
    }
}

#[tokio::test]
async fn failed_publication_removes_staging_and_allows_the_next_wait() {
    let root = tempfile::tempdir().unwrap();
    let waits = manager(root.path());
    let _owner = waits.owner();
    let (release, ready) = oneshot::channel();
    let (path, finished) = register(&waits, "baseline", |guard| async move {
        received(ready).await;
        guard.finish(&SignalResult::Timeout { elapsed_ms: 4 });
    });
    // A directory at the destination forces rename to fail after serialization.
    fs::create_dir(&path).unwrap();
    release.send(()).unwrap();
    received(finished).await;
    assert!(lock(&waits.control).pending.is_none());
    let remaining = fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(remaining, vec![path.clone()]);
    fs::remove_dir(&path).unwrap();

    let (next_path, next_finished) = register(&waits, "baseline", |guard| async move {
        guard.finish(&SignalResult::Timeout { elapsed_ms: 0 });
    });
    received(next_finished).await;
    assert_eq!(result(&next_path)["status"], "timeout");
}

#[tokio::test]
async fn post_act_cancellation_requires_a_different_committed_frame() {
    let root = tempfile::tempdir().unwrap();
    let waits = manager(root.path());
    let _owner = waits.owner();
    let (path, finished) = register(&waits, "same-frame", |guard| async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    waits.cancel_if_superseded("same-frame").await;
    assert!(lock(&waits.control).pending.is_some());
    assert!(!path.exists());
    waits.cancel_if_superseded("new-frame").await;
    received(finished).await;
    assert!(lock(&waits.control).pending.is_none());
    assert_eq!(result(&path)["status"], "cancelled");
}

#[tokio::test]
async fn late_detector_after_shutdown_wakes_once_for_every_terminal_status() {
    // A minimal external detector consumes one terminal JSON, then exits. The
    // callback counts wake-ups without invoking a shell or an actual queue.
    struct Detector {
        stopped: bool,
        wakes: usize,
    }

    impl Detector {
        fn poll(&mut self, path: &Path) {
            if self.stopped || !path.exists() {
                return;
            }
            let terminal = result(path);
            if matches!(
                terminal["status"].as_str(),
                Some("changed" | "timeout" | "error" | "cancelled")
            ) {
                self.wakes += 1;
                self.stopped = true;
            }
        }
    }

    let results = [
        SignalResult::Changed {
            elapsed_ms: 5,
            activity_bbox: Rect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            settled: true,
        },
        SignalResult::Timeout { elapsed_ms: 6 },
        SignalResult::error(7, "daemon_unavailable", "disconnected"),
        SignalResult::Cancelled { elapsed_ms: 8 },
    ];
    for terminal in results {
        let root = tempfile::tempdir().unwrap();
        let waits = manager(root.path());
        let owner = waits.owner();
        let (path, finished) = register(&waits, "baseline", |guard| async move {
            guard.finish(&terminal);
        });
        received(finished).await;
        owner.shutdown().await;
        drop(owner);
        drop(waits);
        let mut detector = Detector {
            stopped: false,
            wakes: 0,
        };
        detector.poll(&path);
        detector.poll(&path);
        assert!(detector.stopped);
        assert_eq!(detector.wakes, 1);
    }
}
