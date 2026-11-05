//! Builds the small Qt bridge for the desktop app. With the default `gui` feature
//! this is the only C++ in the project, and it needs nothing but the compiler and
//! Qt's own tools. `--no-default-features` builds the command line alone.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

const QT_MODULES: [&str; 5] = ["Qt6Core", "Qt6Gui", "Qt6Widgets", "Qt6Qml", "Qt6Quick"];
const LAYER_SHELL_HEADER: &str = "LayerShellQt/window.h";
const LAYER_SHELL_LIB: &str = "-lLayerShellQtInterface";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=ui/bridge.cpp");
    println!("cargo:rerun-if-changed=ui/bridge.h");
    println!("cargo:rerun-if-env-changed=CXX");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    if env::var_os("CARGO_FEATURE_GUI").is_none() {
        return;
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let mut modules: Vec<&str> = QT_MODULES.to_vec();
    modules.push("wayland-client");
    let cflags = pkg_config("--cflags", &modules);
    let libs = pkg_config("--libs", &modules);
    let include_dir = pkg_config("--variable=includedir", &["Qt6Core"]);
    let tools_dir = pkg_config("--variable=libexecdir", &["Qt6Core"]);
    check_layer_shell(include_dir.first().map(String::as_str));

    let source = Path::new("ui/bridge.cpp");
    let moc_output = out_dir.join("bridge.moc");
    let moc = tools_dir
        .first()
        .map_or_else(|| PathBuf::from("moc"), |dir| Path::new(dir).join("moc"));
    run(Command::new(&moc)
        .args(
            cflags
                .iter()
                .filter(|flag| flag.starts_with("-I") || flag.starts_with("-D")),
        )
        .arg(source)
        .arg("-o")
        .arg(&moc_output));

    let compiler = env::var("CXX").unwrap_or_else(|_| "c++".to_string());
    let object = out_dir.join("bridge.o");
    run(Command::new(&compiler)
        .args(["-std=c++17", "-fPIC", "-O2", "-Wall", "-Wextra", "-c"])
        .args(&cflags)
        .arg("-Iui")
        .arg("-I")
        .arg(&out_dir)
        .arg(source)
        .arg("-o")
        .arg(&object));

    let link = |flag: &str| println!("cargo:rustc-link-arg-bin=wispx-gui={flag}");
    link(&object.to_string_lossy());
    link(LAYER_SHELL_LIB);
    for flag in &libs {
        link(flag);
    }
    link("-lstdc++");
}

fn missing_packages(what: &str) -> ! {
    eprintln!(
        "\nThe WispX desktop app needs {what} to build.\n\
         On Arch or CachyOS: pacman -S --needed qt6-base qt6-declarative qt6-wayland layer-shell-qt wayland pkgconf\n\
         On Debian or Ubuntu: apt install qt6-base-dev qt6-base-private-dev qt6-declarative-dev qt6-wayland liblayershellqtinterface-dev libwayland-dev pkg-config g++\n\
         To build only the command line and daemon: cargo build --no-default-features\n"
    );
    exit(1);
}

fn pkg_config(option: &str, modules: &[&str]) -> Vec<String> {
    let output = Command::new("pkg-config")
        .arg(option)
        .args(modules)
        .output();
    match output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        Ok(output) => {
            eprintln!("{}", String::from_utf8_lossy(&output.stderr));
            missing_packages(&modules.join(", "));
        }
        Err(_) => missing_packages("pkg-config"),
    }
}

/// LayerShellQt ships no pkg-config file, so its header is looked for directly.
fn check_layer_shell(qt_include: Option<&str>) {
    let mut places = vec![
        PathBuf::from("/usr/include"),
        PathBuf::from("/usr/local/include"),
    ];
    if let Some(dir) = qt_include {
        places.push(PathBuf::from(dir));
    }
    if !places
        .iter()
        .any(|dir| dir.join(LAYER_SHELL_HEADER).is_file())
    {
        missing_packages("LayerShellQt");
    }
}

fn run(command: &mut Command) {
    let shown = format!("{command:?}");
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            eprintln!("{shown}\nended with {status}");
            exit(1);
        }
        Err(error) => {
            eprintln!("cannot run {shown}: {error}");
            missing_packages("a C++ compiler and Qt's moc");
        }
    }
}
