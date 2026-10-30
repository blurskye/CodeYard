//! Action loading and handlers following actions/runner's `Runner.Worker/ActionManager.cs` and `Handlers/`.
use super::*;
use serde::Deserialize;
use anyhow::ensure;
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
    pre_entrypoint: Option<String>,
    post_entrypoint: Option<String>,
    args: Option<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

pub(super) enum ActionStage<'a> { Prepare(&'a crate::workflow::EnvironmentTemplate), Main }

pub(super) struct PreparedAction {
    runtime: String,
    state: BTreeMap<String, String>,
    post: Option<usize>,
}

enum PostRuntime { Node(String), Docker { image: String, args: Vec<String> } }

/// Looks up an input. Input names compare without case, as on GitHub.
fn input<'a>(inputs: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    inputs
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn export_inputs(env: &mut BTreeMap<String, String>, inputs: &BTreeMap<String, String>) {
    for (key, value) in inputs {
        env.insert(
            format!("INPUT_{}", key.replace(' ', "_").to_ascii_uppercase()),
            value.clone(),
        );
    }
}

/// A Docker action's main entrypoint: `runs.entrypoint`, else the `entrypoint` input, which is
/// how `docker://` steps and actions without one set it on GitHub.
fn docker_entrypoint(configured: Option<&str>, inputs: &BTreeMap<String, String>) -> Option<String> {
    [configured, input(inputs, "entrypoint")]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

/// A Docker action's arguments for every stage: the evaluated `runs.args` when the action sets
/// them, even as an empty list, else the `args` input split like a command line.
fn docker_args(configured: Option<Vec<String>>, inputs: &BTreeMap<String, String>) -> Result<Vec<String>> {
    match configured {
        Some(args) => Ok(args),
        None => Ok(input(inputs, "args").map(commands::words).transpose()?.unwrap_or_default()),
    }
}

pub(super) struct Post {
    /// The shown name of the step that used the action.
    pub(super) name: String,
    directory: PathBuf,
    script: String,
    runtime: PostRuntime,
    condition: String,
    env: BTreeMap<String, String>,
    /// Job exports when the action started. Exports that changed since then apply over `env`.
    exported: BTreeMap<String, String>,
    state: BTreeMap<String, String>,
    /// Main's command files while main runs, so state it saved survives a job timeout.
    main_files: Option<PathBuf>,
    contexts: Contexts,
}

async fn cache_lock(path: PathBuf) -> Result<std::fs::File> {
    let file = tokio::task::spawn_blocking(move || std::fs::OpenOptions::new()
        .create(true).truncate(false).write(true).open(path)).await??;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
}

pub(crate) struct ActionReference<'a> {
    components: Vec<&'a str>,
    version: &'a str,
}
impl<'a> ActionReference<'a> {
    pub(crate) fn parse(reference: &'a str) -> Result<Self> {
        let (repository_path, version) = reference.split_once('@').context("Expected owner/repo[/path]@ref")?;
        let components: Vec<_> = repository_path.split('/').collect();
        ensure!(components.len() >= 2 && components.iter().all(|part| {
            !part.is_empty() && *part != "." && *part != ".."
                && part.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        }) && !version.is_empty() && !version.starts_with('-') && !version.contains(['\0', '\n', '\r']), "Invalid action reference '{reference}'");
        Ok(Self { components, version })
    }
}

async fn git(directory: &Path, args: &[&str], credential: Option<(&str, &str)>) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "0")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .kill_on_drop(true);
    if let Some((origin, token)) = credential {
        use base64::Engine;
        let authorization = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        command.env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", format!("http.{}/.extraheader", origin.trim_end_matches('/')))
            .env("GIT_CONFIG_VALUE_0", format!("Authorization: Basic {authorization}"))
            .env("GIT_CONFIG_KEY_1", "http.followRedirects").env("GIT_CONFIG_VALUE_1", "false");
    }
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
    fn retain_action_bytes(&mut self, bytes: usize) -> Result<()> {
        self.action_bytes = self.action_bytes.saturating_add(bytes);
        ensure!(self.action_bytes <= 64 * 1024 * 1024, "Action lifecycle state exceeds its retention budget");
        Ok(())
    }

    fn retain_state(&mut self, state: &BTreeMap<String, String>) -> Result<()> {
        self.retain_action_bytes(state.iter().map(|(key, value)| 64 + key.len() + value.len()).sum())
    }

    fn retain_post(&mut self, env: &BTreeMap<String, String>, metadata: usize) -> Result<()> {
        let mut bytes = metadata.saturating_add(256);
        for values in [env, &self.saved_env] {
            for (key, value) in values { bytes = bytes.saturating_add(64 + key.len() + value.len()); }
        }
        let mut pending: Vec<_> = self.contexts.values.values().collect();
        while let Some(value) = pending.pop() {
            bytes = bytes.saturating_add(64);
            match value {
                Value::String(value) => bytes = bytes.saturating_add(value.len()),
                Value::Array(values) => pending.extend(values.iter()),
                Value::Object(values) => {
                    for (key, value) in values { bytes = bytes.saturating_add(key.len()); pending.push(value); }
                }
                _ => {}
            }
            ensure!(bytes <= 64 * 1024 * 1024, "Action lifecycle context exceeds its retention budget");
        }
        self.retain_action_bytes(bytes)
    }

    pub(super) fn prepare_step<'a>(&'a mut self, step: &'a Step, index: usize, depth: usize)
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            ensure!(depth <= 20, "Composite actions nested more than 20 levels");
            let StepAction::Uses { reference, with } = &step.action else { return Ok(()); };
            // Local actions exist only after checkout, so they are read when their step runs.
            if reference.starts_with("./") || reference.starts_with("docker://") { return Ok(()); }
            self.retain_action_bytes(256)?;
            if depth == 0 { self.checkpoint_scope(); }
            let outer_env = self.contexts.values.get("env").cloned();
            let mut env = self.env.clone();
            env.extend(self.saved_env.clone());
            self.contexts.values.insert("env".into(), object(&env));
            self.action_path.push(index);
            let mut outputs = BTreeMap::new();
            let result = self.uses(reference, with, env, self.deadline, depth, &mut outputs, ActionStage::Prepare(&step.env)).await;
            self.action_path.pop();
            match outer_env { Some(env) => { self.contexts.values.insert("env".into(), env); }, None => { self.contexts.values.remove("env"); } }
            if depth == 0 { self.scope = None; }
            ensure!(result?, "Action pre handler failed");
            Ok(())
        })
    }

    async fn fetch_action_repository(&self, reference: &str, components: &[&str], version: &str, source: &url::Url, token: Option<&str>) -> Result<(PathBuf, String)> {
        // The source URL participates in immutable cache identity, including forge base paths.
        let encoded = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(format!("{source}\0{version}")));
        let directory = self.cache.join("actions").join(components[0]).join(components[1]).join(encoded);
        let parent = directory.parent().context("Invalid action cache directory")?;
        std::fs::create_dir_all(parent)?;
        type Selection = Arc<tokio::sync::Mutex<Option<String>>>;
        static FETCHED: OnceLock<tokio::sync::Mutex<BTreeMap<PathBuf, Selection>>> = OnceLock::new();
        let selection = FETCHED.get_or_init(|| tokio::sync::Mutex::new(Default::default()))
            .lock().await.entry(directory.clone()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None))).clone();
        let mut selected = selection.lock().await;
        let _lock = cache_lock(directory.with_extension("lock")).await?;
        if let Some(token) = token { self.log.masker.add(&format!("x-access-token:{token}")); }
        let credential = token.map(|token| (source.as_str(), token));
        if selected.is_none() && !directory.join(".git").exists() {
            ensure!(!self.options.action_offline_mode, "Action '{reference}' is not available in the offline cache");
            let staging = tempfile::Builder::new().prefix("download-").tempdir_in(parent)?;
            git(staging.path(), &["init", "--quiet"], None).await?;
            let mut url = source.clone();
            url.set_path(&format!("{}/{}/{}.git", source.path().trim_end_matches('/'), components[0], components[1]));
            git(staging.path(), &["remote", "add", "origin", url.as_str()], None).await?;
            git(staging.path(), &["fetch", "--depth=1", "origin", version], credential).await?;
            git(staging.path(), &["checkout", "--force", "--detach", "FETCH_HEAD"], None).await?;
            std::fs::rename(staging.path(), &directory)?;
        } else if selected.is_none() && !self.options.action_offline_mode {
            git(&directory, &["fetch", "--depth=1", "origin", version], credential).await?;
        }
        if selected.is_none() {
            let commit = git_value(&directory, &["rev-parse", "FETCH_HEAD"]).await;
            ensure!(!commit.is_empty(), "Cached action '{reference}' has no fetched revision");
            *selected = Some(commit);
        }
        Ok((directory, selected.as_ref().expect("selected revision").clone()))
    }

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
        let ActionReference { components, version } = ActionReference::parse(reference)?;
        let origin = self.contexts.values.get("github").and_then(|github| match github {
            Value::Object(github) => github.get("server_url").map(Value::to_text), _ => None,
        }).filter(|origin| !origin.is_empty()).unwrap_or_else(|| "https://github.com".into());
        let origin = url::Url::parse(&origin).context("Invalid action forge URL")?;
        ensure!(matches!(origin.scheme(), "http" | "https") && origin.host_str().is_some()
            && origin.username().is_empty() && origin.password().is_none() && origin.query().is_none() && origin.fragment().is_none(), "Invalid action forge URL");
        let mut sources = vec![origin.clone()];
        if self.options.github_action_fallback && origin.as_str().trim_end_matches('/') != "https://github.com" {
            sources.push(url::Url::parse("https://github.com")?);
        }
        let token = self.contexts.values.get("github").and_then(|github| match github {
            Value::Object(github) => github.get("token").map(Value::to_text), _ => None,
        }).filter(|token| !token.is_empty());
        let mut resolved = None;
        let mut last = None;
        for (index, source) in sources.iter().enumerate() {
            if index > 0 { self.log.line("Warning: Using explicitly permitted GitHub action fallback"); }
            match self.fetch_action_repository(reference, &components, version, source,
                (source == &origin).then_some(token.as_deref()).flatten()).await {
                Ok(selection) => { resolved = Some(selection); break; }
                Err(error) => last = Some(error),
            }
        }
        let (directory, commit) = resolved.ok_or_else(|| last.unwrap_or_else(|| anyhow::anyhow!("Action source is unavailable")))?;
        let checkout_key = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(format!("{}\0{commit}", directory.display())));
        let checkout = self.temp.path().join(format!("action-{checkout_key}"));
        if let Ok(info) = std::fs::symlink_metadata(&checkout) {
            ensure!(info.is_dir(), "Action checkout is not a directory");
        } else {
            let staging = crate::fs::private_tempdir("action-", Some(self.temp.path()))?;
            git(staging.path(), &["clone", "--quiet", "--shared", "--no-checkout", &directory.display().to_string(), "."], None).await?;
            git(staging.path(), &["checkout", "--quiet", "--detach", &commit], None).await?;
            std::fs::rename(staging.path(), &checkout)?;
        }
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
        stage: ActionStage<'_>,
    ) -> Result<bool> {
        let setup = matches!(stage, ActionStage::Prepare(_));
        let templates = supplied.clone();
        let mut supplied = if setup { templates } else {
            self.evaluate(deadline, move |contexts, status| templates.iter()
                .map(|(key, template)| Ok((key.clone(), text(template, contexts, status)?)))
                .collect::<Result<BTreeMap<_, _>>>()).await?
        };
        if reference.starts_with("docker://") {
            export_inputs(&mut env, &supplied);
            let entrypoint = docker_entrypoint(None, &supplied);
            let args = docker_args(None, &supplied)?;
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
                    None,
                )
                .await;
        }
        let directory = until_deadline(deadline, self.action_directory(reference)).await?;
        let file = ["action.yml", "action.yaml"]
            .into_iter()
            .map(|name| directory.join(name))
            .find(|path| path.exists());
        // A metadata file that leads outside the action is refused, so an action cannot make the
        // runner read, and show in an error, another file of the host.
        let root = directory.canonicalize();
        let metadata: Metadata = match file {
            Some(file) => until_deadline(deadline, async move {
                tokio::task::spawn_blocking(move || -> Result<Metadata> {
                    use std::io::Read;
                    let file = file.canonicalize()?;
                    anyhow::ensure!(file.starts_with(root?), "The action's metadata file must be inside the action");
                    let mut options = std::fs::OpenOptions::new();
                    options.read(true);
                    #[cfg(unix)]
                    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK | libc::O_NOFOLLOW);
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
        if setup {
            if matches!(metadata.runs.using.as_str(), "node12" | "node16" | "node20" | "node24") && metadata.runs.pre.is_none() {
                let runtime = until_deadline(deadline, self.node_runtime(&metadata.runs.using, &env)).await?;
                self.retain_action_bytes(runtime.len() + self.action_path.len() * 8 + 128)?;
                self.prepared_actions.insert(self.action_path.clone(), PreparedAction { runtime, state: BTreeMap::new(), post: None });
                return Ok(true);
            }
            if metadata.runs.using == "docker" && metadata.runs.pre_entrypoint.is_none() { return Ok(true); }
            if let ActionStage::Prepare(template) = stage {
                let template = template.clone();
                env.extend(self.evaluate(deadline, move |contexts, status| template.resolve(contexts, status)).await?);
                self.contexts.values.insert("env".into(), object(&env));
            }
            let templates = supplied;
            supplied = self.evaluate(deadline, move |contexts, status| templates.iter()
                .map(|(key, template)| Ok((key.clone(), text(template, contexts, status)?)))
                .collect::<Result<BTreeMap<_, _>>>()).await?;
        }
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
                if let Some((repository, version)) = reference.split_once('@') {
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
                inputs.insert(key, self.text(&input.default, deadline).await?);
            }
        }
        // Input names compare without case, so a supplied value replaces any default.
        for key in supplied.keys() {
            inputs.retain(|name: &String, _| !name.eq_ignore_ascii_case(key));
        }
        inputs.extend(supplied);
        export_inputs(&mut env, &inputs);
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
                    let resolved_shell = self.default_shell.take();
                    let resolved_directory = self.default_working_directory.take();
                    let outer_deadline = self.deadline;
                    self.deadline = self.deadline.min(deadline);
                    self.env.extend(env);
                    self.status.failed = false;
                    let mut discarded = BTreeMap::new();
                    let nested = async {
                        for (index, step) in composite_steps.iter().enumerate() {
                            if setup { self.prepare_step(step, index, depth + 1).await?; }
                            else { self.step(step, index, depth + 1, &mut discarded).await?; }
                        }
                        if !setup {
                            for (key, output) in &metadata.outputs {
                                if let Some(value) = &output.value {
                                    outputs.insert(key.clone(), self.text(value, deadline).await?);
                                }
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
                    self.default_shell = resolved_shell;
                    self.default_working_directory = resolved_directory;
                    self.deadline = outer_deadline;
                    self.status = outer_status;
                    if let Some(steps) = outer_steps {
                        self.contexts.values.insert("steps".into(), steps);
                    }
                    nested
                }
                "node12" | "node16" | "node20" | "node24" => {
                    let prepared = self.prepared_actions.remove(&self.action_path);
                    let already = prepared.is_some();
                    let runtime = match prepared.as_ref() {
                        Some(prepared) => prepared.runtime.clone(),
                        None => until_deadline(deadline, self.node_runtime(&metadata.runs.using, &env)).await?,
                    };
                    let exported = self.saved_env.clone();
                    let mut state = prepared.as_ref().map(|prepared| prepared.state.clone()).unwrap_or_default();
                    let mut ok = true;
                    let main = metadata.runs.main.as_deref().context("Action is missing runs.main")?;
                    let post = match prepared.and_then(|prepared| prepared.post) {
                        Some(index) => {
                            self.retain_post(&env, 0)?;
                            self.posts[index].env = env.clone();
                            self.posts[index].exported = exported.clone();
                            self.posts[index].contexts = self.contexts.clone();
                            Some(index)
                        }
                        None => match metadata.runs.post.as_ref() {
                            Some(script) => {
                                self.retain_post(&env, self.step_name.len() + script.len() + runtime.len() + metadata.runs.post_if.as_ref().map_or(8, String::len))?;
                                self.posts.push(Post {
                                    name: self.step_name.clone(), directory: directory.clone(), script: script.clone(), runtime: PostRuntime::Node(runtime.clone()),
                                    condition: metadata.runs.post_if.clone().unwrap_or_else(|| "always()".into()),
                                    env: env.clone(), exported: exported.clone(), state: state.clone(),
                                    main_files: None, contexts: self.contexts.clone(),
                                });
                                Some(self.posts.len() - 1)
                            }
                            None => None,
                        },
                    };
                    // GitHub never runs a local action's pre: the action exists only once a step
                    // has checked it out.
                    if !already && metadata.runs.pre.is_some() && reference.starts_with("./") {
                        self.log.line(&format!("Warning: `pre` execution is not supported for local action from '{reference}'"));
                    } else if !already {
                        if let Some(pre) = &metadata.runs.pre {
                            let condition = metadata.runs.pre_if.clone().unwrap_or_else(|| "always()".into());
                            if self.evaluate(deadline, move |contexts, status| Ok(expressions::evaluate_condition(&condition, contexts, status)?)).await? {
                                self.prepare_files()?;
                                if let Some(index) = post { self.posts[index].main_files = Some(self.active_files.clone()); }
                                let result = self.node((&directory, pre, &runtime), (env.clone(), &exported), deadline, outputs, &mut state).await;
                                if let Some(index) = post { self.retain_state(&state)?; self.posts[index].state = state.clone(); }
                                ok = result?;
                            }
                        }
                    }
                    if setup {
                        self.retain_state(&state)?;
                        self.retain_action_bytes(runtime.len() + self.action_path.len() * 8 + 128)?;
                        self.prepared_actions.insert(self.action_path.clone(), PreparedAction { runtime, state, post });
                        return Ok(ok);
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
                        self.retain_state(&state)?;
                        self.posts[index].state = state;
                        if result.is_ok() {
                            self.posts[index].main_files = None;
                        }
                    }
                    result
                }
                "docker" => {
                    for (key, value) in &metadata.runs.env {
                        env.insert(key.clone(), self.text(value, deadline).await?);
                    }
                    let configured = match metadata.runs.args.clone() {
                        Some(args) => Some(self.evaluate(deadline, move |contexts, status| args.iter().map(|value| text(value, contexts, status)).collect::<Result<Vec<_>>>()).await?),
                        None => None,
                    };
                    let args = docker_args(configured, &inputs)?;
                    let args_bytes = args.iter().fold(0usize, |bytes, arg| bytes.saturating_add(arg.len() + 8));
                    let entrypoint = docker_entrypoint(metadata.runs.entrypoint.as_deref(), &inputs);
                    let image = metadata.runs.image.as_deref().context("Docker action is missing runs.image")?;
                    let prepared = self.prepared_actions.remove(&self.action_path);
                    let already = prepared.is_some();
                    let mut state = prepared.as_ref().map(|prepared| prepared.state.clone()).unwrap_or_default();
                    let post = match prepared.and_then(|prepared| prepared.post) {
                        Some(index) => {
                            self.retain_post(&env, args_bytes)?;
                            self.posts[index].env = env.clone();
                            self.posts[index].exported = self.saved_env.clone();
                            self.posts[index].contexts = self.contexts.clone();
                            if let PostRuntime::Docker { args: post_args, .. } = &mut self.posts[index].runtime { post_args.clone_from(&args); }
                            Some(index)
                        }
                        None => metadata.runs.post_entrypoint.as_ref().map(|script| -> Result<usize> {
                            self.retain_post(&env, self.step_name.len() + script.len() + image.len() + args_bytes + metadata.runs.post_if.as_ref().map_or(8, String::len))?;
                            self.posts.push(Post {
                                name: self.step_name.clone(), directory: directory.clone(), script: script.clone(), runtime: PostRuntime::Docker { image: image.into(), args: args.clone() },
                                condition: metadata.runs.post_if.clone().unwrap_or_else(|| "always()".into()),
                                env: env.clone(), exported: self.saved_env.clone(), state: BTreeMap::new(),
                                main_files: None, contexts: self.contexts.clone(),
                            });
                            Ok(self.posts.len() - 1)
                        }).transpose()?,
                    };
                    let mut ok = true;
                    if !already && metadata.runs.pre_entrypoint.is_some() && reference.starts_with("./") {
                        self.log.line(&format!("Warning: `pre` execution is not supported for local action from '{reference}'"));
                    } else if !already {
                        if let Some(pre) = &metadata.runs.pre_entrypoint {
                            let condition = metadata.runs.pre_if.clone().unwrap_or_else(|| "always()".into());
                            if self.evaluate(deadline, move |contexts, status| Ok(expressions::evaluate_condition(&condition, contexts, status)?)).await? {
                                self.prepare_files()?;
                                if let Some(index) = post { self.posts[index].main_files = Some(self.active_files.clone()); }
                                let result = self.docker(image, &directory, env.clone(), Some(pre), &args, deadline, outputs, Some(&mut state)).await;
                                if let Some(index) = post { self.retain_state(&state)?; self.posts[index].state = state.clone(); }
                                ok = result?;
                            }
                        }
                    }
                    if setup {
                        self.retain_state(&state)?;
                        self.retain_action_bytes(self.action_path.len() * 8 + 128)?;
                        self.prepared_actions.insert(self.action_path.clone(), PreparedAction { runtime: String::new(), state, post });
                        return Ok(ok);
                    }
                    let result = if ok {
                        self.prepare_files()?;
                        if let Some(index) = post { self.posts[index].main_files = Some(self.active_files.clone()); }
                        self.docker(image, &directory, env, entrypoint.as_deref(), &args, deadline, outputs, Some(&mut state)).await
                    } else { Ok(false) };
                    if let Some(index) = post {
                        self.retain_state(&state)?;
                        self.posts[index].state = state;
                        if result.is_ok() { self.posts[index].main_files = None; }
                    }
                    result
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
        // Action runtimes belong to the runner, not setup-node or GITHUB_PATH. Resolve an
        // absolute executable against the initial host/container PATH and retain it for posts.
        let mut effective = self.environment(env.clone());
        effective.insert("PATH".into(), self.runtime_path.clone());
        effective.remove("NODE_OPTIONS");
        for major in majors {
            for name in [format!("node{major}"), "node".into()] {
                let candidate = if self.podman.job.is_some() {
                    let args = ["-c".into(), "command -v \"$1\"".into(), "sh".into(), name];
                    let output = process::output(&mut self.podman.exec(&effective, &self.options.directory, "/bin/sh", &args), Duration::from_secs(10)).await;
                    match output {
                        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                        _ => continue,
                    }
                } else {
                    let Some(path) = std::env::split_paths(&self.runtime_path)
                        .filter(|path| path.is_absolute()).map(|path| path.join(&name))
                        .find_map(|path| path.canonicalize().ok().filter(|path| path.is_file())) else { continue; };
                    path.display().to_string()
                };
                if !Path::new(&candidate).is_absolute() { continue; }
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
        match post.contexts.values.get("secrets") {
            Some(secrets) => { self.contexts.values.insert("secrets".into(), secrets.clone()); }
            None => { self.contexts.values.remove("secrets"); }
        }
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
        let condition = post.condition.clone();
        match self.evaluate(self.deadline, move |contexts, status| Ok(expressions::evaluate_condition(&condition, contexts, status)?)).await {
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
        // Explicit cleanup steps and every post handler share one job cleanup deadline.
        let deadline = self.deadline;
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
            Ok(()) => match post.runtime {
                PostRuntime::Node(runtime) => self.node(
                    (&post.directory, &post.script, &runtime),
                    (post.env, &post.exported), deadline, &mut outputs, &mut post.state,
                ).await,
                PostRuntime::Docker { image, args } => self.docker(
                    &image, &post.directory, post_env, Some(&post.script), &args,
                    deadline, &mut outputs, Some(&mut post.state),
                ).await,
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
        mut state: Option<&mut BTreeMap<String, String>>,
    ) -> Result<bool> {
        let mut env = self.environment(env);
        if let Some(state) = state.as_ref() {
            for (key, value) in state.iter() { env.insert(format!("STATE_{key}"), value.clone()); }
        }
        env.insert("GITHUB_EVENT_PATH".into(), self.restore_event()?.display().to_string());
        let files = self.active_files.clone();
        // Building the action's image stops with the step: on its deadline or a cancellation.
        let mut cancel = self.cancel.clone();
        let (mut command, container) = tokio::select! {
            command = until_deadline(
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
            ) => command?,
            _ = raised(&mut cancel) => bail!("The operation was canceled."),
        };
        let mut commands = Commands::new(
            self.options.directory.clone(),
            std::mem::take(&mut self.matchers),
            self.step_debug(&env),
        );
        commands.stopped = self.command_stopped.take();
        let result = process::execute(
            &mut command,
            &self.log,
            &mut commands,
            &mut self.cancel,
            deadline,
            env.get("ACTIONS_ALLOW_UNSECURE_COMMANDS").is_some_and(|value| value.eq_ignore_ascii_case("true")),
            false,
        )
        .await;
        let cleaned = self.podman.cleanup_action(&container).await;
        if !cleaned {
            self.options.containment_lost.store(true, std::sync::atomic::Ordering::Release);
        }
        self.command_stopped = commands.stopped.take();
        self.matchers = commands.matchers;
        commands::update_outputs(outputs, commands.outputs);
        self.export_env(commands.env);
        self.paths.extend(commands.paths);
        if let Some(state) = state.as_mut() { state.extend(commands.state); }
        anyhow::ensure!(cleaned, "Docker action cleanup failed; refusing further execution");
        self.read_files(&files, outputs, state)?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn docker_actions_take_entrypoint_and_args_from_inputs_without_runs_values() {
        let supplied = inputs(&[("Entrypoint", "/bin/sh"), ("ARGS", "-c 'echo \"a b\"'")]);
        assert_eq!(
            docker_entrypoint(None, &supplied).as_deref(),
            Some("/bin/sh")
        );
        assert_eq!(
            docker_args(None, &supplied).unwrap(),
            ["-c", "echo \"a b\""]
        );
        assert_eq!(
            docker_entrypoint(Some("/main.sh"), &supplied).as_deref(),
            Some("/main.sh")
        );
        assert_eq!(
            docker_entrypoint(Some(""), &inputs(&[("entrypoint", "")])),
            None
        );
        // `runs.args`, even an empty list, replaces the input.
        assert!(docker_args(Some(Vec::new()), &supplied).unwrap().is_empty());
        assert!(docker_args(None, &BTreeMap::new()).unwrap().is_empty());
        let mut env = BTreeMap::new();
        export_inputs(&mut env, &inputs(&[("my input", "x"), ("args", "y")]));
        assert_eq!(env.get("INPUT_MY_INPUT").map(String::as_str), Some("x"));
        assert_eq!(env.get("INPUT_ARGS").map(String::as_str), Some("y"));
    }
}
