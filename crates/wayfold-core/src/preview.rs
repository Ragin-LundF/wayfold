//! Bounded preview decoding (P01/P02). Every reader is capped: text bytes, CSV rows, image
//! pixels, one PDF page. HTML and Markdown are sanitized: no scripts, no remote resources.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_TEXT_BYTES: u64 = 512 * 1024;
pub const MAX_CSV_ROWS: usize = 500;
pub const MAX_IMAGE_PIXELS: u64 = 60_000_000;

#[derive(Clone, Debug, PartialEq)]
pub enum Preview {
    Text {
        text: String,
        truncated: bool,
    },
    Markdown(String),
    /// Sanitized static HTML.
    Html(String),
    Csv {
        rows: Vec<Vec<String>>,
        truncated: bool,
    },
    Image {
        width: usize,
        height: usize,
    },
    /// Quick Look rendering (PDF first page, documents) as a PNG file.
    Thumbnail(PathBuf),
    Unsupported(String),
}

fn read_capped(path: &Path, cap: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut buf = Vec::with_capacity(len.min(cap) as usize);
    f.take(cap).read_to_end(&mut buf)?;
    Ok((buf, len > cap))
}

fn is_binary(b: &[u8]) -> bool {
    b[..b.len().min(8192)].contains(&0)
}

/// Strip everything that could execute or fetch: scripts, styles, frames, images, forms,
/// and URL schemes. Relative anchors stay as plain text links (never followed automatically).
pub fn sanitize_html(html: &str) -> String {
    ammonia::Builder::default()
        .rm_tags(["img", "picture", "source", "video", "audio", "svg", "form", "input", "link"])
        .url_schemes(Default::default())
        .clean(html)
        .to_string()
}

/// Markdown images and inline HTML images would be fetched by the renderer; replace them
/// with their alt text.
pub fn sanitize_markdown(md: &str) -> String {
    let img = regex::Regex::new(r"!\[([^\]]*)\]\s*(\([^)]*\)|\[[^\]]*\])").unwrap();
    let html_img = regex::Regex::new(r"(?is)<\s*(img|script|iframe|object|embed|link|style)\b[^>]*>").unwrap();
    let s = img.replace_all(md, "[image: $1]");
    html_img.replace_all(&s, "").into_owned()
}

/// Decode a preview for `path`. Blocking; run on the CPU/IO pool. `thumb_dir` receives
/// Quick Look PNGs.
pub fn load(path: &Path, thumb_dir: &Path) -> Preview {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let text = |cap| match read_capped(path, cap) {
        Ok((b, _)) if is_binary(&b) => Err("Binary file".to_string()),
        Ok((b, t)) => Ok((String::from_utf8_lossy(&b).into_owned(), t)),
        Err(e) => Err(e.to_string()),
    };
    match ext.as_str() {
        "md" | "markdown" => match text(MAX_TEXT_BYTES) {
            Ok((s, _)) => Preview::Markdown(sanitize_markdown(&s)),
            Err(e) => Preview::Unsupported(e),
        },
        "html" | "htm" => match text(MAX_TEXT_BYTES) {
            Ok((s, _)) => Preview::Html(sanitize_html(&s)),
            Err(e) => Preview::Unsupported(e),
        },
        "csv" | "tsv" => csv_preview(path, if ext == "tsv" { b'\t' } else { b',' }),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "ico" => match imagesize::size(path) {
            Ok(d) if (d.width as u64) * (d.height as u64) <= MAX_IMAGE_PIXELS => Preview::Image { width: d.width, height: d.height },
            Ok(d) => Preview::Unsupported(format!("Image too large to preview ({}×{})", d.width, d.height)),
            Err(e) => Preview::Unsupported(format!("Unreadable image: {e}")),
        },
        "pdf" | "heic" | "svg" | "key" | "pages" | "numbers" | "docx" | "xlsx" | "pptx" | "rtf" | "mov" | "mp4" => {
            match crate::platform::quick_look_thumbnail(path, 900, thumb_dir) {
                Ok(png) => Preview::Thumbnail(png),
                Err(e) => Preview::Unsupported(e.to_string()),
            }
        }
        _ => match text(MAX_TEXT_BYTES) {
            Ok((text, truncated)) => Preview::Text { text, truncated },
            Err(e) => Preview::Unsupported(e),
        },
    }
}

fn csv_preview(path: &Path, delim: u8) -> Preview {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) => return Preview::Unsupported(e.to_string()),
    };
    let mut rdr = csv::ReaderBuilder::new().delimiter(delim).has_headers(false).flexible(true).from_reader(f.take(4 * MAX_TEXT_BYTES));
    let mut rows = Vec::new();
    for rec in rdr.records() {
        if rows.len() == MAX_CSV_ROWS {
            return Preview::Csv { rows, truncated: true };
        }
        match rec {
            Ok(r) => rows.push(r.iter().take(50).map(|c| c.chars().take(200).collect()).collect()),
            Err(e) => {
                return if rows.is_empty() {
                    Preview::Unsupported(format!("Malformed CSV: {e}"))
                } else {
                    Preview::Csv { rows, truncated: true }
                };
            }
        }
    }
    Preview::Csv { rows, truncated: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn p02_html_cannot_execute_or_fetch() {
        let out = sanitize_html(
            r#"<h1 onclick="x()">Hi</h1><script>alert(1)</script><img src="https://e.com/a.png"><a href="https://e.com">l</a><iframe src="https://e.com"></iframe><style>body{background:url(https://e.com)}</style>"#,
        );
        assert!(out.contains("<h1>Hi</h1>"), "{out}");
        for bad in ["script", "onclick", "img", "https://", "iframe", "style"] {
            assert!(!out.contains(bad), "{bad} in {out}");
        }
    }

    #[test]
    fn p02_markdown_images_removed() {
        let out = sanitize_markdown("# T\n![logo](https://e.com/x.png) and <img src=\"http://e\"> ![r][ref]");
        assert!(!out.contains("https://e.com") && !out.contains("<img"), "{out}");
        assert!(out.contains("[image: logo]"));
    }

    #[test]
    fn p02_bounded_readers_fail_safely() {
        let t = tempfile::tempdir().unwrap();
        let big = t.path().join("big.txt");
        fs::write(&big, vec![b'a'; (MAX_TEXT_BYTES + 10) as usize]).unwrap();
        match load(&big, t.path()) {
            Preview::Text { text, truncated } => assert!(truncated && text.len() as u64 == MAX_TEXT_BYTES),
            p => panic!("{p:?}"),
        }
        let bin = t.path().join("x.dat");
        fs::write(&bin, b"\x00\x01\x02").unwrap();
        assert!(matches!(load(&bin, t.path()), Preview::Unsupported(_)));
        let csv = t.path().join("a.csv");
        fs::write(&csv, (0..600).map(|i| format!("{i},x\n")).collect::<String>()).unwrap();
        assert!(matches!(load(&csv, t.path()), Preview::Csv { truncated: true, ref rows } if rows.len() == MAX_CSV_ROWS));
        let bad_png = t.path().join("bad.png");
        fs::write(&bad_png, b"not a png").unwrap();
        assert!(matches!(load(&bad_png, t.path()), Preview::Unsupported(_)));
    }
}
