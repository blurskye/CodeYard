//! Repack only verified, reachable objects in a fresh quarantine repository.
use crate::imports::archive::refs;
use crate::imports::archive::ArchiveSource;
use crate::imports::MAX_IMPORT_PACK_BYTES;
use anyhow::ensure;
use anyhow::Result;
use codeyard_git::repository::process;
use std::collections::HashMap;

pub struct ArchiveGit {
    pub refs: HashMap<String, String>,
    pub default_branch: Option<String>,
    pub pack: Vec<u8>,
}

pub async fn prepare(source: &ArchiveSource) -> Result<ArchiveGit> {
    let (refs, default_branch) = refs(source)?;
    ensure!(
        source.git_files.len() <= 100_000,
        "Too many migration Git entries"
    );
    let mut bytes = 0;
    for data in source.git_files.values() {
        ensure!(
            data.len() <= MAX_IMPORT_PACK_BYTES - bytes,
            "Migration Git data exceeds its size limit"
        );
        bytes += data.len();
    }
    let mut packs = Vec::new();
    let mut loose = Vec::new();
    for (path, data) in &source.git_files {
        if path.starts_with("objects/pack/") {
            packs.push(data.as_slice());
        } else if path.starts_with("objects/") {
            loose.push((path.clone(), data.clone()));
        }
    }
    ensure!(packs.len() <= 64, "Too many migration packs");
    let temporary = tokio::task::spawn_blocking(move || -> Result<_> {
        let directory = tempfile::tempdir()?;
        git2::Repository::init_bare(directory.path())?;
        for (path, bytes) in loose {
            ensure!(
                crate::imports::archive::safe_path(std::path::Path::new(&path))
                    && crate::imports::archive::git_object_path(&path),
                "Invalid migration object"
            );
            let path = directory.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, bytes)?;
        }
        Ok(directory)
    })
    .await??;
    for pack in packs {
        let mut command = process::git_command()?;
        command
            .arg("--git-dir")
            .arg(temporary.path())
            // Connectivity is checked by fsck only after every independent pack is indexed.
            .args(["index-pack", "--stdin"]);
        let (status, _) = process::buffered(command, pack, 65536, process::RPC_TIMEOUT).await?;
        ensure!(status.success(), "Invalid migration pack");
    }
    if refs.is_empty() {
        return Ok(ArchiveGit {
            refs,
            default_branch,
            pack: Vec::new(),
        });
    }
    let mut input = String::from("start\n");
    for (name, sha) in &refs {
        input.push_str(&format!("create {name} {sha}\n"));
    }
    input.push_str("prepare\ncommit\n");
    let mut command = process::git_command()?;
    command
        .arg("--git-dir")
        .arg(temporary.path())
        .args(["update-ref", "--stdin"]);
    let (status, _) =
        process::buffered(command, input.as_bytes(), 65536, process::RPC_TIMEOUT).await?;
    ensure!(
        status.success(),
        "Migration references missing or invalid objects"
    );
    if let Some(branch) = &default_branch {
        if refs.contains_key(&format!("refs/heads/{branch}")) {
            tokio::fs::write(
                temporary.path().join("HEAD"),
                format!("ref: refs/heads/{branch}\n"),
            )
            .await?;
        }
    }
    let mut command = process::git_command()?;
    command
        .arg("--git-dir")
        .arg(temporary.path())
        .args(["fsck", "--strict", "--no-dangling"]);
    let (status, _) = process::buffered(command, &[], 1024 * 1024, process::RPC_TIMEOUT).await?;
    ensure!(status.success(), "Invalid migration object graph");
    let mut tips: Vec<_> = refs.values().cloned().collect();
    tips.sort_unstable();
    tips.dedup();
    let input = format!("{}\n", tips.join("\n"));
    let mut command = process::git_command()?;
    command
        .arg("--git-dir")
        .arg(temporary.path())
        .args(["pack-objects", "--revs", "--stdout"]);
    let (status, pack) = process::buffered(
        command,
        input.as_bytes(),
        MAX_IMPORT_PACK_BYTES,
        process::RPC_TIMEOUT,
    )
    .await?;
    ensure!(status.success(), "Migration Git repack failed");
    Ok(ArchiveGit {
        refs,
        default_branch,
        pack,
    })
}
