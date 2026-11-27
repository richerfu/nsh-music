//! Compiles `android/java/com/richerfu/nshmusic/NshMediaBridge.java` into
//! `$OUT_DIR/nsh_media_bridge.dex` when building for Android (`target_os = "android"`).
//!
//! On all non-Android targets this script returns immediately so macOS, iOS,
//! Windows, Linux, and HarmonyOS builds never require a JDK or Android SDK.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BRIDGE_JAVA_REL: &str = "android/java/com/richerfu/nshmusic/NshMediaBridge.java";

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    println!("cargo:rerun-if-changed={BRIDGE_JAVA_REL}");
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_SDK_ROOT");
    println!("cargo:rerun-if-env-changed=JAVA_HOME");

    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR missing"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR missing"));
    let java_src = manifest_dir.join(BRIDGE_JAVA_REL);
    assert!(
        java_src.is_file(),
        "Android media bridge Java source not found at {}",
        java_src.display()
    );

    let sdk_root = locate_android_sdk().unwrap_or_else(|| {
        panic!(
            "Android SDK not found: set ANDROID_HOME or ANDROID_SDK_ROOT \
             (checked standard macOS/Linux/Windows SDK locations)"
        )
    });
    let android_jar = locate_newest_android_jar(&sdk_root).unwrap_or_else(|| {
        panic!(
            "No platforms/android-*/android.jar found under Android SDK at {}",
            sdk_root.display()
        )
    });
    let d8 = locate_newest_d8(&sdk_root).unwrap_or_else(|| {
        panic!(
            "No build-tools/*/d8 found under Android SDK at {}",
            sdk_root.display()
        )
    });
    let javac = locate_javac().unwrap_or_else(|| {
        panic!("javac not found: install a JDK and set JAVA_HOME or ensure `javac` is on PATH")
    });

    let classes_dir = out_dir.join("java_classes");
    let dex_dir = out_dir.join("dex");
    let _ = fs::remove_dir_all(&classes_dir);
    let _ = fs::remove_dir_all(&dex_dir);
    fs::create_dir_all(&classes_dir).expect("failed to create java_classes dir");
    fs::create_dir_all(&dex_dir).expect("failed to create dex output dir");

    let javac_status = Command::new(&javac)
        .args(["--release", "8", "-cp"])
        .arg(&android_jar)
        .arg("-d")
        .arg(&classes_dir)
        .arg(&java_src)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", javac.display()));
    assert!(
        javac_status.success(),
        "javac failed compiling {} with status {javac_status}",
        java_src.display()
    );

    let mut class_files = Vec::new();
    collect_class_files(&classes_dir, &mut class_files);
    class_files.sort();
    assert!(
        !class_files.is_empty(),
        "javac produced no .class files in {}",
        classes_dir.display()
    );

    let mut d8_cmd = Command::new(&d8);
    d8_cmd
        .args(["--min-api", "26", "--lib"])
        .arg(&android_jar)
        .arg("--output")
        .arg(&dex_dir);
    for cf in &class_files {
        d8_cmd.arg(cf);
    }
    let d8_status = d8_cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {e}", d8.display()));
    assert!(d8_status.success(), "d8 failed with status {d8_status}");

    let built_dex = dex_dir.join("classes.dex");
    assert!(
        built_dex.is_file(),
        "d8 did not produce expected {}",
        built_dex.display()
    );
    let final_dex = out_dir.join("nsh_media_bridge.dex");
    fs::copy(&built_dex, &final_dex).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} -> {}: {e}",
            built_dex.display(),
            final_dex.display()
        )
    });
}

fn locate_android_sdk() -> Option<PathBuf> {
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(val) = env::var_os(var) {
            let p = PathBuf::from(val);
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    if cfg!(windows) {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            let p = PathBuf::from(local).join("Android").join("Sdk");
            if p.is_dir() {
                return Some(p);
            }
        }
    } else if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        for rel in ["Library/Android/sdk", "Android/Sdk"] {
            let p = home.join(rel);
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    None
}

fn locate_newest_android_jar(sdk_root: &Path) -> Option<PathBuf> {
    let platforms_dir = sdk_root.join("platforms");
    let mut candidates: Vec<(u32, PathBuf)> = fs::read_dir(platforms_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            let api = name.strip_prefix("android-")?.parse::<u32>().ok()?;
            let jar = e.path().join("android.jar");
            jar.is_file().then_some((api, jar))
        })
        .collect();
    candidates.sort_by_key(|(api, _)| *api);
    candidates.pop().map(|(_, jar)| jar)
}

fn locate_newest_d8(sdk_root: &Path) -> Option<PathBuf> {
    let bt_dir = sdk_root.join("build-tools");
    let d8_name = if cfg!(windows) { "d8.bat" } else { "d8" };
    let mut candidates: Vec<(Vec<u64>, PathBuf)> = fs::read_dir(bt_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let bin = e.path().join(d8_name);
            if !bin.is_file() {
                return None;
            }
            let ver = e
                .file_name()
                .to_str()
                .unwrap_or_default()
                .split(['.', '-'])
                .map(|part| part.parse::<u64>().unwrap_or(0))
                .collect::<Vec<_>>();
            Some((ver, bin))
        })
        .collect();
    candidates.sort_by(|(a, _), (b, _)| a.cmp(b));
    candidates.pop().map(|(_, bin)| bin)
}

fn locate_javac() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "javac.exe" } else { "javac" };

    if let Some(java_home) = env::var_os("JAVA_HOME") {
        let p = PathBuf::from(java_home).join("bin").join(exe);
        if is_runnable_javac(&p) {
            return Some(p);
        }
    }

    if is_runnable_javac(Path::new(exe)) {
        return Some(PathBuf::from(exe));
    }

    let mut fallbacks: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        fallbacks.push(PathBuf::from(
            "/Applications/Android Studio.app/Contents/jbr/Contents/Home/bin/javac",
        ));
        fallbacks.push(PathBuf::from("/opt/homebrew/opt/openjdk/bin/javac"));
        fallbacks.push(PathBuf::from("/usr/local/opt/openjdk/bin/javac"));
        if let Some(home) = env::var_os("HOME") {
            fallbacks.push(PathBuf::from(home).join(".jenv/shims/javac"));
        }
    } else if !cfg!(windows) {
        fallbacks.push(PathBuf::from("/usr/bin/javac"));
    }

    fallbacks.into_iter().find(|p| is_runnable_javac(p))
}

fn is_runnable_javac(path: &Path) -> bool {
    Command::new(path)
        .arg("-version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn collect_class_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_class_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("class") {
            out.push(path);
        }
    }
}
