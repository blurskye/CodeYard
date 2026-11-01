//! `codeyard backup` and `codeyard restore` for a single-node installation.
//!
//! A backup directory holds the database (`database.sqlite` or `database.pgdump`), a bare copy
//! of every repository the database names and of every kept deleted one under `deleted/`, the
//! local uploads under `objects/` and the node key. The database is snapshotted first and Git
//! after it, so a restored database is never newer than the Git data it describes. The new
//! database replaces the previous backup's only after every copy has succeeded. Anything Git has
//! that the database does not, such as a later push to a pull request, is settled by
//! `codeyard repo verify --repair`, which a restore runs at the end. Running the backup into
//! the same directory again copies only what changed.
mod repair;

pub use repair::{rebuild, verify};

use crate::backend::{db, fs::private_dir, git::mirror};
use anyhow::{bail, ensure, Context, Result};
use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

const SQLITE_FILE: &str = "database.sqlite";
const POSTGRES_FILE: &str = "database.pgdump";
const REPOSITORIES_DIR: &str = "repositories";
const UPLOADS_DIR: &str = "objects";
const NODE_KEY: &str = "node.key";

/// Where this installation keeps its data.
pub struct Places<'a> {
    pub database_url: &'a str,
    pub state_dir: &'a Path,
    pub repos_path: &'a Path,
}

fn private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Makes the entries of a directory durable, so a renamed or new file survives a power loss.
fn sync_dir(path: &Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

/// Copies a file, then syncs the copy and the directory that holds it.
fn copy_synced(from: &Path, to: &Path) -> Result<()> {
    reject_symlink(to)?;
    let parent = to.parent().context("copy destination has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut std::fs::File::open(from)?, temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;
    temporary.persist(to).map_err(|error| error.error)?;
    if let Some(parent) = to.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// The bare repositories directly under `root`, by directory name.
fn repositories_in(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut repositories = Vec::new();
    for entry in entries {
        let entry = entry?;
        ensure!(
            !entry.file_type()?.is_symlink(),
            "repository directory contains an unsupported symlink: {}",
            entry.path().display()
        );
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
            if name.ends_with(".git") && path.join("HEAD").is_file() {
                repositories.push((name.to_owned(), path));
            }
        }
    }
    repositories.sort();
    Ok(repositories)
}

fn directory_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("a repository path has no directory name")?;
    Ok(name.to_owned())
}

/// Mirrors each repository into `copy`. A mirror copies every object file, so the commits that
/// pull requests and CI runs name come along even when no ref reaches them. Returns the copies
/// whose repository is no longer listed; the caller removes them once the new database is in
/// place, so the previous backup keeps everything its database names until then.
async fn mirror_all(repositories: &[(String, PathBuf)], copy: &Path) -> Result<Vec<PathBuf>> {
    private_dir(copy)?;
    let retained: HashSet<_> = repositories.iter().map(|(name, _)| name.as_str()).collect();
    reject_symlink(copy)?;
    for (name, path) in repositories {
        reject_symlink(&copy.join(name))?;
        mirror::mirror(path, &copy.join(name))
            .await
            .with_context(|| format!("copy repository {name}"))?;
    }
    Ok(repositories_in(copy)?
        .into_iter()
        .filter(|(name, _)| !retained.contains(name.as_str()))
        .map(|(_, path)| path)
        .collect())
}

/// The kept deleted repositories under each root.
fn kept_deleted(roots: &BTreeSet<PathBuf>) -> Result<Vec<(String, PathBuf)>> {
    let mut kept = Vec::new();
    for root in roots {
        kept.extend(repositories_in(&root.join(mirror::DELETED_DIR))?);
    }
    Ok(kept)
}

/// `path` with its symlinks and `..` resolved, also when its last components do not exist yet.
fn resolved(path: &Path) -> Result<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            other => {
                resolved.push(other);
                // An existing prefix may be a symlink, which `..` then leaves from its target.
                if let Ok(real) = resolved.canonicalize() {
                    resolved = real;
                }
            }
        }
    }
    Ok(resolved)
}

/// Refuses a destination that overlaps the data the backup reads. The copy would otherwise read
/// its own output, such as an upload tree copied into itself.
fn check_destination(dir: &Path, places: &Places<'_>) -> Result<()> {
    let dir = resolved(dir)?;
    let targets = [
        resolved(&dir.join(REPOSITORIES_DIR))?,
        resolved(&dir.join(mirror::DELETED_DIR))?,
        resolved(&dir.join(UPLOADS_DIR))?,
    ];
    for name in [
        REPOSITORIES_DIR,
        mirror::DELETED_DIR,
        UPLOADS_DIR,
        SQLITE_FILE,
        POSTGRES_FILE,
        NODE_KEY,
        "release.key",
        "database.sqlite.partial",
        "database.pgdump.partial",
        "backup.lock",
    ] {
        reject_symlink(&dir.join(name))?;
    }
    if !places.database_url.is_empty() {
        if let Some(database) = db::sqlite_file(places.database_url)? {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = database.as_os_str().to_owned();
                path.push(suffix);
                let source = resolved(Path::new(&path))?;
                for target in targets.iter().cloned().chain([
                    dir.join(SQLITE_FILE),
                    dir.join("database.sqlite.partial"),
                    dir.join(POSTGRES_FILE),
                    dir.join("database.pgdump.partial"),
                    dir.join(NODE_KEY),
                    dir.join("release.key"),
                    dir.join("backup.lock"),
                ]) {
                    ensure!(
                        !source.starts_with(&target),
                        "backup destination collides with the live SQLite database or its sidecars"
                    );
                }
            }
        }
    }
    for source in [
        places.state_dir.join(UPLOADS_DIR),
        places.repos_path.to_path_buf(),
    ] {
        let source = resolved(&source)?;
        for target in &targets {
            ensure!(
                !target.starts_with(&source) && !source.starts_with(target),
                "backup directory {} overlaps {}; choose a directory outside it",
                dir.display(),
                source.display()
            );
        }
    }
    Ok(())
}

/// Copies the files under `source` to `copy`, skipping files whose size and modification time
/// already match, and adds to `stale` the files and directories `source` no longer has, for the
/// caller to remove. Names starting with a dot, such as the uploads' `.staging`, are skipped. A
/// file is written under a temporary name and renamed into place once it is synced, so an
/// interrupted copy never leaves a short file behind.
fn reject_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.is_symlink(),
            "unsupported symlink {}; use a bind mount instead",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn copy_tree(source: &Path, copy: &Path, stale: &mut Vec<PathBuf>) -> Result<usize> {
    reject_symlink(source)?;
    reject_symlink(copy)?;
    private_dir(copy)?;
    let mut copied = 0;
    let mut changed = false;
    let mut present = std::collections::HashSet::new();
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        present.insert(name.clone());
        let (from, to) = (entry.path(), copy.join(&name));
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "unsupported source symlink {}; use a bind mount instead",
            from.display()
        );
        reject_symlink(&to)?;
        if kind.is_dir() {
            changed |= !to.is_dir();
            copied += copy_tree(&from, &to, stale)?;
        } else if kind.is_file() {
            let metadata = entry.metadata()?;
            let modified = metadata.modified()?;
            let current = std::fs::metadata(&to).ok();
            if current.is_some_and(|current| {
                current.len() == metadata.len() && current.modified().ok() == Some(modified)
            }) {
                continue;
            }
            let mut temporary = tempfile::NamedTempFile::new_in(copy)?;
            std::io::copy(&mut std::fs::File::open(&from)?, temporary.as_file_mut())?;
            temporary.as_file().set_modified(modified)?;
            temporary.as_file().sync_all()?;
            temporary.persist(&to).map_err(|error| error.error)?;
            copied += 1;
            changed = true;
        }
    }
    for entry in std::fs::read_dir(copy)? {
        let entry = entry?;
        if !present.contains(&entry.file_name()) {
            stale.push(entry.path());
        }
    }
    if changed {
        sync_dir(copy)?;
    }
    Ok(copied)
}

/// Removes what `mirror_all` and `copy_tree` found stale.
fn remove_stale(stale: &[PathBuf]) -> Result<()> {
    for path in stale {
        reject_symlink(path)?;
        if path.is_dir() {
            std::fs::remove_dir_all(path)?;
        } else if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

/// A libpq connection URL without its password, and the password, which libpq tools read
/// from `PGPASSWORD` so that it never appears in their arguments.
fn libpq_url(database_url: &str) -> Result<(String, Option<String>)> {
    let mut url = url::Url::parse(database_url).context("invalid DATABASE_URL")?;
    // The URL keeps its password percent-encoded; decoding it as a form value keeps a `+`.
    let mut password = url.password().and_then(|password| {
        url::form_urlencoded::parse(format!("p={}", password.replace('+', "%2B")).as_bytes())
            .next()
            .map(|(_, value)| value.into_owned())
    });
    let mut query = Vec::new();
    for (name, value) in url.query_pairs() {
        // SQLx accepts a few spellings libpq does not.
        let name = match name.as_ref() {
            "password" => {
                password = Some(value.into_owned());
                continue;
            }
            "statement-cache-capacity" => continue,
            "ssl-mode" => "sslmode",
            "ssl-root-cert" | "ssl-ca" => "sslrootcert",
            "ssl-cert" => "sslcert",
            "ssl-key" => "sslkey",
            other if other.starts_with("options[") => {
                bail!(
                    "pg_dump cannot take the options[...] parameters of DATABASE_URL; use options="
                )
            }
            other => other,
        }
        .to_owned();
        query.push((name, value.into_owned()));
    }
    if url.password().is_some() {
        url.set_password(None)
            .map_err(|_| anyhow::anyhow!("invalid DATABASE_URL"))?;
    }
    if query.is_empty() {
        url.set_query(None);
    } else {
        let query = query
            .into_iter()
            .map(|(name, value)| {
                format!(
                    "{}={}",
                    crate::webui::encode_component(&name),
                    crate::webui::encode_component(&value)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&query));
    }
    Ok((url.to_string(), password))
}

/// Runs a PostgreSQL client tool against `database_url`.
async fn postgres_tool(program: &str, database_url: &str, arguments: &[&str]) -> Result<()> {
    let (url, password) = libpq_url(database_url)?;
    let mut command = tokio::process::Command::new(program);
    command
        .arg(format!("--dbname={url}"))
        .args(arguments)
        .env("PGCONNECT_TIMEOUT", "10")
        .kill_on_drop(true);
    if let Some(password) = password {
        command.env("PGPASSWORD", password);
    }
    let output = command.output().await.with_context(|| {
        format!("could not start {program}; install the PostgreSQL client tools")
    })?;
    ensure!(
        output.status.success(),
        "{program} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn directory_empty(path: &Path) -> Result<bool> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => Ok(entries.next().transpose()?.is_none()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn lock_destination(dir: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let path = dir.join("backup.lock");
    reject_symlink(&path)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    // SAFETY: the owned descriptor remains open for the whole backup or restore.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another backup or restore is using {}",
        dir.display()
    );
    Ok(file)
}

/// Writes a backup of this installation into `dir`.
pub async fn backup(pool: &db::DbPool, places: &Places<'_>, dir: &Path) -> Result<()> {
    ensure!(
        !db::cluster::cluster_enabled(pool).await?,
        "codeyard backup covers a single-node installation; this one is in cluster mode"
    );
    let s3 = crate::backend::storage::s3::S3Config::optional_from_env()?.is_some();
    let started = std::time::Instant::now();
    let before = db::backups::list_repositories(pool).await?;
    ensure!(
        before.iter().all(|repo| repo.storage_node_id.is_none()),
        "single-node backup cannot include repositories assigned to storage nodes"
    );
    check_destination(dir, places)?;
    for repo in &before {
        let source = resolved(Path::new(&repo.disk_path))?;
        let root = resolved(dir)?;
        for name in [REPOSITORIES_DIR, mirror::DELETED_DIR, UPLOADS_DIR] {
            let target = resolved(&root.join(name))?;
            ensure!(
                !source.starts_with(&target) && !target.starts_with(&source),
                "backup destination overlaps repository {}",
                repo.id
            );
        }
    }
    private_dir(dir)?;
    let _lock = lock_destination(dir)?;
    let file = match pool {
        db::DbPool::Sqlite(_) => SQLITE_FILE,
        db::DbPool::Postgres(_) => POSTGRES_FILE,
    };
    // The snapshot waits in a partial file until every copy has succeeded, so a failed run
    // leaves the previous complete backup as it was.
    let partial = dir.join(format!("{file}.partial"));
    if partial.exists() {
        std::fs::remove_file(&partial)?;
    }
    let after = match pool {
        db::DbPool::Sqlite(_) => {
            let target = partial.to_str().context("backup path is not UTF-8")?;
            db::backups::copy_sqlite(pool, target).await?;
            db::backups::sqlite_snapshot_repositories(&partial).await?
        }
        db::DbPool::Postgres(_) => {
            let (snapshot_tx, snapshot, repositories) =
                db::backups::postgres_snapshot(pool).await?;
            let snapshot = format!("--snapshot={snapshot}");
            let target = format!("--file={}", partial.display());
            postgres_tool(
                "pg_dump",
                places.database_url,
                &[
                    "--format=custom",
                    "--no-owner",
                    "--no-privileges",
                    "--lock-wait-timeout=30s",
                    &snapshot,
                    &target,
                ],
            )
            .await?;
            snapshot_tx.commit().await?;
            repositories
        }
    };
    private_file(&partial)?;
    std::fs::File::open(&partial)?.sync_all()?;

    // The exact snapshot inventory is authoritative, including rows created and removed while
    // the snapshot ran. Missing required storage refuses publication rather than losing it.
    ensure!(
        after.iter().all(|repo| repo.storage_node_id.is_none()),
        "snapshot contains placed repositories; single-node backup refused"
    );
    for repository in &after {
        let source = resolved(Path::new(&repository.disk_path))?;
        for name in [REPOSITORIES_DIR, mirror::DELETED_DIR, UPLOADS_DIR] {
            let target = resolved(&dir.join(name))?;
            ensure!(
                !source.starts_with(&target) && !target.starts_with(&source),
                "backup destination overlaps repository {}",
                repository.id
            );
        }
    }
    // A repository's directory can sit outside REPOS_PATH, so its parent is a root too.
    let mut roots = BTreeSet::from([places.repos_path.to_path_buf()]);
    for repository in before.iter().chain(&after) {
        if repository.storage_node_id.is_none() {
            if let Some(parent) = Path::new(&repository.disk_path).parent() {
                roots.insert(parent.to_path_buf());
            }
        }
    }
    let mut repositories = Vec::new();
    let mut deleted = std::collections::HashMap::new();
    for repository in &after {
        let mut path = PathBuf::from(&repository.disk_path);
        let name = directory_name(&path)?;
        if !path.is_dir() {
            // Deleted after the snapshot: the newest kept copy, `deleted/<seconds>-<name>`.
            let parent = path.parent().context("repository path has no parent")?;
            let copies = match deleted.entry(parent.to_owned()) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(mirror::deleted_copies(parent)?)
                }
            };
            path = copies.get(&name).cloned().unwrap_or_default();
        }
        ensure!(
            path.is_dir(),
            "snapshot repository {} is missing; backup not published",
            repository.id
        );
        reject_symlink(&path)?;
        repositories.push((name, path));
    }
    let kept = kept_deleted(&roots)?;
    let mut stale = mirror_all(&repositories, &dir.join(REPOSITORIES_DIR)).await?;
    stale.extend(mirror_all(&kept, &dir.join(mirror::DELETED_DIR)).await?);
    println!(
        "Copied {} repositories and {} kept deleted repositories.",
        repositories.len(),
        kept.len()
    );

    if s3 {
        println!("Uploads are in S3 and are not copied; back up the bucket with its own tools.");
    } else {
        let (source, copy) = (places.state_dir.join(UPLOADS_DIR), dir.join(UPLOADS_DIR));
        reject_symlink(&source)?;
        if source.is_dir() {
            let (copied, removed) = tokio::task::spawn_blocking(move || {
                let mut removed = Vec::new();
                copy_tree(&source, &copy, &mut removed).map(|copied| (copied, removed))
            })
            .await??;
            stale.extend(removed);
            println!("Copied {copied} changed upload files.");
        }
    }
    // Offline backups also migrate the legacy standalone signer before copying keys.
    if places.repos_path.join(NODE_KEY).is_file() {
        crate::backend::cluster::identity::release_key(places.state_dir, places.repos_path)?;
    }
    if let Some(key) = crate::backend::cluster::load_existing(places.state_dir)? {
        let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
        writeln!(temporary, "{}", key.secret_hex())?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(dir.join(NODE_KEY))
            .map_err(|error| error.error)?;
        sync_dir(dir)?;
    }
    let signer = places.state_dir.join("release.key");
    if signer.is_file() {
        copy_synced(&signer, &dir.join("release.key"))?;
        private_file(&dir.join("release.key"))?;
    }
    // Publish the new database last. Until this rename the previous backup is still complete:
    // nothing it names has been removed yet.
    ensure!(
        started.elapsed().as_secs() < db::files::RESTORE_WINDOW_SECONDS as u64,
        "backup exceeded the object retention window; refusing publication"
    );
    std::fs::rename(&partial, dir.join(file))?;
    let opposite = dir.join(if file == SQLITE_FILE {
        POSTGRES_FILE
    } else {
        SQLITE_FILE
    });
    match std::fs::remove_file(opposite) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    sync_dir(dir)?;
    println!("Database copied to {}.", dir.join(file).display());
    tokio::task::spawn_blocking(move || remove_stale(&stale)).await??;
    println!(
        "Backup complete. Keep SECRETS_MASTER_KEY and the rest of .env, and the SSH host key, with it."
    );
    Ok(())
}

/// Restores a backup into this installation's empty database, repository directory and
/// state directory, then repairs and rebuilds what the database and Git disagree on.
/// Returns the open database, migrated to this version.
pub async fn restore(places: &Places<'_>, dir: &Path) -> Result<db::DbPool> {
    let s3 = crate::backend::storage::s3::S3Config::optional_from_env()?.is_some();
    let _lock = lock_destination(dir)?;
    let key = std::env::var_os("NODE_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| places.state_dir.join(NODE_KEY));
    let backup_key = dir.join(NODE_KEY);
    if backup_key.is_file() {
        let saved =
            crate::backend::cluster::NodeKey::parse(&std::fs::read_to_string(&backup_key)?)?;
        let existing = if key.exists() || std::env::var_os("NODE_KEY").is_some() {
            crate::backend::cluster::load_existing(places.state_dir)?
        } else {
            None
        };
        ensure!(
            existing.is_none_or(|key| key.public_hex() == saved.public_hex()),
            "existing node identity conflicts with backup; neither key has been overwritten"
        );
    }
    ensure!(
        !(dir.join(SQLITE_FILE).exists() && dir.join(POSTGRES_FILE).exists()),
        "backup contains ambiguous mixed-engine snapshots"
    );
    let source_objects = resolved(&dir.join(UPLOADS_DIR))?;
    let target_objects = resolved(&places.state_dir.join(UPLOADS_DIR))?;
    ensure!(
        !source_objects.starts_with(&target_objects)
            && !target_objects.starts_with(&source_objects),
        "restore object source and destination overlap"
    );
    // Never silently retain an unrelated release signer from an earlier installation.
    let saved_signer = dir.join("release.key");
    let current_signer = places.state_dir.join("release.key");
    if saved_signer.exists() && current_signer.exists() {
        let saved =
            crate::backend::cluster::NodeKey::parse(&std::fs::read_to_string(saved_signer)?)?;
        let current =
            crate::backend::cluster::NodeKey::parse(&std::fs::read_to_string(current_signer)?)?;
        ensure!(
            saved.public_hex() == current.public_hex(),
            "restore would replace a different release signing identity"
        );
    }
    ensure!(
        directory_empty(places.repos_path)?,
        "restore needs an empty REPOS_PATH; {} already holds repositories",
        places.repos_path.display()
    );
    let uploads = places.state_dir.join(UPLOADS_DIR);
    ensure!(
        s3 || directory_empty(&uploads)?,
        "restore needs an empty upload directory; {} is not empty",
        uploads.display()
    );
    let pool = match db::sqlite_file(places.database_url)? {
        Some(path) => {
            let source = dir.join(SQLITE_FILE);
            ensure!(source.is_file(), "{} has no {SQLITE_FILE}", dir.display());
            for suffix in ["", "-wal", "-shm"] {
                let mut name = path.as_os_str().to_owned();
                name.push(suffix);
                ensure!(
                    !Path::new(&name).exists(),
                    "restore needs a new SQLite database; {} exists",
                    Path::new(&name).display()
                );
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let parent = path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let mut partial = tempfile::NamedTempFile::new_in(parent)?;
            std::io::copy(&mut std::fs::File::open(&source)?, partial.as_file_mut())?;
            partial.as_file().sync_all()?;
            partial
                .persist_noclobber(&path)
                .map_err(|error| error.error)?;
            if let Some(parent) = std::path::absolute(&path)?.parent() {
                sync_dir(parent)?;
            }
            db::init_db(places.database_url).await?
        }
        None => {
            let source = dir.join(POSTGRES_FILE);
            ensure!(source.is_file(), "{} has no {POSTGRES_FILE}", dir.display());
            let pool = db::init_db(places.database_url).await?;
            ensure!(
                db::backups::is_empty(&pool).await?,
                "restore needs an empty PostgreSQL database; this one has tables"
            );
            let source = source.to_str().context("backup path is not UTF-8")?;
            postgres_tool(
                "pg_restore",
                places.database_url,
                &[
                    "--exit-on-error",
                    "--single-transaction",
                    "--no-owner",
                    "--no-privileges",
                    source,
                ],
            )
            .await?;
            pool
        }
    };
    db::run_migrations(&pool).await?;
    println!("Database restored.");

    let root = std::path::absolute(places.repos_path)?;
    private_dir(&root)?;
    for (name, path) in repositories_in(&dir.join(REPOSITORIES_DIR))? {
        mirror::mirror(&path, &root.join(&name))
            .await
            .with_context(|| format!("restore repository {name}"))?;
    }
    for (name, path) in repositories_in(&dir.join(mirror::DELETED_DIR))? {
        mirror::mirror(&path, &root.join(mirror::DELETED_DIR).join(&name))
            .await
            .with_context(|| format!("restore deleted repository {name}"))?;
    }
    // The rows name the directory the backup came from; point them at this one. A repository
    // deleted after the snapshot is only under `deleted/`, and the checks below bring it back.
    for repository in db::backups::list_repositories(&pool).await? {
        if repository.storage_node_id.is_some() {
            continue;
        }
        let name = directory_name(Path::new(&repository.disk_path))?;
        let here = root.join(&name);
        let here = here.to_str().context("repository path is not UTF-8")?;
        if repository.disk_path != here {
            db::backups::set_disk_path(&pool, repository.id, here).await?;
        }
    }
    println!("Repositories restored.");

    let copy = dir.join(UPLOADS_DIR);
    if !s3 && copy.is_dir() {
        let copied =
            tokio::task::spawn_blocking(move || copy_tree(&copy, &uploads, &mut Vec::new()))
                .await??;
        println!("Restored {copied} upload files.");
    }
    if backup_key.is_file() && !key.exists() && std::env::var_os("NODE_KEY").is_none() {
        private_dir(key.parent().context("node key has no parent")?)?;
        copy_synced(&dir.join(NODE_KEY), &key)?;
        private_file(&key)?;
    }
    let signer = places.state_dir.join("release.key");
    if dir.join("release.key").is_file() && !signer.exists() {
        private_dir(places.state_dir)?;
        copy_synced(&dir.join("release.key"), &signer)?;
        private_file(&signer)?;
    }
    let problems = verify(&pool, true).await?;
    rebuild(&pool).await?;
    if problems > 0 {
        bail!("restore finished with {problems} problems left; see the list above");
    }
    println!("Restore complete.");
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libpq_urls_carry_no_password() {
        let (url, password) =
            libpq_url("postgres://codeyard:s%40cret@db:5432/codeyard?ssl-mode=require").unwrap();
        assert_eq!(url, "postgres://codeyard@db:5432/codeyard?sslmode=require");
        assert_eq!(password.as_deref(), Some("s@cret"));
        let (url, password) =
            libpq_url("postgres://codeyard@db/codeyard?password=x&statement-cache-capacity=0")
                .unwrap();
        assert_eq!(url, "postgres://codeyard@db/codeyard");
        assert_eq!(password.as_deref(), Some("x"));
    }

    #[test]
    fn copying_a_tree_skips_unchanged_files_and_removes_deleted_ones() {
        let source = tempfile::tempdir().unwrap();
        let copy = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("ab/cd")).unwrap();
        std::fs::create_dir_all(source.path().join(".staging")).unwrap();
        std::fs::write(source.path().join("ab/cd/one"), "1").unwrap();
        std::fs::write(source.path().join(".staging/partial"), "p").unwrap();
        let mut stale = Vec::new();
        assert_eq!(
            copy_tree(source.path(), copy.path(), &mut stale).unwrap(),
            1
        );
        assert_eq!(std::fs::read(copy.path().join("ab/cd/one")).unwrap(), b"1");
        assert!(!copy.path().join(".staging").exists());
        assert_eq!(
            copy_tree(source.path(), copy.path(), &mut stale).unwrap(),
            0
        );
        assert!(stale.is_empty());
        std::fs::remove_file(source.path().join("ab/cd/one")).unwrap();
        copy_tree(source.path(), copy.path(), &mut stale).unwrap();
        // The previous backup keeps the file until the new database is published.
        assert!(copy.path().join("ab/cd/one").exists());
        remove_stale(&stale).unwrap();
        assert!(!copy.path().join("ab/cd/one").exists());
    }

    #[test]
    fn a_backup_directory_may_not_sit_inside_the_data_it_copies() {
        let places = Places {
            database_url: "",
            state_dir: Path::new("/data/state"),
            repos_path: Path::new("/data/repos"),
        };
        assert!(check_destination(Path::new("/data/state/objects/backup"), &places).is_err());
        assert!(check_destination(Path::new("/data/state"), &places).is_err());
        assert!(check_destination(Path::new("/data/repos/backup"), &places).is_err());
        assert!(check_destination(Path::new("/data/backups/one"), &places).is_ok());
        assert!(check_destination(Path::new("/data/backups/../repos/one"), &places).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_backup_directory_reached_through_a_symlink_is_checked_where_it_points() {
        let data = tempfile::tempdir().unwrap();
        let state = data.path().join("state");
        std::fs::create_dir_all(state.join(UPLOADS_DIR)).unwrap();
        std::os::unix::fs::symlink(state.join(UPLOADS_DIR), data.path().join("backups")).unwrap();
        let places = Places {
            database_url: "",
            state_dir: &state,
            repos_path: &data.path().join("repos"),
        };
        assert!(check_destination(&data.path().join("backups/one"), &places).is_err());
    }
}
