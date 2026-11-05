//! Builds the small Qt bridge for the desktop app on Linux. With the default `gui`
//! feature this is the only C++ in the project, and it needs nothing but the compiler
//! and Qt's own tools. `--no-default-features` builds the command line alone.
//!
//! On Windows there is no Qt. The programs get their icon and manifest here instead,
//! from an object file written byte by byte so that no resource compiler is needed.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

const QT_MODULES: [&str; 5] = ["Qt6Core", "Qt6Gui", "Qt6Widgets", "Qt6Qml", "Qt6Quick"];
const LAYER_SHELL_HEADER: &str = "LayerShellQt/window.h";
const LAYER_SHELL_LIB: &str = "-lLayerShellQtInterface";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resources();
        return;
    }
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

const ICON_FILE: &str = "windows/src/Assets/WispX.ico";
const MANIFEST_FILE: &str = "packaging/windows/wispx.manifest";
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_MANIFEST: u16 = 24;
const LANGUAGE_EN_US: u16 = 0x0409;
const DIRECTORY_SIZE: usize = 16;
const ENTRY_SIZE: usize = 8;
const DATA_ENTRY_SIZE: usize = 16;
const HIGH_BIT: u32 = 0x8000_0000;

/// One resource: its type, its number and its bytes.
struct Resource {
    kind: u16,
    id: u16,
    data: Vec<u8>,
}

fn windows_resources() {
    println!("cargo:rerun-if-changed={ICON_FILE}");
    println!("cargo:rerun-if-changed={MANIFEST_FILE}");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    // The machine, and that machine's relocation for "address relative to the image".
    let (machine, relocation) = match arch.as_str() {
        "x86_64" => (0x8664u16, 3u16),
        "aarch64" => (0xAA64, 2),
        "x86" => (0x14C, 7),
        _ => {
            println!("cargo:warning=no icon or manifest on {arch}");
            return;
        }
    };
    let read = |path: &str| {
        std::fs::read(path).unwrap_or_else(|error| {
            eprintln!("cannot read {path}: {error}");
            exit(1);
        })
    };
    let mut resources = icon_resources(&read(ICON_FILE));
    resources.push(Resource {
        kind: RT_MANIFEST,
        id: 1,
        data: read(MANIFEST_FILE),
    });
    let object = resource_object(&resources, machine, relocation);
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let path = out_dir.join("wispx-resources.obj");
    std::fs::write(&path, object).unwrap_or_else(|error| {
        eprintln!("cannot write {}: {error}", path.display());
        exit(1);
    });
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// An `.ico` file becomes one resource per image, and a list of them that is numbered 1.
/// The list is the file's own, with each image's file offset replaced by its number.
fn icon_resources(file: &[u8]) -> Vec<Resource> {
    let count = usize::from(u16_at(file, 4));
    let mut resources = Vec::with_capacity(count + 1);
    let mut group = file[..6].to_vec();
    for index in 0..count {
        let entry = &file[6 + index * 16..6 + (index + 1) * 16];
        let size = u32_at(entry, 8) as usize;
        let offset = u32_at(entry, 12) as usize;
        let id = u16::try_from(index + 1).expect("an icon file has few images");
        group.extend_from_slice(&entry[..12]);
        group.extend_from_slice(&id.to_le_bytes());
        resources.push(Resource {
            kind: RT_ICON,
            id,
            data: file[offset..offset + size].to_vec(),
        });
    }
    resources.push(Resource {
        kind: RT_GROUP_ICON,
        id: 1,
        data: group,
    });
    resources
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn offset(value: usize) -> u32 {
    u32::try_from(value).expect("resources are far smaller than 4 GB")
}

/// A directory header, with all its entries numbered and none named.
fn push_directory(out: &mut Vec<u8>, entries: usize) {
    out.extend_from_slice(&[0; 12]);
    push_u16(out, 0);
    push_u16(out, u16::try_from(entries).expect("few resources"));
}

/// A COFF object of the kind a resource compiler leaves behind: `.rsrc$01` has the tree
/// the system reads, type then number then language, ending in entries that give each
/// resource's address, and `.rsrc$02` has the bytes those addresses are relocated to.
fn resource_object(resources: &[Resource], machine: u16, relocation: u16) -> Vec<u8> {
    let mut sorted: Vec<&Resource> = resources.iter().collect();
    sorted.sort_by_key(|resource| (resource.kind, resource.id));
    let mut kinds: Vec<u16> = sorted.iter().map(|resource| resource.kind).collect();
    kinds.dedup();

    // The tree's layout comes first, since each level points at the next.
    let root_size = DIRECTORY_SIZE + kinds.len() * ENTRY_SIZE;
    let mut name_at = Vec::with_capacity(kinds.len());
    let mut position = root_size;
    for kind in &kinds {
        name_at.push(position);
        let members = sorted.iter().filter(|r| r.kind == *kind).count();
        position += DIRECTORY_SIZE + members * ENTRY_SIZE;
    }
    let languages_at = position;
    let language_size = DIRECTORY_SIZE + ENTRY_SIZE;
    let entries_at = languages_at + sorted.len() * language_size;
    let tree_size = entries_at + sorted.len() * DATA_ENTRY_SIZE;

    let mut tree = Vec::with_capacity(tree_size);
    push_directory(&mut tree, kinds.len());
    for (kind, at) in kinds.iter().zip(&name_at) {
        push_u32(&mut tree, u32::from(*kind));
        push_u32(&mut tree, offset(*at) | HIGH_BIT);
    }
    let mut member = 0;
    for kind in &kinds {
        let members: Vec<&&Resource> = sorted.iter().filter(|r| r.kind == *kind).collect();
        push_directory(&mut tree, members.len());
        for resource in members {
            push_u32(&mut tree, u32::from(resource.id));
            push_u32(
                &mut tree,
                offset(languages_at + member * language_size) | HIGH_BIT,
            );
            member += 1;
        }
    }
    for index in 0..sorted.len() {
        push_directory(&mut tree, 1);
        push_u32(&mut tree, u32::from(LANGUAGE_EN_US));
        push_u32(&mut tree, offset(entries_at + index * DATA_ENTRY_SIZE));
    }
    let mut data = Vec::new();
    let mut relocations = Vec::with_capacity(sorted.len());
    for (index, resource) in sorted.iter().enumerate() {
        relocations.push(entries_at + index * DATA_ENTRY_SIZE);
        // The linker adds the section's address to this offset into it.
        push_u32(&mut tree, offset(data.len()));
        push_u32(&mut tree, offset(resource.data.len()));
        push_u32(&mut tree, 0);
        push_u32(&mut tree, 0);
        data.extend_from_slice(&resource.data);
        data.resize(data.len().next_multiple_of(8), 0);
    }
    assert_eq!(tree.len(), tree_size);

    const FILE_HEADER: usize = 20;
    const SECTION_HEADER: usize = 40;
    const RELOCATION: usize = 10;
    const SYMBOL: usize = 18;
    const READ_ONLY_DATA: u32 = 0x4000_0040;
    let tree_at = FILE_HEADER + 2 * SECTION_HEADER;
    let relocations_at = tree_at + tree.len();
    let data_at = relocations_at + relocations.len() * RELOCATION;
    let symbols_at = data_at + data.len();

    let mut out = Vec::with_capacity(symbols_at + 2 * SYMBOL + 4);
    push_u16(&mut out, machine);
    push_u16(&mut out, 2);
    push_u32(&mut out, 0);
    push_u32(&mut out, offset(symbols_at));
    push_u32(&mut out, 2);
    push_u16(&mut out, 0);
    push_u16(&mut out, 0);

    let mut section = |name: &[u8; 8], size: usize, at: usize, relocs_at: usize, relocs: usize| {
        out.extend_from_slice(name);
        push_u32(&mut out, 0);
        push_u32(&mut out, 0);
        push_u32(&mut out, offset(size));
        push_u32(&mut out, offset(at));
        push_u32(&mut out, offset(relocs_at));
        push_u32(&mut out, 0);
        push_u16(&mut out, u16::try_from(relocs).expect("few resources"));
        push_u16(&mut out, 0);
        push_u32(&mut out, READ_ONLY_DATA);
    };
    section(
        b".rsrc$01",
        tree.len(),
        tree_at,
        relocations_at,
        relocations.len(),
    );
    section(b".rsrc$02", data.len(), data_at, 0, 0);

    out.extend_from_slice(&tree);
    for at in relocations {
        push_u32(&mut out, offset(at));
        // The symbol for `.rsrc$02`, the second of the two below.
        push_u32(&mut out, 1);
        push_u16(&mut out, relocation);
    }
    out.extend_from_slice(&data);

    for (name, number) in [(b".rsrc$01", 1u16), (b".rsrc$02", 2)] {
        out.extend_from_slice(name);
        push_u32(&mut out, 0);
        push_u16(&mut out, number);
        push_u16(&mut out, 0);
        // A static symbol, with nothing more to say about it.
        out.push(3);
        out.push(0);
    }
    // An empty string table is only its own length.
    push_u32(&mut out, 4);
    out
}
