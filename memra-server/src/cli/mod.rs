use std::ffi::OsString;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::{DEFAULT_HTTP_BIND, DEFAULT_HTTP_PORT};

pub mod admin;
pub mod phase4;

pub const DEFAULT_MIN_SCORE: f64 = 0.5;

#[derive(Debug, Clone, PartialEq, Parser)]
#[command(
    name = "ma",
    bin_name = "ma",
    about = "Memra server and administration CLI",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

impl Cli {
    pub fn parse_args() -> Self {
        Self::parse()
    }

    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(args)
    }

    pub fn into_command(self) -> Command {
        match self.command {
            Some(command) => command,
            None => Command::Serve(ServeArgs::default()),
        }
    }
}

// Clap command enums are short-lived parse structures. Boxing the batch-write
// variant would reduce enum size but ripple through every command dispatcher
// and parser test for no runtime win in the CLI path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Command {
    #[command(about = "Serve Memra over stdio or HTTP", alias = "up")]
    Serve(ServeArgs),
    #[command(about = "Manage Memra server administration tasks")]
    Admin(AdminArgs),
    #[command(about = "Initialize project storage and MCP client configuration")]
    Init(InitArgs),
    #[command(about = "Configure Claude Code MCP integration")]
    Setup(SetupArgs),
    #[command(about = "Run health checks for the configured project and clients")]
    Doctor(DoctorArgs),
    #[command(about = "Search memories through the Rust retrieval engine")]
    Search(SearchArgs),
    #[command(about = "Run local benchmark helpers")]
    Bench(BenchArgs),
    #[command(about = "Recall memories through the developer-style Rust API")]
    Recall(RecallArgs),
    #[command(about = "Add a memory through the Rust writer")]
    Add(AddArgs),
    #[command(about = "Remember a developer-scoped memory through the Rust writer")]
    Remember(RememberArgs),
    #[command(about = "Record memory feedback through the Rust writer")]
    Feedback(FeedbackArgs),
    #[command(about = "Confirm AI-origin candidate memories for strengthening")]
    Confirm(ConfirmArgs),
    #[command(
        name = "feedback-due",
        about = "Show experience and dream feedback due"
    )]
    FeedbackDue(FeedbackDueArgs),
    #[command(about = "Build experience proof artifacts through the Rust substrate")]
    ExperienceProof(ExperienceProofArgs),
    #[command(about = "Show a knowledge-graph entity timeline")]
    Timeline(TimelineArgs),
    #[command(about = "Emit a Project Memory Packet for one project")]
    Packet(PacketArgs),
    #[command(
        about = "Show storage and usage statistics for the project",
        alias = "status"
    )]
    Stats(StatsArgs),
    #[command(about = "Render a short operator pulse for the project")]
    Pulse(PulseArgs),
    #[command(about = "Grade the latest nightly consolidation outcome")]
    Grade(GradeArgs),
    #[command(about = "Run Rust LLM provider diagnostics and smoke checks")]
    Llm(LlmArgs),
    #[command(about = "Manage Rust-owned autoresearch worker state and jobs")]
    Research(ResearchArgs),
    #[command(about = "Run Rust-owned background worker supervisor cycles")]
    Background(BackgroundArgs),
    #[command(
        name = "mine-convos",
        about = "Parse conversation exports into exchange-pair chunks"
    )]
    MineConvos(MineConvosArgs),
    #[command(about = "Load a Rust wake context snapshot")]
    Context(ContextArgs),
    #[command(about = "Save a task checkpoint through the Rust writer")]
    Checkpoint(CheckpointArgs),
    #[command(about = "Return a project-resume evidence card from active checkpoints")]
    Resume(ResumeArgs),
    #[command(about = "Run Rust-owned nightly consolidation helpers")]
    Consolidate(ConsolidateArgs),
    #[command(about = "Export verified facts to a Markdown wiki mirror")]
    Wiki(WikiArgs),
    #[command(about = "Ingest static Markdown roots into verified facts")]
    Ingest(IngestArgs),
    #[command(about = "Run Memra hook entrypoints")]
    Hook(HookArgs),
    #[command(about = "Review dream shadow output candidates")]
    Dream(DreamArgs),
    #[command(about = "List the top pending memory review candidates")]
    ReviewQueue(ReviewQueueArgs),
    #[command(about = "Resolve one pending memory review candidate")]
    ReviewResolve(ReviewResolveArgs),
    #[command(about = "Run narrow Rust-owned batch memory mutations")]
    BatchWrite(BatchWriteArgs),
    #[command(about = "Seed a demo project database with sample memories")]
    Demo(DemoArgs),
    #[command(about = "Rebuild the project database from cold-storage JSONL archives")]
    Recover(RecoverArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ServeArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long, conflicts_with = "http", help = "Serve MCP over stdio")]
    stdio: bool,

    #[arg(long, conflicts_with_all = ["stdio", "daemon"], help = "Serve MCP over HTTP")]
    http: bool,

    #[arg(
        long,
        conflicts_with_all = ["stdio", "http"],
        help = "Serve local CLI forwarding over a Unix domain socket"
    )]
    daemon: bool,

    #[arg(long)]
    bind: Option<IpAddr>,

    #[arg(long)]
    port: Option<u16>,

    #[arg(long = "tls-cert", value_name = "PATH")]
    tls_cert: Option<PathBuf>,

    #[arg(long = "tls-key", value_name = "PATH")]
    tls_key: Option<PathBuf>,

    #[arg(long = "socket", value_name = "PATH", requires = "daemon")]
    socket: Option<PathBuf>,
}

impl Default for ServeArgs {
    fn default() -> Self {
        Self {
            project: "memra".to_string(),
            stdio: false,
            http: false,
            daemon: false,
            bind: None,
            port: None,
            tls_cert: None,
            tls_key: None,
            socket: None,
        }
    }
}

impl ServeArgs {
    pub fn transport(&self) -> ServeTransport {
        if self.daemon {
            ServeTransport::Daemon
        } else if self.http {
            ServeTransport::Http
        } else {
            ServeTransport::Stdio
        }
    }

    pub fn bind(&self) -> IpAddr {
        self.bind.unwrap_or(DEFAULT_HTTP_BIND)
    }

    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_HTTP_PORT)
    }

    pub fn project(&self) -> Option<&str> {
        Some(&self.project)
    }

    pub fn bind_override(&self) -> Option<IpAddr> {
        self.bind
    }

    pub fn port_override(&self) -> Option<u16> {
        self.port
    }

    pub fn tls_cert(&self) -> Option<&Path> {
        self.tls_cert.as_deref()
    }

    pub fn tls_key(&self) -> Option<&Path> {
        self.tls_key.as_deref()
    }

    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct InitArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long)]
    force: bool,

    #[arg(long = "dry-run")]
    dry_run: bool,
}

impl InitArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn force(&self) -> bool {
        self.force
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct SetupArgs {
    #[arg(
        long,
        short = 'p',
        value_name = "PROJECT",
        default_value = "my-project"
    )]
    project: String,

    #[arg(long = "install-precompact-hook")]
    install_precompact_hook: bool,

    #[arg(long = "install-sessionstart-hook")]
    install_sessionstart_hook: bool,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "claude-config", value_name = "PATH", hide = true)]
    claude_config_path: Option<PathBuf>,

    #[arg(long = "claude-settings", value_name = "PATH", hide = true)]
    claude_settings_path: Option<PathBuf>,

    #[arg(long = "bin-path", value_name = "PATH", hide = true)]
    bin_path: Option<PathBuf>,
}

impl SetupArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn install_precompact_hook(&self) -> bool {
        self.install_precompact_hook
    }

    pub fn install_sessionstart_hook(&self) -> bool {
        self.install_sessionstart_hook
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn claude_config_path(&self) -> Option<&Path> {
        self.claude_config_path.as_deref()
    }

    pub fn claude_settings_path(&self) -> Option<&Path> {
        self.claude_settings_path.as_deref()
    }

    pub fn bin_path(&self) -> Option<&Path> {
        self.bin_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DoctorArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long)]
    json: bool,

    #[arg(long)]
    full: bool,
}

impl DoctorArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn full(&self) -> bool {
        self.full
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct SearchArgs {
    #[arg(value_name = "QUERY")]
    query: String,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "limit", short = 'n', default_value_t = 5)]
    limit: usize,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long)]
    layer: Option<String>,

    #[arg(long)]
    category: Option<String>,

    #[arg(long = "mode")]
    search_mode: Option<String>,

    #[arg(long = "min-score", default_value_t = DEFAULT_MIN_SCORE)]
    min_score: f64,

    #[arg(long = "include-constitution")]
    include_constitution: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,

    #[arg(long = "no-forward", hide = true)]
    no_forward: bool,

    #[arg(long = "forward-socket", value_name = "PATH", hide = true)]
    forward_socket: Option<PathBuf>,
}

impl SearchArgs {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn layer(&self) -> Option<&str> {
        self.layer.as_deref()
    }

    pub fn category(&self) -> Option<&str> {
        self.category.as_deref()
    }

    pub fn search_mode(&self) -> Option<&str> {
        self.search_mode.as_deref()
    }

    pub fn min_score(&self) -> f64 {
        self.min_score
    }

    pub fn include_constitution(&self) -> bool {
        self.include_constitution
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn no_forward(&self) -> bool {
        self.no_forward
    }

    pub fn forward_socket(&self) -> Option<&Path> {
        self.forward_socket.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BenchArgs {
    #[command(subcommand)]
    command: BenchCommand,
}

impl BenchArgs {
    pub fn command(&self) -> &BenchCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum BenchCommand {
    #[command(about = "Run an in-process retrieval benchmark over a TSV query file")]
    Retrieval(BenchRetrievalArgs),
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BenchRetrievalArgs {
    #[arg(
        long,
        short = 'p',
        value_name = "PROJECT",
        default_value = "memra"
    )]
    project: String,

    #[arg(long = "query-file", value_name = "PATH")]
    query_file: PathBuf,

    #[arg(long = "limit", short = 'n', default_value_t = 5)]
    limit: usize,

    #[arg(long = "min-score", default_value_t = DEFAULT_MIN_SCORE)]
    min_score: f64,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,

    #[arg(long, short = 'j')]
    json: bool,
}

impl BenchRetrievalArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn query_file(&self) -> &Path {
        &self.query_file
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn min_score(&self) -> f64 {
        self.min_score
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct RecallArgs {
    #[arg(value_name = "USER_ID")]
    user_id: String,

    #[arg(value_name = "QUERY")]
    query: String,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "limit", short = 'n', default_value_t = 5)]
    limit: usize,

    #[arg(long = "min-score", default_value_t = DEFAULT_MIN_SCORE)]
    min_score: f64,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl RecallArgs {
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn project_or_user(&self) -> &str {
        self.project.as_deref().unwrap_or(&self.user_id)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn min_score(&self) -> f64 {
        self.min_score
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct AddArgs {
    #[arg(value_name = "CONTENT")]
    content: String,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "confidence", short = 'c', default_value_t = 0.9)]
    confidence: f64,

    #[arg(long)]
    category: Option<String>,

    #[arg(long, short = 'l', default_value = "fact")]
    layer: String,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl AddArgs {
    pub fn content(&self) -> &str {
        &self.content
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    pub fn category(&self) -> Option<&str> {
        self.category.as_deref()
    }

    pub fn layer(&self) -> &str {
        &self.layer
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct RememberArgs {
    #[arg(value_name = "USER_ID")]
    user_id: String,

    #[arg(value_name = "TEXT")]
    text: String,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "confidence", short = 'c', default_value_t = 0.9)]
    confidence: f64,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl RememberArgs {
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn project_or_user(&self) -> &str {
        self.project.as_deref().unwrap_or(&self.user_id)
    }

    pub fn confidence(&self) -> f64 {
        self.confidence
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct FeedbackArgs {
    #[arg(value_name = "MEMORY_ID")]
    memory_id: String,

    #[arg(value_name = "OUTCOME", value_enum)]
    outcome: BatchReportOutcomeValue,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long)]
    reason: Option<String>,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl FeedbackArgs {
    pub fn memory_id(&self) -> &str {
        &self.memory_id
    }

    pub fn outcome(&self) -> BatchReportOutcomeValue {
        self.outcome
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConfirmArgs {
    #[arg(value_name = "MEMORY_ID")]
    memory_id: Option<String>,

    #[arg(
        long,
        short = 'p',
        value_name = "PROJECT",
        default_value = "memra"
    )]
    project: String,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,

    #[arg(long = "since", value_name = "DURATION")]
    since: Option<String>,

    #[arg(long = "source", value_name = "SOURCE", default_value = "ai")]
    source: String,

    #[arg(long = "yes", short = 'y')]
    yes: bool,

    #[arg(long = "dry-run")]
    dry_run: bool,

    #[arg(long, short = 'j')]
    json: bool,
}

impl ConfirmArgs {
    pub fn memory_id(&self) -> Option<&str> {
        self.memory_id.as_deref()
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn since(&self) -> Option<&str> {
        self.since.as_deref()
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn yes(&self) -> bool {
        self.yes
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct FeedbackDueArgs {
    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "limit", short = 'n', default_value_t = 5)]
    limit: usize,

    #[arg(long)]
    refresh: bool,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl FeedbackDueArgs {
    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn refresh(&self) -> bool {
        self.refresh
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ExperienceProofArgs {
    #[arg(value_name = "TOPIC")]
    topic: String,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "limit", short = 'n', default_value_t = 5)]
    limit: usize,

    #[arg(long, value_enum, default_value_t = ExperienceProofTarget::All)]
    target: ExperienceProofTarget,

    #[arg(long)]
    save: bool,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl ExperienceProofArgs {
    pub fn topic(&self) -> &str {
        &self.topic
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn target(&self) -> ExperienceProofTarget {
        self.target
    }

    pub fn save(&self) -> bool {
        self.save
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ExperienceProofTarget {
    All,
    Hermes,
    Harness,
}

impl ExperienceProofTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Hermes => "hermes",
            Self::Harness => "harness",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct TimelineArgs {
    #[arg(value_name = "ENTITY")]
    entity: String,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "limit", short = 'n', default_value_t = 50)]
    limit: usize,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl TimelineArgs {
    pub fn entity(&self) -> &str {
        &self.entity
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PacketArgs {
    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: String,

    #[arg(long, short = 'o', value_name = "PATH")]
    out: Option<PathBuf>,

    #[arg(long, default_value_t = 2)]
    indent: usize,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl PacketArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn out(&self) -> Option<&Path> {
        self.out.as_deref()
    }

    pub fn indent(&self) -> usize {
        self.indent
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct StatsArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long)]
    json: bool,

    #[arg(long)]
    full: bool,
}

impl StatsArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn full(&self) -> bool {
        self.full
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct PulseArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,
}

impl PulseArgs {
    pub fn project(&self) -> &str {
        &self.project
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct GradeArgs {
    #[arg(long)]
    apply: bool,

    #[arg(long = "skip-d")]
    skip_d: bool,

    #[arg(long = "date", value_name = "YYYY-MM-DD")]
    date: Option<String>,

    #[arg(long = "orchestration", value_name = "PATH")]
    orchestration_path: Option<PathBuf>,

    #[arg(long = "evidence", value_name = "PATH")]
    evidence_path: Option<PathBuf>,

    #[arg(long = "logs-dir", value_name = "PATH")]
    logs_dir: Option<PathBuf>,

    #[arg(long = "dogfood-dir", value_name = "PATH")]
    dogfood_dir: Option<PathBuf>,

    #[arg(long = "pending-dir", value_name = "PATH")]
    pending_dir: Option<PathBuf>,
}

impl GradeArgs {
    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn skip_d(&self) -> bool {
        self.skip_d
    }

    pub fn date(&self) -> Option<&str> {
        self.date.as_deref()
    }

    pub fn orchestration_path(&self) -> Option<&Path> {
        self.orchestration_path.as_deref()
    }

    pub fn evidence_path(&self) -> Option<&Path> {
        self.evidence_path.as_deref()
    }

    pub fn logs_dir(&self) -> Option<&Path> {
        self.logs_dir.as_deref()
    }

    pub fn dogfood_dir(&self) -> Option<&Path> {
        self.dogfood_dir.as_deref()
    }

    pub fn pending_dir(&self) -> Option<&Path> {
        self.pending_dir.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct LlmArgs {
    #[command(subcommand)]
    command: LlmCommand,
}

impl LlmArgs {
    pub fn command(&self) -> &LlmCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum LlmCommand {
    #[command(about = "Run a real-key provider smoke check when keys are available")]
    Smoke(LlmSmokeArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct LlmSmokeArgs {
    #[arg(long, value_enum, default_value_t = LlmSmokeProviderArg::All)]
    provider: LlmSmokeProviderArg,

    #[arg(long, value_name = "MODEL")]
    model: Option<String>,

    #[arg(long = "api-base", value_name = "URL")]
    api_base: Option<String>,

    #[arg(long, default_value = "memra-r3-smoke-ok")]
    expected: String,

    #[arg(long, default_value = "Reply with exactly: memra-r3-smoke-ok")]
    prompt: String,

    #[arg(long)]
    strict: bool,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "api-key-env", value_name = "ENV", hide = true)]
    api_key_env: Option<String>,

    #[arg(long = "akm-key", value_name = "KEY", hide = true)]
    akm_key: Option<String>,

    #[arg(long = "akm-bin", value_name = "PATH", hide = true)]
    akm_bin: Option<PathBuf>,
}

impl LlmSmokeArgs {
    pub fn provider(&self) -> LlmSmokeProviderArg {
        self.provider
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn api_base(&self) -> Option<&str> {
        self.api_base.as_deref()
    }

    pub fn expected(&self) -> &str {
        &self.expected
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn strict(&self) -> bool {
        self.strict
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn api_key_env(&self) -> Option<&str> {
        self.api_key_env.as_deref()
    }

    pub fn akm_key(&self) -> Option<&str> {
        self.akm_key.as_deref()
    }

    pub fn akm_bin(&self) -> Option<&Path> {
        self.akm_bin.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum LlmSmokeProviderArg {
    All,
    Openai,
    Deepseek,
    Anthropic,
    Gemini,
}

impl LlmSmokeProviderArg {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Openai => "openai",
            Self::Deepseek => "deepseek",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchArgs {
    #[command(subcommand)]
    command: ResearchCommand,
}

impl ResearchArgs {
    pub fn command(&self) -> &ResearchCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ResearchCommand {
    #[command(about = "Record an autoresearch trigger signal")]
    Signal(ResearchSignalArgs),
    #[command(about = "Queue an autoresearch job when signal/cooldown thresholds are met")]
    Schedule(ResearchScheduleArgs),
    #[command(
        name = "run-pending",
        about = "Run or preview the pending autoresearch worker job"
    )]
    RunPending(ResearchRunPendingArgs),
    #[command(
        name = "run-batch",
        about = "Run or preview the Rust-owned autoresearch batch runner"
    )]
    RunBatch(ResearchRunBatchArgs),
    #[command(about = "Aggregate autoresearch worker TSV files into Python-compatible JSONL")]
    Aggregate(ResearchAggregateArgs),
    #[command(
        name = "metric-diff",
        about = "Compare autoresearch aggregate metrics against the 5 percent R3 gate"
    )]
    MetricDiff(ResearchMetricDiffArgs),
    #[command(
        name = "gemini-auth",
        about = "Inspect cached Gemini CLI auth metadata for unattended autoresearch"
    )]
    GeminiAuth(ResearchGeminiAuthArgs),
    #[command(about = "Print the Rust autoresearch worker state")]
    State(ResearchStateArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchSignalArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long)]
    summary: String,
    #[arg(long = "trigger-source", default_value = "manual")]
    trigger_source: String,
    #[arg(long = "project-root", value_name = "PATH")]
    project_root: Option<PathBuf>,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

impl ResearchSignalArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn trigger_source(&self) -> &str {
        &self.trigger_source
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchScheduleArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long = "trigger-source", default_value = "auto_research_signal")]
    trigger_source: String,
    #[arg(long = "project-root", value_name = "PATH")]
    project_root: Option<PathBuf>,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long = "min-signals", default_value_t = 1)]
    min_signals: usize,
    #[arg(long = "min-interval-hours", default_value_t = 12)]
    min_interval_hours: i64,
    #[arg(long)]
    autorun: bool,
    #[arg(long)]
    json: bool,
}

impl ResearchScheduleArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn trigger_source(&self) -> &str {
        &self.trigger_source
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn min_signals(&self) -> usize {
        self.min_signals
    }

    pub fn min_interval_hours(&self) -> i64 {
        self.min_interval_hours
    }

    pub fn autorun(&self) -> bool {
        self.autorun
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchRunPendingArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

impl ResearchRunPendingArgs {
    pub fn from_background(
        project: String,
        base_dir: Option<PathBuf>,
        dry_run: bool,
        json: bool,
    ) -> Self {
        Self {
            project,
            dry_run,
            base_dir,
            json,
        }
    }

    pub fn from_schedule(args: &ResearchScheduleArgs) -> Self {
        Self {
            project: args.project.clone(),
            dry_run: false,
            base_dir: args.base_dir.clone(),
            json: args.json,
        }
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchRunBatchArgs {
    #[arg(long)]
    tag: String,
    #[arg(long = "project-root", value_name = "PATH")]
    project_root: Option<PathBuf>,
    #[arg(long = "gemini-home", value_name = "PATH")]
    gemini_home: Option<PathBuf>,
    #[arg(long = "gemini-bin", value_name = "PATH")]
    gemini_bin: Option<PathBuf>,
    #[arg(long, default_value = "gemini-3.1-pro-preview", value_name = "MODEL")]
    model: String,
    #[arg(long = "min-experiments", default_value_t = 2)]
    min_experiments: usize,
    #[arg(long = "timeout-seconds", default_value_t = 21600)]
    timeout_seconds: u64,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

impl ResearchRunBatchArgs {
    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn gemini_home(&self) -> Option<&Path> {
        self.gemini_home.as_deref()
    }

    pub fn gemini_bin(&self) -> Option<&Path> {
        self.gemini_bin.as_deref()
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn min_experiments(&self) -> usize {
        self.min_experiments
    }

    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchAggregateArgs {
    #[arg(long)]
    tag: String,
    #[arg(long = "worktree", value_name = "DIRECTION=PATH", required = true)]
    worktrees: Vec<String>,
    #[arg(
        long,
        value_name = "PATH",
        default_value = "autoresearch/parallel_aggregate.jsonl"
    )]
    out: PathBuf,
    #[arg(long)]
    json: bool,
}

impl ResearchAggregateArgs {
    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn worktrees(&self) -> &[String] {
        &self.worktrees
    }

    pub fn out(&self) -> &Path {
        &self.out
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchMetricDiffArgs {
    #[arg(long, value_name = "PATH")]
    baseline: PathBuf,
    #[arg(long, value_name = "PATH")]
    candidate: PathBuf,
    #[arg(long = "baseline-tag")]
    baseline_tag: Option<String>,
    #[arg(long = "candidate-tag")]
    candidate_tag: Option<String>,
    #[arg(long = "threshold-percent", default_value = "5.0")]
    threshold_percent: String,
    #[arg(long)]
    json: bool,
}

impl ResearchMetricDiffArgs {
    pub fn baseline(&self) -> &Path {
        &self.baseline
    }

    pub fn candidate(&self) -> &Path {
        &self.candidate
    }

    pub fn baseline_tag(&self) -> Option<&str> {
        self.baseline_tag.as_deref()
    }

    pub fn candidate_tag(&self) -> Option<&str> {
        self.candidate_tag.as_deref()
    }

    pub fn threshold_percent(&self) -> &str {
        &self.threshold_percent
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchGeminiAuthArgs {
    #[arg(long, default_value = "gemini-3.1-pro-preview", value_name = "MODEL")]
    model: String,
    #[arg(long, value_name = "PATH")]
    home: Option<PathBuf>,
    #[arg(long)]
    check: bool,
    #[arg(long)]
    json: bool,
}

impl ResearchGeminiAuthArgs {
    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    pub fn check(&self) -> bool {
        self.check
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResearchStateArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

impl ResearchStateArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackgroundArgs {
    #[command(subcommand)]
    command: BackgroundCommand,
}

impl BackgroundArgs {
    pub fn command(&self) -> &BackgroundCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum BackgroundCommand {
    #[command(about = "Run one Rust-owned background worker cycle")]
    Run(BackgroundRunArgs),
    #[command(about = "Record an always-on daemon tick in Rust")]
    Tick(BackgroundTickArgs),
    #[command(about = "Put the always-on daemon into a sleep state")]
    Sleep(BackgroundSleepArgs),
    #[command(about = "Record an always-on webhook event in Rust")]
    Webhook(BackgroundWebhookArgs),
    #[command(about = "Wake the always-on daemon if its sleep window expired")]
    Wake(BackgroundWakeArgs),
    #[command(about = "Print the Rust-owned always-on daemon state")]
    State(BackgroundStateArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackgroundRunArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long = "allow-outside-window")]
    allow_outside_window: bool,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long, value_name = "RFC3339")]
    now: Option<String>,
    #[arg(long)]
    json: bool,
}

impl BackgroundRunArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn allow_outside_window(&self) -> bool {
        self.allow_outside_window
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn base_dir_owned(&self) -> Option<PathBuf> {
        self.base_dir.clone()
    }

    pub fn now(&self) -> Option<&str> {
        self.now.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackgroundTickArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long, default_value = "")]
    note: String,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long, value_name = "RFC3339")]
    now: Option<String>,
    #[arg(long)]
    json: bool,
}

impl BackgroundTickArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn note(&self) -> &str {
        &self.note
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn now(&self) -> Option<&str> {
        self.now.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackgroundSleepArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long)]
    seconds: i64,
    #[arg(long, default_value = "")]
    reason: String,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long, value_name = "RFC3339")]
    now: Option<String>,
    #[arg(long)]
    json: bool,
}

impl BackgroundSleepArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn seconds(&self) -> i64 {
        self.seconds
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn now(&self) -> Option<&str> {
        self.now.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackgroundWebhookArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long, default_value = "")]
    summary: String,
    #[arg(long, default_value = "")]
    repository: String,
    #[arg(long = "event-name", default_value = "github_webhook")]
    event_name: String,
    #[arg(long = "changed-path")]
    changed_paths: Vec<String>,
    #[arg(long = "living-doc")]
    living_docs: Vec<String>,
    #[arg(long, value_name = "RFC3339")]
    at: Option<String>,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long, value_name = "RFC3339")]
    now: Option<String>,
    #[arg(long)]
    json: bool,
}

impl BackgroundWebhookArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn event_name(&self) -> &str {
        &self.event_name
    }

    pub fn changed_paths(&self) -> &[String] {
        &self.changed_paths
    }

    pub fn living_docs(&self) -> &[String] {
        &self.living_docs
    }

    pub fn at(&self) -> Option<&str> {
        self.at.as_deref()
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn now(&self) -> Option<&str> {
        self.now.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackgroundWakeArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long, value_name = "RFC3339")]
    now: Option<String>,
    #[arg(long)]
    json: bool,
}

impl BackgroundWakeArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn now(&self) -> Option<&str> {
        self.now.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BackgroundStateArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

impl BackgroundStateArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct MineConvosArgs {
    #[arg(value_name = "PATH")]
    path: PathBuf,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long, short = 'r', value_name = "ROOM")]
    room: Option<String>,

    #[arg(long = "dry-run")]
    dry_run: bool,

    #[arg(long = "max", short = 'n', default_value_t = 500)]
    max_exchanges: usize,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl MineConvosArgs {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn room(&self) -> Option<&str> {
        self.room.as_deref()
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn max_exchanges(&self) -> usize {
        self.max_exchanges
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ContextArgs {
    #[arg(value_name = "USER_ID")]
    user_id: Option<String>,

    #[arg(long, default_value = "wake")]
    mode: String,

    #[arg(
        long,
        short = 'p',
        value_name = "PROJECT",
        default_value = "memra"
    )]
    project: String,

    #[arg(long = "force-refresh")]
    force_refresh: bool,

    #[arg(long = "token-budget", default_value_t = 1200)]
    token_budget: usize,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl ContextArgs {
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn force_refresh(&self) -> bool {
        self.force_refresh
    }

    pub fn token_budget(&self) -> usize {
        self.token_budget
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct CheckpointArgs {
    #[arg(value_name = "TASK_ID")]
    task_id: String,

    #[arg(value_name = "STATE")]
    state: String,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "status", default_value = "in_progress")]
    task_status: String,

    #[arg(long = "next-step")]
    next_step: Option<String>,

    #[arg(long)]
    blocker: Option<String>,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl CheckpointArgs {
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn task_status(&self) -> &str {
        &self.task_status
    }

    pub fn next_step(&self) -> Option<&str> {
        self.next_step.as_deref()
    }

    pub fn blocker(&self) -> Option<&str> {
        self.blocker.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ResumeArgs {
    #[arg(value_name = "QUERY", default_value = "continue previous task")]
    query: String,

    #[arg(long = "user-id", short = 'u')]
    user_id: Option<String>,

    #[arg(long, short = 'p', value_name = "PROJECT")]
    project: Option<String>,

    #[arg(long = "limit", short = 'n', default_value_t = 3)]
    limit: usize,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,
}

impl ResumeArgs {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DreamArgs {
    #[command(subcommand)]
    command: DreamCommand,
}

impl DreamArgs {
    pub fn command(&self) -> &DreamCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum DreamCommand {
    #[command(about = "List shadow_pending dream candidates awaiting review")]
    Pending(DreamPendingArgs),
    #[command(about = "Record a session and queue an autoDream job when thresholds are met")]
    Schedule(DreamScheduleArgs),
    #[command(
        name = "run-pending",
        about = "Run or preview the pending autoDream worker job"
    )]
    RunPending(DreamRunPendingArgs),
    #[command(about = "Promote shadow_pending dream candidates into durable memory")]
    Promote(DreamPromoteArgs),
    #[command(about = "Discard shadow_pending dream candidates")]
    Discard(DreamDiscardArgs),
    #[command(about = "Show a shadow dream batch before promotion")]
    Diff(DreamDiffArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DreamScheduleArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long, default_value = "auto_extract_stop_hook")]
    trigger_source: String,
    #[arg(long)]
    summary: Option<String>,
    #[arg(long = "project-root", value_name = "PATH")]
    project_root: Option<PathBuf>,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
    #[arg(long = "min-sessions", default_value_t = 5)]
    min_sessions: usize,
    #[arg(long = "min-interval-hours", default_value_t = 24)]
    min_interval_hours: i64,
    #[arg(long)]
    autorun: bool,
    #[arg(long)]
    json: bool,
}

impl DreamScheduleArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn trigger_source(&self) -> &str {
        &self.trigger_source
    }

    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    pub fn project_root(&self) -> Option<&Path> {
        self.project_root.as_deref()
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }

    pub fn min_sessions(&self) -> usize {
        self.min_sessions
    }

    pub fn min_interval_hours(&self) -> i64 {
        self.min_interval_hours
    }

    pub fn autorun(&self) -> bool {
        self.autorun
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DreamRunPendingArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
    #[arg(long = "base-dir", value_name = "PATH")]
    base_dir: Option<PathBuf>,
}

impl DreamRunPendingArgs {
    pub fn from_background(
        project: String,
        base_dir: Option<PathBuf>,
        dry_run: bool,
        json: bool,
    ) -> Self {
        Self {
            project,
            dry_run,
            json,
            base_dir,
        }
    }

    pub fn from_schedule(args: &DreamScheduleArgs) -> Self {
        Self {
            project: args.project.clone(),
            dry_run: false,
            json: args.json,
            base_dir: args.base_dir.clone(),
        }
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn base_dir(&self) -> Option<&Path> {
        self.base_dir.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DreamPendingArgs {
    #[arg(long)]
    date: Option<String>,
    #[arg(long = "db", value_name = "PATH")]
    db_path: Option<PathBuf>,
    #[arg(long, default_value_t = 50)]
    limit: usize,
}

impl DreamPendingArgs {
    pub fn date(&self) -> Option<&str> {
        self.date.as_deref()
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DreamPromoteArgs {
    target: String,
    #[arg(long)]
    auto: bool,
    #[arg(long = "db", value_name = "PATH")]
    db_path: Option<PathBuf>,
}

impl DreamPromoteArgs {
    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn auto(&self) -> bool {
        self.auto
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DreamDiscardArgs {
    target: String,
    #[arg(long = "db", value_name = "PATH")]
    db_path: Option<PathBuf>,
}

impl DreamDiscardArgs {
    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DreamDiffArgs {
    target: String,
    #[arg(long = "db", value_name = "PATH")]
    db_path: Option<PathBuf>,
}

impl DreamDiffArgs {
    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct HookArgs {
    #[command(subcommand)]
    command: HookCommand,
}

impl HookArgs {
    pub fn command(&self) -> &HookCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct ConsolidateArgs {
    #[command(subcommand)]
    command: ConsolidateCommand,
}

impl ConsolidateArgs {
    pub fn command(&self) -> &ConsolidateCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ConsolidateCommand {
    #[command(name = "now-ms", about = "Print current Unix epoch milliseconds")]
    NowMs,
    #[command(
        name = "refresh-playbooks",
        about = "Run the curated high-signal playbook refresh stage"
    )]
    RefreshPlaybooks(ConsolidateRefreshPlaybooksArgs),
    #[command(
        name = "refresh-experience",
        about = "Run the Phase 8 experience substrate refresh stage"
    )]
    RefreshExperience(ConsolidateRefreshExperienceArgs),
    #[command(
        name = "strengthen-prune",
        about = "Run the Phase 1 strengthen/prune stage"
    )]
    StrengthenPrune(ConsolidateStrengthenPruneArgs),
    #[command(name = "connect", about = "Run the Phase 2 relation-connect stage")]
    Connect(ConsolidateConnectArgs),
    #[command(name = "chain", about = "Run the Phase 3 version-chain stage")]
    Chain(ConsolidateChainArgs),
    #[command(name = "accuracy", about = "Run the Phase 4 accuracy stage")]
    Accuracy(ConsolidateAccuracyArgs),
    #[command(name = "synapse", about = "Run the Phase 5 synapse stage")]
    Synapse(ConsolidateSynapseArgs),
    #[command(
        name = "candidate-ttl",
        about = "Expire stale, unconfirmed AI memory candidates"
    )]
    CandidateTtl(ConsolidateCandidateTtlArgs),
    #[command(
        name = "runtime",
        about = "Run the Phase 6 consolidation runtime stage"
    )]
    Runtime(ConsolidateRuntimeArgs),
    #[command(name = "reality-check", about = "Run the Phase 7 reality-check stage")]
    RealityCheck(ConsolidateRealityCheckArgs),
    #[command(name = "dream", about = "Run the Phase 9 dream consolidation stage")]
    Dream(ConsolidateDreamArgs),
    #[command(
        name = "dream-evolve",
        about = "Run the Phase 10 dream feedback evolution stage"
    )]
    DreamEvolve(ConsolidateDreamEvolveArgs),
    #[command(
        name = "build-summary",
        about = "Build nightly consolidation orchestration JSON from stage TSV"
    )]
    BuildSummary(ConsolidateBuildSummaryArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RealityCheckDetection {
    Path,
    Drift,
    Llm,
}

impl RealityCheckDetection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Drift => "drift",
            Self::Llm => "llm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StrengthenPrunePhase {
    Strengthen,
    Prune,
}

impl StrengthenPrunePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strengthen => "strengthen",
            Self::Prune => "prune",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AccuracyDetection {
    Contradiction,
    Semantic,
    Stale,
    Migration,
    Cold,
}

impl AccuracyDetection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contradiction => "contradiction",
            Self::Semantic => "semantic",
            Self::Stale => "stale",
            Self::Migration => "migration",
            Self::Cold => "cold",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateStrengthenPruneArgs {
    #[arg(long = "db", value_name = "PATH")]
    db: Option<PathBuf>,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    phase: Option<StrengthenPrunePhase>,
    #[arg(long = "max-per-phase")]
    max_per_phase: Option<u32>,
}

impl ConsolidateStrengthenPruneArgs {
    pub fn db(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn phase(&self) -> Option<StrengthenPrunePhase> {
        self.phase
    }

    pub fn max_per_phase(&self) -> Option<u32> {
        self.max_per_phase
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateConnectArgs {
    #[arg(long = "db", value_name = "PATH")]
    db: Option<PathBuf>,
    #[arg(long)]
    apply: bool,
    #[arg(long = "max-links")]
    max_links: Option<u32>,
    #[arg(long = "cross-layer")]
    cross_layer: bool,
    #[arg(long = "same-threshold")]
    same_threshold: Option<String>,
    #[arg(long = "cross-threshold")]
    cross_threshold: Option<String>,
    #[arg(long = "max-links-per-note")]
    max_links_per_note: Option<u32>,
    #[arg(long = "batch-size")]
    batch_size: Option<u32>,
}

impl ConsolidateConnectArgs {
    pub fn db(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn max_links(&self) -> Option<u32> {
        self.max_links
    }

    pub fn cross_layer(&self) -> bool {
        self.cross_layer
    }

    pub fn same_threshold(&self) -> Option<&str> {
        self.same_threshold.as_deref()
    }

    pub fn cross_threshold(&self) -> Option<&str> {
        self.cross_threshold.as_deref()
    }

    pub fn max_links_per_note(&self) -> Option<u32> {
        self.max_links_per_note
    }

    pub fn batch_size(&self) -> Option<u32> {
        self.batch_size
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateChainArgs {
    #[arg(long = "db", value_name = "PATH")]
    db: Option<PathBuf>,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    threshold: Option<String>,
    #[arg(long = "max-chains")]
    max_chains: Option<u32>,
}

impl ConsolidateChainArgs {
    pub fn db(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn threshold(&self) -> Option<&str> {
        self.threshold.as_deref()
    }

    pub fn max_chains(&self) -> Option<u32> {
        self.max_chains
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateAccuracyArgs {
    #[arg(long = "db", value_name = "PATH")]
    db: Option<PathBuf>,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    detection: Option<AccuracyDetection>,
    #[arg(long = "max-per-phase")]
    max_per_phase: Option<u32>,
}

impl ConsolidateAccuracyArgs {
    pub fn db(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn detection(&self) -> Option<AccuracyDetection> {
        self.detection
    }

    pub fn max_per_phase(&self) -> Option<u32> {
        self.max_per_phase
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateSynapseArgs {
    #[arg(long = "db", value_name = "PATH")]
    db: Option<PathBuf>,
    #[arg(long)]
    apply: bool,
    #[arg(long = "max-hebbian")]
    max_hebbian: Option<u32>,
    #[arg(long = "max-pruned")]
    max_pruned: Option<u32>,
    #[arg(long = "max-bridges")]
    max_bridges: Option<u32>,
}

impl ConsolidateSynapseArgs {
    pub fn db(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn max_hebbian(&self) -> Option<u32> {
        self.max_hebbian
    }

    pub fn max_pruned(&self) -> Option<u32> {
        self.max_pruned
    }

    pub fn max_bridges(&self) -> Option<u32> {
        self.max_bridges
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateCandidateTtlArgs {
    #[arg(long, default_value = "memra")]
    project: String,
    #[arg(long = "db-path", value_name = "PATH")]
    db_path: Option<PathBuf>,
    #[arg(long = "json")]
    json: bool,
}

impl ConsolidateCandidateTtlArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateRefreshExperienceArgs {
    #[arg(long, default_value_t = 12)]
    limit: u32,
}

impl ConsolidateRefreshExperienceArgs {
    pub fn limit(&self) -> u32 {
        self.limit
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateRuntimeArgs {
    #[arg(long = "db-path", value_name = "PATH")]
    db_path: Option<PathBuf>,
    #[arg(long = "dry-run")]
    dry_run: bool,
    #[arg(long = "summary-output", value_name = "PATH")]
    summary_output: Option<PathBuf>,
    #[arg(long = "assert-runtime-truth")]
    assert_runtime_truth: bool,
    #[arg(long = "rust-writer")]
    rust_writer: bool,
}

impl ConsolidateRuntimeArgs {
    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn summary_output(&self) -> Option<&Path> {
        self.summary_output.as_deref()
    }

    pub fn assert_runtime_truth(&self) -> bool {
        self.assert_runtime_truth
    }

    pub fn rust_writer(&self) -> bool {
        self.rust_writer
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateRealityCheckArgs {
    #[arg(long = "db", value_name = "PATH")]
    db: Option<PathBuf>,
    #[arg(long)]
    apply: bool,
    #[arg(long)]
    detection: Option<RealityCheckDetection>,
    #[arg(long = "max-per-phase")]
    max_per_phase: Option<u32>,
    #[arg(long = "llm-max")]
    llm_max: Option<u32>,
    #[arg(long = "project-dir", value_name = "PATH")]
    project_dir: Option<PathBuf>,
}

impl ConsolidateRealityCheckArgs {
    pub fn db(&self) -> Option<&Path> {
        self.db.as_deref()
    }

    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn detection(&self) -> Option<RealityCheckDetection> {
        self.detection
    }

    pub fn max_per_phase(&self) -> Option<u32> {
        self.max_per_phase
    }

    pub fn llm_max(&self) -> Option<u32> {
        self.llm_max
    }

    pub fn project_dir(&self) -> Option<&Path> {
        self.project_dir.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateDreamArgs {
    #[arg(long)]
    apply: bool,
    #[arg(long = "max-candidates")]
    max_candidates: Option<u32>,
    #[arg(long = "lookback-hours")]
    lookback_hours: Option<u32>,
    #[arg(long)]
    project: Option<String>,
    #[arg(long = "db-path", value_name = "PATH")]
    db_path: Option<PathBuf>,
    #[arg(long = "json")]
    json: bool,
}

impl ConsolidateDreamArgs {
    pub fn apply(&self) -> bool {
        self.apply
    }

    pub fn max_candidates(&self) -> Option<u32> {
        self.max_candidates
    }

    pub fn lookback_hours(&self) -> Option<u32> {
        self.lookback_hours
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateDreamEvolveArgs {
    #[arg(long)]
    project: Option<String>,
    #[arg(long = "db-path", value_name = "PATH")]
    db_path: Option<PathBuf>,
    #[arg(long = "config-path", value_name = "PATH")]
    config_path: Option<PathBuf>,
    #[arg(long = "lookback-days")]
    lookback_days: Option<u32>,
    #[arg(long)]
    apply: bool,
}

impl ConsolidateDreamEvolveArgs {
    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    pub fn lookback_days(&self) -> Option<u32> {
        self.lookback_days
    }

    pub fn apply(&self) -> bool {
        self.apply
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateRefreshPlaybooksArgs {
    #[arg(long)]
    dry_run: bool,
    #[arg(long = "refresh-evidence")]
    refresh_evidence: bool,
}

impl ConsolidateRefreshPlaybooksArgs {
    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn refresh_evidence(&self) -> bool {
        self.refresh_evidence
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ConsolidateBuildSummaryArgs {
    #[arg(long = "summary-tsv", value_name = "PATH")]
    summary_tsv: PathBuf,
    #[arg(long = "summary-json", value_name = "PATH")]
    summary_json: PathBuf,
    #[arg(long = "stage6-summary", value_name = "PATH")]
    stage6_summary: PathBuf,
}

impl ConsolidateBuildSummaryArgs {
    pub fn summary_tsv(&self) -> &Path {
        &self.summary_tsv
    }

    pub fn summary_json(&self) -> &Path {
        &self.summary_json
    }

    pub fn stage6_summary(&self) -> &Path {
        &self.stage6_summary
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum HookCommand {
    #[command(
        name = "sessionstart",
        alias = "session-start",
        about = "Surface SessionStart checkpoint context and local warnings"
    )]
    SessionStart(HookSessionStartArgs),
    #[command(
        name = "pending-nudge",
        about = "Surface pending Memra review/correction files"
    )]
    PendingNudge(HookPendingNudgeArgs),
    #[command(
        name = "add-rule-validator",
        about = "Validate add_rule hook payloads before Memra writes"
    )]
    AddRuleValidator,
    #[command(
        name = "precompact",
        alias = "pre-compact",
        about = "Save local runtime checkpoint before context compaction"
    )]
    PreCompact,
    #[command(
        name = "postcompact",
        alias = "post-compact",
        about = "Reinject Memra operating rules after context compaction"
    )]
    PostCompact,
    #[command(
        name = "stop",
        about = "Run the Phase 1 Stop hook through the Rust-owned hook entrypoint"
    )]
    Stop,
    #[command(
        name = "posttool",
        alias = "post-tool",
        about = "Run the Phase 1 PostToolUse hook through the Rust-owned hook entrypoint"
    )]
    PostToolUse,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct HookSessionStartArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,
}

impl HookSessionStartArgs {
    pub fn project(&self) -> &str {
        &self.project
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct HookPendingNudgeArgs {
    #[arg(long = "pending-dir", value_name = "PATH")]
    pending_dir: Option<PathBuf>,
}

impl HookPendingNudgeArgs {
    pub fn pending_dir(&self) -> Option<&Path> {
        self.pending_dir.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct WikiArgs {
    #[command(subcommand)]
    command: WikiCommand,
}

impl WikiArgs {
    pub fn command(&self) -> &WikiCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum WikiCommand {
    #[command(about = "Dump verified facts to Markdown files under wiki/by-topic")]
    Sync(WikiSyncArgs),
    #[command(about = "Backfill edited wiki Markdown as corrected outcome feedback")]
    Backfill(WikiBackfillArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct WikiSyncArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long = "out", value_name = "DIR")]
    out_dir: Option<PathBuf>,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,

    #[arg(long)]
    dry_run: bool,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long)]
    limit: Option<usize>,
}

impl WikiSyncArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn out_dir(&self) -> Option<&Path> {
        self.out_dir.as_deref()
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn limit(&self) -> Option<usize> {
        self.limit
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct WikiBackfillArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long = "wiki-dir", value_name = "DIR")]
    wiki_dir: Option<PathBuf>,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,

    #[arg(long)]
    dry_run: bool,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long)]
    limit: Option<usize>,
}

impl WikiBackfillArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn wiki_dir(&self) -> Option<&Path> {
        self.wiki_dir.as_deref()
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn limit(&self) -> Option<usize> {
        self.limit
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct IngestArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long = "root", value_name = "PATH")]
    roots: Vec<PathBuf>,

    #[arg(long = "config", value_name = "PATH")]
    config_path: Option<PathBuf>,

    #[arg(long = "state", value_name = "PATH")]
    state_path: Option<PathBuf>,

    #[arg(long = "db", value_name = "PATH", hide = true)]
    db_path: Option<PathBuf>,

    #[arg(long)]
    force: bool,

    #[arg(long)]
    dry_run: bool,

    #[arg(long, short = 'j')]
    json: bool,

    #[arg(long)]
    limit: Option<usize>,
}

impl IngestArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    pub fn state_path(&self) -> Option<&Path> {
        self.state_path.as_deref()
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn force(&self) -> bool {
        self.force
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn json(&self) -> bool {
        self.json
    }

    pub fn limit(&self) -> Option<usize> {
        self.limit
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ReviewQueueArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long, value_enum, default_value_t = ReviewQueueSource::All)]
    source: ReviewQueueSource,

    #[arg(long, default_value_t = 10)]
    limit: usize,

    #[arg(long)]
    json: bool,
}

impl ReviewQueueArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn source(&self) -> ReviewQueueSource {
        self.source
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ReviewQueueSource {
    All,
    MetaFact,
    ReplayDrift,
    DreamFeedback,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct ReviewResolveArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long = "db", value_name = "PATH")]
    db_path: Option<PathBuf>,

    #[arg(long, value_enum)]
    source: ReviewResolveSource,

    #[arg(long)]
    id: String,

    #[arg(long, value_enum)]
    verdict: ReviewResolveVerdict,

    #[arg(long)]
    reason: Option<String>,

    #[arg(long = "merged-into")]
    merged_into: Option<String>,
}

impl ReviewResolveArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn source(&self) -> ReviewResolveSource {
        self.source
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn verdict(&self) -> ReviewResolveVerdict {
        self.verdict
    }

    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub fn merged_into(&self) -> Option<&str> {
        self.merged_into.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ReviewResolveSource {
    MetaFact,
    ReplayDrift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum ReviewResolveVerdict {
    Reject,
    Merge,
    Acknowledge,
    Outdated,
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchWriteArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long = "db", value_name = "PATH")]
    db_path: Option<PathBuf>,

    #[command(subcommand)]
    command: BatchWriteCommand,
}

impl BatchWriteArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }

    pub fn command(&self) -> &BatchWriteCommand {
        &self.command
    }
}

#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum BatchWriteCommand {
    #[command(about = "Add a memory through the Rust WriteOrchestrator")]
    AddMemory(BatchAddMemoryArgs),
    #[command(about = "Mark one note superseded by another through Rust writer helpers")]
    MarkSuperseded(BatchMarkSupersededArgs),
    #[command(about = "Upsert a note relation through Rust writer helpers")]
    UpsertRelation(BatchUpsertRelationArgs),
    #[command(about = "Insert a note event through Rust writer helpers")]
    NoteEvent(BatchNoteEventArgs),
    #[command(about = "Stamp note lineage fields through Rust writer helpers")]
    StampLineage(BatchStampLineageArgs),
    #[command(about = "Update bounded note fields through Rust writer helpers")]
    UpdateNote(BatchUpdateNoteArgs),
    #[command(about = "Report memory outcome through Rust writer feedback helpers")]
    ReportOutcome(BatchReportOutcomeArgs),
    #[command(about = "Increment a note contradiction counter through Rust writer helpers")]
    IncrementContradictCount(BatchIncrementContradictCountArgs),
    #[command(about = "Insert a dream candidate through Rust writer helpers")]
    DreamInsertCandidate(BatchDreamInsertCandidateArgs),
    #[command(about = "Update bounded dream candidate fields through Rust writer helpers")]
    DreamUpdateCandidate(BatchDreamUpdateCandidateArgs),
    #[command(about = "Insert a dream evolution log row through Rust writer helpers")]
    DreamEvolutionLog(BatchDreamEvolutionLogArgs),
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchAddMemoryArgs {
    #[arg(long)]
    content: String,
    #[arg(long)]
    layer: Option<String>,
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    source: Option<String>,
    #[arg(long)]
    confidence: Option<f64>,
    #[arg(long = "metadata-json")]
    metadata_json: Option<String>,
    #[arg(long = "related-id")]
    related_ids: Vec<String>,
    #[arg(long = "root-id")]
    root_id: Option<String>,
    #[arg(long)]
    version: Option<i64>,
    #[arg(long = "topic-key")]
    topic_key: Option<String>,
}

impl BatchAddMemoryArgs {
    pub fn content(&self) -> &str {
        &self.content
    }
    pub fn layer(&self) -> Option<&str> {
        self.layer.as_deref()
    }
    pub fn category(&self) -> Option<&str> {
        self.category.as_deref()
    }
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }
    pub fn metadata_json(&self) -> Option<&str> {
        self.metadata_json.as_deref()
    }
    pub fn related_ids(&self) -> &[String] {
        &self.related_ids
    }
    pub fn root_id(&self) -> Option<&str> {
        self.root_id.as_deref()
    }
    pub fn version(&self) -> Option<i64> {
        self.version
    }
    pub fn topic_key(&self) -> Option<&str> {
        self.topic_key.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchMarkSupersededArgs {
    #[arg(long = "old-id")]
    old_id: String,
    #[arg(long = "new-id")]
    new_id: String,
    #[arg(long)]
    reason: String,
}

impl BatchMarkSupersededArgs {
    pub fn old_id(&self) -> &str {
        &self.old_id
    }
    pub fn new_id(&self) -> &str {
        &self.new_id
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum BatchRelationMode {
    Max,
    Strengthen,
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchUpsertRelationArgs {
    #[arg(long = "from-id")]
    from_id: String,
    #[arg(long = "to-id")]
    to_id: String,
    #[arg(long = "relation-type")]
    relation_type: String,
    #[arg(long, value_enum, default_value_t = BatchRelationMode::Max)]
    mode: BatchRelationMode,
    #[arg(long, default_value_t = 0.5)]
    strength: f64,
    #[arg(long, default_value_t = 0.05)]
    delta: f64,
}

impl BatchUpsertRelationArgs {
    pub fn source_id(&self) -> &str {
        &self.from_id
    }
    pub fn target_id(&self) -> &str {
        &self.to_id
    }
    pub fn relation_type(&self) -> &str {
        &self.relation_type
    }
    pub fn mode(&self) -> BatchRelationMode {
        self.mode
    }
    pub fn strength(&self) -> f64 {
        self.strength
    }
    pub fn delta(&self) -> f64 {
        self.delta
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchNoteEventArgs {
    #[arg(long = "note-id")]
    note_id: String,
    #[arg(long = "event-type")]
    event_type: String,
    #[arg(long = "related-note-id")]
    related_note_id: Option<String>,
    #[arg(long = "payload-json")]
    payload_json: Option<String>,
}

impl BatchNoteEventArgs {
    pub fn note_id(&self) -> &str {
        &self.note_id
    }
    pub fn event_type(&self) -> &str {
        &self.event_type
    }
    pub fn related_note_id(&self) -> Option<&str> {
        self.related_note_id.as_deref()
    }
    pub fn payload_json(&self) -> Option<&str> {
        self.payload_json.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchStampLineageArgs {
    #[arg(long = "note-id")]
    note_id: String,
    #[arg(long = "root-id")]
    root_id: Option<String>,
    #[arg(long)]
    version: Option<i64>,
    #[arg(long = "topic-key")]
    topic_key: Option<String>,
}

impl BatchStampLineageArgs {
    pub fn note_id(&self) -> &str {
        &self.note_id
    }
    pub fn root_id(&self) -> Option<&str> {
        self.root_id.as_deref()
    }
    pub fn version(&self) -> Option<i64> {
        self.version
    }
    pub fn topic_key(&self) -> Option<&str> {
        self.topic_key.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchUpdateNoteArgs {
    #[arg(long = "note-id")]
    note_id: String,
    #[arg(long = "metadata-json")]
    metadata_json: Option<String>,
    #[arg(long)]
    confidence: Option<f64>,
    #[arg(long = "evolution-state")]
    evolution_state: Option<String>,
}

impl BatchUpdateNoteArgs {
    pub fn note_id(&self) -> &str {
        &self.note_id
    }
    pub fn metadata_json(&self) -> Option<&str> {
        self.metadata_json.as_deref()
    }
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }
    pub fn evolution_state(&self) -> Option<&str> {
        self.evolution_state.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BatchReportOutcomeArgs {
    #[arg(long = "memory-id")]
    memory_id: String,
    #[arg(long, value_enum)]
    outcome: BatchReportOutcomeValue,
    #[arg(long)]
    reason: Option<String>,
}

impl BatchReportOutcomeArgs {
    pub fn memory_id(&self) -> &str {
        &self.memory_id
    }
    pub fn outcome(&self) -> BatchReportOutcomeValue {
        self.outcome
    }
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum BatchReportOutcomeValue {
    Confirmed,
    Corrected,
    Outdated,
}

impl BatchReportOutcomeValue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Corrected => "corrected",
            Self::Outdated => "outdated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct BatchIncrementContradictCountArgs {
    #[arg(long = "note-id")]
    note_id: String,
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchDreamInsertCandidateArgs {
    #[arg(long)]
    id: String,
    #[arg(long = "source-type", default_value = "consolidation")]
    source_type: String,
    #[arg(long = "source-id")]
    source_id: Option<String>,
    #[arg(long)]
    summary: String,
    #[arg(long)]
    hypothesis: Option<String>,
    #[arg(long, default_value_t = 0.0)]
    confidence: f64,
    #[arg(long, default_value_t = 1)]
    frequency: i64,
    #[arg(long = "evidence-ids-json")]
    evidence_ids_json: Option<String>,
    #[arg(long, default_value = "pending")]
    verdict: String,
    #[arg(long = "created-at")]
    created_at: String,
}

impl BatchDreamInsertCandidateArgs {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn source_type(&self) -> &str {
        &self.source_type
    }
    pub fn source_id(&self) -> Option<&str> {
        self.source_id.as_deref()
    }
    pub fn summary(&self) -> &str {
        &self.summary
    }
    pub fn hypothesis(&self) -> Option<&str> {
        self.hypothesis.as_deref()
    }
    pub fn confidence(&self) -> f64 {
        self.confidence
    }
    pub fn frequency(&self) -> i64 {
        self.frequency
    }
    pub fn evidence_ids_json(&self) -> Option<&str> {
        self.evidence_ids_json.as_deref()
    }
    pub fn verdict(&self) -> &str {
        &self.verdict
    }
    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchDreamUpdateCandidateArgs {
    #[arg(long = "candidate-id")]
    candidate_id: String,
    #[arg(long)]
    verdict: Option<String>,
    #[arg(long)]
    confidence: Option<f64>,
    #[arg(long = "evaluator-notes")]
    evaluator_notes: Option<String>,
    #[arg(long = "evaluated-at")]
    evaluated_at: Option<String>,
    #[arg(long = "discarded-at")]
    discarded_at: Option<String>,
    #[arg(long = "promoted-to")]
    promoted_to: Option<String>,
    #[arg(long = "promoted-at")]
    promoted_at: Option<String>,
    #[arg(long = "writer-status")]
    writer_status: Option<String>,
    #[arg(long = "writer-reason")]
    writer_reason: Option<String>,
}

impl BatchDreamUpdateCandidateArgs {
    pub fn candidate_id(&self) -> &str {
        &self.candidate_id
    }
    pub fn verdict(&self) -> Option<&str> {
        self.verdict.as_deref()
    }
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }
    pub fn evaluator_notes(&self) -> Option<&str> {
        self.evaluator_notes.as_deref()
    }
    pub fn evaluated_at(&self) -> Option<&str> {
        self.evaluated_at.as_deref()
    }
    pub fn discarded_at(&self) -> Option<&str> {
        self.discarded_at.as_deref()
    }
    pub fn promoted_to(&self) -> Option<&str> {
        self.promoted_to.as_deref()
    }
    pub fn promoted_at(&self) -> Option<&str> {
        self.promoted_at.as_deref()
    }
    pub fn writer_status(&self) -> Option<&str> {
        self.writer_status.as_deref()
    }
    pub fn writer_reason(&self) -> Option<&str> {
        self.writer_reason.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Args)]
pub struct BatchDreamEvolutionLogArgs {
    #[arg(long)]
    id: String,
    #[arg(long = "run-at")]
    run_at: String,
    #[arg(long = "period-start")]
    period_start: String,
    #[arg(long = "period-end")]
    period_end: String,
    #[arg(long = "promoted-count")]
    promoted_count: Option<i64>,
    #[arg(long = "confirmed-count")]
    confirmed_count: Option<i64>,
    #[arg(long = "corrected-count")]
    corrected_count: Option<i64>,
    #[arg(long = "outdated-count")]
    outdated_count: Option<i64>,
    #[arg(long = "precision-rate")]
    precision_rate: Option<f64>,
    #[arg(long = "weight-adjustments-json")]
    weight_adjustments_json: Option<String>,
    #[arg(long = "discard-patterns-json")]
    discard_patterns_json: Option<String>,
    #[arg(long = "llm-model")]
    llm_model: Option<String>,
    #[arg(long = "llm-tokens-used")]
    llm_tokens_used: Option<i64>,
    #[arg(long = "created-at")]
    created_at: String,
}

impl BatchDreamEvolutionLogArgs {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn run_at(&self) -> &str {
        &self.run_at
    }
    pub fn period_start(&self) -> &str {
        &self.period_start
    }
    pub fn period_end(&self) -> &str {
        &self.period_end
    }
    pub fn promoted_count(&self) -> Option<i64> {
        self.promoted_count
    }
    pub fn confirmed_count(&self) -> Option<i64> {
        self.confirmed_count
    }
    pub fn corrected_count(&self) -> Option<i64> {
        self.corrected_count
    }
    pub fn outdated_count(&self) -> Option<i64> {
        self.outdated_count
    }
    pub fn precision_rate(&self) -> Option<f64> {
        self.precision_rate
    }
    pub fn weight_adjustments_json(&self) -> Option<&str> {
        self.weight_adjustments_json.as_deref()
    }
    pub fn discard_patterns_json(&self) -> Option<&str> {
        self.discard_patterns_json.as_deref()
    }
    pub fn llm_model(&self) -> Option<&str> {
        self.llm_model.as_deref()
    }
    pub fn llm_tokens_used(&self) -> Option<i64> {
        self.llm_tokens_used
    }
    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

impl BatchIncrementContradictCountArgs {
    pub fn note_id(&self) -> &str {
        &self.note_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct DemoArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long)]
    force: bool,
}

impl DemoArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn force(&self) -> bool {
        self.force
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RecoverArgs {
    #[arg(long, value_name = "PROJECT", default_value = "memra")]
    project: String,

    #[arg(long = "dry-run")]
    dry_run: bool,

    #[arg(long)]
    yes: bool,
}

impl RecoverArgs {
    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn yes(&self) -> bool {
        self.yes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeTransport {
    Stdio,
    Http,
    Daemon,
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct AdminArgs {
    #[command(subcommand)]
    command: AdminCommand,
}

impl AdminArgs {
    pub fn into_command(self) -> AdminCommand {
        self.command
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum AdminCommand {
    #[command(about = "Create the manual revert kill-switch file")]
    Revert(AdminRevertArgs),
    #[command(about = "Provision a new API key for an authenticated actor")]
    AddKey(AddKeyArgs),
    #[command(about = "Revoke an API key by stored BLAKE3 hash")]
    RevokeKey(RevokeKeyArgs),
    #[command(about = "List configured API key metadata without raw key material")]
    ListKeys,
    #[command(about = "Reload auth configuration for a running server")]
    ReloadAuth,
    #[command(about = "Revoke all write tokens for a session")]
    SessionRevoke(SessionRevokeArgs),
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct AdminRevertArgs {
    #[arg(long, short = 'r')]
    reason: String,

    #[arg(long)]
    force: bool,
}

impl AdminRevertArgs {
    pub fn reason(&self) -> &str {
        &self.reason
    }

    pub fn force(&self) -> bool {
        self.force
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct AddKeyArgs {
    #[arg(long, value_name = "NAME")]
    name: String,

    #[arg(long = "actor-id", value_name = "ACTOR_ID")]
    actor_id: String,
}

impl AddKeyArgs {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct RevokeKeyArgs {
    #[arg(value_name = "NAME")]
    name: String,
}

impl RevokeKeyArgs {
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Args)]
pub struct SessionRevokeArgs {
    #[arg(
        value_name = "SESSION_ID",
        help = "Session whose write tokens to revoke"
    )]
    session_id: String,

    /// Override the SQLite DB path (defaults to the project default path).
    #[arg(long = "db", value_name = "PATH")]
    db_path: Option<PathBuf>,
}

impl SessionRevokeArgs {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.db_path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        AdminCommand, BackgroundCommand, BatchRelationMode, BatchReportOutcomeValue,
        BatchWriteCommand, BenchCommand, Cli, Command, ConsolidateCommand, DEFAULT_MIN_SCORE,
        ExperienceProofTarget, LlmCommand, LlmSmokeProviderArg, ResearchCommand, ReviewQueueSource,
        ReviewResolveSource, ReviewResolveVerdict, ServeTransport, WikiCommand,
    };

    #[test]
    fn defaults_to_stdio_serve() -> Result<(), String> {
        let parsed = Cli::try_parse_from(["ma"]).map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Serve(args) => {
                assert_eq!(args.transport(), ServeTransport::Stdio);
                assert_eq!(args.bind().to_string(), "127.0.0.1");
                assert_eq!(args.port(), 7331);
                assert_eq!(args.project(), Some("memra"));
                assert!(args.tls_cert().is_none());
                assert!(args.tls_key().is_none());
                assert!(args.socket().is_none());
                Ok(())
            }
            other => Err(format!("expected serve command, got {other:?}")),
        }
    }

    #[test]
    fn parses_http_serve_options() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "serve",
            "--http",
            "--project",
            "demo-project",
            "--bind",
            "0.0.0.0",
            "--port",
            "8443",
            "--tls-cert",
            "cert.pem",
            "--tls-key",
            "key.pem",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Serve(args) => {
                assert_eq!(args.transport(), ServeTransport::Http);
                assert_eq!(args.bind().to_string(), "0.0.0.0");
                assert_eq!(args.port(), 8443);
                assert_eq!(args.project(), Some("demo-project"));
                assert_eq!(
                    args.tls_cert()
                        .map(|path| path.to_string_lossy().to_string()),
                    Some("cert.pem".to_string())
                );
                assert_eq!(
                    args.tls_key()
                        .map(|path| path.to_string_lossy().to_string()),
                    Some("key.pem".to_string())
                );
                Ok(())
            }
            other => Err(format!("expected serve command, got {other:?}")),
        }
    }

    #[test]
    fn parses_daemon_serve_options() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "serve",
            "--daemon",
            "--project",
            "demo-project",
            "--socket",
            "/tmp/ma-demo.sock",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Serve(args) => {
                assert_eq!(args.transport(), ServeTransport::Daemon);
                assert_eq!(args.project(), Some("demo-project"));
                assert_eq!(args.socket(), Some(Path::new("/tmp/ma-demo.sock")));
                Ok(())
            }
            other => Err(format!("expected serve command, got {other:?}")),
        }
    }

    #[test]
    fn parses_admin_subcommands() -> Result<(), String> {
        let add_key = Cli::try_parse_from([
            "ma",
            "admin",
            "add-key",
            "--name",
            "local",
            "--actor-id",
            "alice",
        ])
        .map_err(|error| error.to_string())?;
        match add_key.into_command() {
            Command::Admin(args) => match args.into_command() {
                AdminCommand::AddKey(args) => {
                    assert_eq!(args.name(), "local");
                    assert_eq!(args.actor_id(), "alice");
                }
                other => return Err(format!("expected admin add-key, got {other:?}")),
            },
            other => return Err(format!("expected admin add-key, got {other:?}")),
        }

        let revoke_key = Cli::try_parse_from(["ma", "admin", "revoke-key", "local"])
            .map_err(|error| error.to_string())?;
        match revoke_key.into_command() {
            Command::Admin(args) => match args.into_command() {
                AdminCommand::RevokeKey(args) => assert_eq!(args.name(), "local"),
                other => return Err(format!("expected admin revoke-key, got {other:?}")),
            },
            other => return Err(format!("expected admin revoke-key, got {other:?}")),
        }

        let list_keys =
            Cli::try_parse_from(["ma", "admin", "list-keys"]).map_err(|error| error.to_string())?;
        match list_keys.into_command() {
            Command::Admin(args) => match args.into_command() {
                AdminCommand::ListKeys => {}
                other => return Err(format!("expected admin list-keys, got {other:?}")),
            },
            other => return Err(format!("expected admin list-keys, got {other:?}")),
        }

        let reload_auth = Cli::try_parse_from(["ma", "admin", "reload-auth"])
            .map_err(|error| error.to_string())?;
        match reload_auth.into_command() {
            Command::Admin(args) => match args.into_command() {
                AdminCommand::ReloadAuth => {}
                other => return Err(format!("expected admin reload-auth, got {other:?}")),
            },
            other => return Err(format!("expected admin reload-auth, got {other:?}")),
        }

        let revert = Cli::try_parse_from([
            "ma",
            "admin",
            "revert",
            "--reason",
            "manual rollback",
            "--force",
        ])
        .map_err(|error| error.to_string())?;
        match revert.into_command() {
            Command::Admin(args) => match args.into_command() {
                AdminCommand::Revert(args) => {
                    assert_eq!(args.reason(), "manual rollback");
                    assert!(args.force());
                }
                other => return Err(format!("expected admin revert, got {other:?}")),
            },
            other => return Err(format!("expected admin revert, got {other:?}")),
        }
        Ok(())
    }

    #[test]
    fn parses_init_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "init",
            "--project",
            "memra",
            "--force",
            "--dry-run",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Init(args) => {
                assert_eq!(args.project(), "memra");
                assert!(args.force());
                assert!(args.dry_run());
                Ok(())
            }
            other => Err(format!("expected init command, got {other:?}")),
        }
    }

    #[test]
    fn parses_doctor_command() -> Result<(), String> {
        let parsed =
            Cli::try_parse_from(["ma", "doctor", "--project", "alpha", "--json", "--full"])
                .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Doctor(args) => {
                assert_eq!(args.project(), "alpha");
                assert!(args.json());
                assert!(args.full());
                Ok(())
            }
            other => Err(format!("expected doctor command, got {other:?}")),
        }
    }

    #[test]
    fn parses_mine_convos_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "mine-convos",
            "/tmp/session.jsonl",
            "--project",
            "alpha",
            "--room",
            "ops",
            "--dry-run",
            "--max",
            "5",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::MineConvos(args) => {
                assert_eq!(args.path(), Path::new("/tmp/session.jsonl"));
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.room(), Some("ops"));
                assert!(args.dry_run());
                assert_eq!(args.max_exchanges(), 5);
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected mine-convos command, got {other:?}")),
        }
    }

    #[test]
    fn parses_llm_smoke_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "llm",
            "smoke",
            "--provider",
            "deepseek",
            "--model",
            "deepseek-chat",
            "--api-base",
            "http://127.0.0.1:8080/v1",
            "--expected",
            "ok",
            "--prompt",
            "Reply ok",
            "--strict",
            "--json",
            "--api-key-env",
            "MA_R3_SMOKE_TEST_KEY",
            "--akm-key",
            "MA_R3_SMOKE_TEST_KEY",
            "--akm-bin",
            "/missing/akm",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Llm(args) => match args.command() {
                LlmCommand::Smoke(smoke) => {
                    assert_eq!(smoke.provider(), LlmSmokeProviderArg::Deepseek);
                    assert_eq!(smoke.model(), Some("deepseek-chat"));
                    assert_eq!(smoke.api_base(), Some("http://127.0.0.1:8080/v1"));
                    assert_eq!(smoke.expected(), "ok");
                    assert_eq!(smoke.prompt(), "Reply ok");
                    assert!(smoke.strict());
                    assert!(smoke.json());
                    assert_eq!(smoke.api_key_env(), Some("MA_R3_SMOKE_TEST_KEY"));
                    assert_eq!(smoke.akm_key(), Some("MA_R3_SMOKE_TEST_KEY"));
                    assert_eq!(
                        smoke.akm_bin().map(|path| path.display().to_string()),
                        Some("/missing/akm".to_string())
                    );
                    Ok(())
                }
            },
            other => Err(format!("expected llm smoke, got {other:?}")),
        }
    }

    #[test]
    fn parses_research_commands() -> Result<(), String> {
        let signal = Cli::try_parse_from([
            "ma",
            "research",
            "signal",
            "--project",
            "demo",
            "--summary",
            "push updated docs",
            "--trigger-source",
            "github_webhook_push",
            "--project-root",
            "/tmp/repo",
            "--base-dir",
            "/tmp/ma-projects",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match signal.into_command() {
            Command::Research(args) => match args.command() {
                ResearchCommand::Signal(signal) => {
                    assert_eq!(signal.project(), "demo");
                    assert_eq!(signal.summary(), "push updated docs");
                    assert_eq!(signal.trigger_source(), "github_webhook_push");
                    assert_eq!(signal.project_root(), Some(Path::new("/tmp/repo")));
                    assert_eq!(signal.base_dir(), Some(Path::new("/tmp/ma-projects")));
                    assert!(signal.json());
                }
                other => return Err(format!("expected research signal, got {other:?}")),
            },
            other => return Err(format!("expected research command, got {other:?}")),
        }

        let schedule = Cli::try_parse_from([
            "ma",
            "research",
            "schedule",
            "--project",
            "demo",
            "--min-signals",
            "2",
            "--min-interval-hours",
            "3",
            "--autorun",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match schedule.into_command() {
            Command::Research(args) => match args.command() {
                ResearchCommand::Schedule(schedule) => {
                    assert_eq!(schedule.project(), "demo");
                    assert_eq!(schedule.min_signals(), 2);
                    assert_eq!(schedule.min_interval_hours(), 3);
                    assert!(schedule.autorun());
                    assert!(schedule.json());
                }
                other => return Err(format!("expected research schedule, got {other:?}")),
            },
            other => return Err(format!("expected research command, got {other:?}")),
        }

        let run = Cli::try_parse_from([
            "ma",
            "research",
            "run-pending",
            "--project",
            "demo",
            "--dry-run",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match run.into_command() {
            Command::Research(args) => match args.command() {
                ResearchCommand::RunPending(run) => {
                    assert_eq!(run.project(), "demo");
                    assert!(run.dry_run());
                    assert!(run.json());
                }
                other => return Err(format!("expected research run-pending, got {other:?}")),
            },
            other => return Err(format!("expected research command, got {other:?}")),
        }

        let run_batch = Cli::try_parse_from([
            "ma",
            "research",
            "run-batch",
            "--tag",
            "auto-test",
            "--project-root",
            "/tmp/repo",
            "--gemini-home",
            "/tmp/gemini-home",
            "--gemini-bin",
            "/tmp/gemini",
            "--model",
            "gemini-3.1-pro-preview",
            "--min-experiments",
            "2",
            "--timeout-seconds",
            "60",
            "--dry-run",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match run_batch.into_command() {
            Command::Research(args) => match args.command() {
                ResearchCommand::RunBatch(batch) => {
                    assert_eq!(batch.tag(), "auto-test");
                    assert_eq!(batch.project_root(), Some(Path::new("/tmp/repo")));
                    assert_eq!(batch.gemini_home(), Some(Path::new("/tmp/gemini-home")));
                    assert_eq!(batch.gemini_bin(), Some(Path::new("/tmp/gemini")));
                    assert_eq!(batch.model(), "gemini-3.1-pro-preview");
                    assert_eq!(batch.min_experiments(), 2);
                    assert_eq!(batch.timeout_seconds(), 60);
                    assert!(batch.dry_run());
                    assert!(batch.json());
                }
                other => return Err(format!("expected research run-batch, got {other:?}")),
            },
            other => return Err(format!("expected research command, got {other:?}")),
        }

        let aggregate = Cli::try_parse_from([
            "ma",
            "research",
            "aggregate",
            "--tag",
            "rust-candidate",
            "--worktree",
            "memory-quality=/tmp/quality",
            "--worktree",
            "memory-usage=/tmp/usage",
            "--out",
            "/tmp/aggregate.jsonl",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match aggregate.into_command() {
            Command::Research(args) => match args.command() {
                ResearchCommand::Aggregate(aggregate) => {
                    assert_eq!(aggregate.tag(), "rust-candidate");
                    assert_eq!(
                        aggregate.worktrees(),
                        &[
                            "memory-quality=/tmp/quality".to_string(),
                            "memory-usage=/tmp/usage".to_string()
                        ]
                    );
                    assert_eq!(aggregate.out(), Path::new("/tmp/aggregate.jsonl"));
                    assert!(aggregate.json());
                }
                other => return Err(format!("expected research aggregate, got {other:?}")),
            },
            other => return Err(format!("expected research command, got {other:?}")),
        }

        let metric_diff = Cli::try_parse_from([
            "ma",
            "research",
            "metric-diff",
            "--baseline",
            "/tmp/python.jsonl",
            "--candidate",
            "/tmp/rust.jsonl",
            "--baseline-tag",
            "python-baseline",
            "--candidate-tag",
            "rust-candidate",
            "--threshold-percent",
            "4.5",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match metric_diff.into_command() {
            Command::Research(args) => match args.command() {
                ResearchCommand::MetricDiff(diff) => {
                    assert_eq!(diff.baseline(), Path::new("/tmp/python.jsonl"));
                    assert_eq!(diff.candidate(), Path::new("/tmp/rust.jsonl"));
                    assert_eq!(diff.baseline_tag(), Some("python-baseline"));
                    assert_eq!(diff.candidate_tag(), Some("rust-candidate"));
                    assert_eq!(diff.threshold_percent(), "4.5");
                    assert!(diff.json());
                }
                other => return Err(format!("expected research metric-diff, got {other:?}")),
            },
            other => return Err(format!("expected research command, got {other:?}")),
        }

        let gemini_auth = Cli::try_parse_from([
            "ma",
            "research",
            "gemini-auth",
            "--home",
            "/tmp/gemini-home",
            "--model",
            "gemini-3.1-pro-preview",
            "--check",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match gemini_auth.into_command() {
            Command::Research(args) => match args.command() {
                ResearchCommand::GeminiAuth(auth) => {
                    assert_eq!(auth.home(), Some(Path::new("/tmp/gemini-home")));
                    assert_eq!(auth.model(), "gemini-3.1-pro-preview");
                    assert!(auth.check());
                    assert!(auth.json());
                }
                other => return Err(format!("expected research gemini-auth, got {other:?}")),
            },
            other => return Err(format!("expected research command, got {other:?}")),
        }

        Ok(())
    }

    #[test]
    fn parses_background_commands() -> Result<(), String> {
        let run = Cli::try_parse_from([
            "ma",
            "background",
            "run",
            "--project",
            "demo",
            "--base-dir",
            "/tmp/ma-projects",
            "--now",
            "2026-04-03T12:00:00+00:00",
            "--dry-run",
            "--allow-outside-window",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match run.into_command() {
            Command::Background(args) => match args.command() {
                BackgroundCommand::Run(run) => {
                    assert_eq!(run.project(), "demo");
                    assert_eq!(run.base_dir(), Some(Path::new("/tmp/ma-projects")));
                    assert_eq!(run.now(), Some("2026-04-03T12:00:00+00:00"));
                    assert!(run.dry_run());
                    assert!(run.allow_outside_window());
                    assert!(run.json());
                }
                other => return Err(format!("expected background run, got {other:?}")),
            },
            other => return Err(format!("expected background command, got {other:?}")),
        }

        let state = Cli::try_parse_from([
            "ma",
            "background",
            "state",
            "--project",
            "demo",
            "--base-dir",
            "/tmp/ma-projects",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match state.into_command() {
            Command::Background(args) => match args.command() {
                BackgroundCommand::State(state) => {
                    assert_eq!(state.project(), "demo");
                    assert_eq!(state.base_dir(), Some(Path::new("/tmp/ma-projects")));
                    assert!(state.json());
                }
                other => return Err(format!("expected background state, got {other:?}")),
            },
            other => return Err(format!("expected background command, got {other:?}")),
        }

        let tick = Cli::try_parse_from([
            "ma",
            "background",
            "tick",
            "--project",
            "demo",
            "--base-dir",
            "/tmp/ma-projects",
            "--now",
            "2026-04-03T12:00:00+00:00",
            "--note",
            "still alive",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match tick.into_command() {
            Command::Background(args) => match args.command() {
                BackgroundCommand::Tick(tick) => {
                    assert_eq!(tick.project(), "demo");
                    assert_eq!(tick.base_dir(), Some(Path::new("/tmp/ma-projects")));
                    assert_eq!(tick.now(), Some("2026-04-03T12:00:00+00:00"));
                    assert_eq!(tick.note(), "still alive");
                    assert!(tick.json());
                }
                other => return Err(format!("expected background tick, got {other:?}")),
            },
            other => return Err(format!("expected background command, got {other:?}")),
        }

        let sleep = Cli::try_parse_from([
            "ma",
            "background",
            "sleep",
            "--project",
            "demo",
            "--base-dir",
            "/tmp/ma-projects",
            "--now",
            "2026-04-03T12:00:00+00:00",
            "--seconds",
            "60",
            "--reason",
            "quiet window",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match sleep.into_command() {
            Command::Background(args) => match args.command() {
                BackgroundCommand::Sleep(sleep) => {
                    assert_eq!(sleep.project(), "demo");
                    assert_eq!(sleep.base_dir(), Some(Path::new("/tmp/ma-projects")));
                    assert_eq!(sleep.now(), Some("2026-04-03T12:00:00+00:00"));
                    assert_eq!(sleep.seconds(), 60);
                    assert_eq!(sleep.reason(), "quiet window");
                    assert!(sleep.json());
                }
                other => return Err(format!("expected background sleep, got {other:?}")),
            },
            other => return Err(format!("expected background command, got {other:?}")),
        }

        let webhook = Cli::try_parse_from([
            "ma",
            "background",
            "webhook",
            "--project",
            "demo",
            "--base-dir",
            "/tmp/ma-projects",
            "--now",
            "2026-04-03T12:00:00+00:00",
            "--at",
            "2026-04-03T11:59:00+00:00",
            "--summary",
            "push received",
            "--repository",
            "memra/memra",
            "--event-name",
            "push",
            "--changed-path",
            "ACTIVE.md",
            "--living-doc",
            "PROJECT_FACTS.md",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match webhook.into_command() {
            Command::Background(args) => match args.command() {
                BackgroundCommand::Webhook(webhook) => {
                    assert_eq!(webhook.project(), "demo");
                    assert_eq!(webhook.base_dir(), Some(Path::new("/tmp/ma-projects")));
                    assert_eq!(webhook.now(), Some("2026-04-03T12:00:00+00:00"));
                    assert_eq!(webhook.at(), Some("2026-04-03T11:59:00+00:00"));
                    assert_eq!(webhook.summary(), "push received");
                    assert_eq!(webhook.repository(), "memra/memra");
                    assert_eq!(webhook.event_name(), "push");
                    assert_eq!(webhook.changed_paths(), &["ACTIVE.md".to_string()]);
                    assert_eq!(webhook.living_docs(), &["PROJECT_FACTS.md".to_string()]);
                    assert!(webhook.json());
                }
                other => return Err(format!("expected background webhook, got {other:?}")),
            },
            other => return Err(format!("expected background command, got {other:?}")),
        }

        let wake = Cli::try_parse_from([
            "ma",
            "background",
            "wake",
            "--project",
            "demo",
            "--base-dir",
            "/tmp/ma-projects",
            "--now",
            "2026-04-03T12:00:00+00:00",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match wake.into_command() {
            Command::Background(args) => match args.command() {
                BackgroundCommand::Wake(wake) => {
                    assert_eq!(wake.project(), "demo");
                    assert_eq!(wake.base_dir(), Some(Path::new("/tmp/ma-projects")));
                    assert_eq!(wake.now(), Some("2026-04-03T12:00:00+00:00"));
                    assert!(wake.json());
                }
                other => return Err(format!("expected background wake, got {other:?}")),
            },
            other => return Err(format!("expected background command, got {other:?}")),
        }

        Ok(())
    }

    #[test]
    fn parses_search_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "search",
            "release workflow",
            "--project",
            "alpha",
            "--limit",
            "4",
            "--json",
            "--layer",
            "event_log",
            "--category",
            "event",
            "--mode",
            "lexical",
            "--min-score",
            "0.1",
            "--include-constitution",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Search(args) => {
                assert_eq!(args.query(), "release workflow");
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.limit(), 4);
                assert!(args.json());
                assert_eq!(args.layer(), Some("event_log"));
                assert_eq!(args.category(), Some("event"));
                assert_eq!(args.search_mode(), Some("lexical"));
                assert_eq!(args.min_score(), 0.1);
                assert!(args.include_constitution());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                assert!(!args.no_forward());
                assert!(args.forward_socket().is_none());
                Ok(())
            }
            other => Err(format!("expected search command, got {other:?}")),
        }
    }

    #[test]
    fn parses_default_min_score_thresholds() -> Result<(), String> {
        let parsed = Cli::try_parse_from(["ma", "search", "release workflow"])
            .map_err(|error| error.to_string())?;
        match parsed.into_command() {
            Command::Search(args) => assert_eq!(args.min_score(), DEFAULT_MIN_SCORE),
            other => return Err(format!("expected search command, got {other:?}")),
        }

        let parsed = Cli::try_parse_from([
            "ma",
            "bench",
            "retrieval",
            "--query-file",
            "/tmp/queries.tsv",
        ])
        .map_err(|error| error.to_string())?;
        match parsed.into_command() {
            Command::Bench(args) => match args.command() {
                BenchCommand::Retrieval(retrieval) => {
                    assert_eq!(retrieval.min_score(), DEFAULT_MIN_SCORE);
                }
            },
            other => return Err(format!("expected bench retrieval command, got {other:?}")),
        }

        let parsed = Cli::try_parse_from(["ma", "recall", "u-1", "project continuity"])
            .map_err(|error| error.to_string())?;
        match parsed.into_command() {
            Command::Recall(args) => assert_eq!(args.min_score(), DEFAULT_MIN_SCORE),
            other => return Err(format!("expected recall command, got {other:?}")),
        }

        Ok(())
    }

    #[test]
    fn parses_bench_retrieval_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "bench",
            "retrieval",
            "--project",
            "alpha",
            "--query-file",
            "/tmp/queries.tsv",
            "--limit",
            "7",
            "--min-score",
            "0.4",
            "--db",
            "/tmp/alpha.sqlite3",
            "--json",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Bench(args) => match args.command() {
                BenchCommand::Retrieval(retrieval) => {
                    assert_eq!(retrieval.project(), "alpha");
                    assert_eq!(retrieval.query_file(), Path::new("/tmp/queries.tsv"));
                    assert_eq!(retrieval.limit(), 7);
                    assert_eq!(retrieval.min_score(), 0.4);
                    assert_eq!(retrieval.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                    assert!(retrieval.json());
                    Ok(())
                }
            },
            other => Err(format!("expected bench retrieval command, got {other:?}")),
        }
    }

    #[test]
    fn parses_recall_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "recall",
            "u-1",
            "project continuity",
            "--project",
            "alpha",
            "--limit",
            "2",
            "--min-score",
            "0.1",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Recall(args) => {
                assert_eq!(args.user_id(), "u-1");
                assert_eq!(args.query(), "project continuity");
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.project_or_user(), "alpha");
                assert_eq!(args.limit(), 2);
                assert_eq!(args.min_score(), 0.1);
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected recall command, got {other:?}")),
        }
    }

    #[test]
    fn recall_project_defaults_to_user_id() -> Result<(), String> {
        let parsed = Cli::try_parse_from(["ma", "recall", "u-1", "project continuity"])
            .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Recall(args) => {
                assert_eq!(args.user_id(), "u-1");
                assert_eq!(args.project(), None);
                assert_eq!(args.project_or_user(), "u-1");
                Ok(())
            }
            other => Err(format!("expected recall command, got {other:?}")),
        }
    }

    #[test]
    fn parses_add_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "add",
            "Memra prefers Rust CLI parity",
            "--project",
            "alpha",
            "--confidence",
            "0.8",
            "--category",
            "preference",
            "--layer",
            "fact",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Add(args) => {
                assert_eq!(args.content(), "Memra prefers Rust CLI parity");
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.confidence(), 0.8);
                assert_eq!(args.category(), Some("preference"));
                assert_eq!(args.layer(), "fact");
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected add command, got {other:?}")),
        }
    }

    #[test]
    fn add_project_is_optional_for_cwd_default() -> Result<(), String> {
        let parsed = Cli::try_parse_from(["ma", "add", "Use .ma-project by default"])
            .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Add(args) => {
                assert_eq!(args.content(), "Use .ma-project by default");
                assert_eq!(args.project(), None);
                Ok(())
            }
            other => Err(format!("expected add command, got {other:?}")),
        }
    }

    #[test]
    fn parses_remember_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "remember",
            "u-1",
            "Use the M1 for heavy tests",
            "--project",
            "alpha",
            "--confidence",
            "0.85",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Remember(args) => {
                assert_eq!(args.user_id(), "u-1");
                assert_eq!(args.text(), "Use the M1 for heavy tests");
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.project_or_user(), "alpha");
                assert_eq!(args.confidence(), 0.85);
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected remember command, got {other:?}")),
        }
    }

    #[test]
    fn remember_project_defaults_to_user_id() -> Result<(), String> {
        let parsed = Cli::try_parse_from(["ma", "remember", "u-1", "Use M1 for heavy tests"])
            .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Remember(args) => {
                assert_eq!(args.user_id(), "u-1");
                assert_eq!(args.project(), None);
                assert_eq!(args.project_or_user(), "u-1");
                Ok(())
            }
            other => Err(format!("expected remember command, got {other:?}")),
        }
    }

    #[test]
    fn parses_feedback_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "feedback",
            "note-1",
            "confirmed",
            "--project",
            "alpha",
            "--reason",
            "real recall smoke passed",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Feedback(args) => {
                assert_eq!(args.memory_id(), "note-1");
                assert_eq!(args.outcome(), BatchReportOutcomeValue::Confirmed);
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.reason(), Some("real recall smoke passed"));
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected feedback command, got {other:?}")),
        }
    }

    #[test]
    fn parses_confirm_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "confirm",
            "note-1",
            "--project",
            "alpha",
            "--since",
            "24h",
            "--source",
            "ai",
            "--yes",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Confirm(args) => {
                assert_eq!(args.memory_id(), Some("note-1"));
                assert_eq!(args.project(), "alpha");
                assert_eq!(args.since(), Some("24h"));
                assert_eq!(args.source(), "ai");
                assert!(args.yes());
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected confirm command, got {other:?}")),
        }
    }

    #[test]
    fn parses_experience_proof_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "experience-proof",
            "AI slop",
            "--project",
            "alpha",
            "--limit",
            "4",
            "--target",
            "hermes",
            "--save",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::ExperienceProof(args) => {
                assert_eq!(args.topic(), "AI slop");
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.limit(), 4);
                assert_eq!(args.target(), ExperienceProofTarget::Hermes);
                assert!(args.save());
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected experience-proof command, got {other:?}")),
        }
    }

    #[test]
    fn parses_timeline_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "timeline",
            "Alice",
            "--project",
            "alpha",
            "--limit",
            "12",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Timeline(args) => {
                assert_eq!(args.entity(), "Alice");
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.limit(), 12);
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected timeline command, got {other:?}")),
        }
    }

    #[test]
    fn parses_context_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "context",
            "u-1",
            "--project",
            "alpha",
            "--mode",
            "wake",
            "--force-refresh",
            "--token-budget",
            "300",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Context(args) => {
                assert_eq!(args.user_id(), Some("u-1"));
                assert_eq!(args.project(), "alpha");
                assert_eq!(args.mode(), "wake");
                assert!(args.force_refresh());
                assert_eq!(args.token_budget(), 300);
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected context command, got {other:?}")),
        }
    }

    #[test]
    fn parses_checkpoint_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "checkpoint",
            "task-1",
            "current state",
            "--project",
            "alpha",
            "--status",
            "blocked",
            "--next-step",
            "unblock it",
            "--blocker",
            "waiting",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Checkpoint(args) => {
                assert_eq!(args.task_id(), "task-1");
                assert_eq!(args.state(), "current state");
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.task_status(), "blocked");
                assert_eq!(args.next_step(), Some("unblock it"));
                assert_eq!(args.blocker(), Some("waiting"));
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected checkpoint command, got {other:?}")),
        }
    }

    #[test]
    fn parses_resume_command() -> Result<(), String> {
        let parsed = Cli::try_parse_from([
            "ma",
            "resume",
            "continue task-1",
            "--user-id",
            "u-1",
            "--project",
            "alpha",
            "--limit",
            "2",
            "--json",
            "--db",
            "/tmp/alpha.sqlite3",
        ])
        .map_err(|error| error.to_string())?;

        match parsed.into_command() {
            Command::Resume(args) => {
                assert_eq!(args.query(), "continue task-1");
                assert_eq!(args.user_id(), Some("u-1"));
                assert_eq!(args.project(), Some("alpha"));
                assert_eq!(args.limit(), 2);
                assert!(args.json());
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                Ok(())
            }
            other => Err(format!("expected resume command, got {other:?}")),
        }
    }

    #[test]
    fn parses_stats_demo_and_recover_commands() -> Result<(), String> {
        let stats = Cli::try_parse_from(["ma", "stats", "--project", "alpha", "--json", "--full"])
            .map_err(|error| error.to_string())?;
        match stats.into_command() {
            Command::Stats(args) => {
                assert_eq!(args.project(), "alpha");
                assert!(args.json());
                assert!(args.full());
            }
            other => return Err(format!("expected stats command, got {other:?}")),
        }

        let status =
            Cli::try_parse_from(["ma", "status", "--project", "alpha", "--json", "--full"])
                .map_err(|error| error.to_string())?;
        match status.into_command() {
            Command::Stats(args) => {
                assert_eq!(args.project(), "alpha");
                assert!(args.json());
                assert!(args.full());
            }
            other => return Err(format!("expected status alias to stats, got {other:?}")),
        }

        let pulse = Cli::try_parse_from(["ma", "pulse", "--project", "alpha"])
            .map_err(|error| error.to_string())?;
        match pulse.into_command() {
            Command::Pulse(args) => {
                assert_eq!(args.project(), "alpha");
            }
            other => return Err(format!("expected pulse command, got {other:?}")),
        }

        let grade = Cli::try_parse_from([
            "ma",
            "grade",
            "--apply",
            "--skip-d",
            "--date",
            "2026-05-15",
            "--orchestration",
            "/tmp/orchestration.json",
            "--evidence",
            "/tmp/evidence.json",
            "--dogfood-dir",
            "/tmp/dogfood",
            "--pending-dir",
            "/tmp/pending",
        ])
        .map_err(|error| error.to_string())?;
        match grade.into_command() {
            Command::Grade(args) => {
                assert!(args.apply());
                assert!(args.skip_d());
                assert_eq!(args.date(), Some("2026-05-15"));
                assert_eq!(
                    args.orchestration_path(),
                    Some(Path::new("/tmp/orchestration.json"))
                );
                assert_eq!(args.evidence_path(), Some(Path::new("/tmp/evidence.json")));
                assert_eq!(args.dogfood_dir(), Some(Path::new("/tmp/dogfood")));
                assert_eq!(args.pending_dir(), Some(Path::new("/tmp/pending")));
            }
            other => return Err(format!("expected grade command, got {other:?}")),
        }

        let consolidate_now = Cli::try_parse_from(["ma", "consolidate", "now-ms"])
            .map_err(|error| error.to_string())?;
        match consolidate_now.into_command() {
            Command::Consolidate(args) => match args.command() {
                ConsolidateCommand::NowMs => {}
                other => return Err(format!("expected consolidate now-ms, got {other:?}")),
            },
            other => return Err(format!("expected consolidate command, got {other:?}")),
        }

        let wiki = Cli::try_parse_from([
            "ma",
            "wiki",
            "sync",
            "--project",
            "alpha",
            "--out",
            "/tmp/wiki",
            "--db",
            "/tmp/alpha.sqlite3",
            "--limit",
            "5",
            "--dry-run",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match wiki.into_command() {
            Command::Wiki(args) => match args.command() {
                WikiCommand::Sync(sync) => {
                    assert_eq!(sync.project(), "alpha");
                    assert_eq!(sync.out_dir(), Some(Path::new("/tmp/wiki")));
                    assert_eq!(sync.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                    assert_eq!(sync.limit(), Some(5));
                    assert!(sync.dry_run());
                    assert!(sync.json());
                }
                other => return Err(format!("expected wiki sync command, got {other:?}")),
            },
            other => return Err(format!("expected wiki command, got {other:?}")),
        }

        let wiki_backfill = Cli::try_parse_from([
            "ma",
            "wiki",
            "backfill",
            "--project",
            "alpha",
            "--wiki-dir",
            "/tmp/wiki",
            "--db",
            "/tmp/alpha.sqlite3",
            "--limit",
            "5",
            "--dry-run",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match wiki_backfill.into_command() {
            Command::Wiki(args) => match args.command() {
                WikiCommand::Backfill(backfill) => {
                    assert_eq!(backfill.project(), "alpha");
                    assert_eq!(backfill.wiki_dir(), Some(Path::new("/tmp/wiki")));
                    assert_eq!(backfill.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                    assert_eq!(backfill.limit(), Some(5));
                    assert!(backfill.dry_run());
                    assert!(backfill.json());
                }
                other => return Err(format!("expected wiki backfill command, got {other:?}")),
            },
            other => return Err(format!("expected wiki command, got {other:?}")),
        }

        let ingest = Cli::try_parse_from([
            "ma",
            "ingest",
            "--project",
            "alpha",
            "--root",
            "/tmp/rules",
            "--config",
            "/tmp/ingest-roots.yaml",
            "--state",
            "/tmp/ingest-state.json",
            "--db",
            "/tmp/alpha.sqlite3",
            "--limit",
            "7",
            "--force",
            "--dry-run",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match ingest.into_command() {
            Command::Ingest(args) => {
                assert_eq!(args.project(), "alpha");
                assert_eq!(args.roots(), &[Path::new("/tmp/rules").to_path_buf()]);
                assert_eq!(
                    args.config_path(),
                    Some(Path::new("/tmp/ingest-roots.yaml"))
                );
                assert_eq!(args.state_path(), Some(Path::new("/tmp/ingest-state.json")));
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                assert_eq!(args.limit(), Some(7));
                assert!(args.force());
                assert!(args.dry_run());
                assert!(args.json());
            }
            other => return Err(format!("expected ingest command, got {other:?}")),
        }

        let review = Cli::try_parse_from([
            "ma",
            "review-queue",
            "--project",
            "alpha",
            "--source",
            "meta-fact",
            "--limit",
            "3",
            "--json",
        ])
        .map_err(|error| error.to_string())?;
        match review.into_command() {
            Command::ReviewQueue(args) => {
                assert_eq!(args.project(), "alpha");
                assert_eq!(args.source(), ReviewQueueSource::MetaFact);
                assert_eq!(args.limit(), 3);
                assert!(args.json());
            }
            other => return Err(format!("expected review-queue command, got {other:?}")),
        }

        let resolve = Cli::try_parse_from([
            "ma",
            "review-resolve",
            "--project",
            "alpha",
            "--db",
            "/tmp/alpha.sqlite3",
            "--source",
            "meta-fact",
            "--id",
            "candidate-1",
            "--verdict",
            "reject",
            "--reason",
            "duplicate was false positive",
        ])
        .map_err(|error| error.to_string())?;
        match resolve.into_command() {
            Command::ReviewResolve(args) => {
                assert_eq!(args.project(), "alpha");
                assert_eq!(args.db_path(), Some(Path::new("/tmp/alpha.sqlite3")));
                assert_eq!(args.source(), ReviewResolveSource::MetaFact);
                assert_eq!(args.id(), "candidate-1");
                assert_eq!(args.verdict(), ReviewResolveVerdict::Reject);
                assert_eq!(args.reason(), Some("duplicate was false positive"));
            }
            other => return Err(format!("expected review-resolve command, got {other:?}")),
        }

        let batch = Cli::try_parse_from([
            "ma",
            "batch-write",
            "--project",
            "alpha",
            "--db",
            "/tmp/alpha.sqlite3",
            "upsert-relation",
            "--from-id",
            "a",
            "--to-id",
            "b",
            "--relation-type",
            "supports",
            "--mode",
            "max",
            "--strength",
            "0.8",
        ])
        .map_err(|error| error.to_string())?;
        match batch.into_command() {
            Command::BatchWrite(args) => {
                assert_eq!(args.project(), "alpha");
                assert_eq!(
                    args.db_path(),
                    Some(std::path::Path::new("/tmp/alpha.sqlite3"))
                );
                match args.command() {
                    BatchWriteCommand::UpsertRelation(inner) => {
                        assert_eq!(inner.source_id(), "a");
                        assert_eq!(inner.target_id(), "b");
                        assert_eq!(inner.relation_type(), "supports");
                        assert_eq!(inner.mode(), BatchRelationMode::Max);
                        assert_eq!(inner.strength(), 0.8);
                    }
                    other => {
                        return Err(format!("expected upsert-relation command, got {other:?}"));
                    }
                }
            }
            other => return Err(format!("expected batch-write command, got {other:?}")),
        }

        let report_outcome = Cli::try_parse_from([
            "ma",
            "batch-write",
            "--project",
            "alpha",
            "--db",
            "/tmp/alpha.sqlite3",
            "report-outcome",
            "--memory-id",
            "note-1",
            "--outcome",
            "confirmed",
            "--reason",
            "real recall smoke passed",
        ])
        .map_err(|error| error.to_string())?;
        match report_outcome.into_command() {
            Command::BatchWrite(args) => {
                assert_eq!(args.project(), "alpha");
                assert_eq!(
                    args.db_path(),
                    Some(std::path::Path::new("/tmp/alpha.sqlite3"))
                );
                match args.command() {
                    BatchWriteCommand::ReportOutcome(inner) => {
                        assert_eq!(inner.memory_id(), "note-1");
                        assert_eq!(inner.outcome(), BatchReportOutcomeValue::Confirmed);
                        assert_eq!(inner.reason(), Some("real recall smoke passed"));
                    }
                    other => {
                        return Err(format!("expected report-outcome command, got {other:?}"));
                    }
                }
            }
            other => return Err(format!("expected batch-write command, got {other:?}")),
        }

        let demo = Cli::try_parse_from(["ma", "demo", "--project", "alpha", "--force"])
            .map_err(|error| error.to_string())?;
        match demo.into_command() {
            Command::Demo(args) => {
                assert_eq!(args.project(), "alpha");
                assert!(args.force());
            }
            other => return Err(format!("expected demo command, got {other:?}")),
        }

        let recover =
            Cli::try_parse_from(["ma", "recover", "--project", "alpha", "--dry-run", "--yes"])
                .map_err(|error| error.to_string())?;
        match recover.into_command() {
            Command::Recover(args) => {
                assert_eq!(args.project(), "alpha");
                assert!(args.dry_run());
                assert!(args.yes());
            }
            other => return Err(format!("expected recover command, got {other:?}")),
        }

        Ok(())
    }
}
