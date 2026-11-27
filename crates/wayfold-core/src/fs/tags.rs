//! Finder-compatible tags (v0.3): the `com.apple.metadata:_kMDItemUserTags` extended
//! attribute holds a binary plist array of "Name\nColorIndex" strings. Custom tags and
//! their color indexes are preserved on write.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const ATTR: &str = "com.apple.metadata:_kMDItemUserTags";

/// A Finder tag: its name (localized for the standard tags, e.g. "Blau") and color index
/// (0 = none). A standard tag is matched by color and name, see [`Tag::is_standard`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tag {
    pub name: String,
    pub color: u8,
}

impl Tag {
    fn parse(raw: &str) -> Self {
        let (name, color) = raw.split_once('\n').unwrap_or((raw, ""));
        let color = color.parse().ok().or_else(|| color_index(name)).unwrap_or(0);
        Self { name: name.to_owned(), color }
    }

    /// Whether this is the standard tag `standard` (from [`standard()`] for `english`): same color and
    /// its localized or English name, so a custom tag that only shares the color does not match.
    pub fn is_standard(&self, standard: &Tag, english: &str) -> bool {
        self.color == standard.color && (self.name == standard.name || self.name == english)
    }

    fn raw(&self) -> String {
        if self.color == 0 { self.name.clone() } else { format!("{}\n{}", self.name, self.color) }
    }
}

/// The standard tag for an English color name ("Red"), named as Finder names it on this Mac.
pub fn standard(english: &str) -> Option<Tag> {
    let color = color_index(english)?;
    Some(Tag { name: local_name(color).unwrap_or_else(|| english.to_owned()), color })
}

/// Localized name of a standard tag from Finder's preferences.
/// shortcut: assumes Finder's default tag order (FavoriteTagNames slot per color); a user who
/// reordered or renamed the favorite tags gets that slot's name. Upgrade: read Finder's tag dictionary.
fn local_name(color: u8) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let prefs = Path::new(&home).join("Library/Preferences/com.apple.finder.plist");
    let dict: plist::Dictionary = plist::from_file(prefs).ok()?;
    favorite_name(dict.get("FavoriteTagNames")?.as_array()?, color)
}

fn favorite_name(names: &[plist::Value], color: u8) -> Option<String> {
    // Slots 1..=7 are Red, Orange, Yellow, Green, Blue, Purple, Gray.
    let slot = [6, 7, 5, 2, 4, 3, 1].iter().position(|&c| c == color)? + 1;
    names.get(slot)?.as_string().filter(|n| !n.is_empty()).map(str::to_owned)
}

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
    let n = unsafe { libc::getxattr(cpath.as_ptr(), cname.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW) };
    if n <= 0 {
        return vec![];
    }
    buf.truncate(n as usize);
    plist::from_bytes::<Vec<String>>(&buf).unwrap_or_default()
}

/// Tags on `path`.
pub fn read(path: &Path) -> Vec<Tag> {
    raw_tags(path).iter().map(|t| Tag::parse(t)).collect()
}

/// Replace the tag set.
pub fn write(path: &Path, tags: &[Tag]) -> io::Result<()> {
    let values: Vec<String> = tags.iter().map(Tag::raw).collect();
    let cpath = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let cname = CString::new(ATTR).unwrap();
    if values.is_empty() {
        let r = unsafe { libc::removexattr(cpath.as_ptr(), cname.as_ptr(), libc::XATTR_NOFOLLOW) };
        let err = io::Error::last_os_error();
        return if r == 0 || err.raw_os_error() == Some(libc::ENOATTR) { Ok(()) } else { Err(err) };
    }
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &values).map_err(io::Error::other)?;
    let r = unsafe { libc::setxattr(cpath.as_ptr(), cname.as_ptr(), buf.as_ptr().cast(), buf.len(), 0, libc::XATTR_NOFOLLOW) };
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
        let custom = Tag { name: "Project X".into(), color: 0 };
        let blau = Tag { name: "Blau".into(), color: 4 };
        write(&f, &[blau.clone(), custom.clone()]).unwrap();
        assert_eq!(raw_tags(&f), ["Blau\n4", "Project X"]);
        assert_eq!(read(&f), [blau, custom]);
        write(&f, &[]).unwrap();
        assert!(read(&f).is_empty());
    }

    #[test]
    fn localized_and_colorless_standard_names_get_their_color() {
        assert_eq!(Tag::parse("Rot\n6"), Tag { name: "Rot".into(), color: 6 });
        assert_eq!(Tag::parse("Red"), Tag { name: "Red".into(), color: 6 });
        assert_eq!(Tag::parse("Work"), Tag { name: "Work".into(), color: 0 });
        assert_eq!(color_index("Blue"), Some(4));
        assert!(color_index("Teal").is_none());
    }

    #[test]
    fn standard_match_needs_color_and_standard_name() {
        let blue = Tag { name: "Blau".into(), color: 4 };
        assert!(Tag { name: "Blau".into(), color: 4 }.is_standard(&blue, "Blue"));
        assert!(Tag { name: "Blue".into(), color: 4 }.is_standard(&blue, "Blue"));
        assert!(!Tag { name: "Work".into(), color: 4 }.is_standard(&blue, "Blue"));
        assert!(!Tag { name: "Blau".into(), color: 6 }.is_standard(&blue, "Blue"));
    }

    #[test]
    fn favorite_names_map_slots_by_color() {
        let names: Vec<plist::Value> =
            ["", "Rot", "Orange", "Gelb", "Grün", "Blau", "Lila", "Grau"].into_iter().map(plist::Value::from).collect();
        assert_eq!(favorite_name(&names, 6).as_deref(), Some("Rot"));
        assert_eq!(favorite_name(&names, 4).as_deref(), Some("Blau"));
        assert_eq!(favorite_name(&names, 3).as_deref(), Some("Lila"));
        assert_eq!(favorite_name(&names, 1).as_deref(), Some("Grau"));
        assert_eq!(favorite_name(&[plist::Value::from(""), plist::Value::from("")], 6), None, "empty slot");
        assert_eq!(favorite_name(&names, 0), None);
        assert_eq!(favorite_name(&names[..3], 2), None, "short array");
    }
}
