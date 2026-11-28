//! `SAVE-008`: explicit verified backup and restore operations.
//!
//! Backup bytes are immutable copies of a complete verified save. Retention
//! management is intentionally limited to opt-in successful-save history;
//! manual and migration-preflight backups are never eligible for pruning.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::loader::{LoadError, inspect_save, inspect_save_file};

/// Why this backup was created. Only successful-save history is pruneable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupPurpose {
    MigrationPreflight,
    Manual,
    SuccessfulSaveHistory,
}

/// Sidecar metadata bound to a verified source save.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackupMetadata {
    pub backup_id: String,
    pub save_id: String,
    pub root_hash: String,
    pub format_version: String,
    pub created_at: String,
    pub purpose: BackupPurpose,
    pub plan_digest: Option<String>,
    pub target_schema: Option<String>,
}

/// Opt-in retention limits for successful-save history only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackupPolicy {
    pub max_history_count: usize,
    pub max_history_bytes: u64,
}

/// Verified backup creation result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupOutcome {
    pub archive_path: PathBuf,
    pub metadata_path: PathBuf,
    pub metadata: BackupMetadata,
    pub bytes_written: u64,
}

/// Structured backup/restore failure.
#[derive(Debug)]
pub enum BackupError {
    Io {
        stage: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Load(LoadError),
    InvalidInput {
        code: &'static str,
        message: String,
    },
    Metadata {
        code: &'static str,
        message: String,
    },
}

impl BackupError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "save.backup.io",
            Self::Load(_) => "save.backup.invalid_save",
            Self::InvalidInput { code, .. } | Self::Metadata { code, .. } => code,
        }
    }
}

impl std::fmt::Display for BackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                stage,
                path,
                source,
            } => write!(f, "backup I/O at {stage} for {}: {source}", path.display()),
            Self::Load(error) => write!(f, "backup save is invalid: {error}"),
            Self::InvalidInput { code, message } | Self::Metadata { code, message } => {
                write!(f, "{code}: {message}")
            }
        }
    }
}

impl std::error::Error for BackupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Load(error) => Some(error),
            _ => None,
        }
    }
}

/// Creates an immutable, byte-identical verified backup beside `source`.
/// `created_at` is supplied by the caller to avoid hidden clock reads.
pub fn create_backup(
    source: impl AsRef<Path>,
    backup_id: &str,
    created_at: &str,
    purpose: BackupPurpose,
    plan_digest: Option<String>,
    target_schema: Option<String>,
) -> Result<BackupOutcome, BackupError> {
    validate_component(backup_id)?;
    if !crate::is_valid_timestamp(created_at) {
        return Err(input_error(
            "save.backup.invalid_metadata",
            "created_at must be RFC 3339 UTC",
        ));
    }
    if purpose == BackupPurpose::MigrationPreflight && plan_digest.is_none() {
        return Err(input_error(
            "save.backup.invalid_metadata",
            "migration backup requires plan_digest",
        ));
    }
    let source = source.as_ref();
    let source_bytes = fs::read(source).map_err(|error| io_error("source_read", source, error))?;
    let inspected = inspect_save(&source_bytes).map_err(BackupError::Load)?;
    let manifest = inspected.manifest();
    let paths = BackupPaths::new(source, backup_id);
    if paths.archive.exists() || paths.metadata.exists() {
        return Err(input_error(
            "save.backup.already_exists",
            "backup ID already has a published archive or metadata sidecar",
        ));
    }

    write_new_verified(&paths.archive_temp, &source_bytes, "backup_write")?;
    let archive_check = inspect_save_file(&paths.archive_temp).map_err(BackupError::Load)?;
    if archive_check.manifest().save_id != manifest.save_id
        || archive_check.manifest().integrity.root_hash != manifest.integrity.root_hash
    {
        return Err(input_error(
            "save.backup.readback_mismatch",
            "verified temporary backup identity differs from source",
        ));
    }
    let metadata = BackupMetadata {
        backup_id: backup_id.to_owned(),
        save_id: manifest.save_id.clone(),
        root_hash: manifest.integrity.root_hash.clone(),
        format_version: manifest.save_format_version.to_string(),
        created_at: created_at.to_owned(),
        purpose,
        plan_digest,
        target_schema,
    };
    let metadata_bytes = serde_json::to_vec(&metadata)
        .map_err(|error| metadata_error("save.backup.invalid_metadata", error.to_string()))?;

    publish_new_file(&paths.archive_temp, &paths.archive, "backup_publish")?;
    if let Err(error) = write_new_verified(&paths.metadata_temp, &metadata_bytes, "metadata_write")
        .and_then(|()| publish_new_file(&paths.metadata_temp, &paths.metadata, "metadata_publish"))
    {
        if let Err(rollback_error) = fs::remove_file(&paths.archive) {
            if rollback_error.kind() != io::ErrorKind::NotFound {
                return Err(io_error("backup_rollback", &paths.archive, rollback_error));
            }
        }
        return Err(error);
    }
    Ok(BackupOutcome {
        archive_path: paths.archive,
        metadata_path: paths.metadata,
        metadata,
        bytes_written: u64::try_from(source_bytes.len()).expect("usize fits u64"),
    })
}

/// Reads and verifies both the backup archive and its identity sidecar.
pub fn inspect_backup(
    source: impl AsRef<Path>,
    backup_id: &str,
) -> Result<BackupOutcome, BackupError> {
    validate_component(backup_id)?;
    let paths = BackupPaths::new(source.as_ref(), backup_id);
    let bytes =
        fs::read(&paths.archive).map_err(|error| io_error("backup_read", &paths.archive, error))?;
    let inspected = inspect_save(&bytes).map_err(BackupError::Load)?;
    let metadata_bytes = fs::read(&paths.metadata)
        .map_err(|error| io_error("metadata_read", &paths.metadata, error))?;
    let metadata: BackupMetadata = serde_json::from_slice(&metadata_bytes)
        .map_err(|error| metadata_error("save.backup.invalid_metadata", error.to_string()))?;
    if !crate::is_valid_timestamp(&metadata.created_at)
        || (metadata.purpose == BackupPurpose::MigrationPreflight && metadata.plan_digest.is_none())
    {
        return Err(metadata_error(
            "save.backup.invalid_metadata",
            "timestamp or migration plan metadata is invalid",
        ));
    }
    if metadata.backup_id != backup_id
        || metadata.save_id != inspected.manifest().save_id
        || metadata.root_hash != inspected.manifest().integrity.root_hash
        || metadata.format_version != inspected.manifest().save_format_version.to_string()
    {
        return Err(metadata_error(
            "save.backup.identity_mismatch",
            "sidecar does not match verified backup",
        ));
    }
    Ok(BackupOutcome {
        archive_path: paths.archive,
        metadata_path: paths.metadata,
        metadata,
        bytes_written: u64::try_from(bytes.len()).expect("usize fits u64"),
    })
}

/// Restores a verified backup using a same-directory temporary file.
/// The backup is never removed, including when publication or read-back fails.
pub fn restore_backup(
    source: impl AsRef<Path>,
    backup_id: &str,
    target: impl AsRef<Path>,
) -> Result<(), BackupError> {
    let source = source.as_ref();
    let backup = inspect_backup(source, backup_id)?;
    let bytes = fs::read(&backup.archive_path)
        .map_err(|error| io_error("backup_read", &backup.archive_path, error))?;
    let target = target.as_ref();
    let paths = BackupPaths::new(source, backup_id);
    reject_backup_path_collision(target, &paths)?;
    let temp = target.with_file_name(format!(".{}.restore-{backup_id}.tmp", file_stem(target)?));
    write_new_verified(&temp, &bytes, "restore_write")?;
    inspect_save_file(&temp).map_err(BackupError::Load)?;
    fs::rename(&temp, target).map_err(|error| io_error("restore_replace", target, error))?;
    let restored = inspect_save_file(target).map_err(BackupError::Load)?;
    if restored.manifest().save_id != backup.metadata.save_id
        || restored.manifest().integrity.root_hash != backup.metadata.root_hash
    {
        return Err(metadata_error(
            "save.backup.restore_mismatch",
            "restored target differs from verified backup",
        ));
    }
    Ok(())
}

/// Removes only explicitly enumerated successful-save history beyond policy.
/// Manual and migration-preflight backups are never eligible for deletion.
pub fn prune_history(
    source: impl AsRef<Path>,
    policy: BackupPolicy,
    active_save_id: &str,
) -> Result<Vec<PathBuf>, BackupError> {
    let source = source.as_ref();
    let active = inspect_save_file(source).map_err(BackupError::Load)?;
    if active.manifest().save_id != active_save_id {
        return Err(input_error(
            "save.backup.active_save_mismatch",
            "active save identity does not match the requested retention target",
        ));
    }
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = file_stem(source)?;
    let prefix = format!(".{stem}.backup-meta-");
    let mut history = Vec::new();
    for entry in fs::read_dir(parent).map_err(|error| io_error("history_list", parent, error))? {
        let entry = entry.map_err(|error| io_error("history_list", parent, error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(id) = name.strip_prefix(&prefix) else {
            continue;
        };
        if id.is_empty() || validate_component(id).is_err() {
            continue;
        }
        let backup = inspect_backup(source, id)?;
        if backup.metadata.purpose == BackupPurpose::SuccessfulSaveHistory
            && backup.metadata.save_id == active_save_id
        {
            history.push(backup);
        }
    }
    history.sort_by(|a, b| {
        (timestamp_key(&a.metadata.created_at), &a.metadata.backup_id)
            .cmp(&(timestamp_key(&b.metadata.created_at), &b.metadata.backup_id))
    });
    let mut total: u64 = history.iter().map(|item| item.bytes_written).sum();
    let mut removed = Vec::new();
    while history.len() > policy.max_history_count || total > policy.max_history_bytes {
        if history.len() <= 1 {
            return Err(input_error(
                "save.backup.last_recovery_point",
                "policy would remove the last verified history backup",
            ));
        }
        let oldest = history.remove(0);
        total = total.saturating_sub(oldest.bytes_written);
        fs::remove_file(&oldest.metadata_path)
            .map_err(|error| io_error("history_prune", &oldest.metadata_path, error))?;
        fs::remove_file(&oldest.archive_path)
            .map_err(|error| io_error("history_prune", &oldest.archive_path, error))?;
        removed.push(oldest.archive_path);
    }
    Ok(removed)
}

fn reject_backup_path_collision(
    target: &Path,
    backup_paths: &BackupPaths,
) -> Result<(), BackupError> {
    let target_path = normalized_path(target)?;
    for owned_path in [
        &backup_paths.archive,
        &backup_paths.archive_temp,
        &backup_paths.metadata,
        &backup_paths.metadata_temp,
    ] {
        let owned_path = normalized_path(owned_path)?;
        if paths_match_for_host(&owned_path, &target_path) {
            return Err(input_error(
                "save.backup.target_conflict",
                "restore target must not overlap a backup-owned path",
            ));
        }
    }
    Ok(())
}

fn paths_match_for_host(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    }

    #[cfg(not(windows))]
    {
        left == right
    }
}

fn normalized_path(path: &Path) -> Result<PathBuf, BackupError> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|error| io_error("restore_path", path, error));
    }

    let file_name = path.file_name().ok_or_else(|| {
        input_error(
            "save.backup.invalid_path",
            "restore target must identify a file",
        )
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| io_error("restore_path", parent, error))?;
    Ok(canonical_parent.join(file_name))
}

struct BackupPaths {
    archive: PathBuf,
    archive_temp: PathBuf,
    metadata: PathBuf,
    metadata_temp: PathBuf,
}

impl BackupPaths {
    fn new(source: &Path, id: &str) -> Self {
        let stem = source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("save");
        let parent = source.parent().unwrap_or_else(|| Path::new("."));
        Self {
            archive: parent.join(format!(".{stem}.backup-{id}.sav")),
            archive_temp: parent.join(format!(".{stem}.backup-{id}.tmp")),
            metadata: parent.join(format!(".{stem}.backup-meta-{id}")),
            metadata_temp: parent.join(format!(".{stem}.backup-meta-{id}.tmp")),
        }
    }
}

fn publish_new_file(
    temporary: &Path,
    destination: &Path,
    stage: &'static str,
) -> Result<(), BackupError> {
    match fs::hard_link(temporary, destination) {
        Ok(()) => {
            if let Err(error) = fs::remove_file(temporary) {
                let _ = fs::remove_file(destination);
                return Err(io_error("backup_temp_cleanup", temporary, error));
            }
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(temporary);
            if error.kind() == io::ErrorKind::AlreadyExists {
                return Err(input_error(
                    "save.backup.already_exists",
                    "backup ID already has a published archive or metadata sidecar",
                ));
            }
            Err(io_error(stage, destination, error))
        }
    }
}

fn write_new_verified(path: &Path, bytes: &[u8], stage: &'static str) -> Result<(), BackupError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_error(stage, path, error))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(io_error(stage, path, error));
    }
    drop(file);
    let readback = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = fs::remove_file(path);
            return Err(io_error("readback", path, error));
        }
    };
    if readback != bytes {
        let _ = fs::remove_file(path);
        return Err(input_error(
            "save.backup.readback_mismatch",
            "written bytes differ from source",
        ));
    }
    if let Err(error) = OpenOptions::new().read(true).open(path) {
        let _ = fs::remove_file(path);
        return Err(io_error("reopen", path, error));
    }
    Ok(())
}

fn timestamp_key(value: &str) -> u128 {
    let date = u128::from(value[0..4].parse::<u16>().unwrap_or(0));
    let month = u128::from(value[5..7].parse::<u8>().unwrap_or(0));
    let day = u128::from(value[8..10].parse::<u8>().unwrap_or(0));
    let hour = u128::from(value[11..13].parse::<u8>().unwrap_or(0));
    let minute = u128::from(value[14..16].parse::<u8>().unwrap_or(0));
    let second = u128::from(value[17..19].parse::<u8>().unwrap_or(0));
    let fraction = value
        .get(19..value.len().saturating_sub(1))
        .filter(|part| part.starts_with('.'))
        .map(|part| {
            let digits = &part[1..];
            let nanos = format!("{digits:0<9}");
            nanos[..9].parse::<u128>().unwrap_or(0)
        })
        .unwrap_or(0);
    (((((date * 13 + month) * 32 + day) * 24 + hour) * 60 + minute) * 61 + second) * 1_000_000_000
        + fraction
}

fn validate_component(value: &str) -> Result<(), BackupError> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(input_error(
            "save.backup.invalid_id",
            "backup ID must use 1..=96 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

fn file_stem(path: &Path) -> Result<&str, BackupError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            input_error(
                "save.backup.invalid_path",
                "target filename must be valid UTF-8",
            )
        })
}

fn io_error(stage: &'static str, path: &Path, source: io::Error) -> BackupError {
    BackupError::Io {
        stage,
        path: path.to_path_buf(),
        source,
    }
}

fn input_error(code: &'static str, message: impl Into<String>) -> BackupError {
    BackupError::InvalidInput {
        code,
        message: message.into(),
    }
}

fn metadata_error(code: &'static str, message: impl Into<String>) -> BackupError {
    BackupError::Metadata {
        code,
        message: message.into(),
    }
}
