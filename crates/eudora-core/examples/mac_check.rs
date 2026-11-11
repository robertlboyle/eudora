//! Checks the Mac's own pieces that a test can't (AppKit works on the main thread, which a
//! test's never serves; here it's this program's own): the system's icon for files of a few
//! kinds. `cargo run --release -p eudora-core --example mac_check [OUT.png]` (OUT.png: the PDF
//! icon, to look at).

fn main() {
    for name in ["a.pdf", "b.docx", "c.zip", "noext"] {
        let png = eudora_core::file_icons::icon_png(name);
        let shown = png.as_ref().map(|p| format!("{} bytes, PNG: {}", p.len(), p.get(1..4) == Some(b"PNG"))).unwrap_or_else(|| "none".into());
        println!("Icon for {name}: {shown}");
        if let (Some(png), Some(out), "a.pdf") = (png, std::env::args().nth(1), name) {
            std::fs::write(&out, png).expect("writing the icon");
        }
    }
}
