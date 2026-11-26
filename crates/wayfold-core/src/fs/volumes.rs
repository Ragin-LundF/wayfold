//! Mounted volumes from the kernel mount table, read with MNT_NOWAIT so a stalled
//! network share never blocks this call.

use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub path: PathBuf,
    pub fs_type: String,
    pub source: String,
    pub local: bool,
    /// Shown in the sidebar (root volume and /Volumes/*, not system/firmlink mounts).
    pub user_visible: bool,
}

impl Mount {
    pub fn name(&self) -> String {
        if self.path == Path::new("/") {
            "Macintosh HD".into()
        } else {
            self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        }
    }
}

static MOUNTS: RwLock<Vec<Mount>> = RwLock::new(Vec::new());

/// Re-read the mount table and return user-visible volumes.
pub fn refresh_mounts() -> Vec<Mount> {
    let all = read_mount_table();
    *MOUNTS.write().unwrap() = all.clone();
    all.into_iter().filter(|m| m.user_visible).collect()
}

pub fn mounts() -> Vec<Mount> {
    let cached = MOUNTS.read().unwrap().clone();
    if cached.is_empty() { refresh_mounts() } else { cached.into_iter().filter(|m| m.user_visible).collect() }
}

/// True when the longest mount prefix of `path` is not a local filesystem.
pub fn is_network_path(path: &Path) -> bool {
    if MOUNTS.read().unwrap().is_empty() {
        refresh_mounts();
    }
    let m = MOUNTS.read().unwrap();
    m.iter()
        .filter(|m| path.starts_with(&m.path))
        .max_by_key(|m| m.path.as_os_str().len())
        .is_some_and(|m| !m.local)
}

fn read_mount_table() -> Vec<Mount> {
    let mut buf: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo returns a pointer into static libc storage valid until the next call;
    // we copy everything out immediately. Calls are serialized by the MOUNTS write lock usage.
    let n = unsafe { libc::getmntinfo(&mut buf, libc::MNT_NOWAIT) };
    if n <= 0 || buf.is_null() {
        return vec![];
    }
    let items = unsafe { std::slice::from_raw_parts(buf, n as usize) };
    items
        .iter()
        .map(|s| {
            let cstr = |a: &[libc::c_char]| unsafe { CStr::from_ptr(a.as_ptr()) }.to_string_lossy().into_owned();
            let path = PathBuf::from(cstr(&s.f_mntonname));
            let flags = s.f_flags as i32;
            let local = flags & libc::MNT_LOCAL != 0;
            let user_visible = path == Path::new("/")
                || (path.starts_with("/Volumes") && flags & libc::MNT_DONTBROWSE == 0);
            Mount { path, fs_type: cstr(&s.f_fstypename), source: cstr(&s.f_mntfromname), local, user_visible }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_local_and_visible() {
        let v = refresh_mounts();
        assert!(v.iter().any(|m| m.path == Path::new("/") && m.local));
        assert!(!is_network_path(Path::new("/Users")));
    }
}
