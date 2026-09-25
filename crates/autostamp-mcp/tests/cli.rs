//! Native command-line behavior and nonmutating failure regressions.
#![cfg(all(feature = "cli", not(target_arch = "wasm32")))]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

const CATALOG: &str =
    r#"{"tools":[{"name":"ping","inputSchema":{"type":"object","additionalProperties":false}}]}"#;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/cli-test-data");
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap().join(format!(
            "case-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("catalog.json"), CATALOG).unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_autostamp-mcp"));
        command.current_dir(&self.0);
        command
    }

    fn offline(&self) -> Command {
        let mut command = self.command();
        command.args([
            "--catalog",
            "catalog.json",
            "--endpoint",
            "http://127.0.0.1:1/mcp",
            "--package",
            "fixture:cli@0.1.0",
            "--output",
            "output with spaces",
        ]);
        command
    }

    fn output(&self) -> PathBuf {
        self.0.join("output with spaces")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn failure(output: Output, message: &str) {
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
}

#[test]
fn help_version_and_usage_exit_codes() {
    let sandbox = Sandbox::new();
    for flag in ["--help", "--version"] {
        success(sandbox.command().arg(flag).output().unwrap());
    }
    let output = sandbox.command().output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    failure(output, "--package");
    failure(
        sandbox
            .offline()
            .args(["--server", "http://localhost"])
            .output()
            .unwrap(),
        "cannot be used with",
    );
    failure(
        sandbox
            .offline()
            .args(["--token-env", "DO_NOT_READ"])
            .output()
            .unwrap(),
        "cannot be used with",
    );
    failure(
        sandbox
            .command()
            .args(["--token", "not-supported"])
            .output()
            .unwrap(),
        "unexpected argument",
    );
}

#[test]
fn writes_all_artifacts_preserves_neighbors_and_refuses_user_edits() {
    let sandbox = Sandbox::new();
    success(sandbox.offline().output().unwrap());
    let root = sandbox.output();
    for path in [
        "Cargo.toml",
        "wasm.toml",
        "README.md",
        "src/lib.rs",
        "wit/world.wit",
        ".autostamp-files.json",
    ] {
        assert!(root.join(path).is_file(), "{path}");
    }
    fs::write(root.join("user.txt"), "mine").unwrap();
    let ledger = fs::read(root.join(".autostamp-files.json")).unwrap();
    success(sandbox.offline().output().unwrap());
    assert_eq!(
        fs::read(root.join(".autostamp-files.json")).unwrap(),
        ledger
    );
    fs::write(root.join("src/lib.rs"), "// user edit").unwrap();
    failure(sandbox.offline().output().unwrap(), "edited");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "// user edit"
    );
    assert_eq!(fs::read_to_string(root.join("user.txt")).unwrap(), "mine");
    assert_eq!(
        fs::read(root.join(".autostamp-files.json")).unwrap(),
        ledger
    );
}

#[test]
fn unowned_output_conflict_is_nonmutating() {
    let sandbox = Sandbox::new();
    fs::create_dir(sandbox.output()).unwrap();
    fs::write(sandbox.output().join("Cargo.toml"), "mine").unwrap();
    failure(sandbox.offline().output().unwrap(), "not owned");
    assert_eq!(fs::read_dir(sandbox.output()).unwrap().count(), 1);
    assert_eq!(
        fs::read_to_string(sandbox.output().join("Cargo.toml")).unwrap(),
        "mine"
    );
}

#[test]
fn auth_dependency_is_removed_only_if_unchanged() {
    let sandbox = Sandbox::new();
    success(
        sandbox
            .offline()
            .args(["--bearer-secret", "host-key"])
            .output()
            .unwrap(),
    );
    let dependency = sandbox
        .output()
        .join("wit/deps/wasmcloud-secrets/secrets.wit");
    let original = fs::read(&dependency).unwrap();
    fs::write(&dependency, "user edit").unwrap();
    failure(sandbox.offline().output().unwrap(), "edited");
    assert_eq!(fs::read_to_string(&dependency).unwrap(), "user edit");
    fs::write(&dependency, original).unwrap();
    success(sandbox.offline().output().unwrap());
    assert!(!dependency.exists());
    assert!(
        !fs::read_to_string(sandbox.output().join("wit/world.wit"))
            .unwrap()
            .contains("wasmcloud:secrets")
    );
}

#[test]
fn bounded_offline_catalog_preserves_schema_error_context() {
    let sandbox = Sandbox::new();
    success(
        sandbox
            .offline()
            .args(["--max-catalog-bytes", &CATALOG.len().to_string()])
            .output()
            .unwrap(),
    );
    failure(
        sandbox
            .offline()
            .args(["--max-catalog-bytes", &(CATALOG.len() - 1).to_string()])
            .output()
            .unwrap(),
        "offline catalog byte limit",
    );
    fs::write(
        sandbox.0.join("catalog.json"),
        r#"{"tools":[{"name":"bad","inputSchema":{"type":"object"}}]}"#,
    )
    .unwrap();
    let output = sandbox.offline().output().unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("tool \"bad\""));
    failure(output, "/tools/0/inputSchema");
}

#[test]
fn malformed_and_incomplete_catalogs_fail_without_output() {
    let sandbox = Sandbox::new();
    for catalog in [
        r#"{"tools":[],"nextCursor":""}"#,
        r#"{"tools":[],"tools":[]}"#,
        r#"{}"#,
        "not JSON",
    ] {
        fs::write(sandbox.0.join("catalog.json"), catalog).unwrap();
        assert!(!sandbox.offline().output().unwrap().status.success());
        assert!(!sandbox.output().exists());
    }
    fs::write(sandbox.0.join("catalog.json"), [255]).unwrap();
    failure(sandbox.offline().output().unwrap(), "UTF-8");
    fs::remove_file(sandbox.0.join("catalog.json")).unwrap();
    failure(
        sandbox.offline().output().unwrap(),
        "catalog.json: inspect catalog:",
    );
}

#[test]
fn empty_catalog_generates_no_dummy_tool_exports() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.0.join("catalog.json"), r#"{"tools":[]}"#).unwrap();
    success(sandbox.offline().output().unwrap());
    let wit = fs::read_to_string(sandbox.output().join("wit/world.wit")).unwrap();
    assert!(!wit.contains(": func("));
}

#[test]
fn invalid_limits_are_rejected_before_io() {
    let sandbox = Sandbox::new();
    for (flag, value, expected) in [
        ("--timeout-ms", "0", "limits must be positive"),
        ("--max-response-bytes", "1", "event bytes must not exceed"),
        ("--max-event-bytes", "0", "limits must be positive"),
        ("--max-pages", "0", "positive page/tool limits"),
        ("--max-tools", "0", "positive page/tool limits"),
        ("--max-catalog-bytes", "11", "at least 12 catalog bytes"),
    ] {
        failure(
            sandbox.offline().args([flag, value]).output().unwrap(),
            expected,
        );
        assert!(!sandbox.output().exists());
    }
}

#[test]
fn discovery_token_environment_is_explicit_and_never_printed() {
    let sandbox = Sandbox::new();
    for value in [None, Some(""), Some("bad token"), Some("bad\ntoken")] {
        let mut command = sandbox.command();
        command.args([
            "--server",
            "http://127.0.0.1:1/mcp",
            "--package",
            "fixture:cli@0.1.0",
            "--output",
            "output",
            "--token-env",
            "AUTOSTAMP_TEST_TOKEN",
        ]);
        command.env_remove("AUTOSTAMP_TEST_TOKEN");
        if let Some(value) = value {
            command.env("AUTOSTAMP_TEST_TOKEN", value);
        }
        failure(command.output().unwrap(), "--token-env variable");
        assert!(!sandbox.0.join("output").exists());
    }
}

#[test]
fn endpoint_and_package_errors_do_not_echo_credentials_or_write() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .command()
        .args([
            "--server",
            "http://user:credential@localhost/mcp",
            "--package",
            "fixture:cli@0.1.0",
            "--output",
            "output",
        ])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stderr).contains("credential"));
    failure(output, "without userinfo");
    let output = sandbox
        .command()
        .args([
            "--server",
            "http://localhost/mcp",
            "--package",
            "fixture:cli",
            "--output",
            "output",
        ])
        .output()
        .unwrap();
    failure(output, "explicit version");
    assert!(!sandbox.0.join("output").exists());
}
