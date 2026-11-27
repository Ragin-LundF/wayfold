//! Archives (F08): create ZIP/TAR/TAR.GZ/TAR.ZST; list, preview and safely extract
//! ZIP/TAR/GZ/ZST/TAR.GZ/TAR.ZST/7z/RAR (RAR extract-only via `unrar`, whose license forbids
//! building a RAR compressor).
//!
//! Extraction is all-or-nothing: entries are validated (no absolute paths, no `..`, no
//! special files, links must stay inside), limits are enforced while writing, output is
//! staged in a hidden temp folder next to the destination and published under a free name
//! (`ops::unique_name`), never over an existing item. Any failure or cancellation removes
//! the staging folder. Passwords are passed through to the codec and never logged.
//!
//! All functions block on I/O; `start_extract`/`start_create` run them on the worker pools.

mod create;
mod extract;
mod read;
#[cfg(test)]
mod tests;

pub use create::create_archive;
pub use extract::extract_archive;
pub use read::{list_entries, preview_entry};

use crate::pool::{self, CancelToken};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    Zip,
    Tar,
    TarGz,
    TarZst,
    /// A single gzip-compressed file.
    Gz,
    /// A single zstd-compressed file.
    Zst,
    SevenZ,
    Rar,
}

/// (suffix, format), longest suffixes first so `.tar.gz` wins over `.gz`.
const SUFFIXES: [(&str, ArchiveFormat); 10] = [
    (".tar.gz", ArchiveFormat::TarGz),
    (".tar.zst", ArchiveFormat::TarZst),
    (".tgz", ArchiveFormat::TarGz),
    (".tzst", ArchiveFormat::TarZst),
    (".zip", ArchiveFormat::Zip),
    (".tar", ArchiveFormat::Tar),
    (".gz", ArchiveFormat::Gz),
    (".zst", ArchiveFormat::Zst),
    (".7z", ArchiveFormat::SevenZ),
    (".rar", ArchiveFormat::Rar),
];

impl ArchiveFormat {
    /// Formats that can be created.
    pub const CREATE: [ArchiveFormat; 4] = [ArchiveFormat::Zip, ArchiveFormat::Tar, ArchiveFormat::TarGz, ArchiveFormat::TarZst];

    /// Format by file name suffix (case-insensitive).
    pub fn detect(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_string_lossy().to_lowercase();
        SUFFIXES.iter().find(|(s, _)| name.ends_with(s) && name.len() > s.len()).map(|&(_, f)| f)
    }

    pub fn can_create(self) -> bool {
        Self::CREATE.contains(&self)
    }

    /// ZIP (ZipCrypto/AES), 7z (AES) and RAR accept a password for extraction.
    pub fn supports_password(self) -> bool {
        matches!(self, Self::Zip | Self::SevenZ | Self::Rar)
    }

    /// One compressed file instead of a container: extracts to a file, not a folder.
    pub fn is_single_stream(self) -> bool {
        matches!(self, Self::Gz | Self::Zst)
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => ".zip",
            Self::Tar => ".tar",
            Self::TarGz => ".tar.gz",
            Self::TarZst => ".tar.zst",
            Self::Gz => ".gz",
            Self::Zst => ".zst",
            Self::SevenZ => ".7z",
            Self::Rar => ".rar",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Zip => "ZIP",
            Self::Tar => "TAR",
            Self::TarGz => "TAR.GZ",
            Self::TarZst => "TAR.ZST",
            Self::Gz => "GZ",
            Self::Zst => "ZST",
            Self::SevenZ => "7z",
            Self::Rar => "RAR",
        }
    }
}

/// Output name for an archive: its file name without the archive suffix
/// (`photos.tar.gz` → `photos`, `notes.txt.gz` → `notes.txt`).
pub fn output_name(archive: &Path) -> OsString {
    let name = archive.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let lower = name.to_lowercase();
    match SUFFIXES.iter().find(|(s, _)| lower.ends_with(s) && lower.len() > s.len()) {
        Some((s, _)) => name[..name.len() - s.len()].into(),
        None => format!("{name} contents").into(),
    }
}

/// Caps on what one extraction may produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_total_bytes: u64,
    pub max_entries: u64,
    /// Expanded/compressed ratio, checked per entry (when the format records the compressed
    /// size) and for the whole archive, once the output passes `RATIO_FLOOR` bytes.
    pub max_ratio: u64,
}

/// Below this many expanded bytes the ratio is not checked (tiny files compress extremely well).
pub const RATIO_FLOOR: u64 = 16 << 20;

impl Default for Limits {
    fn default() -> Self {
        Self { max_total_bytes: 64 << 30, max_entries: 1_000_000, max_ratio: 1000 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    HardLink,
    /// Device, FIFO, socket or unknown type: never extracted.
    Special,
}

/// One entry for browsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Expanded size when the format records it.
    pub size: Option<u64>,
    pub encrypted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<ArchiveEntry>,
    /// More entries exist than `read::MAX_LISTED`.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchiveError {
    Unsupported(String),
    PasswordRequired,
    WrongPassword,
    /// An entry would escape the destination or is a special file.
    Unsafe {
        entry: String,
        reason: &'static str,
    },
    LimitExceeded(String),
    Corrupt(String),
    Io(String),
    Cancelled,
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(s) => write!(f, "Unsupported archive: {s}"),
            Self::PasswordRequired => write!(f, "This archive is encrypted: enter its password"),
            Self::WrongPassword => write!(f, "Wrong password"),
            Self::Unsafe { entry, reason } => write!(f, "Unsafe entry \"{entry}\": {reason}"),
            Self::LimitExceeded(s) => write!(f, "Extraction limit exceeded: {s}"),
            Self::Corrupt(s) => write!(f, "Damaged archive: {s}"),
            Self::Io(s) => write!(f, "{s}"),
            Self::Cancelled => write!(f, "Cancelled"),
        }
    }
}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub bytes: u64,
    pub entries: u64,
    pub current: String,
}

#[derive(Clone, Debug)]
pub enum ArchiveEvent {
    Progress(Progress),
    /// Final: the published archive or folder/file, or why nothing was published.
    Done(Result<PathBuf, ArchiveError>),
}

pub struct ArchiveJob {
    pub events: async_channel::Receiver<ArchiveEvent>,
    pub cancel: CancelToken,
}

/// No `Debug`: it carries a password.
pub struct ExtractRequest {
    pub archive: PathBuf,
    /// Folder that receives the output.
    pub dest_dir: PathBuf,
    /// Name of the published folder (or file for single-stream formats); made unique.
    pub name: OsString,
    pub password: Option<String>,
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub struct CreateRequest {
    pub sources: Vec<PathBuf>,
    /// Wanted archive path; a free name is chosen if it exists.
    pub dest: PathBuf,
    pub format: ArchiveFormat,
}

pub fn start_extract(req: ExtractRequest) -> ArchiveJob {
    let dir = req.dest_dir.clone();
    spawn_job(&dir, move |cancel, progress| {
        extract_archive(&req.archive, &req.dest_dir, &req.name, req.password.as_deref(), req.limits, cancel, progress)
    })
}

pub fn start_create(req: CreateRequest) -> ArchiveJob {
    let dir = req.dest.parent().map(PathBuf::from).unwrap_or_default();
    spawn_job(&dir, move |cancel, progress| create_archive(&req.sources, &req.dest, req.format, cancel, progress))
}

type ProgressFn<'a> = &'a (dyn Fn(Progress) + Sync);

fn spawn_job(dir: &Path, job: impl FnOnce(&CancelToken, ProgressFn) -> Result<PathBuf, ArchiveError> + Send + 'static) -> ArchiveJob {
    let (tx, rx) = async_channel::unbounded();
    let cancel = CancelToken::new();
    let token = cancel.clone();
    pool::for_path(dir).execute(move || {
        let progress_tx = tx.clone();
        let result = job(&token, &move |p| {
            let _ = progress_tx.try_send(ArchiveEvent::Progress(p));
        });
        let _ = tx.send_blocking(ArchiveEvent::Done(result));
    });
    ArchiveJob { events: rx, cancel }
}

/// Publish `staged` as `dir/name` (or the first free `name N`), never replacing anything.
fn publish(staged: &Path, dir: &Path, name: &std::ffi::OsStr) -> Result<PathBuf, ArchiveError> {
    let mut target = dir.join(name);
    for _ in 0..16 {
        if crate::ops::exists(&target) {
            target = crate::ops::unique_name(dir, name);
        }
        match crate::ops::rename_excl(staged, &target) {
            Ok(()) => return Ok(target),
            // Lost a race for the free name: pick the next one.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ArchiveError::Io("no free destination name".into()))
}

/// Hidden temp name next to the destination; the prefix marks it as Wayfold scratch.
fn temp_path(dir: &Path) -> PathBuf {
    dir.join(format!(".wayfold-tmp-{}", crate::ops::next_id()))
}
