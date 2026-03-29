
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use oci_client::secrets::RegistryAuth;

use crate::registry::client::{
    auth_from_file, containers_auth_path, encode_auth_token, AuthEntry, AuthFile,
};

pub struct LoginArgs {
    /// Registry reference, e.g. `registry.example.com` or `registry.example.com/private`.
    pub registry: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_stdin: bool,
    pub authfile: Option<PathBuf>,
}

pub async fn run(args: LoginArgs) -> Result<()> {
    let registry = args.registry.trim().to_string();
    if registry.is_empty() {
        bail!("registry must not be empty");
    }

    let auth_path = args
        .authfile
        .or_else(containers_auth_path)
        .context("could not determine auth file path; set REGISTRY_AUTH_FILE")?;

    let username = match args.username {
        Some(u) if !u.is_empty() => u,
        _ => {
            // If credentials already exist, offer to reuse the username.
            let existing_user = auth_from_file(&auth_path, &registry).and_then(|a| match a {
                RegistryAuth::Basic(u, _) => Some(u),
                _ => None,
            });

            if let Some(ref existing) = existing_user {
                eprint!("Username [{}]: ", existing);
                io::stderr().flush().ok();
                let mut line = String::new();
                io::stdin().read_line(&mut line)?;
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    existing.clone()
                } else {
                    trimmed.to_string()
                }
            } else {
                prompt_username()?
            }
        }
    };

    let password = if args.password_stdin {
        let mut p = String::new();
        io::stdin().read_line(&mut p)?;
        p.trim().to_string()
    } else if let Some(p) = args.password {
        p
    } else {
        rpassword::prompt_password("Password: ").context("reading password")?
    };

    if password.is_empty() {
        bail!("password must not be empty");
    }

    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating auth file directory: {}", parent.display()))?;
    }

    let mut file: AuthFile = std::fs::read_to_string(&auth_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    file.auths.insert(
        registry.clone(),
        AuthEntry {
            auth: encode_auth_token(&username, &password),
            ..Default::default()
        },
    );

    let json = serde_json::to_string_pretty(&file).context("serialising auth file")?;
    std::fs::write(&auth_path, &json)
        .with_context(|| format!("writing auth file: {}", auth_path.display()))?;

    println!("Login Succeeded!");
    eprintln!("  registry : {}", registry);
    eprintln!("  authfile : {}", auth_path.display());

    Ok(())
}

fn prompt_username() -> Result<String> {
    eprint!("Username: ");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let u = line.trim().to_string();
    if u.is_empty() {
        bail!("username must not be empty");
    }
    Ok(u)
}
