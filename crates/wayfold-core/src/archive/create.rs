//! Archive creation: ZIP, TAR, TAR.GZ, TAR.ZST. Written to a hidden temp file and
//! published under a free name; symlinks are stored as links, never followed.

use super::{ArchiveError, ArchiveFormat, Progress, ProgressFn, publish, temp_path};
use crate::pool::CancelToken;
use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Archive `sources` (files or folders, stored under their own names) as `dest`.
pub fn create_archive(
    sources: &[PathBuf],
    dest: &Path,
    format: ArchiveFormat,
    cancel: &CancelToken,
    progress: ProgressFn,
) -> Result<PathBuf, ArchiveError> {
    if !format.can_create() {
        return Err(ArchiveError::Unsupported(format!("cannot create {} archives", format.label())));
    }
    let dir = dest.parent().ok_or_else(|| ArchiveError::Io("invalid destination".into()))?;
    let name = dest.file_name().ok_or_else(|| ArchiveError::Io("invalid destination".into()))?;
    let temp = temp_path(dir);
    let items = collect(sources, &temp)?;
    let result = write(&temp, &items, format, cancel, progress).and_then(|()| publish(&temp, dir, name));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

struct Item {
    path: PathBuf,
    name: String,
    md: Metadata,
}

/// All paths to store, parents before children, without following symlinks.
fn collect(sources: &[PathBuf], skip: &Path) -> Result<Vec<Item>, ArchiveError> {
    let mut items = Vec::new();
    for src in sources {
        let base = src.parent().unwrap_or(Path::new("/"));
        let mut stack = vec![src.clone()];
        while let Some(path) = stack.pop() {
            if path == skip {
                continue;
            }
            let md = fs::symlink_metadata(&path)?;
            let ft = md.file_type();
            if !(ft.is_file() || ft.is_dir() || ft.is_symlink()) {
                return Err(ArchiveError::Unsupported(format!("cannot archive special file {}", path.display())));
            }
            if ft.is_dir() {
                let mut children: Vec<PathBuf> = fs::read_dir(&path)?.map(|e| e.map(|e| e.path())).collect::<io::Result<_>>()?;
                children.sort_by(|a, b| b.cmp(a));
                stack.extend(children);
            }
            let name = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().into_owned();
            items.push(Item { path, name, md });
        }
    }
    Ok(items)
}

fn write(temp: &Path, items: &[Item], format: ArchiveFormat, cancel: &CancelToken, progress: ProgressFn) -> Result<(), ArchiveError> {
    let out = BufWriter::new(File::options().write(true).create_new(true).open(temp)?);
    let mut counter = Counter { cancel, progress, bytes: 0, entries: 0 };
    let result = match format {
        ArchiveFormat::Zip => write_zip(out, items, &mut counter),
        ArchiveFormat::Tar => write_tar(out, items, &mut counter).and_then(|mut w| Ok(w.flush()?)),
        ArchiveFormat::TarGz => write_tar(flate2::write::GzEncoder::new(out, flate2::Compression::default()), items, &mut counter)
            .and_then(|w| Ok(w.finish()?.flush()?)),
        ArchiveFormat::TarZst => write_tar(zstd::Encoder::new(out, 3)?, items, &mut counter).and_then(|w| Ok(w.finish()?.flush()?)),
        _ => unreachable!("checked by can_create"),
    };
    // Codec errors caused by our cancelled reader surface as I/O errors.
    if cancel.is_cancelled() { Err(ArchiveError::Cancelled) } else { result }
}

struct Counter<'a> {
    cancel: &'a CancelToken,
    progress: ProgressFn<'a>,
    bytes: u64,
    entries: u64,
}

impl Counter<'_> {
    fn next(&mut self, item: &Item) -> Result<(), ArchiveError> {
        if self.cancel.is_cancelled() {
            return Err(ArchiveError::Cancelled);
        }
        self.entries += 1;
        (self.progress)(Progress { bytes: self.bytes, entries: self.entries, current: item.name.clone() });
        Ok(())
    }
}

/// File reader that stops on cancel and counts bytes for progress.
struct Tracked<'a, 'b> {
    file: File,
    counter: &'a mut Counter<'b>,
}

impl Read for Tracked<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.counter.cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let n = self.file.read(buf)?;
        self.counter.bytes += n as u64;
        Ok(n)
    }
}

fn write_zip(out: impl Write + io::Seek, items: &[Item], counter: &mut Counter) -> Result<(), ArchiveError> {
    use super::read::zip_err;
    let mut zw = zip::ZipWriter::new(out);
    for item in items {
        counter.next(item)?;
        let mode = item.md.permissions().mode() & 0o777;
        let opts = zip::write::SimpleFileOptions::default().unix_permissions(mode);
        if item.md.is_dir() {
            zw.add_directory(format!("{}/", item.name), opts).map_err(zip_err)?;
        } else if item.md.file_type().is_symlink() {
            let target = fs::read_link(&item.path)?;
            zw.add_symlink(&item.name, target.to_string_lossy(), opts).map_err(zip_err)?;
        } else {
            let opts = opts.compression_method(zip::CompressionMethod::Deflated).large_file(item.md.len() >= u32::MAX as u64);
            zw.start_file(&item.name, opts).map_err(zip_err)?;
            io::copy(&mut Tracked { file: File::open(&item.path)?, counter }, &mut zw)?;
        }
    }
    zw.finish().map_err(zip_err)?.flush()?;
    Ok(())
}

fn write_tar<W: Write>(out: W, items: &[Item], counter: &mut Counter) -> Result<W, ArchiveError> {
    let mut b = tar::Builder::new(out);
    b.follow_symlinks(false);
    for item in items {
        counter.next(item)?;
        if item.md.is_file() {
            let mut h = tar::Header::new_gnu();
            h.set_metadata(&item.md);
            b.append_data(&mut h, &item.name, Tracked { file: File::open(&item.path)?, counter })?;
        } else {
            b.append_path_with_name(&item.path, &item.name)?;
        }
    }
    Ok(b.into_inner()?)
}
