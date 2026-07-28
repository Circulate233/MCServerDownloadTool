use std::fs;

use mc_server_download_tool::manifest::{FileDownload, LoaderKind, Manifest, load_manifest};
use serde_json::{Value, json};

fn valid_manifest() -> Value {
    json!({
        "schema_version": 1,
        "minecraft": { "version": "1.21.1" },
        "java": {
            "major": 21,
            "min_memory_mb": 2048,
            "max_memory_mb": 4096,
            "jvm_args": ["-XX:+UseG1GC"],
            "server_args": ["nogui"]
        },
        "loader": {
            "kind": "fabric",
            "version": "0.16.10",
            "installer": {
                "url": "https://maven.fabricmc.net/installer.jar",
                "sha1": "1111111111111111111111111111111111111111",
                "size": 1024
            },
            "output": {
                "type": "exact_jar",
                "path": "fabric-server-launch.jar",
                "main_class": "net.fabricmc.loader.impl.launch.server.FabricServerLauncher"
            }
        },
        "files": [{
            "name": "Example Mod",
            "type": "mod",
            "path": "mods/example.jar",
            "download": {
                "mode": "automatic",
                "url": "https://downloads.example.com/example.jar"
            },
            "project_page": "https://example.com/project/example",
            "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "size": 1024
        }]
    })
}

fn parse(value: &Value) -> Result<mc_server_download_tool::manifest::ValidatedManifest, String> {
    Manifest::from_slice(&serde_json::to_vec(value).unwrap()).map_err(|error| error.to_string())
}

#[test]
fn parses_complete_schema_and_split_java_arguments() {
    let manifest = parse(&valid_manifest()).unwrap();

    assert_eq!(manifest.minecraft().version, "1.21.1");
    assert_eq!(manifest.loader().kind, LoaderKind::Fabric);
    assert_eq!(manifest.java().major, 21);
    assert_eq!(manifest.java().jvm_args, ["-XX:+UseG1GC"]);
    assert_eq!(manifest.java().server_args, ["nogui"]);
    assert!(matches!(
        manifest.files()[0].download,
        FileDownload::Automatic { .. }
    ));
}

#[test]
fn loads_server_install_manifest_from_disk() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("server-install.json");
    fs::write(&path, serde_json::to_vec(&valid_manifest()).unwrap()).unwrap();

    let manifest = load_manifest(&path).unwrap();

    assert_eq!(manifest.files()[0].path, "mods/example.jar");
}

#[test]
fn recursively_rejects_unknown_and_old_schema_fields() {
    for pointer in [
        "root",
        "minecraft",
        "loader",
        "installer",
        "file",
        "download",
    ] {
        let mut value = valid_manifest();
        match pointer {
            "root" => value["unexpected"] = json!(true),
            "minecraft" => value["minecraft"]["unexpected"] = json!(true),
            "loader" => value["loader"]["unexpected"] = json!(true),
            "installer" => value["loader"]["installer"]["unexpected"] = json!(true),
            "file" => value["files"][0]["unexpected"] = json!(true),
            "download" => value["files"][0]["download"]["unexpected"] = json!(true),
            _ => unreachable!(),
        }
        assert!(parse(&value).unwrap_err().contains("unknown field"));
    }

    let mut old = valid_manifest();
    old["files"][0].as_object_mut().unwrap().remove("download");
    old["files"][0]["source"] = json!({"type": "direct", "url": "https://example.com"});
    assert!(parse(&old).is_err());
}

#[test]
fn rejects_schema_and_loader_families_outside_v1() {
    let mut value = valid_manifest();
    value["schema_version"] = json!(2);
    assert!(parse(&value).unwrap_err().contains("schema_version"));

    for unsupported in ["vanilla", "quilt"] {
        let mut value = valid_manifest();
        value["loader"]["kind"] = json!(unsupported);
        assert!(parse(&value).is_err());
    }
}

#[test]
fn accepts_inline_sha1_or_same_origin_sidecar_exclusively() {
    let mut sidecar = valid_manifest();
    sidecar["loader"]["installer"]
        .as_object_mut()
        .unwrap()
        .remove("sha1");
    sidecar["loader"]["installer"]["sha1_sidecar"] =
        json!("https://maven.fabricmc.net/installer.jar.sha1");
    assert!(parse(&sidecar).is_ok());

    sidecar["loader"]["installer"]["sha1"] = json!("1111111111111111111111111111111111111111");
    assert!(parse(&sidecar).unwrap_err().contains("exactly one"));

    let mut cross_origin = valid_manifest();
    cross_origin["loader"]["installer"]
        .as_object_mut()
        .unwrap()
        .remove("sha1");
    cross_origin["loader"]["installer"]["sha1_sidecar"] =
        json!("https://evil.example/installer.jar.sha1");
    assert!(parse(&cross_origin).unwrap_err().contains("same origin"));
}

#[test]
fn cleanroom_is_reachable_with_an_exact_output() {
    let mut value = valid_manifest();
    value["minecraft"]["version"] = json!("1.12.2");
    value["java"]["major"] = json!(8);
    value["loader"]["kind"] = json!("cleanroom");
    value["loader"]["version"] = json!("0.3.0");
    value["loader"]["output"]["path"] = json!("cleanroom-1.12.2.jar");
    value["loader"]["output"]["main_class"] = Value::Null;

    assert_eq!(parse(&value).unwrap().loader().kind, LoaderKind::Cleanroom);
}

#[test]
fn validates_java_loader_urls_hashes_and_outputs() {
    let mut memory = valid_manifest();
    memory["java"]["min_memory_mb"] = json!(8192);
    assert!(parse(&memory).unwrap_err().contains("min_memory_mb"));

    let mut argument = valid_manifest();
    argument["java"]["server_args"][0] = json!("\n");
    assert!(parse(&argument).unwrap_err().contains("server_args"));

    let mut installer = valid_manifest();
    installer["loader"]["installer"]["url"] = json!("http://example.com/installer.jar");
    assert!(parse(&installer).unwrap_err().contains("HTTPS"));

    let mut hash = valid_manifest();
    hash["files"][0]["sha1"] = json!("bad");
    assert!(parse(&hash).unwrap_err().contains("SHA-1"));

    let mut output = valid_manifest();
    output["loader"]["output"]["path"] = json!("../server.jar");
    assert!(parse(&output).is_err());
}

#[test]
fn rejects_unsafe_duplicate_and_reserved_paths() {
    for path in [
        "../server.properties",
        "/absolute/file.jar",
        "C:/server/file.jar",
        "mods\\file.jar",
        "mods/./file.jar",
        "mods/CON.jar",
        "server-install.json",
        ".mcsdt/state.json",
        "start.bat",
        "start.sh",
        "missing-files.txt",
        "mc-server-download-tool.exe",
        "MCServerDownloadTool-windows-x86_64.exe",
        "mcserverdownloadtool-LINUX-x86_64",
        "MCServerDownloadTool-macos-aarch64",
    ] {
        let mut value = valid_manifest();
        value["files"][0]["path"] = json!(path);
        assert!(parse(&value).is_err(), "path should be rejected: {path}");
    }

    let mut duplicate = valid_manifest();
    let mut second = duplicate["files"][0].clone();
    second["path"] = json!("MODS/EXAMPLE.JAR");
    duplicate["files"].as_array_mut().unwrap().push(second);
    let error = parse(&duplicate).unwrap_err();
    assert!(error.contains("files[1].path"), "{error}");
    assert!(error.contains("files[0].path"), "{error}");
    assert!(error.contains("MODS/EXAMPLE.JAR"), "{error}");
}

#[test]
fn rejects_file_target_that_conflicts_with_exact_loader_output() {
    let mut value = valid_manifest();
    value["files"][0]["path"] = json!("FABRIC-SERVER-LAUNCH.JAR");

    let error = parse(&value).unwrap_err();

    assert!(error.contains("files[0].path"), "{error}");
    assert!(error.contains("loader.output.path"), "{error}");
    assert!(error.contains("FABRIC-SERVER-LAUNCH.JAR"), "{error}");
}

#[test]
fn rejects_file_target_that_conflicts_with_either_modern_args_output() {
    for (field, path) in [
        ("loader.output.windows", "libraries/forge/win_args.txt"),
        ("loader.output.unix", "libraries/forge/unix_args.txt"),
    ] {
        let mut value = valid_manifest();
        value["loader"]["kind"] = json!("forge");
        value["loader"]["output"] = json!({
            "type": "modern_args",
            "windows": "libraries/forge/win_args.txt",
            "unix": "libraries/forge/unix_args.txt"
        });
        value["files"][0]["path"] = json!(path.to_ascii_uppercase());

        let error = parse(&value).unwrap_err();

        assert!(error.contains("files[0].path"), "{error}");
        assert!(error.contains(field), "{error}");
        assert!(error.contains(&path.to_ascii_uppercase()), "{error}");
    }
}

#[test]
fn rejects_case_insensitive_collision_between_modern_args_outputs() {
    let mut value = valid_manifest();
    value["loader"]["kind"] = json!("neoforge");
    value["loader"]["output"] = json!({
        "type": "modern_args",
        "windows": "libraries/neoforge/args.txt",
        "unix": "LIBRARIES/NEOFORGE/ARGS.TXT"
    });

    let error = parse(&value).unwrap_err();

    assert!(error.contains("loader.output.unix"), "{error}");
    assert!(error.contains("loader.output.windows"), "{error}");
    assert!(error.contains("LIBRARIES/NEOFORGE/ARGS.TXT"), "{error}");
}

#[test]
fn loader_outputs_reject_every_installer_reserved_target() {
    for path in [
        "server-install.json",
        ".mcsdt/loader.jar",
        "start.bat",
        "start.sh",
        "MCServerDownloadTool-windows-x86_64.exe",
        "MCServerDownloadTool-linux-x86_64",
        "MCServerDownloadTool-macos-aarch64",
    ] {
        let mut value = valid_manifest();
        value["loader"]["output"]["path"] = json!(path);
        let error = parse(&value).unwrap_err();
        assert!(
            error.contains("reserved"),
            "loader output should be rejected as reserved: {path}; {error}"
        );
    }
}

#[test]
fn curseforge_cdn_downloads_require_a_nonblank_key_without_leaking_it() {
    let mut value = valid_manifest();
    value["files"][0]["download"]["url"] = json!("https://edge.forgecdn.net/files/1/2/example.jar");
    assert!(parse(&value).unwrap_err().contains("CurseForge API key"));

    value["curseforge_api_key"] = json!("super-secret-key");
    let manifest = parse(&value).unwrap();
    assert!(!format!("{manifest:?}").contains("super-secret-key"));

    let mut unused = valid_manifest();
    unused["curseforge_api_key"] = json!("unused-secret");
    assert!(parse(&unused).unwrap_err().contains("no CurseForge CDN"));
}

#[test]
fn manual_download_requires_a_project_page_but_no_download_url() {
    let mut value = valid_manifest();
    value["files"][0]["download"] = json!({"mode": "manual"});
    assert!(parse(&value).is_ok());

    value["files"][0]["project_page"] = json!("file:///manual.jar");
    assert!(parse(&value).unwrap_err().contains("project_page"));
}
