//! Read-only filesystem access: entries, progressive listing, volumes, watching.

mod list;
pub mod size;
pub mod tags;
mod volumes;

pub use list::{BATCH, list_dir, stat_entry};
pub use volumes::{Mount, is_network_path, mounts, refresh_mounts};

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Dir,
    /// Symlink; `to_dir` is whether its (existing) target is a directory.
    Symlink {
        to_dir: bool,
        broken: bool,
    },
    Other,
}

/// Hidden and system classification are independent filters (N04).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Visibility {
    Normal,
    /// Dot-prefixed name.
    Hidden,
    /// macOS `UF_HIDDEN` flag on a non-dot name (e.g. ~/Library, /private).
    System,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: OsString,
    pub kind: Kind,
    /// Logical size for files and symlinks; `None` for folders (computed on demand).
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub visibility: Visibility,
    /// Stat failure, shown instead of hiding the entry.
    pub error: Option<String>,
    /// Finder tags (v0.3), read from `com.apple.metadata:_kMDItemUserTags`.
    pub tags: Vec<tags::Tag>,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir | Kind::Symlink { to_dir: true, .. })
    }

    pub fn display_name(&self) -> String {
        self.name.to_string_lossy().into_owned()
    }

    /// Extension in lowercase, empty when none.
    pub fn extension(&self) -> String {
        self.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
    }

    /// Short human kind label for the Kind column.
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            Kind::Dir => "Folder",
            Kind::Symlink { broken: true, .. } => "Broken alias",
            Kind::Symlink { to_dir: true, .. } => "Folder alias",
            Kind::Symlink { .. } => "Alias",
            Kind::Other => "Special",
            Kind::File => kind_for_extension(&self.extension()),
        }
    }
}

pub fn kind_for_extension(ext: &str) -> &'static str {
    match ext {
        "md" | "markdown" => "Markdown",
        "pdf" => "PDF",
        "html" | "htm" => "HTML",
        "csv" | "tsv" => "CSV",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "heic" | "svg" => "Image",
        "zip" | "tar" | "gz" | "tgz" | "zst" | "7z" | "rar" | "xz" | "bz2" => "Archive",
        "rs" | "kt" | "java" | "js" | "ts" | "py" | "c" | "h" | "cpp" | "go" | "swift" | "sh" | "toml" | "json" | "yaml" | "yml"
        | "xml" | "css" => "Code",
        "txt" | "log" | "" => "Text",
        "app" => "Application",
        "dmg" | "iso" => "Disk image",
        "mp3" | "wav" | "flac" | "m4a" | "aac" => "Audio",
        "mp4" | "mov" | "mkv" | "avi" | "webm" => "Video",
        _ => "Document",
    }
}

/// Human-readable size, decimal units like Finder.
pub fn format_size(n: u64) -> String {
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.2} GB", f / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1} MB", f / 1e6)
    } else if n >= 1_000 {
        format!("{:.1} KB", f / 1e3)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_decimal_units() {
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(4200), "4.2 KB");
        assert_eq!(format_size(2_450_000), "2.5 MB");
        assert_eq!(format_size(8_500_000_000), "8.50 GB");
    }
}
