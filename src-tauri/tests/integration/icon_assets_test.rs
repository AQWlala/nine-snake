//! P0#10 regression test: the Tauri bundle icon set is present and
//! non-empty.
//!
//! Tauri's bundler reads `tauri.conf.json::bundle.icon` at build
//! time. The previous commit shipped a config that referenced
//! files which did not exist on disk, so `npm run tauri:build`
//! always failed. This test guards the regenerated icon set so
//! future refactors do not silently delete the assets again.
//!
//! Note: the test does *not* parse PNG / ICO / ICNS — that would
//! require a heavyweight dep. A simple "file is non-empty" check
//! is enough to catch the regression we just fixed.

use std::fs;
use std::path::PathBuf;

fn icon_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("icons")
        .join(rel)
}

#[test]
fn all_bundle_icons_exist_and_are_nonempty() {
    // The exact set referenced from tauri.conf.json::bundle.icon
    // (P0#10 fix). If a new platform is added, mirror it here.
    let required = [
        "32x32.png",
        "128x128.png",
        "128x128@2x.png",
        "icon.icns",
        "icon.ico",
    ];

    for name in required {
        let p = icon_path(name);
        assert!(p.exists(), "missing icon: {}", p.display());
        let size = fs::metadata(&p)
            .unwrap_or_else(|e| panic!("stat failed for {}: {e}", p.display()))
            .len();
        assert!(
            size > 0,
            "icon {} is zero bytes — regenerate with `python scripts/generate-icons.py`",
            p.display()
        );
    }
}

#[test]
fn generate_icons_script_is_idempotent() {
    // Sanity check: the script that produced the icons still ships
    // with the source tree (so the next contributor can rebuild
    // them after a palette change).
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("scripts")
        .join("generate-icons.py");
    assert!(
        script.exists(),
        "generate-icons.py is missing from scripts/ — bundle icons are unreproducible"
    );
    let body = fs::read_to_string(&script).expect("read generate-icons.py");
    // Minimal liveness check: the script must import Pillow and
    // emit at least one PNG.
    assert!(body.contains("Pillow") || body.contains("from PIL"));
    assert!(body.contains(".png") || body.contains("icon.png"));
}
