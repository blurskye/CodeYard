//! Action loading and handlers following actions/runner's `Runner.Worker/ActionManager.cs` and `Handlers/`.
use super::*;
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Clone, Default, Deserialize)]
struct Input {
    #[serde(default)]
    default: String,
}

#[derive(Default, Deserialize)]
struct Output {
    value: Option<String>,
}

#[derive(Default, Deserialize)]
struct Metadata {
    #[serde(default)]
    inputs: BTreeMap<String, Input>,
    #[serde(default)]
    outputs: BTreeMap<String, Output>,
    runs: Runs,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Runs {
    using: String,
    main: Option<String>,
    pre: Option<String>,
    post: Option<String>,
    pre_if: Option<String>,
    post_if: Option<String>,
    #[serde(default)]
    steps: Vec<serde_yaml::Value>,
    image: Option<String>,
    entrypoint: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

pub(super) struct Post {
    directory: PathBuf,
    script: String,
    runtime: String,
    condition: String,
    env: BTreeMap<String, String>,
    /// Job exports when the action started. Exports that changed since then apply over `env`.
    exported: BTreeMap<String, String>,
    state: BTreeMap<String, String>,
    /// Main's command files while main runs, so state it saved survives a job timeout.
    main_files: Option<PathBuf>,
    contexts: Contexts,
}

struct CacheLock(std::fs::File);

impl Drop for CacheLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

async fn cache_lock(path: PathBuf) -> Result<CacheLock> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(CacheLock(file))
    })
    .await?
    .map_err(Into::into)
}

async fn git(directory: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let output = process::output(&mut command, Duration::from_secs(120))
        .await
        .context("Fetching action failed")?;
    if !output.status.success() {
        bail!(
            "Unable to fetch action: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

impl Job {
    async fn action_directory(&self, reference: &str) -> Result<PathBuf> {
        if reference.starts_with("./") {
            let directory = self
                .options
                .directory
                .join(reference)
                .canonicalize()
                .context("Unable to find local action")?;
            if !directory.starts_with(&self.options.directory) {
                bail!("Local action must be inside the workspace");
            }
            return Ok(directory);
        }
        let (repository_path, version) = reference
            .rsplit_once('@')
            .context("Expected owner/repo[/path]@ref")?;
        let components: Vec<_> = repository_path.split('/').collect();
        if components.len() < 2
            || components.iter().any(|part| {
                part.is_empty()
                    || *part == "."
                    || *part == ".."
                    || !part
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
            })
            || version.is_empty()
            || version.starts_with('-')
            || version.contains(['\0', '\n', '\r'])
        {
            bail!("Invalid action reference '{reference}'");
        }
        // A digest keeps the cache name within filename limits however long the ref is.
        let encoded = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(version.as_bytes())
        );
        let directory = self
            .cache
            .join("actions")
            .join(components[0])
            .join(components[1])
            .join(encoded);
        let parent = directory
            .parent()
            .context("Invalid action cache directory")?;
        std::fs::create_dir_all(parent)?;
        static FETCHED: OnceLock<tokio::sync::Mutex<std::collections::BTreeSet<PathBuf>>> =
            OnceLock::new();
        let mut fetched = FETCHED
            .get_or_init(|| tokio::sync::Mutex::new(Default::default()))
            .lock()
            .await;
        let _lock = cache_lock(directory.with_extension("lock")).await?;
        if !directory.join(".git").exists() {
            if self.options.action_offline_mode {
                bail!("Action '{reference}' is not available in the offline cache");
            }
            let parent = directory
                .parent()
                .context("Invalid action cache directory")?;
            std::fs::create_dir_all(parent)?;
            let staging = tempfile::Builder::new()
                .prefix("download-")
                .tempdir_in(parent)?;
            git(staging.path(), &["init", "--quiet"]).await?;
            let url = format!("https://github.com/{}/{}.git", components[0], components[1]);
            git(staging.path(), &["remote", "add", "origin", &url]).await?;
            git(staging.path(), &["fetch", "--depth=1", "origin", version]).await?;
            git(
                staging.path(),
                &["checkout", "--force", "--detach", "FETCH_HEAD"],
            )
            .await?;
            std::fs::rename(staging.path(), &directory)?;
            fetched.insert(directory.clone());
        } else if !self.options.action_offline_mode && !fetched.contains(&directory) {
            git(&directory, &["fetch", "--depth=1", "origin", version]).await?;
            fetched.insert(directory.clone());
        }
        let commit = git_value(&directory, &["rev-parse", "FETCH_HEAD"]).await;
        if commit.is_empty() {
            bail!("Cached action '{reference}' has no fetched revision");
        }
        let checkout = tempfile::Builder::new()
            .prefix("action-")
            .tempdir_in(self.temp.path())?
            .keep();
        git(
            &checkout,
            &[
                "clone",
                "--quiet",
                "--shared",
                "--no-checkout",
                &directory.display().to_string(),
                ".",
            ],
        )
        .await?;
        git(&checkout, &["checkout", "--quiet", "--detach", &commit]).await?;
        let action = components[2..]
            .iter()
            .fold(checkout.clone(), |path, component| path.join(component));
        let action = action
            .canonicalize()
            .context("Action path does not exist")?;
        if !action.starts_with(checkout) {
            bail!("Action path must be inside its repository");
        }
        Ok(action)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn uses(
        &mut self,
        reference: &str,
        supplied: &BTreeMap<String, String>,
        mut env: BTreeMap<String, String>,
        deadline: Instant,
        depth: usize,
        outputs: &mut BTreeMap<String, String>,
    ) -> Result<bool> {
        let mut supplied = supplied
            .iter()
            .map(|(key, template)| Ok((key.clone(), text(template, &self.contexts, self.status)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        if reference.starts_with("docker://") {
            for (key, value) in &supplied {
                env.insert(format!("INPUT_{}", key.replace(' ', "_").to_ascii_uppercase()), value.clone());
            }
            let args = supplied
                .remove("args")
                .map(|args| commands::words(&args))
                .transpose()?
                .unwrap_or_default();
            let entrypoint = supplied.remove("entrypoint");
            let directory = self.options.directory.clone();
            return self
                .docker(
                    reference,
                    &directory,
                    env,
                    entrypoint.as_deref(),
                    &args,
                    deadline,
                    outputs,
                )
                .await;
        }
        let docker_args = supplied.get("args").cloned();
        let docker_entrypoint = supplied.get("entrypoint").cloned();
        let directory = until_deadline(deadline, self.action_directory(reference)).await?;
        let file = ["action.yml", "action.yaml"]
            .into_iter()
            .map(|name| directory.join(name))
            .find(|path| path.exists());
        let metadata: Metadata = match file {
            Some(file) => until_deadline(deadline, async move {
                tokio::task::spawn_blocking(move || -> Result<Metadata> {
                    use std::io::Read;
                    let mut options = std::fs::OpenOptions::new();
                    options.read(true);
                    #[cfg(unix)]
                    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
                    let file = options.open(file)?;
                    anyhow::ensure!(file.metadata()?.is_file(), "Action metadata must be a regular file");
                    let mut content = String::new();
                    file.take(1024 * 1024 + 1).read_to_string(&mut content)?;
                    anyhow::ensure!(content.len() <= 1024 * 1024, "Action metadata is too large");
                    Ok(serde_yaml::from_str(&content)?)
                }).await?
            }).await?,
            // A directory with only a Dockerfile is a Docker action without inputs.
            None if directory.join("Dockerfile").is_file() => Metadata {
                runs: Runs {
                    using: "docker".into(),
                    image: Some("Dockerfile".into()),
                    ..Runs::default()
                },
                ..Metadata::default()
            },
            None => bail!("Can't find 'action.yml', 'action.yaml' or 'Dockerfile' for action"),
        };
        let composite_steps = if metadata.runs.using == "composite" {
            metadata.runs.steps.iter().map(workflow::parse_step).collect::<Result<Vec<_>>>()?
        } else { Vec::new() };
        let old_inputs = self.contexts.values.get("inputs").cloned();
        let mut old_action = BTreeMap::new();
        if let Some(Value::Object(github)) = self.contexts.values.get_mut("github") {
            for key in ["action_path", "action_repository", "action_ref"] {
                old_action.insert(key, github.remove(key));
            }
            github.insert(
                "action_path".into(),
                Value::String(directory.display().to_string()),
            );
            if !reference.starts_with("./") {
                if let Some((repository, version)) = reference.rsplit_once('@') {
                    github.insert(
                        "action_repository".into(),
                        Value::String(repository.split('/').take(2).collect::<Vec<_>>().join("/")),
                    );
                    github.insert("action_ref".into(), Value::String(version.into()));
                }
            }
        }
        // Inputs and step environment were evaluated in caller scope; action-owned
        // defaults, templates and composite steps never receive the secrets context.
        let old_secrets = self.contexts.values.remove("secrets");
        let result = async {
        let mut inputs = BTreeMap::new();
        for (key, input) in metadata.inputs {
            if !supplied.keys().any(|name| name.eq_ignore_ascii_case(&key)) {
                inputs.insert(key, text(&input.default, &self.contexts, self.status)?);
            }
        }
        // Input names compare without case, so a supplied value replaces any default.
        for key in supplied.keys() {
            inputs.retain(|name: &String, _| !name.eq_ignore_ascii_case(key));
        }
        inputs.extend(supplied);
        for (key, value) in &inputs {
            env.insert(
                format!("INPUT_{}", key.replace(' ', "_").to_ascii_uppercase()),
                value.clone(),
            );
        }
        env.insert("GITHUB_ACTION_PATH".into(), directory.display().to_string());
        self.contexts
            .values
            .insert("inputs".into(), object(&inputs));
            match metadata.runs.using.as_str() {
                "composite" => {
                    let outer_steps = self
                        .contexts
                        .values
                        .insert("steps".into(), Value::Object(BTreeMap::new()));
                    let outer_env = self.env.clone();
                    let outer_env_context = self.contexts.values.get("env").cloned();
                    let first_post = self.posts.len();
                    let outer_status = self.status;
                    let outer_saved_env = std::mem::take(&mut self.saved_env);
                    let outer_shell = self.plan.default_shell.take();
                    let outer_directory = self.plan.default_working_directory.take();
                    let outer_deadline = self.deadline;
                    self.deadline = self.deadline.min(deadline);
                    self.env.extend(env);
                    self.status.failed = false;
                    let mut discarded = BTreeMap::new();
                    let nested = async {
                        for step in &composite_steps {
                            self.step(step, depth + 1, &mut discarded).await?;
                        }
                        for (key, output) in &metadata.outputs {
                            if let Some(value) = &output.value {
                                outputs
                                    .insert(key.clone(), text(value, &self.contexts, self.status)?);
                            }
                        }
                        Ok(!self.status.failed)
                    }
                    .await;
                    self.env = outer_env;
                    for post in &mut self.posts[first_post..] {
                        for (key, value) in &outer_saved_env {
                            post.exported.entry(key.clone()).or_insert_with(|| value.clone());
                        }
                    }
                    if let Some(env) = outer_env_context {
                        self.contexts.values.insert("env".into(), env);
                    }
                    let updates = std::mem::replace(&mut self.saved_env, outer_saved_env);
                    self.saved_env.extend(updates);
                    self.plan.default_shell = outer_shell;
                    self.plan.default_working_directory = outer_directory;
                    self.deadline = outer_deadline;
                    self.status = outer_status;
                    if let Some(steps) = outer_steps {
                        self.contexts.values.insert("steps".into(), steps);
                    }
                    nested
                }
                "node12" | "node16" | "node20" | "node24" => {
                    let runtime =
                        until_deadline(deadline, self.node_runtime(&metadata.runs.using, &env))
                            .await?;
                    let exported = self.saved_env.clone();
                    let mut state = BTreeMap::new();
                    let mut ok = true;
                    let main = metadata.runs.main.as_deref().context("Action is missing runs.main")?;
                    let post = match metadata.runs.post.as_ref() {
                        Some(script) => {
                            self.posts.push(Post {
                                directory: directory.clone(), script: script.clone(), runtime: runtime.clone(),
                                condition: metadata.runs.post_if.clone().unwrap_or_else(|| "always()".into()),
                                env: env.clone(), exported: exported.clone(), state: state.clone(),
                                main_files: None, contexts: self.contexts.clone(),
                            });
                            Some(self.posts.len() - 1)
                        }
                        None => None,
                    };
                    if let Some(pre) = &metadata.runs.pre {
                        if expressions::evaluate_condition(
                            metadata.runs.pre_if.as_deref().unwrap_or("always()"),
                            &self.contexts,
                            self.status,
                        )? {
                            self.prepare_files()?;
                            if let Some(index) = post { self.posts[index].main_files = Some(self.active_files.clone()); }
                            let result = self.node((&directory, pre, &runtime), (env.clone(), &exported), deadline, outputs, &mut state).await;
                            if let Some(index) = post { self.posts[index].state = state.clone(); }
                            ok = result?;
                        }
                    }
                    let result = if ok {
                        self.prepare_files()?;
                        if let Some(index) = post {
                            self.posts[index].main_files = Some(self.active_files.clone());
                        }
                        self.node(
                            (&directory, main, &runtime),
                            (env.clone(), &exported),
                            deadline,
                            outputs,
                            &mut state,
                        )
                        .await
                    } else {
                        Ok(false)
                    };
                    if let Some(index) = post {
                        self.posts[index].state = state;
                        if result.is_ok() {
                            self.posts[index].main_files = None;
                        }
                    }
                    result
                }
                "docker" => {
                    for (key, value) in &metadata.runs.env {
                        env.insert(key.clone(), text(value, &self.contexts, self.status)?);
                    }
                    let args = match &docker_args {
                        Some(args) => commands::words(args)?,
                        None => metadata.runs.args.iter().map(|value| text(value, &self.contexts, self.status)).collect::<Result<Vec<_>>>()?,
                    };
                    let entrypoint = docker_entrypoint.or(metadata.runs.entrypoint.clone());
                    let image = metadata
                        .runs
                        .image
                        .as_deref()
                        .context("Docker action is missing runs.image")?;
                    self.docker(
                        image,
                        &directory,
                        env,
                        entrypoint.as_deref(),
                        &args,
                        deadline,
                        outputs,
                    )
                    .await
                }
                using => bail!("Unsupported action runtime '{using}'"),
            }
        }
        .await;
        if let Some(secrets) = old_secrets {
            self.contexts.values.insert("secrets".into(), secrets);
        }
        if let Some(inputs) = old_inputs {
            self.contexts.values.insert("inputs".into(), inputs);
        }
        if let Some(Value::Object(github)) = self.contexts.values.get_mut("github") {
            for (key, value) in old_action {
                if let Some(value) = value {
                    github.insert(key.into(), value);
                } else {
                    github.remove(key);
                }
            }
        }
        result
    }

    async fn node_runtime(&self, using: &str, env: &BTreeMap<String, String>) -> Result<String> {
        let set = |key: &str| {
            env.get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
        };
        // As on GitHub's runners now, actions written for an older Node run on Node 24 unless the
        // workflow asks to keep Node 20; either installed version serves them.
        let majors: &[&str] = if using == "node24" {
            &["24"]
        } else if set("ACTIONS_ALLOW_USE_UNSECURE_NODE_VERSION")
            && !set("FORCE_JAVASCRIPT_ACTIONS_TO_NODE24")
        {
            &["20", "24"]
        } else {
            &["24", "20"]
        };
        // The probe sees the PATH the action will run with, including GITHUB_PATH additions.
        let effective = self.environment(env.clone());
        for major in majors {
            for candidate in [format!("node{major}"), "node".into()] {
                let args = ["--version".into()];
                let mut command = if self.podman.job.is_some() {
                    self.podman
                        .exec(&effective, &self.options.directory, &candidate, &args)
                } else {
                    Command::new(&candidate)
                };
                if self.podman.job.is_none() {
                    command.args(&args).envs(&effective);
                }
                command.kill_on_drop(true);
                if let Ok(output) = process::output(&mut command, Duration::from_secs(10)).await {
                    if output.status.success()
                        && String::from_utf8_lossy(&output.stdout)
                            .trim()
                            .starts_with(&format!("v{major}."))
                    {
                        return Ok(candidate);
                    }
                }
            }
        }
        bail!(
            "Action requires Node.js {}. Install node{} or a matching node on PATH.",
            majors[0],
            majors[0]
        )
    }

    async fn node(
        &mut self,
        (directory, script, runtime): (&Path, &str, &str),
        (env, exported): (BTreeMap<String, String>, &BTreeMap<String, String>),
        deadline: Instant,
        outputs: &mut BTreeMap<String, String>,
        state: &mut BTreeMap<String, String>,
    ) -> Result<bool> {
        self.matchers.reset();
        let script = directory.join(script);
        if !script.is_file() {
            bail!("Action script '{}' does not exist", script.display());
        }
        // `env` holds the exports from before the action started. Exports made since then, by
        // an earlier phase of this action or by later steps, are visible here, as on GitHub.
        let mut merged = env;
        for (key, value) in &self.saved_env {
            if exported.get(key) != Some(value) {
                merged.insert(key.clone(), value.clone());
            }
        }
        merged.remove("NODE_ICU_DATA");
        for (key, value) in state.iter() {
            merged.insert(format!("STATE_{key}"), value.clone());
        }
        let workspace = self.options.directory.clone();
        self.invoke(
            runtime,
            &[script.display().to_string()],
            &workspace,
            merged,
            deadline,
            outputs,
            Some(state),
        )
        .await
    }

    pub(super) async fn post(&mut self, mut post: Post) -> Result<bool> {
        let outer_contexts = self.contexts.clone();
        for name in ["github", "inputs", "steps"] {
            if let Some(value) = post.contexts.values.get(name) {
                self.contexts.values.insert(name.into(), value.clone());
            }
        }
        let mut post_env = post.env.clone();
        for (key, value) in &self.saved_env {
            if post.exported.get(key) != Some(value) { post_env.insert(key.clone(), value.clone()); }
        }
        self.contexts.values.insert("env".into(), object(&post_env));
        match expressions::evaluate_condition(&post.condition, &self.contexts, self.status) {
            Ok(true) => {}
            condition => {
                self.contexts = outer_contexts;
                condition?;
                return Ok(false);
            }
        }
        self.log.line(&format!(
            "Post action started: {}",
            post.directory.display()
        ));
        // Cleanup handlers get their own cancellation grace period after a job timeout.
        let deadline = Instant::now() + Duration::from_secs(60);
        let original = self.cancel.clone();
        let (_tx, cancel) = watch::channel(false);
        self.cancel = cancel;
        // A job timeout can stop main before its saved state was collected.
        if let Some(files) = post.main_files.take() {
            if let Ok(saved) = super::read_command_file(&files.join("STATE")) {
                post.state
                    .extend(commands::pairs(&saved).unwrap_or_default());
            }
        }
        let mut outputs = BTreeMap::new();
        let result = match self.prepare_files() {
            Ok(()) => {
                self.node(
                    (&post.directory, &post.script, &post.runtime),
                    (post.env, &post.exported),
                    deadline,
                    &mut outputs,
                    &mut post.state,
                )
                .await
            }
            Err(error) => Err(error),
        };
        self.cancel = original;
        self.contexts = outer_contexts;
        if !result? {
            bail!("Post action failed");
        }
        self.log.line("Post action completed: success");
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    async fn docker(
        &mut self,
        image: &str,
        directory: &Path,
        env: BTreeMap<String, String>,
        entrypoint: Option<&str>,
        args: &[String],
        deadline: Instant,
        outputs: &mut BTreeMap<String, String>,
    ) -> Result<bool> {
        let mut env = self.environment(env);
        env.insert("GITHUB_EVENT_PATH".into(), self.restore_event()?.display().to_string());
        let files = self.active_files.clone();
        let mut command = until_deadline(
            deadline,
            self.podman.docker_action(
                image,
                directory,
                &self.options.directory,
                self.temp.path(),
                &env,
                entrypoint,
                args,
                deadline,
            ),
        )
        .await?;
        let mut commands = Commands::new(
            self.options.directory.clone(),
            std::mem::take(&mut self.matchers),
        );
        commands.stopped = self.command_stopped.take();
        commands.debug = self.options.debug || env.get("ACTIONS_STEP_DEBUG").is_some_and(|value| value.eq_ignore_ascii_case("true"));
        let result = process::execute(
            &mut command,
            &self.log,
            &mut commands,
            &mut self.cancel,
            deadline,
            env.get("ACTIONS_ALLOW_UNSECURE_COMMANDS").is_some_and(|value| value.eq_ignore_ascii_case("true")),
        )
        .await;
        self.command_stopped = commands.stopped.take();
        self.matchers = commands.matchers;
        commands::update_outputs(outputs, commands.outputs);
        self.saved_env.extend(commands.env);
        self.paths.extend(commands.paths);
        self.read_files(&files, outputs, None)?;
        result
    }
}
