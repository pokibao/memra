//! # memra-server
//!
//! Memra MCP server — stdio + HTTP transport.
//! Phase 1: search_rules, search_checkpoints, get_context are live.
//! Write tools remain stubs until Phase 2.

#![allow(dead_code)] // Phase 1: stub param fields used by rmcp serde macros

use std::path::PathBuf;

use cli::{Command, ServeTransport};
use config::AppConfig;
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

mod audit;
mod cli;
mod config;
mod service;
mod transport;

/// Resolve the SQLite database path from environment or default.
fn resolve_db_path(project: Option<&str>) -> Option<PathBuf> {
    // 1. Explicit env var
    if let Ok(path) = std::env::var("MCP_MEMORY_STORAGE_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
        tracing::warn!(
            "MCP_MEMORY_STORAGE_PATH set but file not found: {}",
            p.display()
        );
    }

    // 2. Project-specific default path
    let project_id = project
        .map(str::to_string)
        .or_else(|| std::env::var("MCP_MEMORY_PROJECT_ID").ok())
        .unwrap_or_else(|| "memra".into());
    let home = dirs_or_home();
    let default_path = home
        .join(".memra")
        .join("projects")
        .join(&project_id)
        .join(".storage")
        .join("memory_anchor.sqlite3");

    if default_path.exists() {
        return Some(default_path);
    }

    tracing::warn!(
        "No SQLite database found at default path: {}",
        default_path.display()
    );
    None
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ma_server=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let command = cli::Cli::parse_args().into_command();
    let config = AppConfig::load()?;
    tracing::debug!(
        config_path = %config::resolve_config_path().display(),
        api_key_count = config.auth.api_keys.len(),
        server_bind = %config.server.bind,
        server_port = config.server.port,
        "Loaded Memra config"
    );

    match command {
        Command::Serve(args) => match args.transport() {
            ServeTransport::Stdio => serve_stdio(args.project()).await,
            ServeTransport::Http => {
                let service = build_service(args.project());
                transport::http::serve_http(&args, &config, service).await
            }
            ServeTransport::Daemon => cli::phase4::serve_cli_daemon(args.project(), args.socket()),
        },
        Command::Admin(args) => {
            let command = args.into_command();
            cli::admin::run_admin(command)
        }
        Command::Init(args) => cli::phase4::run_init(&args),
        Command::Setup(args) => cli::phase4::run_setup(&args),
        Command::Doctor(args) => cli::phase4::run_doctor(&args),
        Command::Search(args) => cli::phase4::run_search(&args),
        Command::Bench(args) => cli::phase4::run_bench(&args),
        Command::Recall(args) => cli::phase4::run_recall(&args),
        Command::Add(args) => cli::phase4::run_add(&args),
        Command::Remember(args) => cli::phase4::run_remember(&args),
        Command::Feedback(args) => cli::phase4::run_feedback(&args),
        Command::Confirm(args) => cli::phase4::run_confirm(&args),
        Command::FeedbackDue(args) => cli::phase4::run_feedback_due(&args),
        Command::ExperienceProof(args) => cli::phase4::run_experience_proof(&args),
        Command::Timeline(args) => cli::phase4::run_timeline(&args),
        Command::Packet(args) => cli::phase4::run_packet(&args),
        Command::Stats(args) => cli::phase4::run_stats(&args),
        Command::Pulse(args) => cli::phase4::run_pulse(&args),
        Command::Grade(args) => cli::phase4::run_grade(&args),
        Command::Llm(args) => cli::phase4::run_llm(&args),
        Command::Research(args) => cli::phase4::run_research(&args),
        Command::Background(args) => cli::phase4::run_background(&args),
        Command::MineConvos(args) => cli::phase4::run_mine_convos(&args),
        Command::Context(args) => cli::phase4::run_context(&args),
        Command::Checkpoint(args) => cli::phase4::run_checkpoint(&args),
        Command::Resume(args) => cli::phase4::run_resume(&args),
        Command::Consolidate(args) => cli::phase4::run_consolidate(&args),
        Command::Wiki(args) => cli::phase4::run_wiki(&args),
        Command::Ingest(args) => cli::phase4::run_ingest(&args),
        Command::Hook(args) => cli::phase4::run_hook(&args),
        Command::Dream(args) => cli::phase4::run_dream(&args),
        Command::ReviewQueue(args) => cli::phase4::run_review_queue(&args),
        Command::ReviewResolve(args) => cli::phase4::run_review_resolve(&args),
        Command::BatchWrite(args) => cli::phase4::run_batch_write(&args),
        Command::Demo(args) => cli::phase4::run_demo(&args),
        Command::Recover(args) => cli::phase4::run_recover(&args),
    }
}

async fn serve_stdio(project: Option<&str>) -> anyhow::Result<()> {
    tracing::info!("Memra v6.0-alpha.1 starting (stdio)");

    let service = build_service(project);
    let server = service.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}

fn build_service(project: Option<&str>) -> service::MemraService {
    let project_id = project.unwrap_or("memra").to_string();
    match resolve_db_path(project) {
        Some(path) => {
            tracing::info!("Opening database: {}", path.display());
            match service::MemraService::with_db(path, project_id) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to open database: {e}");
                    tracing::info!("Falling back to stub mode");
                    service::MemraService::stub()
                }
            }
        }
        None => {
            tracing::info!("No database found, running in stub mode");
            service::MemraService::stub()
        }
    }
}
