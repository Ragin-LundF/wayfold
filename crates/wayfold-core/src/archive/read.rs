//! Format readers: one entry visitor for streamable formats (ZIP, TAR family, GZ, ZST, 7z),
//! RAR through `unrar`'s header cursor, plus listing and bounded previews.

use super::{output_name, ArchiveEntry, ArchiveError, ArchiveFormat, EntryKind, Listing};
use crate::pool::CancelToken;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

/// Listing stops here; the UI shows that more entries exist.
pub const MAX_LISTED: usize = 200_000;
/// Link targets longer than this are rejected as damaged.
const MAX_LINK_LEN: u64 = 4096;
/// RAR previews read the whole entry into memory, so only small entries are previewed.
const MAX_RAR_PREVIEW_ENTRY: u64 = 8 << 20;

/// What the visitor hands out per entry. For symlinks and hardlinks `link` holds the target.
pub(super) struct Item {
    pub name: String,
    pub kind: EntryKind,
    pub size: Option<u64>,
    pub compressed: Option<u64>,
    pub mode: Option<u32>,
    pub link: Option<String>,
    pub encrypted: bool,
}

/// Visitor callback: return `Ok(false)` to stop early.
pub(super) type Visit<'a> = dyn FnMut(&Item, &mut dyn Read) -> Result<bool, ArchiveError> + 'a;

fn kind_from_mode(mode: u32) -> Option<EntryKind> {
    match mode & 0o170000 {
        0 => None,
        0o100000 => Some(EntryKind::File),
        0o040000 => Some(EntryKind::Dir),
        0o120000 => Some(EntryKind::Symlink),
        _ => Some(EntryKind::Special),
    }
}

fn read_link(r: &mut dyn Read) -> Result<String, ArchiveError> {
    let mut buf = Vec::new();
    r.take(MAX_LINK_LEN + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_LINK_LEN {
        return Err(ArchiveError::Corrupt("link target too long".into()));
    }
    String::from_utf8(buf).map_err(|_| ArchiveError::Corrupt("link target is not UTF-8".into()))
}

/// Visit all entries of a streamable format in archive order. RAR is not streamable here.
pub(super) fn visit(path: &Path, format: ArchiveFormat, password: Option<&str>, f: &mut Visit) -> Result<(), ArchiveError> {
    let file = File::open(path)?;
    let packed = file.metadata()?.len();
    let single = |r: &mut dyn Read, f: &mut Visit| {
        let item = Item {
            name: output_name(path).to_string_lossy().into_owned(),
            kind: EntryKind::File,
            size: None,
            compressed: Some(packed),
            mode: None,
            link: None,
            encrypted: false,
        };
        f(&item, r).map(|_| ())
    };
    match format {
        ArchiveFormat::Zip => visit_zip(file, password, f),
        ArchiveFormat::Tar => visit_tar(BufReader::new(file), f),
        ArchiveFormat::TarGz => visit_tar(flate2::read::MultiGzDecoder::new(BufReader::new(file)), f),
        ArchiveFormat::TarZst => visit_tar(zstd::Decoder::new(file)?, f),
        ArchiveFormat::Gz => single(&mut flate2::read::MultiGzDecoder::new(BufReader::new(file)), f),
        ArchiveFormat::Zst => single(&mut zstd::Decoder::new(file)?, f),
        ArchiveFormat::SevenZ => visit_7z(file, password, f),
        ArchiveFormat::Rar => Err(ArchiveError::Unsupported("RAR is not streamable".into())),
    }
}

pub(super) fn zip_err(e: zip::result::ZipError) -> ArchiveError {
    use zip::result::ZipError as Z;
    match e {
        Z::UnsupportedArchive(Z::PASSWORD_REQUIRED) => ArchiveError::PasswordRequired,
        Z::InvalidPassword => ArchiveError::WrongPassword,
        Z::UnsupportedArchive(s) => ArchiveError::Unsupported(s.into()),
        Z::CompressionMethodNotSupported(m) => ArchiveError::Unsupported(format!("ZIP compression method {m}")),
        Z::Io(e) => e.into(),
        other => ArchiveError::Corrupt(other.to_string()),
    }
}

fn visit_zip(file: File, password: Option<&str>, f: &mut Visit) -> Result<(), ArchiveError> {
    let mut za = zip::ZipArchive::new(BufReader::new(file)).map_err(zip_err)?;
    for i in 0..za.len() {
        let encrypted = za.by_index_raw(i).map_err(zip_err)?.encrypted();
        let mut zf = match password {
            Some(p) if encrypted => za.by_index_decrypt(i, p.as_bytes()),
            _ => za.by_index(i),
        }
        .map_err(zip_err)?;
        let name = zf.name().map_err(zip_err)?.into_owned();
        let mode = zf.unix_mode();
        let kind = if zf.is_dir() {
            EntryKind::Dir
        } else if zf.is_symlink() {
            EntryKind::Symlink
        } else {
            mode.and_then(kind_from_mode).unwrap_or(EntryKind::File)
        };
        let link = if kind == EntryKind::Symlink { Some(read_link(&mut zf)?) } else { None };
        let item = Item { name, kind, size: Some(zf.size()), compressed: Some(zf.compressed_size()), mode, link, encrypted };
        if !f(&item, &mut zf)? {
            break;
        }
    }
    Ok(())
}

fn visit_tar(r: impl Read, f: &mut Visit) -> Result<(), ArchiveError> {
    let mut ar = tar::Archive::new(r);
    for entry in ar.entries()? {
        let mut e = entry?;
        let t = e.header().entry_type();
        if t.is_pax_global_extensions() {
            continue;
        }
        let kind = if t.is_dir() {
            EntryKind::Dir
        } else if t.is_symlink() {
            EntryKind::Symlink
        } else if t.is_hard_link() {
            EntryKind::HardLink
        } else if t.is_file() || t.is_contiguous() {
            EntryKind::File
        } else {
            EntryKind::Special
        };
        let lossy = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        let item = Item {
            name: lossy(&e.path_bytes()),
            kind,
            size: Some(e.size()),
            compressed: None,
            mode: e.header().mode().ok(),
            link: e.link_name_bytes().map(|b| lossy(&b)),
            encrypted: false,
        };
        if !f(&item, &mut e)? {
            break;
        }
    }
    Ok(())
}

fn sevenz_err(e: sevenz_rust2::Error) -> ArchiveError {
    use sevenz_rust2::Error as E;
    match e {
        E::PasswordRequired => ArchiveError::PasswordRequired,
        E::MaybeBadPassword(_) => ArchiveError::WrongPassword,
        E::UnsupportedCompressionMethod(m) => ArchiveError::Unsupported(format!("7z method {m}")),
        E::Unsupported(s) => ArchiveError::Unsupported(s.into_owned()),
        E::Io(e, _) => e.into(),
        other => ArchiveError::Corrupt(other.to_string()),
    }
}

fn sevenz_password(password: Option<&str>) -> sevenz_rust2::Password {
    password.map(sevenz_rust2::Password::from).unwrap_or_else(sevenz_rust2::Password::empty)
}

fn sevenz_kind(e: &sevenz_rust2::ArchiveEntry) -> EntryKind {
    if e.is_directory() {
        return EntryKind::Dir;
    }
    // p7zip stores the unix mode in the high 16 bits when bit 0x8000 is set.
    let attrs = e.windows_attributes();
    if e.has_windows_attributes && attrs & 0x8000 != 0 {
        return kind_from_mode(attrs >> 16).unwrap_or(EntryKind::File);
    }
    EntryKind::File
}

fn visit_7z(file: File, password: Option<&str>, f: &mut Visit) -> Result<(), ArchiveError> {
    let mut reader = sevenz_rust2::ArchiveReader::new(BufReader::new(file), sevenz_password(password)).map_err(sevenz_err)?;
    let encrypted: Vec<bool> = (0..reader.archive().files.len()).map(|i| reader.archive().is_file_encrypted(i)).collect();
    let mut index = 0;
    let mut failure = None;
    let result = reader.for_each_entries_with_decision(|e, r| {
        let kind = sevenz_kind(e);
        let link = match kind {
            EntryKind::Symlink => match read_link(r) {
                Ok(l) => Some(l),
                Err(err) => {
                    failure = Some(err);
                    return Ok(sevenz_rust2::EntryDecision::Stop);
                }
            },
            _ => None,
        };
        let item = Item {
            name: e.name().to_string(),
            kind,
            size: Some(e.size()),
            compressed: None,
            mode: None,
            link,
            encrypted: encrypted.get(index).copied().unwrap_or(false),
        };
        index += 1;
        Ok(match f(&item, r) {
            Ok(true) => sevenz_rust2::EntryDecision::Continue,
            Ok(false) => sevenz_rust2::EntryDecision::Stop,
            Err(err) => {
                failure = Some(err);
                sevenz_rust2::EntryDecision::Stop
            }
        })
    });
    match failure {
        Some(err) => Err(err),
        None => result.map_err(sevenz_err),
    }
}

pub(super) fn rar_err(e: unrar::error::UnrarError) -> ArchiveError {
    use unrar::error::Code;
    match e.code {
        Code::MissingPassword => ArchiveError::PasswordRequired,
        Code::BadPassword => ArchiveError::WrongPassword,
        Code::UnknownFormat => ArchiveError::Unsupported("unknown RAR format".into()),
        Code::BadData | Code::BadArchive => ArchiveError::Corrupt(e.to_string()),
        _ => ArchiveError::Io(e.to_string()),
    }
}

pub(super) fn rar_item(h: &unrar::FileHeader) -> Item {
    let kind = if h.is_directory() {
        EntryKind::Dir
    } else {
        // Unix-made archives keep st_mode in file_attr.
        match kind_from_mode(h.file_attr) {
            Some(EntryKind::Symlink) => EntryKind::Symlink,
            Some(EntryKind::Special) => EntryKind::Special,
            _ => EntryKind::File,
        }
    };
    Item {
        name: h.filename.to_string_lossy().into_owned(),
        kind,
        size: Some(h.unpacked_size),
        compressed: None,
        mode: None,
        link: None,
        encrypted: h.is_encrypted(),
    }
}

pub(super) fn rar_open<'a>(path: &'a Path, password: Option<&'a str>) -> unrar::Archive<'a> {
    match password {
        Some(p) => unrar::Archive::with_password(path, p),
        None => unrar::Archive::new(path),
    }
}

fn to_entry(i: &Item) -> ArchiveEntry {
    ArchiveEntry { name: i.name.clone(), kind: i.kind, size: i.size, encrypted: i.encrypted }
}

/// Entries for browsing. Reads headers only where the format allows it.
pub fn list_entries(path: &Path, password: Option<&str>, cancel: &CancelToken) -> Result<Listing, ArchiveError> {
    let format = ArchiveFormat::detect(path).ok_or_else(|| ArchiveError::Unsupported("unknown archive type".into()))?;
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut push = |e: ArchiveEntry| -> Result<bool, ArchiveError> {
        if cancel.is_cancelled() {
            return Err(ArchiveError::Cancelled);
        }
        if entries.len() >= MAX_LISTED {
            truncated = true;
            return Ok(false);
        }
        entries.push(e);
        Ok(true)
    };
    match format {
        ArchiveFormat::Zip => {
            let mut za = zip::ZipArchive::new(BufReader::new(File::open(path)?)).map_err(zip_err)?;
            for i in 0..za.len() {
                let zf = za.by_index_raw(i).map_err(zip_err)?;
                let kind = if zf.is_dir() {
                    EntryKind::Dir
                } else if zf.is_symlink() {
                    EntryKind::Symlink
                } else {
                    zf.unix_mode().and_then(kind_from_mode).unwrap_or(EntryKind::File)
                };
                let e = ArchiveEntry { name: zf.name().map_err(zip_err)?.into_owned(), kind, size: Some(zf.size()), encrypted: zf.encrypted() };
                if !push(e)? {
                    break;
                }
            }
        }
        ArchiveFormat::SevenZ => {
            let archive = sevenz_rust2::Archive::open_with_password(path, &sevenz_password(password)).map_err(sevenz_err)?;
            for (i, f) in archive.files.iter().enumerate() {
                let e = ArchiveEntry { name: f.name().to_string(), kind: sevenz_kind(f), size: Some(f.size()), encrypted: archive.is_file_encrypted(i) };
                if !push(e)? {
                    break;
                }
            }
        }
        ArchiveFormat::Rar => {
            for h in rar_open(path, password).open_for_listing().map_err(rar_err)? {
                if !push(to_entry(&rar_item(&h.map_err(rar_err)?)))? {
                    break;
                }
            }
        }
        _ => visit(path, format, password, &mut |item, _| push(to_entry(item)))?,
    }
    Ok(Listing { entries, truncated })
}

/// First `max_bytes` of the first entry called `name`. By name, not index: 7z visits
/// entries in another order than it lists them.
pub fn preview_entry(path: &Path, password: Option<&str>, name: &str, max_bytes: usize) -> Result<Vec<u8>, ArchiveError> {
    let format = ArchiveFormat::detect(path).ok_or_else(|| ArchiveError::Unsupported("unknown archive type".into()))?;
    if format == ArchiveFormat::Rar {
        return preview_rar(path, password, name, max_bytes);
    }
    let mut out = None;
    visit(path, format, password, &mut |item, r| {
        if item.name != name {
            return Ok(true);
        }
        let mut buf = Vec::new();
        r.take(max_bytes as u64).read_to_end(&mut buf)?;
        out = Some(buf);
        Ok(false)
    })?;
    out.ok_or_else(|| ArchiveError::Io("entry not found".into()))
}

fn preview_rar(path: &Path, password: Option<&str>, name: &str, max_bytes: usize) -> Result<Vec<u8>, ArchiveError> {
    let mut open = rar_open(path, password).open_for_processing().map_err(rar_err)?;
    while let Some(h) = open.read_header().map_err(rar_err)? {
        if rar_item(h.entry()).name == name {
            if h.entry().unpacked_size > MAX_RAR_PREVIEW_ENTRY {
                return Err(ArchiveError::Unsupported("entry too large to preview".into()));
            }
            let (mut data, _) = h.read().map_err(rar_err)?;
            data.truncate(max_bytes);
            return Ok(data);
        }
        open = h.skip().map_err(rar_err)?;
    }
    Err(ArchiveError::Io(io::Error::from(io::ErrorKind::NotFound).to_string()))
}
