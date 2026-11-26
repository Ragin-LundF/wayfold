use super::*;
use std::fs::{self, File};
use std::io::Write;
use std::ffi::OsStr;
use std::os::unix::fs::symlink;

/// RAR 2.9 fixtures copied from the `unrar` crate's test data (MIT OR Apache-2.0):
/// `version.rar` holds `VERSION` = "unrar-0.4.0"; `crypted.rar` holds `.gitignore`,
/// encrypted with the password "unrar". RAR archives cannot be created here (OQ-2).
const VERSION_RAR: &[u8] = &[
    82, 97, 114, 33, 26, 7, 0, 207, 144, 115, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0, 15, 12, 116, 32, 128, 39, 0, 21, 0, 0, 0, 11, 0, 0, 0, 3, 69, 243, 125, 198, 164, 138, 7, 71,
    29, 51, 7, 0, 164, 129, 0, 0, 86, 69, 82, 83, 73, 79, 78, 12, 0, 143, 236, 138, 69, 204, 35, 200, 72, 8, 131, 98, 254, 95, 221, 92, 83, 136, 240, 114, 196, 61, 123,
    0, 64, 7, 0,
];
const CRYPTED_RAR: &[u8] = &[
    82, 97, 114, 33, 26, 7, 0, 207, 144, 115, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0, 211, 217, 116, 36, 132, 50, 0, 32, 0, 0, 0, 18, 0, 0, 0, 3, 243, 138, 3, 110, 45, 129, 3,
    71, 29, 51, 10, 0, 164, 129, 0, 0, 46, 103, 105, 116, 105, 103, 110, 111, 114, 101, 137, 4, 186, 140, 147, 6, 67, 34, 31, 57, 133, 249, 111, 37, 95, 57, 207, 233,
    33, 36, 6, 86, 60, 18, 79, 144, 6, 202, 252, 217, 98, 216, 95, 240, 199, 35, 50, 165, 46, 109, 196, 61, 123, 0, 64, 7, 0,
];

fn quiet() -> impl Fn(Progress) + Sync {
    |_| {}
}

fn extract(archive: &Path, dest: &Path, password: Option<&str>, limits: Limits) -> Result<PathBuf, ArchiveError> {
    extract_archive(archive, dest, &output_name(archive), password, limits, &CancelToken::new(), &quiet())
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// src/docs/{a.txt, sub/b.txt, empty/, link -> a.txt}
fn source_tree(root: &Path) -> PathBuf {
    let docs = root.join("src/docs");
    fs::create_dir_all(docs.join("sub")).unwrap();
    fs::create_dir_all(docs.join("empty")).unwrap();
    fs::write(docs.join("a.txt"), "alpha").unwrap();
    fs::write(docs.join("sub/b.txt"), "beta".repeat(1000)).unwrap();
    symlink("a.txt", docs.join("link")).unwrap();
    docs
}

fn zip_with(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zw = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, data) in entries {
        zw.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        zw.write_all(data).unwrap();
    }
    zw.finish().unwrap();
}

#[test]
fn f08_detect_formats_and_output_names() {
    assert_eq!(ArchiveFormat::detect(Path::new("a.TAR.GZ")), Some(ArchiveFormat::TarGz));
    assert_eq!(ArchiveFormat::detect(Path::new("a.txt.gz")), Some(ArchiveFormat::Gz));
    assert_eq!(ArchiveFormat::detect(Path::new("a.tzst")), Some(ArchiveFormat::TarZst));
    assert_eq!(ArchiveFormat::detect(Path::new("a.docx")), None);
    assert_eq!(ArchiveFormat::detect(Path::new(".zip")), None);
    assert_eq!(output_name(Path::new("/x/photos.tar.gz")), "photos");
    assert_eq!(output_name(Path::new("/x/notes.txt.gz")), "notes.txt");
    assert!(!ArchiveFormat::Rar.can_create() && ArchiveFormat::Rar.supports_password());
}

#[test]
fn f08_round_trip_each_creation_format() {
    let t = tempfile::tempdir().unwrap();
    let docs = source_tree(t.path());
    for format in ArchiveFormat::CREATE {
        let out = t.path().join(format!("out-{}", format.label()));
        fs::create_dir(&out).unwrap();
        let archive = create_archive(&[docs.clone()], &out.join(format!("docs{}", format.extension())), format, &CancelToken::new(), &quiet()).unwrap();
        let listing = list_entries(&archive, None, &CancelToken::new()).unwrap();
        let listed: Vec<&str> = listing.entries.iter().map(|e| e.name.trim_end_matches('/')).collect();
        for want in ["docs", "docs/a.txt", "docs/sub/b.txt", "docs/empty", "docs/link"] {
            assert!(listed.contains(&want), "{format:?} lists {want}: {listed:?}");
        }
        let dest = extract(&archive, &out, None, Limits::default()).unwrap();
        assert_eq!(dest, out.join("docs"), "{format:?}");
        let root = dest.join("docs");
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read_to_string(root.join("sub/b.txt")).unwrap(), "beta".repeat(1000));
        assert!(root.join("empty").is_dir());
        assert_eq!(fs::read_link(root.join("link")).unwrap(), Path::new("a.txt"), "{format:?} keeps the symlink");
        assert!(names(&out).iter().all(|n| !n.starts_with(".wayfold-tmp-")), "no staging left");
    }
}

#[test]
fn f08_extract_single_stream_gz_and_zst() {
    let t = tempfile::tempdir().unwrap();
    let gz = t.path().join("notes.txt.gz");
    let mut enc = flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
    enc.write_all(b"gzip body").unwrap();
    enc.finish().unwrap();
    let zst = t.path().join("data.bin.zst");
    fs::write(&zst, zstd::encode_all(&b"zstd body"[..], 3).unwrap()).unwrap();

    let out = t.path().join("out");
    fs::create_dir(&out).unwrap();
    assert_eq!(fs::read(extract(&gz, &out, None, Limits::default()).unwrap()).unwrap(), b"gzip body");
    assert_eq!(fs::read(extract(&zst, &out, None, Limits::default()).unwrap()).unwrap(), b"zstd body");
    assert_eq!(names(&out), ["data.bin", "notes.txt"]);
    assert_eq!(preview_entry(&gz, None, "notes.txt", 4).unwrap(), b"gzip");
}

#[test]
fn f08_extract_7z_plain_and_encrypted() {
    let t = tempfile::tempdir().unwrap();
    let docs = source_tree(t.path());
    fs::remove_file(docs.join("link")).unwrap();
    let plain = t.path().join("docs.7z");
    sevenz_rust2::compress_to_path(&docs, &plain).unwrap();
    let locked = t.path().join("locked.7z");
    sevenz_rust2::compress_to_path_encrypted(&docs, &locked, "secret".into()).unwrap();

    let listing = list_entries(&plain, None, &CancelToken::new()).unwrap();
    assert!(listing.entries.iter().any(|e| e.name == "sub/b.txt" && e.size == Some(4000)));
    assert_eq!(preview_entry(&plain, None, "sub/b.txt", 4).unwrap(), b"beta");
    let dest = extract(&plain, t.path(), None, Limits::default()).unwrap();
    assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "beta".repeat(1000));

    assert_eq!(extract(&locked, t.path(), None, Limits::default()).unwrap_err(), ArchiveError::PasswordRequired);
    let dest = extract(&locked, t.path(), Some("secret"), Limits::default()).unwrap();
    assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "alpha");
}

#[test]
fn f08_extract_rar_and_rar_password() {
    let t = tempfile::tempdir().unwrap();
    let rar = t.path().join("version.rar");
    fs::write(&rar, VERSION_RAR).unwrap();
    let listing = list_entries(&rar, None, &CancelToken::new()).unwrap();
    assert_eq!(listing.entries[0].name, "VERSION");
    assert_eq!(preview_entry(&rar, None, "VERSION", 5).unwrap(), b"unrar");
    let dest = extract(&rar, t.path(), None, Limits::default()).unwrap();
    assert_eq!(fs::read_to_string(dest.join("VERSION")).unwrap(), "unrar-0.4.0");

    let crypted = t.path().join("crypted.rar");
    fs::write(&crypted, CRYPTED_RAR).unwrap();
    assert!(list_entries(&crypted, None, &CancelToken::new()).unwrap().entries[0].encrypted);
    assert_eq!(extract(&crypted, t.path(), None, Limits::default()).unwrap_err(), ArchiveError::PasswordRequired);
    let dest = extract(&crypted, t.path(), Some("unrar"), Limits::default()).unwrap();
    assert_eq!(fs::read_to_string(dest.join(".gitignore")).unwrap(), "target\nCargo.lock\n");
    assert_eq!(names(t.path()), ["crypted", "crypted.rar", "version", "version.rar"]);
}

#[test]
fn f08_traversal_and_absolute_entries_rejected_without_output() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("out");
    fs::create_dir(&out).unwrap();
    for (i, bad) in ["../evil.txt", "/tmp/abs-evil.txt", "ok/../../evil.txt"].into_iter().enumerate() {
        let z = t.path().join(format!("bad{i}.zip"));
        zip_with(&z, &[("fine.txt", b"x"), (bad, b"evil")]);
        let err = extract(&z, &out, None, Limits::default()).unwrap_err();
        assert!(matches!(err, ArchiveError::Unsafe { .. }), "{bad}: {err:?}");
    }
    assert!(names(&out).is_empty(), "all-or-nothing: {:?}", names(&out));
    assert!(!t.path().join("evil.txt").exists());
}

#[test]
fn f08_symlink_escapes_rejected() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("out");
    fs::create_dir(&out).unwrap();
    let cases: [&[(&str, &str)]; 3] = [
        &[("up", "../../outside")],
        &[("abs", "/etc")],
        // "l" -> "." makes "l/x" live one level higher than its name suggests.
        &[("l", "."), ("l/x", "../escape")],
    ];
    for (i, links) in cases.iter().enumerate() {
        let z = t.path().join(format!("links{i}.zip"));
        let mut zw = zip::ZipWriter::new(File::create(&z).unwrap());
        for (name, target) in *links {
            zw.add_symlink(*name, *target, zip::write::SimpleFileOptions::default()).unwrap();
        }
        zw.finish().unwrap();
        let err = extract(&z, &out, None, Limits::default()).unwrap_err();
        assert!(matches!(err, ArchiveError::Unsafe { .. }), "case {i}: {err:?}");
    }
    assert!(names(&out).is_empty());
}

#[test]
fn f08_special_file_entry_rejected() {
    let t = tempfile::tempdir().unwrap();
    let tar_path = t.path().join("fifo.tar");
    let mut b = tar::Builder::new(File::create(&tar_path).unwrap());
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Fifo);
    h.set_size(0);
    b.append_data(&mut h, "pipe", std::io::empty()).unwrap();
    b.finish().unwrap();
    let err = extract(&tar_path, t.path(), None, Limits::default()).unwrap_err();
    assert!(matches!(err, ArchiveError::Unsafe { .. }), "{err:?}");
}

#[test]
fn f08_decompression_limits_rejected() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("out");
    fs::create_dir(&out).unwrap();
    let bomb = t.path().join("bomb.zip");
    let mut zw = zip::ZipWriter::new(File::create(&bomb).unwrap());
    zw.start_file("zeros.bin", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
    zw.write_all(&vec![0u8; 24 << 20]).unwrap();
    zw.finish().unwrap();

    let small = Limits { max_total_bytes: 1 << 20, ..Limits::default() };
    assert!(matches!(extract(&bomb, &out, None, small), Err(ArchiveError::LimitExceeded(_))));
    let strict_ratio = Limits { max_ratio: 100, ..Limits::default() };
    assert!(matches!(extract(&bomb, &out, None, strict_ratio), Err(ArchiveError::LimitExceeded(_))));

    let many = t.path().join("many.zip");
    zip_with(&many, &[("a", b"1"), ("b", b"2"), ("c", b"3")]);
    let few = Limits { max_entries: 2, ..Limits::default() };
    assert!(matches!(extract(&many, &out, None, few), Err(ArchiveError::LimitExceeded(_))));
    assert!(names(&out).is_empty());
}

#[test]
fn f08_cancel_mid_extract_leaves_nothing() {
    let t = tempfile::tempdir().unwrap();
    let out = t.path().join("out");
    fs::create_dir(&out).unwrap();
    let z = t.path().join("two.zip");
    zip_with(&z, &[("first.txt", b"1"), ("second.txt", b"2")]);
    let cancel = CancelToken::new();
    let c = cancel.clone();
    // Cancel as soon as the first entry is announced (after it was admitted).
    let err = extract_archive(&z, &out, OsStr::new("two"), None, Limits::default(), &cancel, &move |_| c.cancel()).unwrap_err();
    assert_eq!(err, ArchiveError::Cancelled);
    assert!(names(&out).is_empty());
}

#[test]
fn f08_encrypted_zip_needs_correct_password() {
    let t = tempfile::tempdir().unwrap();
    let z = t.path().join("secret.zip");
    let mut zw = zip::ZipWriter::new(File::create(&z).unwrap());
    zw.start_file("s.txt", zip::write::SimpleFileOptions::default().with_aes_encryption(zip::AesMode::Aes256, "pw")).unwrap();
    zw.write_all(b"hidden").unwrap();
    zw.finish().unwrap();

    assert!(list_entries(&z, None, &CancelToken::new()).unwrap().entries[0].encrypted);
    assert_eq!(extract(&z, t.path(), None, Limits::default()).unwrap_err(), ArchiveError::PasswordRequired);
    assert_eq!(extract(&z, t.path(), Some("nope"), Limits::default()).unwrap_err(), ArchiveError::WrongPassword);
    let dest = extract(&z, t.path(), Some("pw"), Limits::default()).unwrap();
    assert_eq!(fs::read_to_string(dest.join("s.txt")).unwrap(), "hidden");
}

#[test]
fn f08_existing_destination_gets_unique_name() {
    let t = tempfile::tempdir().unwrap();
    let z = t.path().join("pack.zip");
    zip_with(&z, &[("f.txt", b"new")]);
    fs::create_dir(t.path().join("pack")).unwrap();
    fs::write(t.path().join("pack/f.txt"), "old").unwrap();
    let dest = extract(&z, t.path(), None, Limits::default()).unwrap();
    assert_eq!(dest, t.path().join("pack 2"));
    assert_eq!(fs::read_to_string(t.path().join("pack/f.txt")).unwrap(), "old");

    // Creating over an existing archive name keeps the old archive too.
    let again = create_archive(&[t.path().join("pack")], &z, ArchiveFormat::Zip, &CancelToken::new(), &quiet()).unwrap();
    assert_eq!(again, t.path().join("pack 2.zip"));
}

#[test]
fn f08_unsupported_and_damaged_archives_report_errors() {
    let t = tempfile::tempdir().unwrap();
    let other = t.path().join("file.docx");
    fs::write(&other, "x").unwrap();
    assert!(matches!(extract(&other, t.path(), None, Limits::default()), Err(ArchiveError::Unsupported(_))));
    let junk = t.path().join("junk.zip");
    fs::write(&junk, "not a zip at all").unwrap();
    assert!(extract(&junk, t.path(), None, Limits::default()).is_err());
    assert!(create_archive(&[junk.clone()], &t.path().join("x.7z"), ArchiveFormat::SevenZ, &CancelToken::new(), &quiet()).is_err());
    assert_eq!(names(t.path()), ["file.docx", "junk.zip"]);
}

#[test]
fn f08_preview_is_bounded() {
    let t = tempfile::tempdir().unwrap();
    let z = t.path().join("big.zip");
    zip_with(&z, &[("readme.txt", b"hello"), ("big.txt", &vec![b'a'; 100_000])]);
    assert_eq!(preview_entry(&z, None, "big.txt", 4096).unwrap().len(), 4096);
    assert_eq!(preview_entry(&z, None, "readme.txt", 4096).unwrap(), b"hello");
}
