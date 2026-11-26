//! File URLs on the system pasteboard (F01 external clipboard): one pasteboard item per file
//! with `public.file-url`, which Finder and other apps read and write.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSURL};
use std::path::PathBuf;

/// Write file URLs; returns the pasteboard change count afterwards.
pub fn write_file_urls(paths: &[PathBuf]) -> Option<i64> {
    let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = paths
        .iter()
        .filter_map(|p| {
            let url = NSURL::from_file_path(p)?;
            let s = url.absoluteString()?;
            let item = NSPasteboardItem::new();
            // SAFETY: NSPasteboardTypeFileURL is a static AppKit constant.
            item.setString_forType(&s, unsafe { NSPasteboardTypeFileURL });
            Some(ProtocolObject::from_retained(item))
        })
        .collect();
    if items.is_empty() {
        return None;
    }
    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    let array = NSArray::from_retained_slice(&items);
    pb.writeObjects(&array).then(|| pb.changeCount() as i64)
}

pub fn change_count() -> Option<i64> {
    Some(NSPasteboard::generalPasteboard().changeCount() as i64)
}

/// File paths currently on the pasteboard (e.g. copied in Finder).
pub fn read_file_urls() -> Vec<PathBuf> {
    let pb = NSPasteboard::generalPasteboard();
    let Some(items) = pb.pasteboardItems() else { return vec![] };
    items
        .iter()
        .filter_map(|item| {
            let s = item.stringForType(unsafe { NSPasteboardTypeFileURL })?;
            NSURL::URLWithString(&s)?.to_file_path()
        })
        .collect()
}
