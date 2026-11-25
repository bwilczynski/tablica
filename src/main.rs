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
    /// Record raw responses from an external source as test fixtures.
    Capture {
        #[command(subcommand)]
        source: CaptureSource,
    },
}

#[derive(Subcommand)]
enum CaptureSource {
    /// Log in to Librus once per kid and save the login chain and two timetable weeks.
    Librus {
        /// Path to the TOML config file.
        #[arg(long)]
        config: PathBuf,
        /// Directory holding secret files.
        #[arg(long, default_value = "/run/secrets")]
        secrets_dir: PathBuf,
        /// Output directory (one `kid{N}` subdirectory per kid).
        #[arg(long)]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        Command::Capture {
            source:
                CaptureSource::Librus {
                    config,
                    secrets_dir,
                    out,
                },
        } => {
            let cfg = tablica::config::load(&config, &secrets_dir)
                .with_context(|| format!("loading {}", config.display()))?;
            tablica::sources::librus::capture(&cfg, &out).await?;
        }
    }
    Ok(())
}
