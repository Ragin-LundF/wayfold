//! Finder-compatible tags (v0.3): the `com.apple.metadata:_kMDItemUserTags` extended
//! attribute holds a binary plist array of "Name\nColorIndex" strings. Custom tags and
//! their color indexes are preserved on write.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const ATTR: &str = "com.apple.metadata:_kMDItemUserTags";

/// Finder color indexes for the seven standard tags.
pub fn color_index(name: &str) -> Option<u8> {
    Some(match name {
        "Gray" => 1,
        "Green" => 2,
        "Purple" => 3,
        "Blue" => 4,
        "Yellow" => 5,
        "Red" => 6,
        "Orange" => 7,
        _ => return None,
    })
}

fn raw_tags(path: &Path) -> Vec<String> {
    let (Ok(cpath), Ok(cname)) = (CString::new(path.as_os_str().as_bytes()), CString::new(ATTR)) else {
        return vec![];
    };
    // SAFETY: valid C strings; a null buffer asks for the size only.
    let len = unsafe { libc::getxattr(cpath.as_ptr(), cname.as_ptr(), std::ptr::null_mut(), 0, 0, libc::XATTR_NOFOLLOW) };
    if len <= 0 {
        return vec![];
    }
    let mut buf = vec![0u8; len as usize];
    let n = unsafe {
        libc::getxattr(cpath.as_ptr(), cname.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW)
    };
    if n <= 0 {
        return vec![];
    }
    buf.truncate(n as usize);
    plist::from_bytes::<Vec<String>>(&buf).unwrap_or_default()
}

/// Tag names on `path` (color suffix stripped).
pub fn read(path: &Path) -> Vec<String> {
    raw_tags(path).into_iter().map(|t| t.split('\n').next().unwrap_or_default().to_owned()).collect()
}

/// Replace the tag set, keeping existing raw values (and their colors) for tags that stay.
pub fn write(path: &Path, names: &[String]) -> io::Result<()> {
    let existing = raw_tags(path);
    let values: Vec<String> = names
        .iter()
        .map(|n| {
            existing
                .iter()
                .find(|r| r.split('\n').next() == Some(n.as_str()))
                .cloned()
                .unwrap_or_else(|| match color_index(n) {
                    Some(c) => format!("{n}\n{c}"),
                    None => n.clone(),
                })
        })
        .collect();
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let cname = CString::new(ATTR).unwrap();
    if values.is_empty() {
        let r = unsafe { libc::removexattr(cpath.as_ptr(), cname.as_ptr(), libc::XATTR_NOFOLLOW) };
        let err = io::Error::last_os_error();
        return if r == 0 || err.raw_os_error() == Some(libc::ENOATTR) { Ok(()) } else { Err(err) };
    }
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &values).map_err(io::Error::other)?;
    let r = unsafe {
        libc::setxattr(cpath.as_ptr(), cname.as_ptr(), buf.as_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW)
    };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_keep_custom() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("a.txt");
        std::fs::write(&f, b"").unwrap();
        assert!(read(&f).is_empty());
        write(&f, &["Red".into(), "Project X".into()]).unwrap();
        assert_eq!(read(&f), ["Red", "Project X"]);
        assert!(raw_tags(&f).contains(&"Red\n6".to_string()));
        write(&f, &["Project X".into(), "Blue".into()]).unwrap();
        assert_eq!(read(&f), ["Project X", "Blue"]);
        write(&f, &[]).unwrap();
        assert!(read(&f).is_empty());
    }
}
