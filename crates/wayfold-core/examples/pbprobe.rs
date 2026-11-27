use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL};
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSArray;
fn main() {
    let pb = NSPasteboard::generalPasteboard();
    let items = pb.pasteboardItems().unwrap();
    println!("items {}", items.len());
    let mut saved = vec![];
    for it in items.iter() {
        let types = it.types();
        for t in types.iter() { println!(" type {t}"); }
        saved.push(it.stringForType(unsafe { NSPasteboardTypeFileURL }).unwrap());
    }
    let tmp = std::env::temp_dir().join("pbprobe.txt");
    std::fs::write(&tmp, b"x").unwrap();
    let c0 = pb.changeCount();
    let ret = wayfold_core::platform::clipboard::write_file_urls(&[tmp.clone()]);
    println!("before {c0} returned {ret:?}");
    for i in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        println!("{i} count {} urls {:?}", pb.changeCount(), wayfold_core::platform::clipboard::read_file_urls());
    }
    pb.clearContents();
    let restored: Vec<_> = saved.iter().map(|s| { let it = NSPasteboardItem::new(); it.setString_forType(s, unsafe { NSPasteboardTypeFileURL }); ProtocolObject::from_retained(it) }).collect();
    println!("restore ok {}", pb.writeObjects(&NSArray::from_retained_slice(&restored)));
    println!("final {:?}", wayfold_core::platform::clipboard::read_file_urls());
}
