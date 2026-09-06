use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use cu_protocol::{CuError, ErrorCode};
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug)]
pub(super) struct SignalStore {
    runtime_dir: PathBuf,
    session_dir: PathBuf,
    identity: Option<(u64, u64)>,
    closed: bool,
}

#[derive(Debug)]
pub(super) struct PreparedSignal {
    path: PathBuf,
    staging_path: PathBuf,
    file: Option<File>,
}

impl PreparedSignal {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PreparedSignal {
    fn drop(&mut self) {
        self.file.take();
        if let Err(error) = remove_file_if_present(&self.staging_path) {
            eprintln!("failed to clean MCP wait staging file: {error}");
        }
    }
}

impl SignalStore {
    pub(super) fn new(runtime_dir: PathBuf, session_id: Uuid) -> Self {
        let session_dir = runtime_dir.join("mcp-waits").join(session_id.to_string());
        Self {
            runtime_dir,
            session_dir,
            identity: None,
            closed: false,
        }
    }

    pub(super) fn prepare(&mut self) -> Result<PreparedSignal, CuError> {
        if self.closed {
            return Err(storage_error("MCP wait signal store is closed"));
        }
        self.prepare_directories()?;
        let id = Uuid::new_v4();
        let path = self.session_dir.join(format!("{id}.json"));
        let staging_path = self.session_dir.join(format!(".{id}.json.tmp"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging_path)
            .map_err(|error| storage_error(format!("failed to create wait signal: {error}")))?;
        Ok(PreparedSignal {
            path,
            staging_path,
            file: Some(file),
        })
    }

    pub(super) fn publish(
        &mut self,
        mut prepared: PreparedSignal,
        result: &impl Serialize,
    ) -> Result<(), CuError> {
        if self.closed {
            return Err(storage_error("MCP wait signal store is closed"));
        }
        self.validate_session()?;
        if prepared.path.parent() != Some(self.session_dir.as_path()) {
            return Err(storage_error("wait signal belongs to a different session"));
        }
        let mut file = prepared
            .file
            .take()
            .ok_or_else(|| storage_error("wait signal has no staging file"))?;
        serde_json::to_writer(&mut file, result).map_err(|error| {
            storage_error(format!("failed to write wait signal result: {error}"))
        })?;
        drop(file);
        fs::rename(&prepared.staging_path, &prepared.path).map_err(|error| {
            storage_error(format!("failed to publish wait signal result: {error}"))
        })?;
        Ok(())
    }

    pub(super) fn cleanup(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if self.identity.is_none() {
            return;
        }
        let result =
            self.validate_session()
                .and_then(|()| match fs::remove_dir(&self.session_dir) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
                    Err(error) => Err(storage_error(format!(
                        "failed to remove empty wait session directory: {error}"
                    ))),
                });
        if let Err(error) = result {
            eprintln!("failed to clean MCP wait signals: {}", error.message);
        }
        self.identity = None;
    }

    fn prepare_directories(&mut self) -> Result<(), CuError> {
        prepare_private_directory(&self.runtime_dir)?;
        prepare_private_directory(&self.runtime_dir.join("mcp-waits"))?;
        if self.identity.is_none() {
            DirBuilder::new()
                .mode(0o700)
                .create(&self.session_dir)
                .map_err(|error| {
                    storage_error(format!("failed to create wait session directory: {error}"))
                })?;
            let metadata = validate_private_directory(&self.session_dir)?;
            self.identity = Some((metadata.dev(), metadata.ino()));
        }
        self.validate_session()
    }

    fn validate_session(&self) -> Result<(), CuError> {
        validate_private_directory(&self.runtime_dir)?;
        validate_private_directory(&self.runtime_dir.join("mcp-waits"))?;
        let metadata = validate_private_directory(&self.session_dir)?;
        if self.identity != Some((metadata.dev(), metadata.ino())) {
            return Err(storage_error("wait session directory identity changed"));
        }
        Ok(())
    }
}

impl Drop for SignalStore {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn prepare_private_directory(directory: &Path) -> Result<(), CuError> {
    match DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(storage_error(format!(
                "failed to create wait runtime directory: {error}"
            )));
        }
    }
    validate_private_directory(directory)?;
    Ok(())
}

fn validate_private_directory(directory: &Path) -> Result<fs::Metadata, CuError> {
    let metadata = fs::symlink_metadata(directory).map_err(|error| {
        storage_error(format!("failed to inspect wait runtime directory: {error}"))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(storage_error("wait runtime path must be a real directory"));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(storage_error(
            "wait runtime directory must be owned by the effective user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(storage_error(
            "wait runtime directory must be private to its owner",
        ));
    }
    Ok(metadata)
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn storage_error(message: impl Into<String>) -> CuError {
    CuError::new(ErrorCode::Internal, message)
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::{Arc, Barrier},
        thread,
        time::{Duration, Instant},
    };

    use serde::{Serializer, ser::SerializeMap};
    use serde_json::{Value, json};

    use super::*;

    fn store(root: &Path) -> SignalStore {
        SignalStore::new(root.join("computer-use"), Uuid::new_v4())
    }

    #[test]
    fn publishes_private_complete_json_for_a_late_detector() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        assert!(!path.exists());
        assert_eq!(
            prepared.file.as_ref().unwrap().metadata().unwrap().mode() & 0o777,
            0o600
        );
        for directory in [
            &store.runtime_dir,
            &store.runtime_dir.join("mcp-waits"),
            &store.session_dir,
        ] {
            assert_eq!(fs::metadata(directory).unwrap().mode() & 0o777, 0o700);
        }
        let result = json!({"status": "changed", "elapsed_ms": 4});
        store.publish(prepared, &result).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            result
        );
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_dir(&store.session_dir).unwrap().count(), 1);
    }

    #[test]
    fn next_prepare_preserves_previous_results_for_late_detectors() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let first = store.prepare().unwrap();
        let first_path = first.path().to_owned();
        let first_result = json!({"status": "cancelled", "elapsed_ms": 4});
        store.publish(first, &first_result).unwrap();
        let second = store.prepare().unwrap();
        assert_ne!(first_path, second.path());
        assert!(!second.path().exists());
        let second_path = second.path().to_owned();
        let second_result = json!({"status": "timeout", "elapsed_ms": 20});
        store.publish(second, &second_result).unwrap();
        let third = store.prepare().unwrap();
        for (path, expected) in [(&first_path, &first_result), (&second_path, &second_result)] {
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(path).unwrap()).unwrap(),
                *expected
            );
        }
        drop(third);
        assert_eq!(fs::read_dir(&store.session_dir).unwrap().count(), 2);
    }

    #[test]
    fn rejects_symlinks_and_insecure_managed_directories() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        symlink(outside.path(), &store.runtime_dir).unwrap();
        assert!(store.prepare().is_err());
        fs::remove_file(&store.runtime_dir).unwrap();
        fs::create_dir(&store.runtime_dir).unwrap();
        fs::set_permissions(&store.runtime_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.prepare().is_err());
        fs::set_permissions(&store.runtime_dir, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(outside.path(), store.runtime_dir.join("mcp-waits")).unwrap();
        assert!(store.prepare().is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn cleanup_removes_only_its_own_empty_session_and_prevents_reuse() {
        let root = tempfile::tempdir().unwrap();
        let mut first = store(root.path());
        let mut second = store(root.path());
        drop(first.prepare().unwrap());
        let second_signal = second.prepare().unwrap();
        let second_path = second_signal.path().to_owned();
        second
            .publish(second_signal, &json!({"status": "timeout"}))
            .unwrap();
        first.cleanup();
        assert!(!first.session_dir.exists());
        assert!(second_path.exists());
        assert!(first.prepare().is_err());
        second.cleanup();
        assert!(second_path.exists());
        assert!(second.prepare().is_err());
    }

    #[test]
    fn cleanup_and_drop_preserve_results_until_the_consumer_removes_them() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        let session_dir = store.session_dir.clone();
        let result = json!({"status": "cancelled", "elapsed_ms": 4});
        store.publish(prepared, &result).unwrap();
        store.cleanup();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            result
        );
        drop(store);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            result
        );
        fs::remove_file(path).unwrap();
        fs::remove_dir(session_dir).unwrap();
    }

    #[test]
    fn drop_without_explicit_cleanup_preserves_published_results() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        let result = json!({"status": "changed", "elapsed_ms": 4});
        store.publish(prepared, &result).unwrap();
        drop(store);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            result
        );
    }

    #[test]
    fn drop_removes_an_empty_session_after_the_consumer_removes_its_result() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        let session_dir = store.session_dir.clone();
        store
            .publish(prepared, &json!({"status": "cancelled"}))
            .unwrap();
        fs::remove_file(path).unwrap();
        drop(store);
        assert!(!session_dir.exists());
    }

    #[test]
    fn cannot_claim_an_existing_session_or_publish_another_sessions_signal() {
        let root = tempfile::tempdir().unwrap();
        let session = Uuid::new_v4();
        let runtime = root.path().join("computer-use");
        let mut first = SignalStore::new(runtime.clone(), session);
        let pending = first.prepare().unwrap();
        let mut duplicate = SignalStore::new(runtime, session);
        assert!(duplicate.prepare().is_err());
        duplicate.cleanup();
        assert!(first.session_dir.exists());
        let mut second = store(root.path());
        let own = second.prepare().unwrap();
        assert!(second.publish(pending, &json!({})).is_err());
        assert_eq!(fs::read_dir(&first.session_dir).unwrap().count(), 0);
        assert!(own.staging_path.exists());
    }

    #[test]
    fn cleanup_refuses_a_replaced_session_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        drop(store.prepare().unwrap());
        let moved = store.session_dir.with_extension("moved");
        fs::rename(&store.session_dir, &moved).unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(&store.session_dir)
            .unwrap();
        let marker = store.session_dir.join("do-not-remove");
        fs::write(&marker, "replacement").unwrap();
        store.cleanup();
        assert!(marker.exists());
        assert!(moved.exists());
    }

    struct FailedResult;

    impl Serialize for FailedResult {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("status", "changed")?;
            Err(serde::ser::Error::custom("injected serialization failure"))
        }
    }

    #[test]
    fn serialization_write_and_rename_errors_remove_staging_without_partial_json() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        assert!(store.publish(prepared, &FailedResult).is_err());
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&store.session_dir).unwrap().count(), 0);
        let mut prepared = store.prepare().unwrap();
        prepared.file = Some(File::open(&prepared.staging_path).unwrap());
        let path = prepared.path().to_owned();
        assert!(
            store
                .publish(prepared, &json!({"status": "changed"}))
                .is_err()
        );
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&store.session_dir).unwrap().count(), 0);
        let prepared = store.prepare().unwrap();
        let staging = prepared.staging_path.clone();
        fs::remove_file(&staging).unwrap();
        assert!(
            store
                .publish(prepared, &json!({"status": "changed"}))
                .is_err()
        );
        assert!(!staging.exists());
        assert_eq!(fs::read_dir(&store.session_dir).unwrap().count(), 0);
    }

    #[test]
    fn cleaned_session_cannot_publish_a_late_result() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        let retained = store.prepare().unwrap();
        let retained_path = retained.path().to_owned();
        store
            .publish(retained, &json!({"status": "cancelled"}))
            .unwrap();
        store.cleanup();
        assert!(
            store
                .publish(prepared, &json!({"status": "changed"}))
                .is_err()
        );
        assert!(!path.exists());
        assert!(retained_path.exists());
        assert_eq!(fs::read_dir(&store.session_dir).unwrap().count(), 1);
    }

    #[test]
    fn failed_prepare_preserves_the_previous_result() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        store
            .publish(prepared, &json!({"status": "timeout"}))
            .unwrap();
        fs::set_permissions(&store.session_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.prepare().is_err());
        assert!(path.exists());
        fs::set_permissions(&store.session_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    struct PausedResult(Arc<Barrier>);

    impl Serialize for PausedResult {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("status", "changed")?;
            self.0.wait();
            self.0.wait();
            map.serialize_entry("elapsed_ms", &42)?;
            map.end()
        }
    }

    #[test]
    fn detector_cannot_observe_partial_json_during_publication() {
        let root = tempfile::tempdir().unwrap();
        let mut store = store(root.path());
        let prepared = store.prepare().unwrap();
        let path = prepared.path().to_owned();
        let barrier = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            let reader_barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                reader_barrier.wait();
                let during_write = fs::read(&path);
                reader_barrier.wait();
                assert_eq!(during_write.unwrap_err().kind(), io::ErrorKind::NotFound);
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    assert!(Instant::now() < deadline, "signal was not published");
                    match fs::read(&path) {
                        Ok(bytes) => {
                            assert_eq!(
                                serde_json::from_slice::<Value>(&bytes).unwrap(),
                                json!({"status": "changed", "elapsed_ms": 42})
                            );
                            break;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            thread::yield_now();
                        }
                        Err(error) => panic!("unexpected detector read error: {error}"),
                    }
                }
            });
            store.publish(prepared, &PausedResult(barrier)).unwrap();
        });
    }
}
