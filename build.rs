use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let build_time = chrono::Utc::now().to_rfc3339();
    println!("cargo:rustc-env=EDGELINK_BUILD_TIME={build_time}");

    set_git_revision_hash();
    check_patch();
    gen_use_plugins_file();
    build_static_files();
}

fn gen_use_plugins_file() {
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("__use_node_plugins.rs");
    let plugins_dir = Path::new("node-plugins");
    let mut plugin_names = Vec::new();

    if plugins_dir.is_dir() {
        for entry in fs::read_dir(plugins_dir).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_dir() {
                let plugin_name = entry.file_name().to_string_lossy().replace("-", "_");
                plugin_names.push(plugin_name);
            }
        }
    }

    let mut file_content = String::new();
    for plugin_name in plugin_names {
        file_content.push_str(&format!("extern crate {plugin_name};\n"));
    }

    fs::write(&dest_path, file_content).unwrap();

    println!("cargo:rerun-if-changed=node-plugins");
}

/// Make the current git hash available to the build as the environment
/// variable `EDGELINK_BUILD_GIT_HASH`.
fn set_git_revision_hash() {
    let args = &["rev-parse", "--short=10", "HEAD"];
    let Ok(output) = Command::new("git").args(args).output() else {
        return;
    };
    let rev = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if rev.is_empty() {
        return;
    }
    println!("cargo:rustc-env=EDGELINK_BUILD_GIT_HASH={rev}");
}

fn check_patch() {
    if env::consts::OS == "windows" {
        let output = Command::new("patch.exe")
            .arg("--version")
            .output()
            .expect("Failed to execute `patch.exe --version`, the GNU Patch program is required to build this project");

        if output.status.success() {
            let version_info = String::from_utf8_lossy(&output.stdout);
            let first_line = version_info.lines().next().unwrap_or("Unknown version");
            if !first_line.to_lowercase().contains("patch") {
                eprintln!("Error: The Patch program is required to build this project, but got: {first_line}");
                std::process::exit(1);
            }
        } else {
            let error_info = String::from_utf8_lossy(&output.stderr);
            eprintln!("Error: Failed to get patch.exe version: {error_info}");
            std::process::exit(1);
        }
    }
}

fn build_static_files() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=crates/web/public");
    println!("cargo:rerun-if-changed=crates/web/client/src");
    println!("cargo:rerun-if-changed=crates/web/client/index.html");
    println!("cargo:rerun-if-changed=crates/web/client/package.json");
    println!("cargo:rerun-if-changed=crates/web/client/package-lock.json");
    println!("cargo:rerun-if-changed=crates/web/client/vite.config.ts");
    println!("cargo:rerun-if-changed=3rd-party/node-red/packages");
    println!("cargo:rerun-if-changed=3rd-party/node-red/package.json");
    println!("cargo:rerun-if-changed=3rd-party/node-red/package-lock.json");

    let static_dir = PathBuf::from("target/ui_static");
    let public_dir = PathBuf::from("crates/web/public");
    let node_red_dir = PathBuf::from("3rd-party/node-red/packages/node_modules/@node-red/editor-client/public");
    let node_red_nodes_dir = PathBuf::from("3rd-party/node-red/packages/node_modules/@node-red/nodes");
    let node_red_root = PathBuf::from("3rd-party/node-red");

    // Build Node-RED if needed
    if node_red_root.exists() {
        // Check if Node-RED needs to be built
        let package_json = node_red_root.join("package.json");
        let node_modules = node_red_root.join("node_modules");

        let want_version = package_version(&package_json);
        let built_version = fs::read_to_string(static_dir.join(".editor-version")).ok().map(|v| v.trim().to_string());
        let version_changed = want_version.is_some() && want_version != built_version;
        if package_json.exists() && (!node_modules.exists() || !node_red_dir.exists() || version_changed) {
            println!("cargo:warning=Building Node-RED editor...");
            build_node_red(&node_red_root);
            if let Some(version) = &want_version {
                let _ = fs::create_dir_all(&static_dir);
                let _ = fs::write(static_dir.join(".editor-version"), version);
            }
        }
    }

    // Only build if source directories exist
    if public_dir.exists() || node_red_dir.exists() || node_red_nodes_dir.exists() {
        println!("cargo:warning=Building static files directory...");

        // Create static directory if it doesn't exist
        std::fs::create_dir_all(&static_dir).expect("Failed to create static directory");

        // Incrementally copy from public directory
        if public_dir.exists() {
            copy_dir_contents_incremental(&public_dir, &static_dir).expect("Failed to copy public files");
        }

        // Incrementally copy from node-red directory
        if node_red_dir.exists() {
            copy_dir_contents_incremental(&node_red_dir, &static_dir).expect("Failed to copy node-red files");
        }

        // Copy Node-RED nodes directory to static/nodes
        if node_red_nodes_dir.exists() {
            let static_nodes_dir = static_dir.join("nodes");
            std::fs::create_dir_all(&static_nodes_dir).expect("Failed to create static nodes directory");
            copy_dir_contents_incremental(&node_red_nodes_dir, &static_nodes_dir)
                .expect("Failed to copy node-red nodes");
        }

        // The scan palette entry exists only when the runtime registers the node.
        copy_scan_editor(&static_dir);
        // The Modbus palette entry exists only when the runtime registers the node.
        copy_modbus_editor(&static_dir);
        copy_ai_editor(&static_dir);
        copy_db_editor(&static_dir);

        // Copy Node-RED core nodes lib directories to static/
        if node_red_nodes_dir.exists() {
            copy_node_lib_directories(&node_red_nodes_dir, &static_dir).expect("Failed to copy node lib directories");
        }

        // Copy Node-RED icon files to static/icons (for icon requests like /icons/node-red/file-in.svg)
        if node_red_nodes_dir.exists() {
            copy_node_red_icons(&node_red_nodes_dir, &static_dir).expect("Failed to copy node-red icons");
        }

        // Copy Node-RED locales for i18n support
        copy_node_red_locales(&static_dir).expect("Failed to copy node-red locales");

        copy_client_page(&static_dir);

        println!("cargo:warning=Static files build complete!");
    }
}

/// The scan node is registered only with the `runtime_scan` feature. Ship its editor
/// form in that build, and drop a leftover copy so a later default build does not
/// keep offering a node the runtime will not load.
#[cfg(feature = "runtime_scan")]
fn copy_scan_editor(static_dir: &Path) {
    println!("cargo:rerun-if-changed=crates/web/scan-editor/80-scan.html");
    let src = PathBuf::from("crates/web/scan-editor/80-scan.html");
    if !src.exists() {
        return;
    }
    let dest_dir = static_dir.join("nodes/core/function");
    std::fs::create_dir_all(&dest_dir).expect("Failed to create the scan editor directory");
    std::fs::copy(&src, dest_dir.join("80-scan.html")).expect("Failed to copy the scan editor");
}

#[cfg(not(feature = "runtime_scan"))]
fn copy_scan_editor(static_dir: &Path) {
    println!("cargo:rerun-if-changed=crates/web/scan-editor/80-scan.html");
    let dest = static_dir.join("nodes/core/function/80-scan.html");
    if dest.exists() {
        let _ = std::fs::remove_file(dest);
    }
}

/// The Modbus node is registered only with the `nodes_modbus` feature. Ship its editor
/// form in that build, and drop a leftover copy so a later default build does not
/// keep offering a node the runtime will not load.
#[cfg(feature = "nodes_modbus")]
fn copy_modbus_editor(static_dir: &Path) {
    println!("cargo:rerun-if-changed=crates/web/modbus-editor/82-modbus.html");
    let src = PathBuf::from("crates/web/modbus-editor/82-modbus.html");
    if !src.exists() {
        return;
    }
    let dest_dir = static_dir.join("nodes/core/network");
    std::fs::create_dir_all(&dest_dir).expect("Failed to create the modbus editor directory");
    std::fs::copy(&src, dest_dir.join("82-modbus.html")).expect("Failed to copy the modbus editor");
}

#[cfg(not(feature = "nodes_modbus"))]
fn copy_modbus_editor(static_dir: &Path) {
    println!("cargo:rerun-if-changed=crates/web/modbus-editor/82-modbus.html");
    let dest = static_dir.join("nodes/core/network/82-modbus.html");
    if dest.exists() {
        let _ = std::fs::remove_file(dest);
    }
}

fn copy_ai_editor(static_dir: &Path) {
    println!("cargo:rerun-if-changed=crates/web/ai-editor/90-ai-provider.html");
    println!("cargo:rerun-if-changed=crates/web/ai-editor/91-ai-chat.html");
    println!("cargo:rerun-if-changed=crates/web/ai-editor/96-ai-split.html");
    println!("cargo:rerun-if-changed=crates/web/ai-editor/97-ai-structured.html");
    println!("cargo:rerun-if-changed=crates/web/ai-editor/98-ai-embed.html");
    println!("cargo:rerun-if-changed=crates/web/ai-editor/99-ai-agent.html");
    let dest_dir = static_dir.join("nodes/core/function");
    let _ = std::fs::create_dir_all(&dest_dir);
    let files = [
        (cfg!(feature = "nodes_ai"), "90-ai-provider.html"),
        (cfg!(feature = "nodes_ai"), "91-ai-chat.html"),
        (cfg!(feature = "nodes_ai_text"), "96-ai-split.html"),
        (cfg!(feature = "nodes_ai_text"), "97-ai-structured.html"),
        (cfg!(feature = "nodes_ai_embeddings"), "98-ai-embed.html"),
        (cfg!(feature = "nodes_ai_agent"), "99-ai-agent.html"),
    ];
    for (enabled, name) in files {
        let dest = dest_dir.join(name);
        if enabled {
            let src = PathBuf::from("crates/web/ai-editor").join(name);
            if src.exists() {
                let _ = std::fs::copy(&src, &dest);
            }
        } else if dest.exists() {
            let _ = std::fs::remove_file(dest);
        }
    }
}

fn copy_db_editor(static_dir: &Path) {
    println!("cargo:rerun-if-changed=crates/web/db-editor/92-postgres-config.html");
    println!("cargo:rerun-if-changed=crates/web/db-editor/93-postgres.html");
    println!("cargo:rerun-if-changed=crates/web/db-editor/94-redis-config.html");
    println!("cargo:rerun-if-changed=crates/web/db-editor/95-redis.html");
    let dest_dir = static_dir.join("nodes/core/storage");
    let _ = std::fs::create_dir_all(&dest_dir);
    let postgres = cfg!(feature = "nodes_postgres");
    let redis = cfg!(feature = "nodes_redis");
    for (feature, name) in [
        ("postgres", "92-postgres-config.html"),
        ("postgres", "93-postgres.html"),
        ("redis", "94-redis-config.html"),
        ("redis", "95-redis.html"),
    ] {
        let dest = dest_dir.join(name);
        let enabled = (feature == "postgres" && postgres) || (feature == "redis" && redis);
        if enabled {
            let src = PathBuf::from("crates/web/db-editor").join(name);
            if src.exists() {
                let _ = std::fs::copy(&src, &dest);
            }
        } else if dest.exists() {
            let _ = std::fs::remove_file(dest);
        }
    }
}

fn package_version(package_json: &Path) -> Option<String> {
    let text = fs::read_to_string(package_json).ok()?;
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("\"version\"") else {
            continue;
        };
        let rest = rest.trim().trim_start_matches(':').trim();
        let value = rest.trim_matches(|c| c == '"' || c == ',');
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Build the client-only page and copy it under `ui_static/client`, leaving the editor
/// `index.html` at the static root untouched.
fn copy_client_page(static_dir: &Path) {
    let client_root = PathBuf::from("crates/web/client");
    if !client_root.join("package.json").exists() {
        return;
    }
    let dist_index = client_root.join("dist/index.html");
    if !dist_index.exists() {
        if node_version().is_none() {
            println!("cargo:warning=Node.js not found, skipping client page build");
            return;
        }
        println!("cargo:warning=Building client page...");
        build_npm_project(&client_root, "client page");
    }
    if dist_index.exists() {
        let dest = static_dir.join("client");
        std::fs::create_dir_all(&dest).expect("Failed to create client static directory");
        copy_dir_contents_incremental(&client_root.join("dist"), &dest).expect("Failed to copy client page");
    }
}

/// Incrementally copy directory contents, only copying files that are newer or don't exist
fn copy_dir_contents_incremental(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::fs;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            fs::create_dir_all(&dst_path)?;
            copy_dir_contents_incremental(&src_path, &dst_path)?;
        } else {
            let should_copy = if !dst_path.exists() {
                true
            } else {
                let src_metadata = fs::metadata(&src_path)?;
                let dst_metadata = fs::metadata(&dst_path)?;

                // Compare file size first (faster than time comparison)
                if src_metadata.len() != dst_metadata.len() {
                    true
                } else {
                    // Compare modification time if sizes are equal
                    let src_time = src_metadata.modified().ok();
                    let dst_time = dst_metadata.modified().ok();

                    match (src_time, dst_time) {
                        (Some(src_t), Some(dst_t)) => src_t > dst_t,
                        _ => true, // If we can't get timestamps, copy to be safe
                    }
                }
            };

            if should_copy {
                if let Some(parent) = dst_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src_path, &dst_path)?;
            }
        }
    }

    Ok(())
}

fn build_node_red(node_red_root: &Path) {
    match node_version() {
        None => {
            println!("cargo:warning=Node.js not found, skipping Node-RED editor build");
            return;
        }
        Some((major, minor, version)) if major < 22 || (major == 22 && minor < 9) => {
            panic!("Node-RED 5.0.7 needs Node.js >= 22.9 (found {version})");
        }
        Some(_) => {}
    }
    let modules = node_red_root.join("node_modules");
    if modules.exists() {
        fs::remove_dir_all(&modules).expect("Failed to remove stale Node-RED node_modules");
    }
    build_npm_project(node_red_root, "Node-RED editor");
}

fn node_version() -> Option<(u32, u32, String)> {
    let node_cmd = if cfg!(target_os = "windows") { "node.exe" } else { "node" };
    let output = std::process::Command::new(node_cmd).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().trim_start_matches('v').to_string();
    let mut parts = text.split('.');
    let major = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, text))
}

fn build_npm_project(root: &Path, label: &str) {
    use std::process::Command;

    let npm_cmd = if cfg!(target_os = "windows") { "npm.cmd" } else { "npm" };
    if Command::new(npm_cmd).arg("--version").output().is_err() {
        panic!("npm is required to build the {label}");
    }

    let install_arg = if root.join("package-lock.json").exists() { "ci" } else { "install" };
    println!("cargo:warning=Installing {label} dependencies...");
    let install_result = Command::new(npm_cmd).arg(install_arg).current_dir(root).status();
    match install_result {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("npm {install_arg} failed for {label} (exit {status})"),
        Err(e) => panic!("Failed to install {label} dependencies: {e}"),
    }

    println!("cargo:warning=Building {label}...");
    let build_result = Command::new(npm_cmd).args(["run", "build"]).current_dir(root).status();
    match build_result {
        Ok(status) if status.success() => {}
        Ok(status) => panic!("npm run build failed for {label} (exit {status})"),
        Err(e) => panic!("Failed to build {label}: {e}"),
    }
}

/// Copy Node-RED icon files to static/icons directory for proper icon serving
fn copy_node_red_icons(node_red_nodes_dir: &Path, static_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let icons_src = node_red_nodes_dir.join("icons");
    if !icons_src.exists() {
        return Ok(());
    }

    let icons_dest = static_dir.join("icons/node-red");
    std::fs::create_dir_all(&icons_dest)?;

    // Copy all icon files from node-red/nodes/icons to static/icons/node-red/
    copy_dir_contents_incremental(&icons_src, &icons_dest)?;
    println!("cargo:warning=Copied Node-RED icons from {} to {}", icons_src.display(), icons_dest.display());

    Ok(())
}

fn copy_node_lib_directories(node_red_nodes_dir: &Path, static_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let core_dir = node_red_nodes_dir.join("core");
    if !core_dir.exists() {
        return Ok(());
    }

    // Copy core/*/lib/* to static/core/*/lib/*
    copy_core_lib_directories(&core_dir, static_dir)?;

    Ok(())
}

fn copy_core_lib_directories(core_dir: &Path, static_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !core_dir.is_dir() {
        return Ok(());
    }

    // Iterate through core/* directories (common, function, network, etc.)
    for entry in std::fs::read_dir(core_dir)? {
        let entry = entry?;
        let category_path = entry.path();

        if category_path.is_dir() {
            let category_name = category_path.file_name().unwrap().to_str().unwrap();
            let lib_dir = category_path.join("lib");

            if lib_dir.exists() && lib_dir.is_dir() {
                // Create static/core/{category}/lib directory
                let dest_base = static_dir.join("core").join(category_name).join("lib");
                std::fs::create_dir_all(&dest_base)?;

                // Copy all lib contents to static/core/{category}/lib/
                copy_dir_contents_incremental(&lib_dir, &dest_base)?;
                println!("cargo:warning=Copied {} to {}", lib_dir.display(), dest_base.display());
            }
        }
    }

    Ok(())
}

/// Copy Node-RED locale files to static/locales for i18n support
fn copy_node_red_locales(static_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let editor_locales_dir = PathBuf::from("3rd-party/node-red/packages/node_modules/@node-red/editor-client/locales");
    let nodes_locales_dir = PathBuf::from("3rd-party/node-red/packages/node_modules/@node-red/nodes/locales");
    let dest_locales_dir = static_dir.join("locales");

    std::fs::create_dir_all(&dest_locales_dir)?;

    // Copy editor client locales
    if editor_locales_dir.exists() {
        for entry in std::fs::read_dir(&editor_locales_dir)? {
            let entry = entry?;
            let lang_dir = entry.path();

            if lang_dir.is_dir() {
                let lang_name = lang_dir.file_name().unwrap().to_str().unwrap();
                let dest_lang_dir = dest_locales_dir.join(lang_name);
                std::fs::create_dir_all(&dest_lang_dir)?;

                // Copy all JSON files from this language directory
                for lang_entry in std::fs::read_dir(&lang_dir)? {
                    let lang_entry = lang_entry?;
                    let src_file = lang_entry.path();

                    if src_file.is_file() && src_file.extension().is_some_and(|ext| ext == "json") {
                        let dest_file = dest_lang_dir.join(lang_entry.file_name());
                        std::fs::copy(&src_file, &dest_file)?;
                    }
                }
            }
        }
        println!(
            "cargo:warning=Copied editor locales from {} to {}",
            editor_locales_dir.display(),
            dest_locales_dir.display()
        );
    }

    // Copy nodes locales
    if nodes_locales_dir.exists() {
        for entry in std::fs::read_dir(&nodes_locales_dir)? {
            let entry = entry?;
            let lang_dir = entry.path();

            if lang_dir.is_dir() {
                let lang_name = lang_dir.file_name().unwrap().to_str().unwrap();
                let dest_lang_dir = dest_locales_dir.join(lang_name);
                std::fs::create_dir_all(&dest_lang_dir)?;

                // Copy messages.json if it exists
                let messages_file = lang_dir.join("messages.json");
                if messages_file.exists() {
                    let dest_messages = dest_lang_dir.join("messages.json");
                    std::fs::copy(&messages_file, &dest_messages)?;
                }

                // Copy all node category directories (common, function, network, etc.)
                for lang_entry in std::fs::read_dir(&lang_dir)? {
                    let lang_entry = lang_entry?;
                    let category_path = lang_entry.path();

                    if category_path.is_dir() {
                        let category_name = category_path.file_name().unwrap().to_str().unwrap();
                        let dest_category_dir = dest_lang_dir.join(category_name);
                        std::fs::create_dir_all(&dest_category_dir)?;

                        // Copy all JSON files in this category
                        for category_entry in std::fs::read_dir(&category_path)? {
                            let category_entry = category_entry?;
                            let src_file = category_entry.path();

                            if src_file.is_file() && src_file.extension().is_some_and(|ext| ext == "json") {
                                let dest_file = dest_category_dir.join(category_entry.file_name());
                                std::fs::copy(&src_file, &dest_file)?;
                            }
                        }
                    }
                }
            }
        }
        println!(
            "cargo:warning=Copied nodes locales from {} to {}",
            nodes_locales_dir.display(),
            dest_locales_dir.display()
        );
    }

    Ok(())
}
