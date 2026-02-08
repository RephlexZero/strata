use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let root_dir = Path::new(&manifest_dir).parent().unwrap().parent().unwrap();
    let source_dir = root_dir.join("vendor").join("librist");

    // Ensure the submodule is initialized
    if !source_dir.join("meson.build").exists() {
        // Try to initialize it if we can
        let status = Command::new("git")
            .args(&["submodule", "update", "--init", "--recursive"])
            .current_dir(root_dir)
            .status()
            .ok();

        if status.is_none() || !status.unwrap().success() {
            println!(
                "cargo:warning=librist submodule not found or failed to update. Build might fail."
            );
        }
    }

    println!("cargo:rerun-if-changed={}", source_dir.display());

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let install_dir = out_dir.join("dist");
    let build_dir = out_dir.join("build");

    // Configure with Meson
    // We deliberately disable encryption (mbedtls) to avoid complex dependency linkage for this static build
    // unless strictly required.
    let mut cmd = Command::new("meson");
    cmd.args(&[
        "setup",
        build_dir.to_str().unwrap(),
        source_dir.to_str().unwrap(),
        "--default-library=static",
        "--prefix",
        install_dir.to_str().unwrap(),
        "-Dbuilt_tools=false",
        "-Dtest=false",
        "-Duse_mbedtls=false",
        "--buildtype=release",
    ]);

    // Support cross-compilation via MESON_CROSS_FILE env var
    if let Ok(cross_file) = env::var("MESON_CROSS_FILE") {
        cmd.arg("--cross-file");
        cmd.arg(&cross_file);
    }

    // Check if we need to reconfigure (if build dir exists)
    // Meson usually handles this, or fails if we try to setup on existing dir.
    // If build_dir exists, we might want to use "configure" or "setup --reconfigure"
    if build_dir.exists() {
        cmd.arg("--reconfigure");
    }

    let status = cmd.status().expect("Failed to run meson setup");
    if !status.success() {
        panic!("Meson setup failed");
    }

    // Compile
    let status = Command::new("meson")
        .args(&["compile", "-C", build_dir.to_str().unwrap()])
        .status()
        .expect("Failed to run meson compile");
    if !status.success() {
        panic!("Meson compile failed");
    }

    // Install (to local dist)
    let status = Command::new("meson")
        .args(&["install", "-C", build_dir.to_str().unwrap()])
        .status()
        .expect("Failed to run meson install");
    if !status.success() {
        panic!("Meson install failed");
    }

    // Link settings
    let lib_path = install_dir.join("lib"); // or lib64 on some systems, meson handles this... check?
                                            // Meson --prefix usually puts libs in lib or lib/x86_64-linux-gnu depending on configuration.
                                            // Since we are not doing a system install, it typically defaults to `lib`.
                                            // But we should check both just in case.

    // Actually, let's search for the library file to be sure
    let lib_search_paths = vec![
        lib_path.clone(),
        install_dir.join("lib64"),
        install_dir.join("lib").join("x86_64-linux-gnu"),
        install_dir.join("lib").join("aarch64-linux-gnu"),
    ];

    let mut found_lib = false;
    for path in &lib_search_paths {
        if path.exists() {
            println!("cargo:rustc-link-search=native={}", path.display());
            found_lib = true;
        }
    }

    // In case meson put it somewhere else or we are cross compiling, this might leak, but for this devcontainer:
    if !found_lib {
        println!("cargo:warning=Could not find lib directory in local install. Guessing 'lib'.");
        println!("cargo:rustc-link-search=native={}", lib_path.display());
    }

    println!("cargo:rustc-link-lib=static=rist");

    // Output metadata for other crates to access include paths if needed
    // This allows C/C++ builds in other crates to find the headers via DEP_RIST_INCLUDE
    let include_path = install_dir.join("include");
    println!("cargo:include={}", include_path.display());

    // Bindgen
    // Native bindings.rs generation

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include_path.display()))
        // We might need to include the source include dir if installed headers aren't enough (usually they are)
        .allowlist_function("rist_.*")
        .allowlist_function("librist_.*")
        .allowlist_type("rist_.*")
        .allowlist_var("RIST_.*")
        .generate()
        .expect("Unable to generate bindings");

    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("Couldn't write bindings!");
}
