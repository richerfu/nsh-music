use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_HOME");
    println!("cargo:rerun-if-env-changed=ANDROID_NDK_ROOT");
    println!("cargo:rerun-if-env-changed=ANDROID_HOME");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("android") {
        return;
    }

    let target = env::var("TARGET").unwrap_or_default();
    let env_target = target.replace('-', "_");
    println!("cargo:rerun-if-env-changed=CXX_{env_target}");
    println!("cargo:rerun-if-env-changed=CC_{env_target}");

    let sysroot_triple = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "aarch64-linux-android",
        Ok("arm") => "arm-linux-androideabi",
        Ok("x86_64") => "x86_64-linux-android",
        Ok("x86") => "i686-linux-android",
        _ => "",
    };

    // IMPORTANT: Never add NDK `sysroot/usr/lib/<triple>` directly to
    // `rustc-link-search`! That directory contains static `libc.a`/`libm.a`/`libdl.a`,
    // which would shadow the API-level dynamic `libc.so`/`libm.so`/`libdl.so` and
    // statically link Bionic into `libnsh.so` (causing SIGSEGV in `getauxval` at load).
    // Stage ONLY `libc++_static.a`, `libc++abi.a`, and (for 32-bit ARMv7)
    // `libunwind.a` into a clean subdir of `OUT_DIR`.
    if !sysroot_triple.is_empty()
        && let Some(lib_dir) = find_ndk_sysroot_lib_dir(&env_target, sysroot_triple)
        && let Ok(out_dir) = env::var("OUT_DIR")
    {
        let stage_dir = PathBuf::from(out_dir).join("android-cxx-static");
        let _ = fs::create_dir_all(&stage_dir);
        for lib_name in ["libc++_static.a", "libc++abi.a", "libunwind.a"] {
            let src = lib_dir.join(lib_name);
            let dst = stage_dir.join(lib_name);
            if src.is_file() {
                let _ = fs::copy(&src, &dst);
            }
        }
        println!("cargo:rustc-link-search=native={}", stage_dir.display());
    }

    // `cpal` on Android links C++ `oboe-sys` (`liboboe.a`), while `nsh package`
    // bundles only `libnsh.so` (no `libc++_shared.so`). Link LLVM libc++ and
    // libc++abi statically so `__cxa_pure_virtual`, `__gxx_personality_v0`,
    // `operator new/delete`, etc. resolve inside `libnsh.so`.
    println!("cargo:rustc-link-lib=static=c++_static");
    println!("cargo:rustc-link-lib=static=c++abi");
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm") {
        println!("cargo:rustc-link-lib=static=unwind");
    }
}

fn find_ndk_sysroot_lib_dir(env_target: &str, sysroot_triple: &str) -> Option<PathBuf> {
    // 1. Prefer the exact NDK toolchain containing `CXX_<target>` or `CC_<target>`.
    for var in [format!("CXX_{env_target}"), format!("CC_{env_target}")] {
        if let Ok(compiler) = env::var(&var)
            && let Some(dir) = sysroot_lib_from_compiler(Path::new(&compiler), sysroot_triple)
        {
            return Some(dir);
        }
    }

    // 2. Fallback to `ANDROID_NDK_HOME` / `ANDROID_NDK_ROOT`.
    for var in ["ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"] {
        if let Ok(ndk_root) = env::var(var)
            && let Some(dir) = sysroot_lib_from_ndk(Path::new(&ndk_root), sysroot_triple)
        {
            return Some(dir);
        }
    }

    // 3. Fallback to newest NDK inside `ANDROID_HOME/ndk`.
    if let Ok(sdk_root) = env::var("ANDROID_HOME") {
        let ndk_parent = Path::new(&sdk_root).join("ndk");
        if let Ok(entries) = fs::read_dir(&ndk_parent) {
            let mut candidates: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_dir())
                .collect();
            candidates.sort();
            for ndk_root in candidates.into_iter().rev() {
                if let Some(dir) = sysroot_lib_from_ndk(&ndk_root, sysroot_triple) {
                    return Some(dir);
                }
            }
        }
    }

    None
}

fn sysroot_lib_from_compiler(compiler: &Path, sysroot_triple: &str) -> Option<PathBuf> {
    // `<prebuilt>/bin/<target>-clang++` -> `<prebuilt>/sysroot/usr/lib/<sysroot_triple>`
    let prebuilt = compiler.parent()?.parent()?;
    let lib_dir = prebuilt
        .join("sysroot")
        .join("usr")
        .join("lib")
        .join(sysroot_triple);
    lib_dir.join("libc++_static.a").is_file().then_some(lib_dir)
}

fn sysroot_lib_from_ndk(ndk_root: &Path, sysroot_triple: &str) -> Option<PathBuf> {
    let prebuilt_parent = ndk_root.join("toolchains").join("llvm").join("prebuilt");
    let entries = fs::read_dir(&prebuilt_parent).ok()?;
    for entry in entries.flatten() {
        let lib_dir = entry
            .path()
            .join("sysroot")
            .join("usr")
            .join("lib")
            .join(sysroot_triple);
        if lib_dir.join("libc++_static.a").is_file() {
            return Some(lib_dir);
        }
    }
    None
}
