//! Preferences, session and favorites as small JSON files written atomically
//! (temp file + fsync + rename) in ~/Library/Application Support/Waypoint.

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("WAYPOINT_DATA_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join("Library/Application Support/Waypoint")
}

/// Load `name.json`, falling back to `T::default()` when missing or unreadable.
pub fn load<T: DeserializeOwned + Default>(name: &str) -> T {
    let path = data_dir().join(format!("{name}.json"));
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable {}: {e}", path.display());
            T::default()
        }),
        Err(_) => T::default(),
    }
}

pub fn save<T: Serialize>(name: &str, value: &T) -> io::Result<()> {
    let dir = data_dir();
    fs::create_dir_all(&dir)?;
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(&dir.join(format!("{name}.json")), &bytes)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

/// Serde helper keeping paths byte-exact (non-UTF-8 names survive a round trip).
pub mod path_bytes {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        Bytes(Vec<u8>),
    }

    pub fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        match p.to_str() {
            Some(t) => Repr::Text(t.to_owned()),
            None => Repr::Bytes(p.as_os_str().as_bytes().to_vec()),
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(match Repr::deserialize(d)? {
            Repr::Text(t) => PathBuf::from(t),
            Repr::Bytes(b) => PathBuf::from(std::ffi::OsString::from_vec(b)),
        })
    }
}

/// Same as `path_bytes` for `Vec<PathBuf>`.
pub mod paths_bytes {
    use super::*;

    #[derive(Serialize, Deserialize)]
    struct P(#[serde(with = "path_bytes")] PathBuf);

    pub fn serialize<S: Serializer>(v: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
        v.iter().map(|p| P(p.clone())).collect::<Vec<_>>().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<P>::deserialize(d)?.into_iter().map(|p| p.0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[derive(Serialize, Deserialize, Default, PartialEq, Debug)]
    struct S {
        #[serde(with = "path_bytes")]
        p: PathBuf,
    }

    #[test]
    fn non_utf8_paths_round_trip() {
        let p = PathBuf::from(OsString::from_vec(vec![b'/', b'a', 0xff, b'b']));
        let json = serde_json::to_string(&S { p: p.clone() }).unwrap();
        assert_eq!(serde_json::from_str::<S>(&json).unwrap().p, p);
        let json = serde_json::to_string(&S { p: "/tmp/x".into() }).unwrap();
        assert_eq!(json, r#"{"p":"/tmp/x"}"#);
    }

    #[test]
    fn atomic_write_replaces() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("a.json");
        write_atomic(&f, b"1").unwrap();
        write_atomic(&f, b"2").unwrap();
        assert_eq!(fs::read(&f).unwrap(), b"2");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
    }
}
