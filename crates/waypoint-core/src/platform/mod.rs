//! macOS services behind one module (03-architecture "Native and terminal services").
//! External programs are started with argument vectors, never through a shell.

pub mod clipboard;
pub mod spotlight;

use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL, NSURLBookmarkResolutionOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Open with the default application (F05) via LaunchServices.
pub fn open_default(path: &Path) -> io::Result<()> {
    run_open(&[], path)
}

/// Open with a chosen application bundle.
pub fn open_with(path: &Path, app: &Path) -> io::Result<()> {
    run_open(&["-a".as_ref(), app.as_os_str()], path)
}

fn run_open(args: &[&std::ffi::OsStr], path: &Path) -> io::Result<()> {
    let status = Command::new("/usr/bin/open").args(args).arg("--").arg(path).stdin(Stdio::null()).status()?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("open exited with {status}"))) }
}

/// Applications registered to open `path`, default first (Open With list).
pub fn list_open_with(path: &Path) -> Vec<PathBuf> {
    let Some(url) = NSURL::from_file_path(path) else { return vec![] };
    let ws = NSWorkspace::sharedWorkspace();
    let default = ws.URLForApplicationToOpenURL(&url).and_then(|u| u.to_file_path());
    let mut apps: Vec<PathBuf> = ws.URLsForApplicationsToOpenURL(&url).iter().filter_map(|u| u.to_file_path()).collect();
    if let Some(d) = default {
        apps.retain(|a| a != &d);
        apps.insert(0, d);
    }
    apps
}

/// Resolve a symlink or Finder alias to its original (F05 "reveal original").
pub fn resolve_original(path: &Path) -> Option<PathBuf> {
    let md = std::fs::symlink_metadata(path).ok()?;
    if md.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        let abs = if target.is_absolute() { target } else { path.parent()?.join(target) };
        return Some(abs);
    }
    let url = NSURL::from_file_path(path)?;
    let resolved = NSURL::URLByResolvingAliasFileAtURL_options_error(
        &url,
        NSURLBookmarkResolutionOptions::WithoutUI | NSURLBookmarkResolutionOptions::WithoutMounting,
    )
    .ok()?
    .to_file_path()?;
    (resolved != path).then_some(resolved)
}

/// Ask macOS to connect to a server URL; the OS shows its own mount/credential UI (R01).
pub fn connect_to_server(address: &str) -> Result<(), String> {
    let address = address.trim();
    let scheme = address.split("://").next().unwrap_or_default().to_ascii_lowercase();
    if !address.contains("://") || !["smb", "nfs", "afp", "ftp", "http", "https", "webdav", "davs"].contains(&scheme.as_str()) {
        return Err("Use a server address like smb://host/share, nfs://host/path or https://host/webdav".into());
    }
    let url = NSURL::URLWithString(&NSString::from_str(address)).ok_or("Invalid server address")?;
    if NSWorkspace::sharedWorkspace().openURL(&url) { Ok(()) } else { Err("macOS could not open the server address".into()) }
}

/// Native Quick Look panel for formats without an in-app preview (P02 fallback).
pub fn quick_look(path: &Path) -> io::Result<()> {
    Command::new("/usr/bin/qlmanage").arg("-p").arg("--").arg(path).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}

/// Render a Quick Look thumbnail PNG (used for PDF first page and other documents).
/// Blocking; call on a worker. Returns the PNG path inside `out_dir`.
pub fn quick_look_thumbnail(path: &Path, size: u32, out_dir: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let status = Command::new("/usr/bin/qlmanage")
        .args(["-t", "-s", &size.to_string(), "-o"])
        .arg(out_dir)
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    let name = format!("{}.png", path.file_name().unwrap_or_default().to_string_lossy());
    let png = out_dir.join(name);
    if status.success() && png.exists() { Ok(png) } else { Err(io::Error::other("no Quick Look thumbnail available")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f05_resolves_symlink_original() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("orig"), b"").unwrap();
        std::os::unix::fs::symlink("orig", t.path().join("link")).unwrap();
        assert_eq!(resolve_original(&t.path().join("link")), Some(t.path().join("orig")));
        assert_eq!(resolve_original(&t.path().join("orig")), None);
    }

    #[test]
    fn r01_rejects_non_server_addresses() {
        assert!(connect_to_server("rm -rf /").is_err());
        assert!(connect_to_server("file:///etc").is_err());
    }
}
