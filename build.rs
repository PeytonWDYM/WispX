fn main() {
    for path in [
        "assets/wispx.rc",
        "assets/wispx.ico",
        "assets/wispx.manifest",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    slint_build::compile_with_config(
        "ui/options.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
    )
    .expect("compile Options UI");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            // embed-resource selects RC syntax from the build host on MSVC. Use
            // the same windres command on both hosts when targeting Windows GNU.
            let output = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"))
                .join("wispx-resource.o");
            let compiler = std::env::var_os("RC_x86_64_pc_windows_gnu")
                .unwrap_or_else(|| "x86_64-w64-mingw32-windres".into());
            let status = std::process::Command::new(compiler)
                .args(["-I", "assets", "-i", "assets/wispx.rc", "-O", "coff", "-o"])
                .arg(&output)
                .status()
                .expect("run Windows GNU resource compiler");
            assert!(
                status.success(),
                "compile Windows GNU icon, manifest and version resources"
            );
            println!("cargo:rustc-link-arg-bin=WispX={}", output.display());
        } else {
            embed_resource::compile("assets/wispx.rc", embed_resource::NONE)
                .manifest_required()
                .expect("compile WispX icon, manifest and version resources");
        }
    }
}
