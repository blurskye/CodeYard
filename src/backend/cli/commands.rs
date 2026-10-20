use crate::backend::db;
use crate::backend::db::models::AccessLevel;
use crate::backend::db::DbPool;
use crate::backend::repositories::store::repo_store;
use anyhow::Result;

/// Keep untrusted fields on one terminal line without interpreting control bytes.
fn terminal_text(value: &str) -> String {
    let mut text = String::new();
    for character in value.chars() {
        if character.is_control() {
            text.extend(character.escape_default());
        } else {
            text.push(character);
        }
    }
    text
}

/// Copy every table between SQLite and PostgreSQL and print the row counts
pub async fn copy_database_cmd(from: &str, to: &str) -> Result<()> {
    let counts = db::copy::copy_database(from, to).await?;
    println!("{:<40} {:>12}", "Table", "Rows");
    for count in &counts {
        println!("{:<40} {:>12}", count.table, count.rows);
    }
    println!(
        "Copied {} tables. Not copied: repository directories, uploaded files (state dir objects or S3) and key files; move those separately.",
        counts.len()
    );
    Ok(())
}

/// Create a new user
pub async fn create_user_cmd(pool: &DbPool, username: &str, email: Option<&str>) -> Result<()> {
    validate_cli_username(username)?;
    let user = match db::users::transitions::create_cli_user(pool, username, email).await? {
        db::users::transitions::AccountAdmission::User(user) => user,
        db::users::transitions::AccountAdmission::SetupRequired => anyhow::bail!(
            "Complete /setup first, or run codeyard user bootstrap <new-admin> --password-stdin locally"
        ),
        _ => anyhow::bail!("Account creation refused"),
    };
    println!("Created user: {} (id={})", user.username, user.id);
    Ok(())
}

fn validate_cli_username(username: &str) -> Result<()> {
    use crate::backend::identity::credentials;
    credentials::validate_username(username).map_err(|_| anyhow::anyhow!(
        "Username must be 3-1024 bytes, at most 255 characters, and contain only letters, numbers, hyphens and underscores"
    ))?;
    credentials::validate_new_account_name(username)
        .map_err(|_| anyhow::anyhow!("That username is reserved"))
}

pub async fn bootstrap_user_cmd(pool: &DbPool, username: &str, mode: &str) -> Result<()> {
    use crate::backend::identity::{credentials, passwords};
    use std::io::Read;

    validate_cli_username(username)?;
    let mut password = String::new();
    std::io::stdin()
        .take(passwords::MAX_PASSWORD_BYTES as u64 + 3)
        .read_to_string(&mut password)?;
    let password = password.strip_suffix('\n').unwrap_or(&password);
    let password = password.strip_suffix('\r').unwrap_or(password);
    credentials::validate_new_password(password)
        .map_err(|_| anyhow::anyhow!("Password must be 8-1024 bytes"))?;
    let hash = passwords::hash_password(password)
        .await
        .map_err(|_| anyhow::anyhow!("Could not hash administrator password"))?;
    match db::users::transitions::bootstrap_admin(pool, username, &hash, mode).await? {
        db::users::transitions::SetupClaim::Completed(user) => {
            println!(
                "Setup completed. Sign in as {} (id={}).",
                user.username, user.id
            );
            Ok(())
        }
        db::users::transitions::SetupClaim::AlreadyCompleted => {
            anyhow::bail!("Setup is already complete; sign in with an existing administrator")
        }
        db::users::transitions::SetupClaim::ExistingAccounts => {
            anyhow::bail!("That namespace already exists; choose a new administrator username")
        }
    }
}

/// List all users
pub async fn list_users_cmd(pool: &DbPool) -> Result<()> {
    let users = db::users::accounts::list_users(pool).await?;
    if users.is_empty() {
        println!("No users found.");
        return Ok(());
    }

    println!("{:<5} {:<20} {:<30} Admin", "ID", "Username", "Email");
    println!("{}", "-".repeat(70));
    for user in users {
        println!(
            "{:<5} {:<20} {:<30} {}",
            user.id,
            terminal_text(&user.username),
            terminal_text(user.email.as_deref().unwrap_or_default()),
            if user.is_admin { "yes" } else { "no" }
        );
    }
    Ok(())
}

/// Add SSH key for a user
pub async fn add_key_cmd(
    pool: &DbPool,
    username: &str,
    key_name: &str,
    public_key: &str,
) -> Result<()> {
    let user = db::users::accounts::get_user_by_username(pool, username)
        .await?
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", username))?;

    let key = db::users::ssh_keys::add_ssh_key(pool, user.id, key_name, public_key).await?;
    println!(
        "Added SSH key '{}' for user '{}'",
        terminal_text(&key.name),
        terminal_text(username)
    );
    println!("Fingerprint: {}", terminal_text(&key.fingerprint));
    Ok(())
}

/// Add SSH key from file
pub async fn add_key_from_file_cmd(
    pool: &DbPool,
    username: &str,
    key_name: &str,
    key_path: &str,
) -> Result<()> {
    let public_key = std::fs::read_to_string(key_path)?;
    add_key_cmd(pool, username, key_name, &public_key).await
}

/// List SSH keys for a user
pub async fn list_keys_cmd(pool: &DbPool, username: &str) -> Result<()> {
    let user = db::users::accounts::get_user_by_username(pool, username)
        .await?
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", username))?;

    let keys = db::users::ssh_keys::list_user_keys(pool, user.id).await?;
    if keys.is_empty() {
        println!("No SSH keys found for user '{}'.", username);
        return Ok(());
    }

    println!("SSH keys for user '{}':", username);
    println!("{:<5} {:<20} {:<15} Fingerprint", "ID", "Name", "Type");
    println!("{}", "-".repeat(80));
    for key in keys {
        println!(
            "{:<5} {:<20} {:<15} {}",
            key.id,
            terminal_text(&key.name),
            terminal_text(&key.key_type),
            terminal_text(&key.fingerprint)
        );
    }
    Ok(())
}

/// Create a new repository
pub async fn create_repo_cmd(
    pool: &DbPool,
    state_dir: &std::path::Path,
    repos_path: &str,
    owner: &str,
    name: &str,
    description: Option<&str>,
    is_private: bool,
) -> Result<()> {
    let user = db::users::accounts::get_user_by_username(pool, owner)
        .await?
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", owner))?;

    crate::backend::repositories::lifecycle::validate_description(description.unwrap_or(""))?;
    let context =
        crate::backend::cluster::client_context(pool, state_dir, std::path::Path::new(repos_path))
            .await?;
    let created = crate::backend::repositories::lifecycle::provision(
        (pool, context.as_ref()),
        std::path::Path::new(repos_path),
        db::repositories::creation::RepositoryCreationOwner::personal(user.id),
        name,
        description,
        is_private,
        None,
    )
    .await;
    if let Some(context) = &context {
        context.iroh.close().await;
    }
    let repo = created.map_err(anyhow::Error::from)?;

    println!("Created repository: {}/{}", owner, repo.name);
    println!("Path: {}", repo_store(&repo, context.as_ref()).location());
    println!("Private: {}", if is_private { "yes" } else { "no" });
    Ok(())
}

/// List repositories for a user
pub async fn list_repos_cmd(pool: &DbPool, username: &str) -> Result<()> {
    let user = db::users::accounts::get_user_by_username(pool, username)
        .await?
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", username))?;

    let repos = db::repositories::records::list_user_repositories(pool, user.id).await?;
    if repos.is_empty() {
        println!("No repositories found for user '{}'.", username);
        return Ok(());
    }

    println!("Repositories for user '{}':", username);
    println!("{:<5} {:<30} {:<10} Description", "ID", "Name", "Private");
    println!("{}", "-".repeat(80));
    for repo in repos {
        println!(
            "{:<5} {:<30} {:<10} {}",
            repo.id,
            terminal_text(&repo.name),
            if repo.is_private { "yes" } else { "no" },
            terminal_text(repo.description.as_deref().unwrap_or_default())
        );
    }
    Ok(())
}

/// Grant access to a repository
pub async fn grant_access_cmd(
    pool: &DbPool,
    owner: &str,
    repo_name: &str,
    username: &str,
    level: &str,
) -> Result<()> {
    let repo = db::repositories::records::get_repository(pool, owner, repo_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Repository not found: {}/{}", owner, repo_name))?;

    let user = db::users::accounts::get_user_by_username(pool, username)
        .await?
        .ok_or_else(|| anyhow::anyhow!("User not found: {}", username))?;

    let access_level = match level.to_lowercase().as_str() {
        "read" => AccessLevel::Read,
        "write" => AccessLevel::Write,
        "admin" => AccessLevel::Admin,
        _ => {
            return Err(anyhow::anyhow!(
                "Invalid access level: {}. Use: read, write, admin",
                level
            ));
        }
    };

    db::repositories::access::grant_repo_access(pool, user.id, repo.id, access_level).await?;
    println!(
        "Granted {} access to user '{}' for repository '{}/{}'",
        level, username, owner, repo_name
    );
    Ok(())
}

/// Show repository info
pub async fn repo_info_cmd(
    pool: &DbPool,
    state_dir: &std::path::Path,
    repos_path: &std::path::Path,
    owner: &str,
    repo_name: &str,
) -> Result<()> {
    let repo = db::repositories::records::get_repository(pool, owner, repo_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Repository not found: {}/{}", owner, repo_name))?;

    println!("Repository: {}/{}", owner, repo_name);
    println!("ID: {}", repo.id);
    println!("Private: {}", if repo.is_private { "yes" } else { "no" });
    println!("Default branch: {}", terminal_text(&repo.default_branch));
    let context = crate::backend::cluster::client_context(pool, state_dir, repos_path).await?;
    let store = repo_store(&repo, context.as_ref());
    println!("Disk path: {}", terminal_text(store.location()));
    println!(
        "Description: {}",
        terminal_text(repo.description.as_deref().unwrap_or_default())
    );
    println!("Created: {}", repo.created_at);

    if store.exists().await {
        match tokio::try_join!(store.info(), store.commit_count()) {
            Ok((info, commit_count)) => {
                println!("\nGit Info:");
                println!("  Branches: {}", terminal_text(&info.branches.join(", ")));
                println!("  Commits: {}", commit_count);
            }
            Err(e) => {
                println!(
                    "\nGit Info: Error reading - {}",
                    terminal_text(&e.to_string())
                );
            }
        }
    }
    if let Some(context) = &context {
        context.iroh.close().await;
    }

    Ok(())
}
