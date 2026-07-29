use std::fs;

use mc_server_download_tool::manifest::{FileDownload, LoaderKind, Manifest, load_manifest};
use serde_json::{Value, json};

fn valid_manifest() -> Value {
    json!({
        "schema_version": 1,
        "curseforge_api_key": "test-key",
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
                "url": "https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar",
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
                "url": "https://edge.forgecdn.net/files/1234/56/example.jar"
            },
            "project_page": "https://www.curseforge.com/minecraft/mc-mods/example/files/123456",
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
    sidecar["loader"]["installer"]["sha1_sidecar"] = json!(
        "https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar.sha1"
    );
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
    value["loader"]["installer"]["url"] = json!(
        "https://github.com/CleanroomMC/Cleanroom/releases/download/0.3.0/cleanroom-0.3.0-installer.jar"
    );
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
    for field in ["loader.output.windows", "loader.output.unix"] {
        let mut value = valid_manifest();
        value["loader"]["kind"] = json!("forge");
        value["loader"]["version"] = json!("52.0.2");
        value["loader"]["installer"]["url"] = json!(
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.0.2/forge-1.21.1-52.0.2-installer.jar"
        );
        value["loader"]["output"] = json!({
            "type": "modern_args",
            "windows": "libraries/net/minecraftforge/forge/1.21.1-52.0.2/win_args.txt",
            "unix": "libraries/net/minecraftforge/forge/1.21.1-52.0.2/unix_args.txt"
        });
        let exact_path = if field.ends_with("windows") {
            "libraries/net/minecraftforge/forge/1.21.1-52.0.2/win_args.txt"
        } else {
            "libraries/net/minecraftforge/forge/1.21.1-52.0.2/unix_args.txt"
        };
        value["files"][0]["path"] = json!(exact_path.to_ascii_uppercase());

        let error = parse(&value).unwrap_err();

        assert!(error.contains("files[0].path"), "{error}");
        assert!(error.contains(field), "{error}");
        assert!(error.contains(&exact_path.to_ascii_uppercase()), "{error}");
    }
}

#[test]
fn rejects_any_modern_args_output_that_deviates_from_the_exact_coordinate() {
    let mut value = valid_manifest();
    value["loader"]["kind"] = json!("neoforge");
    value["loader"]["version"] = json!("21.1.1");
    value["loader"]["installer"]["url"] = json!(
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.1/neoforge-21.1.1-installer.jar"
    );
    value["loader"]["output"] = json!({
        "type": "modern_args",
        "windows": "libraries/net/neoforged/neoforge/21.1.1/win_args.txt",
        "unix": "LIBRARIES/NET/NEOFORGED/NEOFORGE/21.1.1/WIN_ARGS.TXT"
    });

    let error = parse(&value).unwrap_err();

    assert!(error.contains("loader.output"), "{error}");
    assert!(error.contains("exact output contract"), "{error}");
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
    value.as_object_mut().unwrap().remove("curseforge_api_key");
    value["files"][0]["download"]["url"] = json!("https://edge.forgecdn.net/files/1/2/example.jar");
    assert!(parse(&value).unwrap_err().contains("CurseForge API key"));

    value["curseforge_api_key"] = json!("super-secret-key");
    let manifest = parse(&value).unwrap();
    assert!(!format!("{manifest:?}").contains("super-secret-key"));

    let mut unused = valid_manifest();
    unused["curseforge_api_key"] = json!("unused-secret");
    unused["files"][0]["download"] = json!({"mode": "manual"});
    assert!(parse(&unused).unwrap_err().contains("no CurseForge CDN"));
}

#[test]
fn manual_download_requires_a_project_page_but_no_download_url() {
    let mut value = valid_manifest();
    value["files"][0]["download"] = json!({"mode": "manual"});
    value.as_object_mut().unwrap().remove("curseforge_api_key");
    assert!(parse(&value).is_ok());

    value["files"][0]["project_page"] = json!("file:///manual.jar");
    assert!(parse(&value).unwrap_err().contains("project_page"));
}

#[test]
fn manifest_and_collection_limits_fail_before_unbounded_work() {
    let oversized = vec![b' '; 8 * 1024 * 1024 + 1];
    let error = Manifest::from_slice(&oversized).unwrap_err().to_string();
    assert!(error.contains("manifest"));
    assert!(error.contains("8388608"));

    let mut too_many_files = valid_manifest();
    let template = too_many_files["files"][0].clone();
    too_many_files["files"] = Value::Array(
        (0..20_001)
            .map(|index| {
                let mut file = template.clone();
                file["path"] = json!(format!("mods/file-{index}.jar"));
                file
            })
            .collect(),
    );
    let error = parse(&too_many_files).unwrap_err();
    assert!(error.contains("files"), "{error}");
    assert!(error.contains("20000"), "{error}");

    let mut too_many_args = valid_manifest();
    too_many_args["java"]["jvm_args"] =
        Value::Array((0..513).map(|_| json!("-Dsafe=true")).collect());
    let error = parse(&too_many_args).unwrap_err();
    assert!(error.contains("argument array"), "{error}");

    let mut long_arg = valid_manifest();
    long_arg["java"]["server_args"] = json!(["x".repeat(8 * 1024 + 1)]);
    let error = parse(&long_arg).unwrap_err();
    assert!(error.contains("server_args[0]"), "{error}");
    assert!(error.contains("8192"), "{error}");
}

#[test]
fn loader_urls_reject_origin_authority_suffix_and_coordinate_variants() {
    for url in [
        "https://evil.example/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar",
        "https://maven.fabricmc.net:444/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar",
        "https://user@maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar",
        "https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar?mirror=1",
        "https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-1.1.1.jar#fragment",
        "https://maven.fabricmc.net/net/fabricmc/fabric-installer/1.1.1/fabric-installer-wrong.jar",
    ] {
        let mut value = valid_manifest();
        value["loader"]["installer"]["url"] = json!(url);
        let error = parse(&value).unwrap_err();
        assert!(error.contains("loader.installer.url"), "{url}: {error}");
    }

    let mut forge = valid_manifest();
    forge["loader"]["kind"] = json!("forge");
    forge["loader"]["version"] = json!("52.0.2");
    forge["loader"]["installer"]["url"] = json!(
        "https://maven.minecraftforge.net/net/minecraftforge/forge/1.21.1-52.0.3/forge-1.21.1-52.0.3-installer.jar"
    );
    assert!(parse(&forge).unwrap_err().contains("loader.installer.url"));

    let mut neoforge = valid_manifest();
    neoforge["loader"]["kind"] = json!("neoforge");
    neoforge["loader"]["version"] = json!("21.1.1");
    neoforge["loader"]["installer"]["url"] = json!(
        "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.2/neoforge-21.1.2-installer.jar"
    );
    assert!(
        parse(&neoforge)
            .unwrap_err()
            .contains("loader.installer.url")
    );

    let mut cleanroom = valid_manifest();
    cleanroom["minecraft"]["version"] = json!("1.12.2");
    cleanroom["java"]["major"] = json!(8);
    cleanroom["loader"]["kind"] = json!("cleanroom");
    cleanroom["loader"]["version"] = json!("0.6.7-alpha");
    cleanroom["loader"]["installer"]["url"] = json!(
        "https://github.com/Other/Cleanroom/releases/download/0.6.7-alpha/cleanroom-0.6.7-alpha-installer.jar"
    );
    assert!(
        parse(&cleanroom)
            .unwrap_err()
            .contains("loader.installer.url")
    );
}

#[test]
fn curseforge_urls_reject_origin_port_suffix_and_path_variants() {
    for url in [
        "https://evil.example/files/1234/56/example.jar",
        "https://edge.forgecdn.net:444/files/1234/56/example.jar",
        "https://user@edge.forgecdn.net/files/1234/56/example.jar",
        "https://edge.forgecdn.net/files/1234/56/example.jar?key=value",
        "https://edge.forgecdn.net/files/1234/56/example.jar#fragment",
        "https://edge.forgecdn.net/files/not-a-number/56/example.jar",
        "https://edge.forgecdn.net/files/1234/56/sub/example.jar",
    ] {
        let mut value = valid_manifest();
        value["files"][0]["download"]["url"] = json!(url);
        let error = parse(&value).unwrap_err();
        assert!(error.contains("download.url"), "{url}: {error}");
    }

    for page in [
        "https://evil.example/minecraft/mc-mods/example/files/123456",
        "https://www.curseforge.com:444/minecraft/mc-mods/example/files/123456",
        "https://user@www.curseforge.com/minecraft/mc-mods/example/files/123456",
        "https://www.curseforge.com/minecraft/mc-mods/example/files/123456?x=1",
        "https://www.curseforge.com/minecraft/texture-packs/example/files/123456",
        "https://www.curseforge.com/minecraft/mc-mods/example/files/not-a-number",
        "https://www.curseforge.com/minecraft/mc-mods/example/files/123456/extra",
    ] {
        let mut value = valid_manifest();
        value["files"][0]["project_page"] = json!(page);
        let error = parse(&value).unwrap_err();
        assert!(error.contains("project_page"), "{page}: {error}");
    }
}

#[test]
fn windows_device_aliases_and_unicode_digit_variants_are_rejected() {
    for component in [
        "CON",
        "con.txt",
        "CONIN$",
        "CONOUT$.log",
        "COM1",
        "LPT9.jar",
        "COM¹.jar",
        "COM².jar",
        "COM³.jar",
        "LPT¹.jar",
        "LPT².jar",
        "LPT³.jar",
    ] {
        let mut value = valid_manifest();
        value["files"][0]["path"] = json!(format!("mods/{component}"));
        let error = parse(&value).unwrap_err();
        assert!(
            error.contains("reserved Windows device name"),
            "{component}: {error}"
        );
    }
}
