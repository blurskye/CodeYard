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

use crate::backend::{db, git::mirror};
use anyhow::{bail, ensure, Context, Result};
use std::collections::BTreeSet;
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

fn uploads_in_s3() -> bool {
    ["S3_ENDPOINT", "S3_BUCKET"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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
    std::fs::copy(from, to)?;
    std::fs::File::open(to)?.sync_all()?;
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
        let path = entry?.path();
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
    for (name, path) in repositories {
        mirror::mirror(path, &copy.join(name))
            .await
            .with_context(|| format!("copy repository {name}"))?;
    }
    Ok(repositories_in(copy)?
        .into_iter()
        .filter(|(name, _)| !repositories.iter().any(|(kept, _)| kept == name))
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
        dir.join(REPOSITORIES_DIR),
        dir.join(mirror::DELETED_DIR),
        dir.join(UPLOADS_DIR),
    ];
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
fn copy_tree(source: &Path, copy: &Path, stale: &mut Vec<PathBuf>) -> Result<usize> {
    std::fs::create_dir_all(copy)?;
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
            let mut temporary_name = std::ffi::OsString::from(".");
            temporary_name.push(&name);
            temporary_name.push(".partial");
            let temporary = copy.join(temporary_name);
            std::fs::copy(&from, &temporary)?;
            let written = std::fs::File::options().write(true).open(&temporary)?;
            written.set_modified(modified)?;
            written.sync_all()?;
            std::fs::rename(&temporary, &to)?;
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
    url.set_password(None)
        .map_err(|_| anyhow::anyhow!("invalid DATABASE_URL"))?;
    if query.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(query);
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

/// Writes a backup of this installation into `dir`.
pub async fn backup(pool: &db::DbPool, places: &Places<'_>, dir: &Path) -> Result<()> {
    ensure!(
        !db::cluster::cluster_enabled(pool).await?,
        "codeyard backup covers a single-node installation; this one is in cluster mode"
    );
    check_destination(dir, places)?;
    private_dir(dir)?;
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
    // The rows read before the snapshot include the repositories deleted while it runs. The
    // snapshot still names them, and their kept copies sit under `deleted/` beside them.
    let before = db::backups::list_repositories(pool).await?;
    match pool {
        db::DbPool::Sqlite(_) => {
            let target = partial.to_str().context("backup path is not UTF-8")?;
            db::backups::copy_sqlite(pool, target).await?;
        }
        db::DbPool::Postgres(_) => {
            let target = format!("--file={}", partial.display());
            postgres_tool(
                "pg_dump",
                places.database_url,
                &["--format=custom", "--no-owner", "--no-privileges", &target],
            )
            .await?;
        }
    }
    private_file(&partial)?;
    std::fs::File::open(&partial)?.sync_all()?;

    // Git after the database, so every repository state the database names is in the copy. The
    // rows are read again after the snapshot, so a repository created meanwhile is copied too.
    let after = db::backups::list_repositories(pool).await?;
    // A repository's directory can sit outside REPOS_PATH, so its parent is a root too.
    let mut roots = BTreeSet::from([places.repos_path.to_path_buf()]);
    for repository in before.iter().chain(&after) {
        if repository.storage_node_id.is_none() {
            if let Some(parent) = Path::new(&repository.disk_path).parent() {
                roots.insert(parent.to_path_buf());
            }
        }
    }
    // A repository deleted since the snapshot is copied from its place while its retirement
    // has not moved it yet. Once moved, it is among the kept deleted ones read below, and a
    // move in between fails its copy, so the backup is not published without it.
    let live: BTreeSet<&str> = after.iter().map(|row| row.disk_path.as_str()).collect();
    let deleted = before
        .iter()
        .filter(|row| !live.contains(row.disk_path.as_str()));
    let mut repositories = Vec::new();
    for repository in after
        .iter()
        .chain(deleted)
        .filter(|row| row.storage_node_id.is_none())
    {
        let path = PathBuf::from(&repository.disk_path);
        if live.contains(repository.disk_path.as_str()) || path.is_dir() {
            repositories.push((directory_name(&path)?, path));
        }
    }
    let kept = kept_deleted(&roots)?;
    let mut stale = mirror_all(&repositories, &dir.join(REPOSITORIES_DIR)).await?;
    stale.extend(mirror_all(&kept, &dir.join(mirror::DELETED_DIR)).await?);
    println!(
        "Copied {} repositories and {} kept deleted repositories.",
        repositories.len(),
        kept.len()
    );

    if uploads_in_s3() {
        println!("Uploads are in S3 and are not copied; back up the bucket with its own tools.");
    } else {
        let (source, copy) = (places.state_dir.join(UPLOADS_DIR), dir.join(UPLOADS_DIR));
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
    for name in [NODE_KEY, "release.key"] {
        let key = places.state_dir.join(name);
        if key.is_file() {
            copy_synced(&key, &dir.join(name))?;
            private_file(&dir.join(name))?;
        }
    }
    // Publish the new database last. Until this rename the previous backup is still complete:
    // nothing it names has been removed yet.
    std::fs::rename(&partial, dir.join(file))?;
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
    ensure!(
        repositories_in(places.repos_path)?.is_empty()
            && repositories_in(&places.repos_path.join(mirror::DELETED_DIR))?.is_empty(),
        "restore needs an empty REPOS_PATH; {} already holds repositories",
        places.repos_path.display()
    );
    let uploads = places.state_dir.join(UPLOADS_DIR);
    ensure!(
        uploads_in_s3()
            || std::fs::read_dir(&uploads)
                .ok()
                .is_none_or(|mut entries| entries.next().is_none()),
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
            let partial = path.with_extension("restoring");
            std::fs::copy(&source, &partial)?;
            private_file(&partial)?;
            std::fs::File::open(&partial)?.sync_all()?;
            std::fs::rename(&partial, &path)?;
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
    if !uploads_in_s3() && copy.is_dir() {
        let copied =
            tokio::task::spawn_blocking(move || copy_tree(&copy, &uploads, &mut Vec::new()))
                .await??;
        println!("Restored {copied} upload files.");
    }
    for name in [NODE_KEY, "release.key"] {
        let key = places.state_dir.join(name);
        if dir.join(name).is_file() && !key.exists() {
            private_dir(places.state_dir)?;
            copy_synced(&dir.join(name), &key)?;
            private_file(&key)?;
        }
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
