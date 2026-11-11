//! Copies the icons the UI names (`ui/icons.txt`) from the Font Awesome Pro package, then runs
//! Tauri's build step.
//!
//! Pro icons (`ui/icons/pro/`, git-ignored: they need a Pro license) come from a Pro package:
//! `FONTAWESOME_PRO`, else `fontawesome` in the developer's private folder (`private_dev`,
//! `EUDORA_DEV`). Each icon's Font Awesome Free
//! version is in `ui/icons/free/` (committed: the genuine files of Font Awesome Free's own
//! release, CC BY 4.0), so a build without the Pro package (the published source) still has
//! every icon. `ui/icons/set.js` says which set the build uses.

use std::path::{Path, PathBuf};

fn main() {
    copy_icons();
    app_icon();
    build_time();
    tauri_build::build();
}

/// When this program was built, for Help ▸ About: seconds since 1970 (UTC) in `EUDORA_BUILT`.
/// Stamped again whenever the window's or the core's code changes, so it's never an older build's
/// date.
fn build_time() {
    for dir in ["src", "ui", "../eudora-core/src", "../../Cargo.toml"] {
        println!("cargo:rerun-if-changed={dir}");
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    println!("cargo:rustc-env=EUDORA_BUILT={now}");
}

/// The program's own icon (`icons/`, made by `scripts/make_icons.py`): Tauri bakes it into the
/// program when `lib.rs` is compiled, but nothing told Cargo to compile again when only the icon
/// changed (2026-10-14: a new icon.ico, and the window still showed the old one). A checksum of
/// the icon files goes to `lib.rs` as an environment variable, so a new icon rebuilds it; the
/// toolbar logo (`ui/logo.svg`) is kept the same as `icons/eudora.svg`.
fn app_icon() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut sum: u64 = 0xcbf2_9ce4_8422_2325;
    for name in ["icon.ico", "icon.png", "32x32.png", "128x128.png", "128x128@2x.png", "eudora.svg"] {
        let path = here.join("icons").join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        for b in std::fs::read(&path).unwrap_or_default() {
            sum = (sum ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
    }
    println!("cargo:rustc-env=EUDORA_ICON_SUM={sum:016x}");
    if let Ok(svg) = std::fs::read(here.join("icons/eudora.svg")) {
        write_if_changed(&here.join("ui/logo.svg"), &svg);
    }
}

/// The developer's private folder holding `fontawesome` (Font Awesome Pro): `EUDORA_DEV`.
fn private_dev(here: &Path) -> Option<PathBuf> {
    println!("cargo:rerun-if-env-changed=EUDORA_DEV");
    let mut found: Vec<PathBuf> = Vec::new();
    if let Some(dev) = std::env::var_os("EUDORA_DEV") {
        found.push(PathBuf::from(dev));
    }
    let _ = here;
    found.into_iter().find(|d| d.join("fontawesome").join("svgs").is_dir())
}

fn write_if_changed(path: &Path, data: &[u8]) {
    if std::fs::read(path).ok().as_deref() != Some(data) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    }
}

fn copy_icons() {
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=ui/icons.txt");
    println!("cargo:rerun-if-env-changed=FONTAWESOME_PRO");
    let table = std::fs::read_to_string(here.join("ui/icons.txt")).expect("ui/icons.txt");
    let names: Vec<&str> = table.lines().map(|l| l.split('#').next().unwrap().trim()).filter(|l| !l.is_empty()).collect();
    let package = std::env::var("FONTAWESOME_PRO").map(PathBuf::from).unwrap_or_else(|_| {
        private_dev(&here).map(|dev| dev.join("fontawesome")).unwrap_or_else(|| here.join("../../../fontawesome"))
    });
    let set_js = here.join("ui/icons/set.js");
    if !package.join("svgs/regular").is_dir() {
        println!("cargo:warning=Font Awesome Pro not found at {}: this build uses the Free icons", package.display());
        write_if_changed(&set_js, b"window.ICON_SET = \"free\";\n");
        return;
    }
    println!("cargo:rerun-if-changed={}", package.join("metadata/icons.json").display());
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(package.join("metadata/icons.json")).expect("Font Awesome metadata/icons.json")).unwrap();
    for name in names {
        let pro = std::fs::read(package.join(format!("svgs/regular/{name}.svg")))
            .unwrap_or_else(|_| panic!("ui/icons.txt names \"{name}\", which Font Awesome Pro doesn't have"));
        write_if_changed(&here.join(format!("ui/icons/pro/{name}.svg")), &pro);
        let free: Vec<&str> = metadata[name]["free"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect()).unwrap_or_default();
        if !free.iter().any(|s| *s == "regular" || *s == "solid") {
            panic!("\"{name}\" isn't in Font Awesome Free (regular or solid): pick an icon that is, so source builds have it");
        }
        // The Free fallbacks (ui/icons/free/) are the genuine files of Font Awesome Free's own
        // release (CC BY 4.0), committed with the source: never copied from the Pro package,
        // whose files carry the commercial license even for icons that are also in Free.
        if !here.join(format!("ui/icons/free/{name}.svg")).is_file() {
            panic!(
                "ui/icons/free/{name}.svg is missing: add it from Font Awesome Free's release (svgs/regular, else svgs/solid; github.com/FortAwesome/Font-Awesome/releases)"
            );
        }
    }
    write_if_changed(&set_js, b"window.ICON_SET = \"pro\";\n");
}
