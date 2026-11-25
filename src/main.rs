use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tablica", version, about = "TRMNL e-ink backend")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Path to the TOML config file.
        #[arg(long)]
        config: PathBuf,
        /// Directory holding secret files.
        #[arg(long, default_value = "/run/secrets")]
        secrets_dir: PathBuf,
        /// Directory holding templates (overrides the built-in ones).
        #[arg(long)]
        templates: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve {
            config,
            secrets_dir,
            templates: _,
        } => {
            let cfg = tablica::config::load(&config, &secrets_dir)
                .with_context(|| format!("loading {}", config.display()))?;
            let names: Vec<&str> = cfg.devices.iter().map(|d| d.name.as_str()).collect();
            println!("loaded devices: {}", names.join(", "));
        }
    }
    Ok(())
}
