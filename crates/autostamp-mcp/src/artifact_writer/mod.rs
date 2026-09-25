//! Native, standalone generated-crate artifact writer.
//!
//! Ownership is recorded in [`LEDGER_FILE`](crate::artifact_writer::LEDGER_FILE)
//! using sorted portable paths and SHA-256
//! content digests. Missing owned files are considered already removed: retained
//! files are recreated, and stale files need no deletion. Content edits, including
//! edits to stale files, block the entire generation. Permissions are not tracked.
//!
//! The ledger is structurally validated, not authenticated. A deliberately forged
//! valid ledger can claim an allowed artifact whose content digest matches; it
//! cannot claim arbitrary paths or non-artifact files.
//!
//! All checks precede publication, which uses a private staging directory, backups,
//! and rollback. This is not a crash-consistent or atomically visible multi-file
//! transaction. Callers must serialize writes and prevent concurrent filesystem
//! changes: portable `std` path checks cannot eliminate adversarial symlink races.
//! Static symlinks anywhere along touched paths are rejected. Publication requires
//! hard-link support within the output filesystem; unsupported filesystems fail
//! without being treated as successful writes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Reserved ownership-ledger name, relative to the output directory.
pub const LEDGER_FILE: &str = ".autostamp-files.json";

const MAX_FILES: usize = 4096;
const MAX_LEDGER_BYTES: u64 = 1024 * 1024;
const JOURNAL_FILE: &str = "recovery.json";
static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Stable categories of writer failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterErrorKind {
    /// An output directory or artifact path violates the portable path policy.
    InvalidPath,
    /// The ownership ledger is malformed, unsupported, or structurally unsafe.
    InvalidLedger,
    /// An existing unowned path would be overwritten.
    Conflict,
    /// Previously owned content no longer matches its recorded digest.
    Modified,
    /// A touched path or one of its ancestors is a symbolic link.
    Symlink,
    /// A file or directory has an unexpected filesystem type.
    UnexpectedFileType,
    /// Filesystem state changed between validation and publication.
    ConcurrentChange,
    /// An ordinary filesystem operation failed; any publication was rolled back.
    Io,
    /// Rollback or failure cleanup was incomplete; recovery data is retained.
    RecoveryRequired,
    /// Publication completed, but internal staging cleanup failed.
    CleanupFailed,
}

/// A contextual failure, optionally containing retained recovery data.
///
/// [`WriterError::recovery_dir`] points to private staging when recovery or cleanup
/// is incomplete. Its `recovery.json` records each relative target, backup name,
/// staged name, and before/after digest. Backups that still exist contain original
/// bytes. Do not blindly retry or delete this directory before inspecting it and
/// resolving the reported recovery errors.
#[derive(Debug)]
pub struct WriterError {
    kind: WriterErrorKind,
    path: PathBuf,
    detail: String,
    source: Option<Box<dyn Error + Send + Sync>>,
    recovery_dir: Option<PathBuf>,
    recovery_errors: Vec<WriterError>,
}

impl WriterError {
    /// Returns the stable failure category.
    #[must_use]
    pub fn kind(&self) -> WriterErrorKind {
        self.kind
    }

    /// Returns the path associated with the failure.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the retained private staging directory, if one was created.
    #[must_use]
    pub fn recovery_dir(&self) -> Option<&Path> {
        self.recovery_dir.as_deref()
    }

    /// Returns failures encountered during rollback or cleanup.
    #[must_use]
    pub fn recovery_errors(&self) -> &[WriterError] {
        &self.recovery_errors
    }

    fn new(kind: WriterErrorKind, path: impl Into<PathBuf>, detail: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            detail: detail.into(),
            source: None,
            recovery_dir: None,
            recovery_errors: Vec::new(),
        }
    }

    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        let mut error = Self::new(WriterErrorKind::Io, path, operation);
        error.source = Some(Box::new(source));
        error
    }
}

impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.detail)?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        if let Some(directory) = &self.recovery_dir {
            write!(
                f,
                "; staging retained at {}; inspect recovery.json and backups before retrying",
                directory.display()
            )?;
        }
        for error in &self.recovery_errors {
            write!(f, "; recovery error: {error}")?;
        }
        Ok(())
    }
}

impl Error for WriterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|error| error as &dyn Error)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    generator: String,
    version: u32,
    files: Vec<LedgerEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    directories: Vec<String>,
}

#[derive(Debug)]
struct Ownership {
    files: BTreeMap<PathBuf, String>,
    directories: BTreeSet<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LedgerEntry {
    path: String,
    sha256: String,
}

#[derive(Debug, Eq, PartialEq)]
struct Snapshot {
    ledger_bytes: Option<Vec<u8>>,
    owned: BTreeMap<PathBuf, String>,
    present: BTreeSet<PathBuf>,
    owned_dirs: BTreeSet<PathBuf>,
}

#[derive(Debug)]
struct Operation {
    relative: PathBuf,
    before: Option<String>,
    after: Option<Vec<u8>>,
}

#[derive(Debug, Serialize)]
struct RecoveryEntry {
    target: PathBuf,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
    backup: Option<String>,
    staged: Option<String>,
    directory: bool,
}

#[derive(Debug)]
struct Applied {
    target: PathBuf,
    backup: Option<(PathBuf, String)>,
    installed_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplyEvent {
    AfterBackup,
    AfterInstall,
    DirectoryRemoved,
}

#[derive(Debug)]
struct Transaction {
    root: PathBuf,
    stage: Option<PathBuf>,
    stage_files: BTreeSet<PathBuf>,
    created_dirs: Vec<PathBuf>,
    removed_dirs: Vec<PathBuf>,
    applied: Vec<Applied>,
}

/// Writes a complete desired set of generated artifacts without clobbering users' files.
///
/// Accepted paths are exactly `Cargo.toml`, `wasm.toml`, `README.md`,
/// `wit/world.wit`, `wit/deps/<name>.wit`, `wit/deps/<package>/<name>.wit`,
/// and `src/<module>.rs` with optional module subdirectories.
/// WIT package and file stems also permit hyphens.
/// Module stems and directory names use lowercase ASCII letters, digits, and
/// underscores, do not start with digits, and are not Windows device names.
/// Empty components, `.`/`..`, backslashes, drive syntax, other artifact classes,
/// and the reserved ledger/staging names are rejected, even on Unix.
///
/// The map is the complete desired generation, not a patch. At most 4096 artifacts
/// and a 1 MiB serialized ownership ledger are accepted. Omitted unchanged owned
/// files are removed; empty dependency-package directories created by this writer
/// are pruned. Unrelated files and preexisting directories are preserved. Missing
/// retained files are recreated, and missing
/// stale files are ignored. Existing files without a valid ledger cannot be
/// adopted, even if their bytes happen to match.
///
/// The absent output directory and missing ancestors are created after preflight.
/// Output directory paths may be absolute or relative but must not contain `..`
/// or designate the filesystem root. Symlinks on touched paths are rejected.
///
/// # Errors
///
/// Validation failures do not mutate the output. Apply failures attempt complete
/// rollback, including the old ledger. An incomplete rollback retains staging and
/// returns [`WriterErrorKind::RecoveryRequired`]. A fully published generation whose
/// staging cleanup fails returns [`WriterErrorKind::CleanupFailed`], not success.
/// See the module documentation for concurrency, hard-link, and crash limitations.
pub fn write_crate(out_dir: &Path, files: &BTreeMap<PathBuf, String>) -> Result<(), WriterError> {
    write_with_hook(out_dir, files, &mut |_, _| Ok(()))
}

fn write_with_hook(
    out_dir: &Path,
    files: &BTreeMap<PathBuf, String>,
    hook: &mut impl FnMut(ApplyEvent, &Path) -> io::Result<()>,
) -> Result<(), WriterError> {
    if files.len() > MAX_FILES {
        return Err(WriterError::new(
            WriterErrorKind::InvalidPath,
            out_dir,
            "too many generated artifacts (maximum 4096)",
        ));
    }
    for path in files.keys() {
        validate_artifact(path)?;
    }
    let root = resolve_root(out_dir)?;
    let snapshot = preflight(&root, files)?;
    let operations = plan(files, &snapshot, &root)?;
    if operations.is_empty() {
        return Ok(());
    }
    let mut transaction = Transaction {
        root,
        stage: None,
        stage_files: BTreeSet::new(),
        created_dirs: Vec::new(),
        removed_dirs: Vec::new(),
        applied: Vec::new(),
    };
    if let Err(cause) = transaction.execute(files, &snapshot, &operations, hook) {
        let mut errors = transaction.rollback();
        if errors.is_empty() {
            errors.extend(transaction.cleanup_stage());
            if errors.is_empty() {
                errors.extend(transaction.cleanup_dirs());
            }
        }
        if errors.is_empty() {
            return Err(cause);
        }
        return Err(transaction.recovery_error(
            WriterErrorKind::RecoveryRequired,
            "generation failed; rollback or cleanup is incomplete",
            Some(cause),
            errors,
        ));
    }
    let errors = transaction.cleanup_stage();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(transaction.recovery_error(
            WriterErrorKind::CleanupFailed,
            "generation is fully published, but private staging cleanup failed",
            None,
            errors,
        ))
    }
}

fn validate_artifact(path: &Path) -> Result<(), WriterError> {
    let reject = || {
        WriterError::new(
            WriterErrorKind::InvalidPath,
            path,
            "expected an allowed portable generated-artifact path",
        )
    };
    let name = path.to_str().ok_or_else(reject)?;
    if matches!(
        name,
        "Cargo.toml" | "wasm.toml" | "README.md" | "wit/world.wit"
    ) {
        return Ok(());
    }
    if name.contains(['\\', ':']) || path.is_absolute() {
        return Err(reject());
    }
    if let Some(suffix) = name.strip_prefix("wit/deps/") {
        let parts: Vec<_> = suffix.split('/').collect();
        if !(1..=2).contains(&parts.len()) {
            return Err(reject());
        }
        for (index, part) in parts.iter().enumerate() {
            let stem = if index + 1 == parts.len() {
                part.strip_suffix(".wit").ok_or_else(reject)?
            } else {
                part
            };
            if !portable_module_name(&stem.replace('-', "_")) {
                return Err(reject());
            }
        }
        return Ok(());
    }
    let suffix = name.strip_prefix("src/").ok_or_else(reject)?;
    let mut components = suffix.split('/').peekable();
    while let Some(component) = components.next() {
        let stem = if components.peek().is_none() {
            component.strip_suffix(".rs").ok_or_else(reject)?
        } else {
            component
        };
        if !portable_module_name(stem) {
            return Err(reject());
        }
    }
    Ok(())
}

fn portable_module_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte == b'_');
    let valid_rest =
        bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    let device = matches!(name, "con" | "prn" | "aux" | "nul" | "conin" | "conout")
        || name
            .strip_prefix("com")
            .or_else(|| name.strip_prefix("lpt"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
    valid_first && valid_rest && !device
}

fn resolve_root(path: &Path) -> Result<PathBuf, WriterError> {
    if path.as_os_str().is_empty() || path.components().any(|part| part == Component::ParentDir) {
        return Err(WriterError::new(
            WriterErrorKind::InvalidPath,
            path,
            "output path must be nonempty and contain no parent traversal",
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| WriterError::io("read current directory", path, error))?
            .join(path)
    };
    let root: PathBuf = absolute
        .components()
        .filter(|part| *part != Component::CurDir)
        .collect();
    if !root
        .components()
        .any(|part| matches!(part, Component::Normal(_)))
    {
        return Err(WriterError::new(
            WriterErrorKind::InvalidPath,
            root,
            "the filesystem root cannot be an output directory",
        ));
    }
    Ok(root)
}

fn inspect(path: &Path) -> Result<Option<Metadata>, WriterError> {
    let ancestors: Vec<_> = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect();
    for ancestor in ancestors.into_iter().rev() {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(WriterError::io("inspect path", ancestor, error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(WriterError::new(
                WriterErrorKind::Symlink,
                ancestor,
                "symbolic links are forbidden on touched paths",
            ));
        }
        if ancestor != path && !metadata.is_dir() {
            return Err(WriterError::new(
                WriterErrorKind::UnexpectedFileType,
                ancestor,
                "expected a directory ancestor",
            ));
        }
        if ancestor == path {
            return Ok(Some(metadata));
        }
    }
    Ok(None)
}

fn inspect_file(path: &Path) -> Result<bool, WriterError> {
    match inspect(path)? {
        Some(metadata) if !metadata.is_file() => Err(WriterError::new(
            WriterErrorKind::UnexpectedFileType,
            path,
            "expected a regular file",
        )),
        Some(_) => Ok(true),
        None => Ok(false),
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_digest(path: &Path) -> Result<String, WriterError> {
    fs::read(path)
        .map(|bytes| digest(&bytes))
        .map_err(|error| WriterError::io("read file for digest", path, error))
}

fn read_ledger(root: &Path) -> Result<Option<Vec<u8>>, WriterError> {
    let path = root.join(LEDGER_FILE);
    if !inspect_file(&path)? {
        return Ok(None);
    }
    let file = File::open(&path)
        .map_err(|error| WriterError::io("open ownership ledger", &path, error))?;
    let mut bytes = Vec::new();
    file.take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| WriterError::io("read ownership ledger", &path, error))?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err(WriterError::new(
            WriterErrorKind::InvalidLedger,
            path,
            "ownership ledger exceeds 1 MiB",
        ));
    }
    Ok(Some(bytes))
}

fn parse_ledger(bytes: &[u8], root: &Path) -> Result<Ownership, WriterError> {
    let path = root.join(LEDGER_FILE);
    let invalid = |detail| WriterError::new(WriterErrorKind::InvalidLedger, &path, detail);
    let ledger: Ledger = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("invalid ownership ledger: {error}")))?;
    if ledger.generator != "autostamp"
        || ledger.version != 1
        || ledger.files.len() > MAX_FILES
        || ledger.directories.len() > MAX_FILES
    {
        return Err(invalid(
            "unsupported ownership ledger identity, version, or size".into(),
        ));
    }
    let mut owned = BTreeMap::new();
    for entry in ledger.files {
        let artifact = PathBuf::from(entry.path);
        validate_artifact(&artifact).map_err(|error| invalid(error.to_string()))?;
        if entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(invalid("expected a lowercase SHA-256 digest".into()));
        }
        if owned.insert(artifact, entry.sha256).is_some() {
            return Err(invalid("duplicate ownership entry".into()));
        }
    }
    let mut directories = BTreeSet::new();
    for name in ledger.directories {
        let directory = PathBuf::from(&name);
        if !dependency_directory(&directory)
            || !owned.keys().any(|file| file.starts_with(&directory))
            || !directories.insert(directory)
        {
            return Err(invalid(
                "invalid or duplicate owned dependency directory".into(),
            ));
        }
    }
    Ok(Ownership {
        files: owned,
        directories,
    })
}

fn dependency_directory(path: &Path) -> bool {
    path.to_str()
        .and_then(|name| name.strip_prefix("wit/deps/"))
        .is_some_and(|name| portable_module_name(&name.replace('-', "_")))
}

fn preflight(root: &Path, files: &BTreeMap<PathBuf, String>) -> Result<Snapshot, WriterError> {
    if inspect(root)?.is_some_and(|metadata| !metadata.is_dir()) {
        return Err(WriterError::new(
            WriterErrorKind::UnexpectedFileType,
            root,
            "expected an output directory",
        ));
    }
    let ledger_bytes = read_ledger(root)?;
    let ownership = match &ledger_bytes {
        Some(bytes) => parse_ledger(bytes, root)?,
        None => Ownership {
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
        },
    };
    let owned = ownership.files;
    for relative in &ownership.directories {
        let path = root.join(relative);
        if inspect(&path)?.is_some_and(|metadata| !metadata.is_dir()) {
            return Err(WriterError::new(
                WriterErrorKind::UnexpectedFileType,
                path,
                "expected owned dependency directory",
            ));
        }
    }
    let mut present = BTreeSet::new();
    for (relative, expected) in &owned {
        let target = root.join(relative);
        if inspect_file(&target)? {
            if file_digest(&target)? != *expected {
                return Err(WriterError::new(
                    WriterErrorKind::Modified,
                    target,
                    "owned content was edited; refusing to overwrite or delete it",
                ));
            }
            present.insert(relative.clone());
        }
    }
    for relative in files.keys() {
        let target = root.join(relative);
        if !owned.contains_key(relative) && inspect(&target)?.is_some() {
            return Err(WriterError::new(
                WriterErrorKind::Conflict,
                target,
                "existing path is not owned by a valid prior autostamp ledger",
            ));
        }
    }
    Ok(Snapshot {
        ledger_bytes,
        owned,
        present,
        owned_dirs: ownership.directories,
    })
}

fn plan(
    files: &BTreeMap<PathBuf, String>,
    snapshot: &Snapshot,
    root: &Path,
) -> Result<Vec<Operation>, WriterError> {
    let mut entries = Vec::new();
    let mut operations = Vec::new();
    for (relative, content) in files {
        let hash = digest(content.as_bytes());
        entries.push(LedgerEntry {
            path: relative.to_string_lossy().replace('\\', "/"),
            sha256: hash.clone(),
        });
        if !snapshot.present.contains(relative) || snapshot.owned.get(relative) != Some(&hash) {
            operations.push(Operation {
                relative: relative.clone(),
                before: snapshot
                    .owned
                    .get(relative)
                    .filter(|_| snapshot.present.contains(relative))
                    .cloned(),
                after: Some(content.as_bytes().to_vec()),
            });
        }
    }
    for relative in &snapshot.present {
        if !files.contains_key(relative) {
            operations.push(Operation {
                relative: relative.clone(),
                before: snapshot.owned.get(relative).cloned(),
                after: None,
            });
        }
    }
    operations.sort_by(|left, right| left.relative.cmp(&right.relative));
    let mut directories = BTreeSet::new();
    for relative in files.keys() {
        if let Some(parent) = relative.parent()
            && dependency_directory(parent)
            && (snapshot.owned_dirs.contains(parent) || inspect(&root.join(parent))?.is_none())
        {
            directories.insert(parent.to_string_lossy().replace('\\', "/"));
        }
    }
    let mut ledger_bytes = serde_json::to_vec_pretty(&Ledger {
        generator: "autostamp".into(),
        version: 1,
        files: entries,
        directories: directories.into_iter().collect(),
    })
    .map_err(|error| {
        WriterError::new(
            WriterErrorKind::InvalidLedger,
            root.join(LEDGER_FILE),
            format!("serialize ownership ledger: {error}"),
        )
    })?;
    ledger_bytes.push(b'\n');
    if ledger_bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err(WriterError::new(
            WriterErrorKind::InvalidLedger,
            root.join(LEDGER_FILE),
            "generated ownership ledger would exceed 1 MiB",
        ));
    }
    if snapshot.ledger_bytes.as_ref() != Some(&ledger_bytes) {
        operations.push(Operation {
            relative: LEDGER_FILE.into(),
            before: snapshot.ledger_bytes.as_deref().map(digest),
            after: Some(ledger_bytes),
        });
    }
    Ok(operations)
}

fn check_expected(path: &Path, expected: Option<&str>) -> Result<(), WriterError> {
    let exists = inspect_file(path)?;
    match (exists, expected) {
        (false, None) => Ok(()),
        (true, Some(hash)) if file_digest(path)? == hash => Ok(()),
        _ => Err(WriterError::new(
            WriterErrorKind::ConcurrentChange,
            path,
            "path no longer matches the preflight snapshot",
        )),
    }
}

impl Transaction {
    fn execute(
        &mut self,
        files: &BTreeMap<PathBuf, String>,
        snapshot: &Snapshot,
        operations: &[Operation],
        hook: &mut impl FnMut(ApplyEvent, &Path) -> io::Result<()>,
    ) -> Result<(), WriterError> {
        self.ensure_directory(&self.root.clone())?;
        let stage = self.create_stage()?;
        self.stage = Some(stage.clone());
        let mut recovery = Vec::new();
        for (index, operation) in operations.iter().enumerate() {
            let staged = operation.after.as_ref().map(|_| format!("file-{index}"));
            let backup = operation.before.as_ref().map(|_| format!("backup-{index}"));
            if let (Some(bytes), Some(name)) = (&operation.after, &staged) {
                self.stage_file(&stage.join(name), bytes)?;
            }
            recovery.push(RecoveryEntry {
                target: operation.relative.clone(),
                before_sha256: operation.before.clone(),
                after_sha256: operation.after.as_deref().map(digest),
                backup,
                staged,
                directory: false,
            });
        }
        let stale_dirs: Vec<_> = snapshot
            .owned_dirs
            .iter()
            .filter(|directory| !files.keys().any(|file| file.starts_with(directory)))
            .collect();
        for directory in &stale_dirs {
            recovery.push(RecoveryEntry {
                target: (*directory).clone(),
                before_sha256: None,
                after_sha256: None,
                backup: None,
                staged: None,
                directory: true,
            });
        }
        let journal = serde_json::to_vec_pretty(&recovery).map_err(|error| {
            WriterError::new(
                WriterErrorKind::Io,
                stage.join(JOURNAL_FILE),
                format!("serialize recovery journal: {error}"),
            )
        })?;
        self.stage_file(&stage.join(JOURNAL_FILE), &journal)?;
        for operation in operations {
            if operation.after.is_some() {
                let target = self.root.join(&operation.relative);
                if let Some(parent) = target.parent() {
                    self.ensure_directory(parent)?;
                }
            }
        }
        if preflight(&self.root, files)? != *snapshot {
            return Err(WriterError::new(
                WriterErrorKind::ConcurrentChange,
                &self.root,
                "ownership changed while staging the generation",
            ));
        }
        for (index, operation) in operations.iter().enumerate() {
            let mut applied = Applied {
                target: self.root.join(&operation.relative),
                backup: None,
                installed_digest: None,
            };
            let result = self.apply_one(&stage, index, operation, &mut applied, hook);
            self.applied.push(applied);
            result?;
        }
        for relative in stale_dirs {
            let directory = self.root.join(relative);
            if inspect(&directory)?.is_some() {
                let mut entries = fs::read_dir(&directory).map_err(|error| {
                    WriterError::io("inspect stale dependency directory", &directory, error)
                })?;
                if entries
                    .next()
                    .transpose()
                    .map_err(|error| {
                        WriterError::io("read stale dependency directory", &directory, error)
                    })?
                    .is_none()
                {
                    fs::remove_dir(&directory).map_err(|error| {
                        WriterError::io("remove stale dependency directory", &directory, error)
                    })?;
                    self.removed_dirs.push(directory.clone());
                    hook(ApplyEvent::DirectoryRemoved, &directory).map_err(|error| {
                        WriterError::io("apply hook after directory removal", &directory, error)
                    })?;
                }
            }
        }
        Ok(())
    }

    fn ensure_directory(&mut self, path: &Path) -> Result<(), WriterError> {
        let ancestors: Vec<_> = path
            .ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .collect();
        for ancestor in ancestors.into_iter().rev() {
            match inspect(ancestor)? {
                Some(metadata) if metadata.is_dir() => {}
                Some(_) => {
                    return Err(WriterError::new(
                        WriterErrorKind::UnexpectedFileType,
                        ancestor,
                        "expected a directory",
                    ));
                }
                None => {
                    fs::create_dir(ancestor)
                        .map_err(|error| WriterError::io("create directory", ancestor, error))?;
                    self.created_dirs.push(ancestor.to_path_buf());
                }
            }
        }
        Ok(())
    }

    fn create_stage(&self) -> Result<PathBuf, WriterError> {
        for _ in 0..100 {
            let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let stage = self.root.join(format!(
                ".autostamp-stage-{}-{sequence}",
                std::process::id()
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&stage) {
                Ok(()) => return Ok(stage),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(WriterError::io(
                        "create private staging directory",
                        &stage,
                        error,
                    ));
                }
            }
        }
        Err(WriterError::new(
            WriterErrorKind::Io,
            &self.root,
            "could not allocate a unique staging directory after 100 attempts",
        ))
    }

    fn stage_file(&mut self, path: &Path, bytes: &[u8]) -> Result<(), WriterError> {
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| WriterError::io("create staged file", path, error))?;
        self.stage_files.insert(path.to_path_buf());
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| WriterError::io("write staged file", path, error))
    }

    fn apply_one(
        &mut self,
        stage: &Path,
        index: usize,
        operation: &Operation,
        applied: &mut Applied,
        hook: &mut impl FnMut(ApplyEvent, &Path) -> io::Result<()>,
    ) -> Result<(), WriterError> {
        check_expected(&applied.target, operation.before.as_deref())?;
        if let Some(hash) = &operation.before {
            let backup = stage.join(format!("backup-{index}"));
            check_expected(&backup, None)?;
            fs::rename(&applied.target, &backup).map_err(|error| {
                WriterError::io("move owned file to backup", &applied.target, error)
            })?;
            self.stage_files.insert(backup.clone());
            applied.backup = Some((backup, hash.clone()));
            hook(ApplyEvent::AfterBackup, &applied.target).map_err(|error| {
                WriterError::io("apply hook after backup", &applied.target, error)
            })?;
        }
        if let Some(bytes) = &operation.after {
            let staged = stage.join(format!("file-{index}"));
            let hash = digest(bytes);
            check_expected(&staged, Some(&hash))?;
            check_expected(&applied.target, None)?;
            // Hard links publish staged bytes atomically without replacing a new conflict.
            fs::hard_link(&staged, &applied.target)
                .map_err(|error| WriterError::io("publish staged file", &applied.target, error))?;
            applied.installed_digest = Some(hash);
            hook(ApplyEvent::AfterInstall, &applied.target).map_err(|error| {
                WriterError::io("apply hook after install", &applied.target, error)
            })?;
        }
        Ok(())
    }

    fn rollback(&self) -> Vec<WriterError> {
        let mut errors = Vec::new();
        for directory in self.removed_dirs.iter().rev() {
            let restore = || -> Result<(), WriterError> {
                if inspect(directory)?.is_some() {
                    return Err(WriterError::new(
                        WriterErrorKind::ConcurrentChange,
                        directory,
                        "removed dependency directory was concurrently recreated",
                    ));
                }
                fs::create_dir(directory).map_err(|error| {
                    WriterError::io("restore dependency directory", directory, error)
                })
            };
            if let Err(error) = restore() {
                errors.push(error);
            }
        }
        for applied in self.applied.iter().rev() {
            if let Err(error) = self.rollback_one(applied) {
                errors.push(error);
            }
        }
        errors
    }

    fn rollback_one(&self, applied: &Applied) -> Result<(), WriterError> {
        if let Some(expected) = &applied.installed_digest
            && inspect_file(&applied.target)?
        {
            check_expected(&applied.target, Some(expected))?;
            fs::remove_file(&applied.target).map_err(|error| {
                WriterError::io(
                    "remove installed file during rollback",
                    &applied.target,
                    error,
                )
            })?;
        }
        if let Some((backup, expected)) = &applied.backup {
            check_expected(backup, Some(expected))?;
            check_expected(&applied.target, None)?;
            fs::hard_link(backup, &applied.target).map_err(|error| {
                WriterError::io("restore original file", &applied.target, error)
            })?;
        }
        Ok(())
    }

    fn cleanup_stage(&self) -> Vec<WriterError> {
        let Some(stage) = &self.stage else {
            return Vec::new();
        };
        let journal = stage.join(JOURNAL_FILE);
        let mut errors = Vec::new();
        for path in self.stage_files.iter().filter(|path| **path != journal) {
            if let Err(error) = remove_private_file(path) {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            if self.stage_files.contains(&journal)
                && let Err(error) = remove_private_file(&journal)
            {
                errors.push(error);
            }
            if errors.is_empty()
                && let Err(error) = remove_empty_directory(stage)
            {
                errors.push(error);
            }
        }
        errors
    }

    fn cleanup_dirs(&self) -> Vec<WriterError> {
        let mut errors = Vec::new();
        for path in self.created_dirs.iter().rev() {
            if let Err(error) = remove_empty_directory(path) {
                errors.push(error);
            }
        }
        errors
    }

    fn recovery_error(
        &self,
        kind: WriterErrorKind,
        detail: &'static str,
        cause: Option<WriterError>,
        errors: Vec<WriterError>,
    ) -> WriterError {
        let mut error = WriterError::new(kind, &self.root, detail);
        error.source = cause.map(|cause| Box::new(cause) as Box<dyn Error + Send + Sync>);
        error.recovery_dir = self.stage.clone();
        error.recovery_errors = errors;
        error
    }
}

fn remove_private_file(path: &Path) -> Result<(), WriterError> {
    if inspect_file(path)? {
        fs::remove_file(path)
            .map_err(|error| WriterError::io("remove private staging file", path, error))?;
    }
    Ok(())
}

fn remove_empty_directory(path: &Path) -> Result<(), WriterError> {
    match inspect(path)? {
        Some(metadata) if metadata.is_dir() => fs::remove_dir(path)
            .map_err(|error| WriterError::io("remove empty created directory", path, error)),
        Some(_) => Err(WriterError::new(
            WriterErrorKind::UnexpectedFileType,
            path,
            "refusing to remove a non-directory during cleanup",
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
