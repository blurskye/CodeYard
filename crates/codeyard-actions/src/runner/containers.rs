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
    prefix: String,
    containers: Vec<String>,
    images: Vec<String>,
    pub(super) network: Option<String>,
    deny_docker: bool,
    pub job: Option<String>,
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
            network: None,
            job: None,
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
        let output = super::process::output(self.command().args(args), Duration::from_secs(120))
            .await
            .context("Podman operation failed")?;
        if !output.status.success() {
            bail!("Podman: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().into())
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
        anyhow::ensure!(!self.deny_docker, "Container actions require CI_EXECUTION_MODE=podman");
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
        let id = if job { format!("{}-job", self.prefix) } else { format!("{}-service-{name}", self.prefix) };
        self.containers.push(id.clone());
        let mut args = vec!["create".into(), "--name".into(), id.clone()];
        args.extend(self.options.clone());
        let auth_directory = container.credentials.as_ref()
            .map(|_| crate::fs::private_tempdir("codeyard-registry-", None)).transpose()?;
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
            let path = auth_directory.as_ref().expect("registry directory exists").path().join("auth.json");
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
        for key in container.env.keys() {
            args.extend(["--env".into(), key.clone()]);
        }
        for port in &container.ports {
            args.extend(["--publish".into(), port.clone()]);
        }
        for volume in &container.volumes {
            args.extend(["--volume".into(), volume.clone()]);
        }
        if job {
            for path in [workspace, temp, cache] {
                args.extend([
                    "--volume".into(),
                    format!("{}:{}:rw", path.display(), path.display()),
                ]);
            }
            args.extend([
                "--workdir".into(),
                workspace.display().to_string(),
                "--entrypoint".into(),
                "tail".into(),
            ]);
        }
        args.push(container.image.clone());
        if job {
            args.extend(["-f".into(), "/dev/null".into()]);
        }
        let mut command = self.command();
        command.args(&args).envs(&container.env);
        let output = super::process::output(&mut command, Duration::from_secs(120)).await?;
        if !output.status.success() {
            bail!("Podman: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
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
        for (key, value) in env {
            command.arg("--env").arg(key).env(key, value);
        }
        if let Some(job) = &self.job {
            command.arg(job);
        }
        command.arg(program).args(args);
        command
    }

    #[allow(clippy::too_many_arguments)]
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
    ) -> Result<Command> {
        anyhow::ensure!(!self.deny_docker, "Docker actions require CI_EXECUTION_MODE=podman");
        let built = directory.join(image).is_file() || image == "Dockerfile"
            || image.ends_with("/Dockerfile")
            || image.ends_with(".dockerfile");
        let image = if built {
            let tag = format!("{}-action-image-{}", self.prefix, self.containers.len());
            self.images.push(tag.clone());
            let mut args: Vec<String> = vec!["build".into()];
            for label in self.ownership_labels() {
                args.extend(["--label".into(), label]);
            }
            args.extend([
                "--tag".into(),
                tag.clone(),
                "--file".into(),
                directory.join(image).display().to_string(),
                directory.display().to_string(),
            ]);
            let output = super::process::output(self.command().args(&args), deadline.saturating_duration_since(std::time::Instant::now())).await?;
            anyhow::ensure!(output.status.success(), "Docker action image build failed: {}", String::from_utf8_lossy(&output.stderr));
            tag
        } else {
            image.strip_prefix("docker://").unwrap_or(image).into()
        };
        let id = format!("{}-action-{}", self.prefix, self.containers.len());
        self.containers.push(id.clone());
        let mut command = self.command();
        command.args(["run", "--name", &id]);
        command.args(&self.options);
        command.args([
            "--pull",
            if built {
                "never"
            } else if self.pull {
                "always"
            } else {
                "missing"
            },
        ]);
        if let Some(network) = &self.network {
            command.args(["--network", network]);
        }
        // A repository-root action's directory is the workspace itself, and Podman rejects a
        // repeated mount destination.
        let mut mounted: Vec<&Path> = Vec::new();
        for path in [workspace, temp, directory] {
            if !mounted.contains(&path) {
                mounted.push(path);
                command
                    .arg("--volume")
                    .arg(format!("{}:{}:rw", path.display(), path.display()));
            }
        }
        if workspace != Path::new("/github/workspace") {
            command.arg("--volume").arg(format!("{}:/github/workspace:rw", workspace.display()));
        }
        command.arg("--workdir").arg("/github/workspace");
        for (key, value) in env {
            command.arg("--env").arg(key).env(key, value);
        }
        command.arg("--env").arg("GITHUB_WORKSPACE").env("GITHUB_WORKSPACE", "/github/workspace");
        if let Some(entrypoint) = entrypoint {
            command.args(["--entrypoint", entrypoint]);
        }
        command.arg(image).args(args);
        Ok(command)
    }

    pub async fn cleanup(&mut self) -> bool {
        let mut succeeded = true;
        for id in std::mem::take(&mut self.containers).into_iter().rev() {
            succeeded &= self
                .output(&["rm".into(), "--force".into(), "--volumes".into(), id])
                .await
                .is_ok();
        }
        if let Some(network) = self.network.take() {
            succeeded &= self
                .output(&["network".into(), "rm".into(), network])
                .await
                .is_ok();
        }
        for image in std::mem::take(&mut self.images) {
            succeeded &= self
                .output(&["rmi".into(), "--force".into(), image])
                .await
                .is_ok();
        }
        succeeded
    }
}

impl Drop for Podman {
    fn drop(&mut self) {
        // Cancellation may drop setup or an action before the normal job cleanup.
        for id in self.containers.drain(..) {
            let mut command = std::process::Command::new("podman");
            if let Some(url) = &self.url {
                command.args(["--url", url]);
            }
            let _ = command
                .args(["rm", "--force", "--volumes", &id])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        if let Some(network) = self.network.take() {
            let mut command = std::process::Command::new("podman");
            if let Some(url) = &self.url {
                command.args(["--url", url]);
            }
            let _ = command
                .args(["network", "rm", &network])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        for image in self.images.drain(..) {
            let mut command = std::process::Command::new("podman");
            if let Some(url) = &self.url {
                command.args(["--url", url]);
            }
            let _ = command
                .args(["rmi", "--force", &image])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}
