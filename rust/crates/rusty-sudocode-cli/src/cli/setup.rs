//! Runtime choice for a fresh CLI, sharing the existing frontend service owner.

use std::error::Error;
use std::io::IsTerminal;
use std::path::PathBuf;

use clap::ValueEnum;
use runtime::nexus_mailbox::NodeRecord;
use runtime::{FsBackend, StdFsBackend};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::args::CliOutputFormat;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeMode {
    Local,
    Shared,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preference {
    #[serde(rename = "schemaVersion")]
    version: u8,
    mode: RuntimeMode,
}

fn path() -> PathBuf {
    runtime::config::default_config_home().join("runtime-profile.json")
}

fn preference() -> Result<Option<Preference>> {
    let bytes = match std::fs::read(path()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let preference: Preference = serde_json::from_slice(&bytes)?;
    if preference.version != 1 {
        return Err("Unsupported runtime profile version".into());
    }
    Ok(Some(preference))
}

fn choose() -> Result<Option<RuntimeMode>> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("Noninteractive setup requires --mode local or --mode shared".into());
    }
    let choice = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Choose how to run scode")
        .items(&[
            "Single agent (skip Nexus setup)",
            "Multi-agent (reuse configured Box; otherwise prepare local Nexus)",
            "Decide later",
        ])
        .default(0)
        .interact_opt()?;
    Ok(match choice {
        Some(0) => Some(RuntimeMode::Local),
        Some(1) => Some(RuntimeMode::Shared),
        _ => None,
    })
}

fn activate(mode: RuntimeMode) -> Result<Value> {
    if mode == RuntimeMode::Shared {
        if let Some(record) = NodeRecord::load()? {
            let service = service_host::ensure_local_box(record.endpoint())
                .map_err(|error| error as Box<dyn Error>)?;
            return Ok(json!({"mode": mode, "service": service,
                "box": {"source": "node_record", "endpoint": record.endpoint(),
                    "agentAuthentication": "pending"}, "domainJoinAccepted": false}));
        }
        let service =
            service_host::start_local_services().map_err(|error| error as Box<dyn Error>)?;
        return Ok(json!({"mode": mode, "service": service, "box": null,
            "domainJoinAccepted": false}));
    }
    Ok(json!({"mode": mode, "service": null, "box": null, "domainJoinAccepted": false}))
}

pub(crate) fn run(mode: Option<RuntimeMode>, check: bool, format: CliOutputFormat) -> Result<()> {
    let previous = preference()?;
    let output = if check {
        let configured_box = NodeRecord::load()?.map(|record| {
            json!({"source": "node_record", "endpoint": record.endpoint(),
                "agentAuthentication": "pending"})
        });
        json!({"mode": previous.map(|preference| preference.mode),
            "box": configured_box,
            "services": service_host::inspect_local_services().map_err(|error| error as Box<dyn Error>)?,
            "domainJoinAccepted": false})
    } else {
        let selected = match mode {
            Some(mode) => Some(mode),
            None => choose()?,
        };
        let Some(mode) = selected else {
            return Ok(());
        };
        let output = activate(mode)?;
        let path = path();
        std::fs::create_dir_all(path.parent().ok_or("missing configuration directory")?)?;
        StdFsBackend.write_atomic(
            &path.to_string_lossy(),
            &serde_json::to_vec(&Preference { version: 1, mode })?,
        )?;
        output
    };
    if format == CliOutputFormat::Json {
        println!("{}", serde_json::to_string(&output)?);
    } else if check {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Runtime preference saved.");
        if let Some(endpoint) = output["box"]["endpoint"].as_str() {
            println!("Using configured Box at {endpoint}. Agent credentials are checked when the session connects.");
        } else if output["mode"] == "shared" {
            println!("Local services are ready. Collaboration enrollment is a separate step.");
        }
    }
    Ok(())
}

/// Existing installations retain their current behavior until explicit setup.
/// Protocol/headless callers never read a prompt or provision background services.
pub(crate) fn startup_prompt() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    if let Some(preference) = preference()? {
        if preference.mode == RuntimeMode::Shared {
            activate(preference.mode)?;
        }
    } else if !runtime::config::default_config_home().exists() {
        run(None, false, CliOutputFormat::Text)?;
    }
    Ok(())
}
