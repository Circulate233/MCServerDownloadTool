use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Parser;
use mc_server_download_tool::cli::{
    Cli, ProxyUrl, resolve_manifest_path, resolve_proxy, try_parse_localized_from,
};
use mc_server_download_tool::i18n::{Language, resolve_language};

#[test]
fn parses_all_supported_cli_options() {
    let cli = Cli::try_parse_from([
        "mc-server-download-tool",
        "--manifest",
        "custom.json",
        "--lang",
        "zh-CN",
        "--proxy",
        "http://127.0.0.1:7890",
    ])
    .unwrap();

    assert_eq!(cli.manifest, Some(PathBuf::from("custom.json")));
    assert_eq!(cli.lang, Some(Language::ZhCn));
    let resolved = cli
        .resolve(PathBuf::from("C:/tools/tool.exe").as_path(), None)
        .unwrap();
    assert_eq!(
        resolved.proxy,
        Some("http://127.0.0.1:7890".parse::<ProxyUrl>().unwrap())
    );
}

#[test]
fn rejects_invalid_language_and_proxy_values() {
    assert!(
        Cli::try_parse_from(["tool", "--lang", "fr-FR"])
            .unwrap_err()
            .use_stderr()
    );
    for value in ["file:///proxy", "http:///missing-host"] {
        let cli = Cli::try_parse_from(["tool", "--proxy", value]).unwrap();
        assert!(
            cli.resolve(PathBuf::from("C:/tools/tool.exe").as_path(), None)
                .is_err()
        );
    }
}

#[test]
fn explicit_manifest_path_is_preserved() {
    let path = resolve_manifest_path(
        Some(PathBuf::from("config/server.json")),
        PathBuf::from("C:/tools/mc-server-download-tool.exe").as_path(),
    )
    .unwrap();

    assert_eq!(path, PathBuf::from("config/server.json"));
}

#[test]
fn default_manifest_is_adjacent_to_the_executable() {
    let path = resolve_manifest_path(
        None,
        PathBuf::from("C:/tools/mc-server-download-tool.exe").as_path(),
    )
    .unwrap();

    assert_eq!(path, PathBuf::from("C:/tools/server-install.json"));
}

#[test]
fn proxy_precedence_is_cli_then_uppercase_then_lowercase_environment() {
    let cli: ProxyUrl = "http://cli.example:1".parse().unwrap();
    let selected = resolve_proxy(Some(cli.clone()), |_| {
        Some("http://environment.example:2".to_string())
    })
    .unwrap();
    assert_eq!(selected, Some(cli));

    let values = std::collections::HashMap::from([
        ("HTTP_PROXY", "http://http.example:3"),
        ("ALL_PROXY", "socks5://all.example:4"),
        ("HTTPS_PROXY", "http://https.example:5"),
        ("https_proxy", "http://lower.example:6"),
    ]);
    let selected = resolve_proxy(None, |name| values.get(name).map(ToString::to_string)).unwrap();
    assert_eq!(selected.unwrap().to_string(), "http://https.example:5/");

    let lower = resolve_proxy(None, |name| {
        (name == "http_proxy").then(|| "http://lower.example:7".to_string())
    })
    .unwrap();
    assert_eq!(lower.unwrap().to_string(), "http://lower.example:7/");
}

#[test]
fn invalid_high_precedence_environment_proxy_fails_fast() {
    let error = resolve_proxy(None, |name| match name {
        "HTTPS_PROXY" => Some("file:///invalid".to_string()),
        "ALL_PROXY" => Some("http://valid.example:8".to_string()),
        _ => None,
    })
    .unwrap_err();
    assert!(error.to_string().contains("HTTPS_PROXY"));
}

#[test]
fn language_precedence_is_cli_then_locale_then_english() {
    assert_eq!(
        resolve_language(Some(Language::EnUs), Some("zh_CN.UTF-8")),
        Language::EnUs
    );
    assert_eq!(resolve_language(None, Some("zh_CN.UTF-8")), Language::ZhCn);
    assert_eq!(resolve_language(None, Some("en-GB")), Language::EnUs);
    assert_eq!(resolve_language(None, Some("fr-FR")), Language::EnUs);
    assert_eq!(resolve_language(None, None), Language::EnUs);
}

#[test]
fn process_exit_codes_distinguish_help_usage_and_manifest_io() {
    let executable = env!("CARGO_BIN_EXE_mc-server-download-tool");

    let help = Command::new(executable).arg("--help").output().unwrap();
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("--manifest")
    );

    let usage = Command::new(executable)
        .args(["--lang", "invalid"])
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));
    assert!(!usage.stderr.is_empty());

    let missing = Command::new(executable)
        .args([
            "--manifest",
            "Z:/definitely-missing/manifest.json",
            "--lang",
            "en-US",
        ])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(10));
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .contains("failed to read manifest")
    );
}

#[test]
fn explicit_chinese_help_and_usage_errors_are_fully_localized() {
    let executable = env!("CARGO_BIN_EXE_mc-server-download-tool");

    let help = Command::new(executable)
        .args(["--lang", "zh-CN", "--help"])
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.contains("Minecraft 服务端安装器"));
    assert!(stdout.contains("用法："));
    assert!(stdout.contains("选项："));
    assert!(stdout.contains("清单文件路径"));
    assert!(!stdout.contains("Usage:"));
    assert!(!stdout.contains("Options:"));

    let invalid = Command::new(executable)
        .args(["--lang", "zh-CN", "--manifest"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    let stderr = String::from_utf8(invalid.stderr).unwrap();
    assert!(stderr.contains("命令行参数错误"));
    assert!(stderr.contains("--manifest"));
    assert!(stderr.contains("用法："));
    assert!(stderr.contains("--help"));
    assert!(!stderr.contains("Usage:"));
}

#[test]
fn system_chinese_locale_localizes_help_before_clap_exits() {
    let failure =
        try_parse_localized_from(["mc-server-download-tool", "--help"], Some("zh_CN.UTF-8"))
            .unwrap_err();

    assert_eq!(failure.exit_code(), 0);
    assert!(!failure.use_stderr());
    assert!(failure.rendered().contains("Minecraft 服务端安装器"));
    assert!(failure.rendered().contains("用法："));
    assert!(!failure.rendered().contains("Usage:"));
}

#[test]
fn invalid_language_fails_fast_in_the_locale_language() {
    let failure = try_parse_localized_from(
        ["mc-server-download-tool", "--lang", "fr-FR"],
        Some("zh-CN"),
    )
    .unwrap_err();

    assert_eq!(failure.exit_code(), 2);
    assert!(failure.use_stderr());
    assert!(failure.rendered().contains("fr-FR"));
    assert!(failure.rendered().contains("en-US"));
    assert!(failure.rendered().contains("zh-CN"));
    assert!(failure.rendered().contains("命令行参数错误"));
}

#[test]
fn process_exit_codes_distinguish_manifest_shape_and_semantics() {
    let executable = env!("CARGO_BIN_EXE_mc-server-download-tool");
    let temp = tempfile::tempdir().unwrap();
    let malformed = temp.path().join("malformed.json");
    fs::write(&malformed, b"{").unwrap();
    let malformed_output = Command::new(executable)
        .args(["--manifest", malformed.to_str().unwrap(), "--lang", "en-US"])
        .output()
        .unwrap();
    assert_eq!(malformed_output.status.code(), Some(11));

    let default_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("server-install.json");
    let mut semantic: serde_json::Value =
        serde_json::from_slice(&fs::read(default_path).unwrap()).unwrap();
    semantic["schema_version"] = serde_json::json!(2);
    let invalid = temp.path().join("invalid.json");
    fs::write(&invalid, serde_json::to_vec(&semantic).unwrap()).unwrap();
    let semantic_output = Command::new(executable)
        .args(["--manifest", invalid.to_str().unwrap(), "--lang", "zh-CN"])
        .output()
        .unwrap();
    assert_eq!(semantic_output.status.code(), Some(12));
    assert!(
        String::from_utf8(semantic_output.stderr)
            .unwrap()
            .contains("schema_version 必须为 1")
    );
}

#[test]
fn proxy_credentials_are_rejected_without_echoing_the_secret() {
    let executable = env!("CARGO_BIN_EXE_mc-server-download-tool");
    let output = Command::new(executable)
        .args([
            "--proxy",
            "http://user:password-secret@example.com",
            "--manifest",
            "missing.json",
            "--lang",
            "en-US",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(20));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("proxy URL must not embed credentials"));
    assert!(!stderr.contains("password-secret"));
}

#[test]
fn manifest_api_key_is_never_echoed_by_process_errors() {
    let executable = env!("CARGO_BIN_EXE_mc-server-download-tool");
    let temp = tempfile::tempdir().unwrap();
    let default_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("server-install.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(default_path).unwrap()).unwrap();
    manifest["curseforge_api_key"] = serde_json::json!("manifest-api-secret");
    let path = temp.path().join("secret.json");
    fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let output = Command::new(executable)
        .args(["--manifest", path.to_str().unwrap(), "--lang", "en-US"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(12));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("curseforge_api_key"));
    assert!(!stderr.contains("manifest-api-secret"));
}
