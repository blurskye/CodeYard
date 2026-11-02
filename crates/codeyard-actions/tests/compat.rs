//! Compatibility checks following actions/runner's `src/Test/L0/Worker` at the CLI boundary.
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    exit_code: i32,
    jobs: BTreeMap<String, String>,
    contains: Vec<String>,
    #[serde(default)]
    absent: Vec<String>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
    #[serde(default)]
    job_tokens: Option<serde_json::Value>,
}

fn default_timeout() -> u64 {
    120
}

// The variables let a test binary built elsewhere run here.
fn root() -> PathBuf {
    std::env::var_os("CODEYARD_ACTIONS_COMPAT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compat"))
}

fn binary() -> std::ffi::OsString {
    std::env::var_os("CODEYARD_ACTIONS_BIN")
        .unwrap_or(env!("CARGO_BIN_EXE_codeyard-actions").into())
}

#[test]
fn fixture_manifest() {
    let source = include_str!("compat.rs");
    for group in ["features", "actions"] {
        for entry in fs::read_dir(root().join(group)).unwrap() {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_dir() {
                continue;
            }
            let path = entry.path();
            let name = format!("{group}/{}", entry.file_name().to_string_lossy());
            assert!(
                source.contains(&format!("\"{name}\"")),
                "unregistered fixture: {name}"
            );
            let expected: Expected =
                serde_yaml::from_str(&fs::read_to_string(path.join("expected.yml")).unwrap())
                    .unwrap();
            assert!(!expected.jobs.is_empty(), "{name}: no job conclusions");
            assert!(
                !expected.contains.is_empty(),
                "{name}: no effect assertions"
            );
            let workflow = fs::read_to_string(path.join("workflow.yml")).unwrap();
            let yaml: serde_yaml::Value =
                serde_yaml::from_str(&workflow).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(yaml["name"].as_str(), Some("compat"));
            for action in workflow.lines().filter_map(|line| {
                line.trim_start()
                    .trim_start_matches("- ")
                    .strip_prefix("uses:")
                    .map(str::trim)
            }) {
                if action.starts_with("./") {
                    continue;
                }
                let (reference, tag) = action
                    .split_once(" # ")
                    .unwrap_or_else(|| panic!("{name}: missing original ref comment"));
                let (_, sha) = reference.rsplit_once('@').unwrap();
                assert!(
                    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "{name}: action is not commit-pinned"
                );
                assert!(!tag.is_empty());
            }
        }
    }
    let catalog: Vec<serde_json::Value> =
        serde_json::from_str(&fs::read_to_string(root().join("actions/catalog.json")).unwrap())
            .unwrap();
    assert_eq!(catalog.len(), 100);
    for action in catalog {
        let name = action["name"].as_str().unwrap();
        let workflow =
            fs::read_to_string(root().join("actions").join(name).join("workflow.yml")).unwrap();
        let reference = format!(
            "uses: {}@{} # {}",
            action["action"].as_str().unwrap(),
            action["sha"].as_str().unwrap(),
            action["ref"].as_str().unwrap()
        );
        assert!(
            workflow.contains(&reference),
            "{name}: catalog and workflow pins differ"
        );
    }
}

fn copy_directory(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create fixture directory");
    for entry in fs::read_dir(from).expect("read fixture directory") {
        let entry = entry.expect("read fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("fixture file type").is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy fixture file");
        }
    }
}

fn git(workspace: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git is required for compatibility tests");
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

fn run_fixture(name: &str) {
    let fixture = root().join(name);
    let expected: Expected = serde_yaml::from_str(
        &fs::read_to_string(fixture.join("expected.yml")).expect("read expected.yml"),
    )
    .expect("parse expected.yml");
    let temp = tempfile::tempdir().expect("create isolated run");
    let workspace = temp.path().join("checkout");
    fs::create_dir_all(&workspace).expect("create checkout");
    if fixture.join("workspace").exists() {
        copy_directory(&fixture.join("workspace"), &workspace);
    }
    fs::write(workspace.join("compat-seed.txt"), "compatibility fixture\n").unwrap();
    git(&workspace, &["init", "-b", "main"]);
    git(&workspace, &["add", "."]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=Compatibility",
            "-c",
            "user.email=compat@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "Seed compatibility fixture",
        ],
    );
    let event = temp.path().join("event.json");
    fs::write(
        &event,
        r#"{"ref":"refs/heads/main","inputs":{"message":"hello"},"repository":{"full_name":"compat/example","name":"example","owner":{"login":"compat"},"default_branch":"main"},"sender":{"login":"compat-actor"}}"#,
    )
    .unwrap();
    let secrets = temp.path().join("secrets.env");
    let mut secret_values = expected.secrets.clone();
    if let Ok(token) = std::env::var("ACTIONS_RUNTIME_TOKEN") {
        secret_values.insert("ACTIONS_RUNTIME_TOKEN".into(), token);
    }
    for key in [
        "GITHUB_TOKEN",
        "DIGITALOCEAN_ACCESS_TOKEN",
        "DOCKERHUB_USERNAME",
        "DOCKERHUB_TOKEN",
        "COMPAT_SSH_PRIVATE_KEY",
    ] {
        if let Ok(value) = std::env::var(format!("COMPAT_SECRET_{key}")) {
            secret_values.insert(key.into(), value);
        }
    }
    fs::write(
        &secrets,
        secret_values
            .iter()
            .map(|(k, v)| format!("{k}={}\n", serde_json::to_string(v).unwrap()))
            .collect::<String>(),
    )
    .unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("create isolated home");
    let stdout = temp.path().join("stdout.log");
    let stderr = temp.path().join("stderr.log");
    let mut command = Command::new(binary());
    command
        .arg("workflow_dispatch")
        .arg("-W")
        .arg(fixture.join("workflow.yml"))
        .arg("--directory")
        .arg(&workspace)
        .args([
            "-P",
            "ubuntu-latest=-self-hosted",
            "--actor",
            "compat-actor",
        ])
        .arg("--eventpath")
        .arg(event)
        .arg("--secret-file")
        .arg(secrets)
        .args([
            "--env",
            "GITHUB_REPOSITORY=compat/example",
            "--env",
            "GITHUB_REF=refs/heads/main",
        ])
        .args(["--env", "GITHUB_RUN_ID=123", "--env", "GITHUB_RUN_NUMBER=7"])
        .args(["--concurrent-jobs", "2"])
        .arg("--env")
        .arg(format!(
            "COMPAT_CACHE_KEY={}",
            temp.path().file_name().unwrap().to_string_lossy()
        ))
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap());
    if let Some(tokens) = &expected.job_tokens {
        let path = temp.path().join("job-tokens.json");
        fs::write(&path, serde_json::to_vec(tokens).unwrap()).unwrap();
        command.arg("--job-token-file").arg(path);
    }
    for (key, value) in &expected.env {
        command.arg("--env").arg(format!("{key}={value}"));
    }
    // Credentials for optional service-backed action tests are supplied explicitly by the operator.
    for key in [
        "ACTIONS_RUNTIME_TOKEN",
        "ACTIONS_RUNTIME_URL",
        "ACTIONS_RESULTS_URL",
    ] {
        if let Ok(value) = std::env::var(key) {
            command.arg("--env").arg(format!("{key}={value}"));
        }
    }
    let mut child = command.spawn().expect("start codeyard-actions");
    let deadline = Instant::now() + Duration::from_secs(expected.timeout_seconds);
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait for runner") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed out runner");
            child.wait().expect("reap timed out runner");
            panic!(
                "{name}: runner exceeded {} seconds",
                expected.timeout_seconds
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    let log = format!(
        "{}\n{}",
        fs::read_to_string(stdout).unwrap(),
        fs::read_to_string(stderr).unwrap()
    );
    // Check masking before printing diagnostics, so a failed fixture does not disclose its secrets.
    for secret in secret_values
        .values()
        .flat_map(|secret| secret.lines())
        .filter(|secret| !secret.is_empty())
    {
        assert!(
            !log.contains(secret),
            "{name}: a configured secret leaked into the log"
        );
    }
    assert_eq!(
        status.code(),
        Some(expected.exit_code),
        "{name}: unexpected exit\n{log}"
    );
    for (job, conclusion) in &expected.jobs {
        if conclusion == "skipped" {
            assert!(
                !log.contains(&format!("[compat/{job}] Job started")),
                "{name}: skipped job started\n{log}"
            );
            assert!(
                log.contains(&format!("compat:job:{job}:skipped")),
                "{name}: missing needs.result assertion for skipped job\n{log}"
            );
        } else {
            let line = format!("[compat/{job}] Job completed: {conclusion}");
            assert_eq!(
                log.lines().filter(|actual| *actual == line).count(),
                1,
                "{name}: expected one {line:?}\n{log}"
            );
        }
    }
    let completed = log
        .lines()
        .filter(|line| line.starts_with("[compat/") && line.contains("] Job completed: "))
        .count();
    assert_eq!(
        completed,
        expected
            .jobs
            .values()
            .filter(|result| *result != "skipped")
            .count(),
        "{name}: unexpected job completions\n{log}"
    );
    for line in &expected.contains {
        assert!(log.contains(line), "{name}: missing {line:?}\n{log}");
    }
    for line in &expected.absent {
        assert!(!log.contains(line), "{name}: forbidden log text {line:?}");
    }
}

macro_rules! fixtures {
    ($($test:ident => $path:literal),* $(,)?) => {$ (
        #[test]
        fn $test() { run_fixture($path); }
    )*};
}

macro_rules! optional_fixtures {
    ($reason:literal; $($test:ident => $path:literal),* $(,)?) => {$ (
        #[test]
        #[ignore = $reason]
        fn $test() { run_fixture($path); }
    )*};
}

fixtures! {
    expressions => "features/expressions",
    functions => "features/functions",
    hash_files => "features/hash-files",
    contexts => "features/contexts",
    env => "features/env",
    outputs => "features/outputs",
    file_commands => "features/file-commands",
    workflow_commands => "features/workflow-commands",
    problem_matchers => "features/problem-matchers",
    reusable_workflows => "features/reusable-workflows",
    reusable_validation => "features/reusable-validation",
    oidc_permissions => "features/oidc-permissions",
    masking => "features/masking",
    status_functions => "features/status-functions",
    needs => "features/needs",
    matrix => "features/matrix",
    dynamic_matrix => "features/dynamic-matrix",
    defaults => "features/defaults",
    shells => "features/shells",
    continue_on_error => "features/continue-on-error",
    step_timeout => "features/step-timeout",
    job_timeout => "features/job-timeout",
    composite => "features/composite",
    nested_composite => "features/nested-composite",
}

optional_fixtures! { "needs node";
    local_node => "features/local-node",
}
optional_fixtures! { "needs podman";
    job_container => "features/job-container",
    services => "features/services",
    local_docker => "features/local-docker",
}

optional_fixtures! { "needs network";
    action_setup_yarn => "actions/setup-yarn",
    action_checkout => "actions/checkout",
    action_setup_node => "actions/setup-node",
    action_setup_python => "actions/setup-python",
    action_setup_go => "actions/setup-go",
    action_setup_java => "actions/setup-java",
    action_setup_dotnet => "actions/setup-dotnet",
    action_github_script => "actions/github-script",
    action_cache => "actions/cache",
    action_cache_restore => "actions/cache-restore",
    action_cache_save => "actions/cache-save",
    action_upload_artifact => "actions/upload-artifact",
    action_download_artifact => "actions/download-artifact",
    action_setup_ruby => "actions/setup-ruby",
    action_setup_rust => "actions/setup-rust",
    action_rust_toolchain => "actions/rust-toolchain",
    action_install_action => "actions/install-action",
    action_setup_bun => "actions/setup-bun",
    action_setup_deno => "actions/setup-deno",
    action_setup_pnpm => "actions/setup-pnpm",
    action_setup_uv => "actions/setup-uv",
    action_setup_ruff => "actions/setup-ruff",
    action_setup_poetry => "actions/setup-poetry",
    action_install_poetry => "actions/install-poetry",
    action_setup_pdm => "actions/setup-pdm",
    action_setup_conda => "actions/setup-conda",
    action_setup_micromamba => "actions/setup-micromamba",
    action_setup_pixi => "actions/setup-pixi",
    action_setup_php => "actions/setup-php",
    action_setup_elixir => "actions/setup-elixir",
    action_setup_haskell => "actions/setup-haskell",
    action_setup_clojure => "actions/setup-clojure",
    action_setup_scala => "actions/setup-scala",
    action_setup_sbt => "actions/setup-sbt",
    action_setup_gradle => "actions/setup-gradle",
    action_setup_maven => "actions/setup-maven",
    action_setup_flutter => "actions/setup-flutter",
    action_setup_dart => "actions/setup-dart",
    action_setup_julia => "actions/setup-julia",
    action_setup_lua => "actions/setup-lua",
    action_setup_luarocks => "actions/setup-luarocks",
    action_setup_zig => "actions/setup-zig",
    action_setup_nim => "actions/setup-nim",
    action_setup_ocaml => "actions/setup-ocaml",
    action_setup_r => "actions/setup-r",
    action_setup_quarto => "actions/setup-quarto",
    action_setup_typst => "actions/setup-typst",
    action_setup_pandoc => "actions/setup-pandoc",
    action_setup_tinytex => "actions/setup-tinytex",
    action_setup_terraform => "actions/setup-terraform",
    action_setup_opentofu => "actions/setup-opentofu",
    action_setup_helm => "actions/setup-helm",
    action_setup_kubectl => "actions/setup-kubectl",
    action_setup_kustomize => "actions/setup-kustomize",
    action_setup_kind => "actions/setup-kind",
    action_setup_tflint => "actions/setup-tflint",
    action_setup_trivy => "actions/setup-trivy",
    action_setup_hugo => "actions/setup-hugo",
    action_setup_goreleaser => "actions/setup-goreleaser",
    action_setup_just => "actions/setup-just",
    action_setup_task => "actions/setup-task",
    action_setup_protoc => "actions/setup-protoc",
    action_setup_buf => "actions/setup-buf",
    action_setup_yq => "actions/setup-yq",
    action_setup_jq => "actions/setup-jq",
    action_setup_cmake => "actions/setup-cmake",
    action_setup_ninja => "actions/setup-ninja",
    action_setup_meson => "actions/setup-meson",
    action_setup_bazel => "actions/setup-bazel",
    action_setup_ccache => "actions/setup-ccache",
    action_setup_sccache => "actions/setup-sccache",
    action_setup_vcpkg => "actions/setup-vcpkg",
    action_setup_nix => "actions/setup-nix",
    action_setup_nix_determinate => "actions/setup-nix-determinate",
    action_setup_nix_quick => "actions/setup-nix-quick",
    action_setup_doctl => "actions/setup-doctl",
    action_setup_gcloud => "actions/setup-gcloud",
    action_setup_aws_sam => "actions/setup-aws-sam",
    action_setup_pulumi => "actions/setup-pulumi",
    action_setup_flyctl => "actions/setup-flyctl",
    action_docker_metadata => "actions/docker-metadata",
    action_changed_files => "actions/changed-files",
    action_paths_filter => "actions/paths-filter",
    action_create_json => "actions/create-json",
    action_write_file => "actions/write-file",
    action_read_file => "actions/read-file",
    action_yaml_update => "actions/yaml-update",
    action_branch_names => "actions/branch-names",
    action_slug => "actions/slug",
    action_short_sha => "actions/short-sha",
    action_semver => "actions/semver",
    action_ssh_agent => "actions/ssh-agent",
    action_setup_ssh => "actions/setup-ssh",
}

optional_fixtures! { "needs podman";
    action_setup_terragrunt => "actions/setup-terragrunt",
    action_setup_docker_buildx => "actions/setup-docker-buildx",
    action_setup_docker_qemu => "actions/setup-docker-qemu",
    action_docker_login => "actions/docker-login",
    action_docker_build_push => "actions/docker-build-push",
    action_docker_bake => "actions/docker-bake",
    action_find_replace => "actions/find-replace",
}
