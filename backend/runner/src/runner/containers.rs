//! Podman container operations following actions/runner's `Runner.Worker/ContainerOperationProvider.cs`.
use super::{commands::words, Options};
use crate::workflow::Container;
use anyhow::{bail, Context, Result};
use base64::Engine;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

pub(super) struct Podman {
    url: Option<String>,
    options: Vec<String>,
    pull: bool,
    /// Unique to this job run; it also serves as the job's `RUNNER_TRACKING_ID`.
    pub(super) prefix: String,
    containers: Vec<String>,
    images: Vec<String>,
    action_images: BTreeMap<(std::path::PathBuf, String), String>,
    pub(super) network: Option<String>,
    deny_docker: bool,
    pub job: Option<String>,
    pub path: Option<String>,
}

impl Podman {
    pub fn new(options: &Options, prefix: String) -> Result<Self> {
        Ok(Self {
            url: std::env::var("DOCKER_HOST").ok(),
            options: words(&options.container_options)?,
            pull: options.pull,
            deny_docker: options.deny_docker,
            prefix,
            containers: Vec::new(),
            images: Vec::new(),
            action_images: BTreeMap::new(),
            network: None,
            job: None,
            path: None,
        })
    }

    /// The run's ownership labels. Podman keeps them on the networks and images it creates, so
    /// recovery can find those after a crash.
    fn ownership_labels(&self) -> Vec<String> {
        self.options
            .windows(2)
            .filter(|pair| pair[0] == "--label")
            .map(|pair| pair[1].clone())
            .collect()
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new("podman");
        if let Some(url) = &self.url {
            command.args(["--url", url]);
        }
        command.kill_on_drop(true);
        command
    }

    async fn output(&self, args: &[String]) -> Result<String> {
        podman_output(self.command().args(args)).await
    }

    pub async fn setup(
        &mut self,
        job: Option<&Container>,
        services: &BTreeMap<String, Container>,
        workspace: &Path,
        temp: &Path,
        cache: &Path,
    ) -> Result<serde_json::Value> {
        if job.is_none() && services.is_empty() {
            return Ok(serde_json::json!({}));
        }
        anyhow::ensure!(
            !self.deny_docker,
            "Container actions require CI_EXECUTION_MODE=podman"
        );
        let network = format!("{}-network", self.prefix);
        self.network = Some(network.clone());
        let mut args: Vec<String> = vec!["network".into(), "create".into()];
        for label in self.ownership_labels() {
            args.extend(["--label".into(), label]);
        }
        args.push(network);
        self.output(&args).await?;
        let mut contexts = serde_json::Map::new();
        let mut started = Vec::new();
        for (name, service) in services {
            let id = self
                .create(name, service, workspace, temp, cache, false)
                .await?;
            self.output(&["start".into(), id.clone()]).await?;
            started.push((name, id));
        }
        for (name, id) in started {
            let mut healthy = false;
            for attempt in 0..30 {
                let health = self
                    .output(&[
                        "inspect".into(),
                        "--format".into(),
                        "{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}".into(),
                        id.clone(),
                    ])
                    .await?;
                if health == "healthy" || health == "none" || health.is_empty() {
                    healthy = true;
                    break;
                }
                if health == "unhealthy" {
                    bail!("Service '{name}' is unhealthy");
                }
                tokio::time::sleep(Duration::from_secs(1 + attempt.min(4))).await;
            }
            if !healthy {
                bail!("Service '{name}' failed its health check");
            }
            let ports = self.output(&["port".into(), id.clone()]).await?;
            let mut port_context = serde_json::Map::new();
            for line in ports.lines() {
                if let Some((port, mapping)) = line.split_once(" -> ") {
                    if let Some((_, host)) = mapping.rsplit_once(':') {
                        port_context
                            .insert(port.split('/').next().unwrap_or(port).into(), host.into());
                    }
                }
            }
            contexts.insert(
                name.clone(),
                serde_json::json!({"id":id,"network":self.network,"ports":port_context}),
            );
        }
        if let Some(container) = job {
            let id = self
                .create("job", container, workspace, temp, cache, true)
                .await?;
            self.output(&["start".into(), id.clone()]).await?;
            let environment = self
                .output(&[
                    "inspect".into(),
                    "--format".into(),
                    "{{json .Config.Env}}".into(),
                    id.clone(),
                ])
                .await?;
            self.path = serde_json::from_str::<Vec<String>>(&environment)?
                .into_iter()
                .find_map(|entry| entry.strip_prefix("PATH=").map(str::to_owned));
            self.job = Some(id);
        }
        Ok(serde_json::Value::Object(contexts))
    }

    async fn create(
        &mut self,
        name: &str,
        container: &Container,
        workspace: &Path,
        temp: &Path,
        cache: &Path,
        job: bool,
    ) -> Result<String> {
        let id = if job {
            format!("{}-job", self.prefix)
        } else {
            format!("{}-service-{name}", self.prefix)
        };
        self.containers.push(id.clone());
        let mut args = vec!["create".into(), "--name".into(), id.clone()];
        args.extend(self.options.clone());
        let auth_directory = container
            .credentials
            .as_ref()
            .map(|_| crate::fs::private_tempdir("codeyard-registry-", None))
            .transpose()?;
        if let Some((username, password)) = &container.credentials {
            let first = container.image.split('/').next().unwrap_or("docker.io");
            let registry = if container.image.contains('/')
                && (first.contains(['.', ':']) || first == "localhost")
            {
                first
            } else {
                "docker.io"
            };
            let auth =
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
            let path = auth_directory
                .as_ref()
                .expect("registry directory exists")
                .path()
                .join("auth.json");
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({"auths": {registry: {"auth": auth}}}))?,
            )?;
            args.extend(["--authfile".into(), path.display().to_string()]);
        }
        if let Some(options) = &container.options {
            args.extend(words(options)?);
        }
        args.extend([
            "--pull".into(),
            if self.pull { "always" } else { "missing" }.into(),
        ]);
        if let Some(network) = &self.network {
            args.extend([
                "--network".into(),
                network.clone(),
                "--network-alias".into(),
                name.into(),
            ]);
        }
        for port in &container.ports {
            args.extend(["--publish".into(), port.clone()]);
        }
        for volume in &container.volumes {
            args.extend(["--volume".into(), volume.clone()]);
        }
        if job {
            for path in [workspace, temp, cache] {
                args.extend(["--volume".into(), bind(path, path)?]);
            }
            args.extend([
                "--workdir".into(),
                workspace.display().to_string(),
                "--entrypoint".into(),
                "tail".into(),
            ]);
        }
        let mut command = self.command();
        command.args(&args);
        add_env(&mut command, &container.env);
        command.arg(&container.image);
        if job {
            command.args(["-f", "/dev/null"]);
        }
        podman_output(&mut command).await?;
        drop(auth_directory);
        Ok(id)
    }

    pub fn exec(
        &self,
        env: &BTreeMap<String, String>,
        directory: &Path,
        program: &str,
        args: &[String],
    ) -> Command {
        let mut command = self.command();
        command.args(["exec", "--workdir"]).arg(directory);
        add_env(&mut command, env);
        if let Some(job) = &self.job {
            command.arg(job);
        }
        command.arg(program).args(args);
        command
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "A step's process takes its command, directory, environment, deadline and output maps one by one."
    )]
    pub async fn docker_action(
        &mut self,
        image: &str,
        directory: &Path,
        workspace: &Path,
        temp: &Path,
        env: &BTreeMap<String, String>,
        entrypoint: Option<&str>,
        args: &[String],
        deadline: std::time::Instant,
    ) -> Result<(Command, String)> {
        anyhow::ensure!(
            !self.deny_docker,
            "Docker actions require CI_EXECUTION_MODE=podman"
        );
        let built = builds_image(directory, image);
        let key = (directory.to_owned(), image.to_owned());
        let image = if let Some(image) = self.action_images.get(&key) {
            image.clone()
        } else if built {
            // The Dockerfile must be the action's own, not another file of the host.
            let dockerfile = directory
                .join(image)
                .canonicalize()
                .context("The action's Dockerfile does not exist")?;
            anyhow::ensure!(
                dockerfile.starts_with(directory.canonicalize()?) && dockerfile.is_file(),
                "The action's Dockerfile must be inside the action"
            );
            let tag = format!("{}-action-image-{}", self.prefix, self.images.len());
            self.images.push(tag.clone());
            let mut args: Vec<String> = vec!["build".into()];
            for label in self.ownership_labels() {
                args.extend(["--label".into(), label]);
            }
            args.extend([
                "--tag".into(),
                tag.clone(),
                "--file".into(),
                dockerfile.display().to_string(),
                directory.display().to_string(),
            ]);
            let output = super::process::output(
                self.command().args(&args),
                deadline.saturating_duration_since(std::time::Instant::now()),
            )
            .await?;
            anyhow::ensure!(
                output.status.success(),
                "Docker action image build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            self.action_images.insert(key, tag.clone());
            tag
        } else {
            let image = image.strip_prefix("docker://").unwrap_or(image);
            // A leading dash would make Podman read the image as an option.
            anyhow::ensure!(
                !image.is_empty() && !image.starts_with('-'),
                "Invalid Docker image '{image}'"
            );
            if self.pull {
                self.output(&["pull".into(), "--".into(), image.into()])
                    .await?;
            }
            let inspect = [
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{.Id}}".into(),
                "--".into(),
                image.into(),
            ];
            let resolved = match self.output(&inspect).await {
                Ok(image) => image,
                Err(_) => {
                    self.output(&["pull".into(), "--".into(), image.into()])
                        .await?;
                    self.output(&inspect).await?
                }
            };
            self.action_images.insert(key, resolved.clone());
            resolved
        };
        let id = format!("{}-action-{}", self.prefix, self.containers.len());
        self.containers.push(id.clone());
        let mut command = self.command();
        command.args(["run", "--name", &id]);
        command.args(&self.options);
        command.args(["--pull", "never"]);
        if let Some(network) = &self.network {
            command.args(["--network", network]);
        }
        // A repository-root action's directory is the workspace itself, and Podman rejects a
        // repeated mount destination.
        let mut mounted: Vec<&Path> = Vec::new();
        for path in [workspace, temp, directory] {
            if !mounted.contains(&path) {
                mounted.push(path);
                command.arg("--volume").arg(bind(path, path)?);
            }
        }
        if workspace != Path::new("/github/workspace") {
            command
                .arg("--volume")
                .arg(bind(workspace, Path::new("/github/workspace"))?);
        }
        command.arg("--workdir").arg("/github/workspace");
        add_env(&mut command, env);
        command
            .arg("--env")
            .arg("GITHUB_WORKSPACE")
            .env("GITHUB_WORKSPACE", "/github/workspace");
        if let Some(entrypoint) = entrypoint {
            // One argument, so an entrypoint that starts with a dash stays the entrypoint.
            command.arg(format!("--entrypoint={entrypoint}"));
        }
        command.arg("--").arg(image).args(args);
        Ok((command, id))
    }

    pub async fn cleanup_active_actions(&mut self) -> bool {
        let prefix = format!("{}-action-", self.prefix);
        let actions: Vec<_> = self
            .containers
            .iter()
            .filter(|id| id.starts_with(&prefix))
            .cloned()
            .collect();
        for action in actions {
            if !self.cleanup_action(&action).await {
                return false;
            }
        }
        true
    }

    pub async fn cleanup_action(&mut self, id: &str) -> bool {
        let mut pending = Cleanup {
            url: self.url.clone(),
            containers: vec![id.into()],
            network: None,
            images: vec![],
        };
        if !pending.run().await {
            return false;
        }
        self.containers.retain(|container| container != id);
        true
    }

    pub async fn cleanup(&mut self) -> bool {
        // Keep the live owner unchanged while awaiting: dropping this future must not forget
        // identifiers. Removal is idempotent, including after a lost client reply.
        let mut pending = Cleanup {
            url: self.url.clone(),
            containers: self.containers.clone(),
            network: self.network.clone(),
            images: self.images.clone(),
        };
        let succeeded = pending.run().await;
        self.containers = pending.containers;
        self.network = pending.network;
        self.images = pending.images;
        succeeded
    }
}

struct Cleanup {
    url: Option<String>,
    containers: Vec<String>,
    network: Option<String>,
    images: Vec<String>,
}
impl Cleanup {
    fn empty(&self) -> bool {
        self.containers.is_empty() && self.network.is_none() && self.images.is_empty()
    }

    async fn remove(&self, args: &[&str]) -> bool {
        let mut command = Command::new("podman");
        if let Some(url) = &self.url {
            command.args(["--url", url]);
        }
        command.args(args);
        super::process::output(&mut command, Duration::from_secs(5))
            .await
            .is_ok_and(|output| output.status.success())
    }

    async fn run(&mut self) -> bool {
        tokio::time::timeout(Duration::from_secs(30), async {
            for attempt in 0..3 {
                for id in self.containers.clone().into_iter().rev() {
                    if self
                        .remove(&["rm", "--ignore", "--force", "--volumes", &id])
                        .await
                    {
                        self.containers.retain(|container| container != &id);
                    }
                }
                // Networks and images cannot be retired until all their consumers are gone.
                if self.containers.is_empty() {
                    if let Some(network) = self.network.clone() {
                        if self.remove(&["network", "rm", "--ignore", &network]).await {
                            self.network = None;
                        }
                    }
                    for image in self.images.clone() {
                        if self.remove(&["rmi", "--ignore", "--force", &image]).await {
                            self.images.retain(|candidate| candidate != &image);
                        }
                    }
                }
                if self.empty() {
                    return true;
                }
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }
}

/// Whether a Docker action's image is built from a Dockerfile in the action. A registry image is
/// never built, whatever its name ends with.
fn builds_image(directory: &Path, image: &str) -> bool {
    !image.starts_with("docker://")
        && (directory.join(image).is_file()
            || image == "Dockerfile"
            || image.ends_with("/Dockerfile")
            || image.ends_with(".dockerfile"))
}

/// A `--volume` value that mounts a host directory at `target` in the container. A path with a
/// colon would be read as more than one field, so it is refused.
fn bind(source: &Path, target: &Path) -> Result<String> {
    let (source, target) = (source.display().to_string(), target.display().to_string());
    for path in [&source, &target] {
        anyhow::ensure!(
            !path.contains(':'),
            "Cannot mount '{path}' into a container: the path contains ':'"
        );
    }
    Ok(format!("{source}:{target}:rw"))
}

async fn podman_output(command: &mut Command) -> Result<String> {
    let output = super::process::output(command, Duration::from_secs(120))
        .await
        .context("Podman operation failed")?;
    if !output.status.success() {
        bail!("Podman: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

/// Whether the Podman client or the system loader reads this variable itself. A container's
/// value for it must not change how the client runs, find its helpers or reach its registry.
/// Only names the client is known to read are listed, so a secret such as DOCKER_PASSWORD
/// still keeps its value out of the arguments.
fn client_variable(key: &str) -> bool {
    const NAMES: [&str; 30] = [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TMPDIR",
        "TZ",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "REGISTRY_AUTH_FILE",
        "STORAGE_DRIVER",
        "STORAGE_OPTS",
        "NOTIFY_SOCKET",
        "DBUS_SESSION_BUS_ADDRESS",
        "GODEBUG",
        "GOGC",
        "GOMAXPROCS",
        "GOTRACEBACK",
        "CONTAINER_HOST",
        "CONTAINER_CONNECTION",
        "CONTAINER_SSHKEY",
        "CONTAINER_PASSPHRASE",
        "DOCKER_CONFIG",
        "DOCKER_CERT_PATH",
        "DOCKER_HOST",
        "DOCKER_TLS_VERIFY",
    ];
    const PREFIXES: [&str; 5] = ["LD_", "XDG_", "CONTAINERS_", "BUILDAH_", "PODMAN_"];
    let key = key.to_ascii_uppercase();
    NAMES.contains(&key.as_str()) || PREFIXES.iter().any(|prefix| key.starts_with(prefix))
}

/// Passes variables into a container. As GitHub's runner does with Docker, the arguments name
/// each variable and its value travels in the client's environment, so secrets never show in
/// the process list. A variable the client reads itself, or a name that cannot be an
/// environment variable, keeps its value in the arguments.
fn add_env(command: &mut Command, env: &BTreeMap<String, String>) {
    for (key, value) in env {
        command.arg("--env");
        if key.is_empty() || key.contains(['=', '\0']) || client_variable(key) {
            command.arg(format!("{key}={value}"));
        } else {
            command.arg(key).env(key, value);
        }
    }
}

impl Drop for Podman {
    fn drop(&mut self) {
        let mut pending = Cleanup {
            url: self.url.clone(),
            containers: std::mem::take(&mut self.containers),
            network: self.network.take(),
            images: std::mem::take(&mut self.images),
        };
        if pending.empty() {
            return;
        }
        // A dropped job may be running inside Tokio. A separate, joined runtime lets the same
        // ordered cleanup finish even during standalone runtime shutdown; no children detach.
        let _ = std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                if !runtime.block_on(pending.run()) {
                    eprintln!("Container cleanup failed");
                }
            }
        })
        .join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn arguments(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn variable(command: &Command, key: &str) -> Option<String> {
        command
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == key)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
    }

    #[test]
    fn container_variables_keep_their_values_out_of_the_arguments() {
        let env = BTreeMap::from([
            ("SECRET".to_string(), "hunter2".to_string()),
            ("PATH".to_string(), "/container/bin".to_string()),
            ("LD_PRELOAD".to_string(), "/tmp/hook.so".to_string()),
            ("DOCKER_PASSWORD".to_string(), "hunter2".to_string()),
        ]);
        let podman = Podman {
            url: None,
            options: Vec::new(),
            pull: false,
            prefix: "test".into(),
            containers: Vec::new(),
            images: Vec::new(),
            action_images: BTreeMap::new(),
            network: None,
            deny_docker: false,
            job: Some("job".into()),
            path: None,
        };
        let exec = podman.exec(
            &env,
            Path::new("/work"),
            "sh",
            &["-c".into(), "true".into()],
        );
        // Container creation and Docker actions pass their variables through the same function.
        let mut run = Command::new("podman");
        add_env(&mut run, &env);
        for command in [&exec, &run] {
            let args = arguments(command);
            assert!(!args.iter().any(|arg| arg.contains("hunter2")), "{args:?}");
            for secret in ["SECRET", "DOCKER_PASSWORD"] {
                assert!(args.windows(2).any(|pair| pair == ["--env", secret]));
                assert_eq!(variable(command, secret).as_deref(), Some("hunter2"));
            }
            // The client keeps its own PATH and loader, so it still finds its helpers.
            assert!(args
                .windows(2)
                .any(|pair| pair == ["--env", "PATH=/container/bin"]));
            assert!(args
                .windows(2)
                .any(|pair| pair == ["--env", "LD_PRELOAD=/tmp/hook.so"]));
            assert_eq!(variable(command, "PATH"), None);
            assert_eq!(variable(command, "LD_PRELOAD"), None);
        }
    }

    #[tokio::test]
    async fn a_docker_action_entrypoint_that_starts_with_a_dash_stays_the_entrypoint() {
        let options =
            Options::try_parse_from(["codeyard-actions", "push", "-W", "unused"]).unwrap();
        let mut podman = Podman::new(&options, "test".into()).unwrap();
        // A resolved image needs no Podman call.
        podman.action_images.insert(
            (
                Path::new("/action").to_path_buf(),
                "docker://alpine:3.21".into(),
            ),
            "resolved".into(),
        );
        let (command, _) = podman
            .docker_action(
                "docker://alpine:3.21",
                Path::new("/action"),
                Path::new("/work"),
                Path::new("/temp"),
                &BTreeMap::new(),
                Some("-x"),
                &[],
                std::time::Instant::now() + Duration::from_secs(60),
            )
            .await
            .unwrap();
        let args = arguments(&command);
        assert!(args.contains(&"--entrypoint=-x".to_string()), "{args:?}");
        let image = args.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(args[image + 1], "resolved", "{args:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn docker_actions_refuse_option_images_outside_dockerfiles_and_colon_paths() {
        let root = tempfile::tempdir().unwrap();
        let action = root.path().join("action");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&action).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::os::unix::fs::symlink(outside.join("Dockerfile"), action.join("Dockerfile")).unwrap();
        let options =
            Options::try_parse_from(["codeyard-actions", "push", "-W", "unused"]).unwrap();
        // Each image is refused before Podman runs.
        for (image, refusal) in [
            ("docker://--privileged", "Invalid Docker image"),
            ("-v=/:/host", "Invalid Docker image"),
            ("docker://", "Invalid Docker image"),
            ("Dockerfile", "must be inside the action"),
            ("../outside/Dockerfile", "must be inside the action"),
        ] {
            let mut podman = Podman::new(&options, "test".into()).unwrap();
            let error = podman
                .docker_action(
                    image,
                    &action,
                    Path::new("/work"),
                    Path::new("/temp"),
                    &BTreeMap::new(),
                    None,
                    &[],
                    std::time::Instant::now() + Duration::from_secs(60),
                )
                .await
                .unwrap_err();
            assert!(format!("{error:#}").contains(refusal), "{image}: {error:#}");
        }
        assert!(!builds_image(&action, "docker://example/Dockerfile"));
        assert!(builds_image(&action, "Dockerfile"));
        assert!(bind(&root.path().join("a:b"), Path::new("/github/workspace")).is_err());
        assert!(bind(Path::new("/work"), Path::new("/a:b")).is_err());
        assert_eq!(
            bind(Path::new("/work"), Path::new("/work")).unwrap(),
            "/work:/work:rw"
        );
    }
}
