//! The CLI. Onboarding is two questions; everything after that is a file.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use fleet::*;

mod cmd;
mod cmd2;
mod tui;

use cmd as c1;
use cmd2 as c2;

#[derive(Parser, Debug)]
#[command(
    name = "fleet",
    version,
    about = "A harness for harnesses: route one task across many agent CLIs, each choosing its own harness and model per call based on their strengths.",
    long_about = "fleet is an orchestrator over harnesses. A harness is a TOML file in \
                  $FLEET_HOME/harnesses — add one by writing a file, remove one by deleting \
                  it, and neither needs a restart. Each call asks a chooser (Jev, or any \
                  alternative, or nothing at all) which model to run with."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Print machine-readable output.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a task across one or more harnesses.
    Run(RunArgs),

    /// Interactive terminal session: a REPL over the engine.
    ///
    /// The replacement for an `omp` TUI session — same conversation, same
    /// harnesses, resumable from anywhere over ssh or tmux.
    Tui(TuiArgs),

    /// Manage harnesses: the files ARE the registration.
    #[command(subcommand)]
    Harness(HarnessCmd),

    /// Manage providers: declared once, usable by every harness.
    #[command(subcommand)]
    Providers(ProvidersCmd),

    /// Manage plugins: engine-level hooks that apply to every harness.
    #[command(subcommand)]
    Plugins(PluginsCmd),

    /// Peer fleet nodes: reach them over Tailscale SSH, SSH, or Reason Network.
    #[command(subcommand)]
    Peers(PeersCmd),

    /// Sessions: every conversation is stored in full and can be resumed.
    #[command(subcommand)]
    Sessions(SessionsCmd),

    /// Shared memory: durable, last-version-wins, with full history.
    #[command(subcommand)]
    Memory(MemoryCmd),

    /// Verifier-gated self-modification.
    #[command(subcommand)]
    Selfmod(SelfmodCmd),

    /// What the engine learned from its own runs.
    #[command(subcommand)]
    Lessons(LessonsCmd),

    /// The append-only ledger.
    Ledger(LedgerArgs),

    /// The HTTP API, for a hosted or local web app.
    Serve(ServeArgs),

    /// One-line deploy to a server.
    Deploy(DeployArgs),

    /// Deploy targets: plugins that teach `deploy` a new platform.
    #[command(subcommand)]
    Targets(TargetsCmd),

    /// First run: pick a chooser, pick a provider, detect harnesses.
    Onboard(OnboardArgs),

    /// Credentials: inspect where a key comes from, and force a refresh.
    #[command(subcommand)]
    Credential(CredentialCmd),

    /// Configuration.
    #[command(subcommand)]
    Config(ConfigCmd),

    /// The catalog: everything the fleet can detect, install, and wire.
    #[command(subcommand)]
    Catalog(CatalogCmd),

    /// Find drift between what is declared, installed, and present; and heal it.
    Doctor(DoctorArgs),

    /// The key inventory: what the fleet has, where from, and what is missing.
    #[command(subcommand)]
    Keys(KeysCmd),

    /// Cloud VM runners: run a harness on a machine you do not own.
    #[command(subcommand)]
    Runners(RunnersCmd),

    /// Texting, calling, and email — the fleet's own channels.
    #[command(subcommand)]
    Comms(CommsCmd),

    /// Money: balances, quotes, the fleet's wallet, and gated payments.
    #[command(subcommand)]
    Pay(PayCmd),

    /// Domains: what a name costs, and pointing it somewhere.
    #[command(subcommand)]
    Domain(DomainCmd),

    /// Computer use: which lane drives a screen, and where it runs.
    #[command(subcommand)]
    Computer(ComputerCmd),

    /// Where everything lives.
    Paths,
}

#[derive(Args, Debug)]
pub struct TuiArgs {
    /// Resume a session by id, or `last` for the most recent one.
    #[arg(long, short = 'r')]
    pub resume: Option<String>,

    /// Start pinned to one harness. `auto` (the default) lets the planner choose.
    #[arg(long, short = 'H', default_value = "auto")]
    pub harness: String,

    /// Start in direct-model mode: call this model with no agent CLI in between.
    #[arg(long)]
    pub model: Option<String>,

    /// Which provider serves `--model`.
    #[arg(long)]
    pub provider: Option<String>,
}

#[derive(Args, Debug)]
struct RunArgs {
    /// The task. Omit it to resume the last session.
    task: Vec<String>,

    /// Force one harness. `auto` lets the planner decide.
    #[arg(long, short = 'H', default_value = "auto")]
    harness: String,

    /// Every plausible harness answers the same brief; the answers are merged.
    #[arg(long)]
    unison: bool,

    /// Pipeline: each harness sees what the ones before it produced.
    #[arg(long)]
    serial: bool,

    /// Resume a session by id, or `last` for the most recent one.
    #[arg(long, short = 'r')]
    resume: Option<String>,

    /// Name the session explicitly.
    #[arg(long, short = 's')]
    session: Option<String>,

    /// Show harness output as it streams.
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Text prepended to every brief, inline or a file path.
    #[arg(long)]
    brief: Option<String>,

    /// Skip the lessons briefing, for a clean comparison.
    #[arg(long)]
    no_lessons: bool,

    /// Only print the merged answer.
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Call a MODEL directly, with no agent CLI in between. Give the model id
    /// (`--model deepseek-v4.1-flash`); pair it with `--provider` to choose which
    /// endpoint serves it. This is the "I just want the model" path: no harness
    /// decides anything, and no CLI can fail.
    #[arg(long)]
    model: Option<String>,

    /// Which provider serves `--model`: a name from `fleet providers list`, or a
    /// base URL to use one that is not declared. Defaults to the first enabled
    /// provider that accepts the model.
    #[arg(long)]
    provider: Option<String>,

    /// The API key to use with `--provider`, when it is not the one that
    /// provider declares. Passed through, never stored.
    #[arg(long)]
    api_key: Option<String>,

    /// Accepted for compatibility with `omp -p --session-dir <dir>`.
    ///
    /// The point of accepting it is that swapping an existing caller from omp to
    /// fleet becomes a ONE-VARIABLE change (`OMP_BIN=/usr/local/bin/fleet`), not a
    /// code edit — which is what makes the migration reversible if the new path
    /// misbehaves. The directory is used as the working directory for the run, so
    /// a caller that kept per-chat state there keeps it.
    #[arg(long)]
    session_dir: Option<String>,

    /// Accepted for compatibility with `omp -p -c` (continue). Maps to resuming
    /// the most recent session.
    #[arg(long, short = 'c')]
    continue_session: bool,

    /// Accepted for compatibility with `omp -p` (print mode). It is a NO-OP:
    /// `fleet run` is already non-interactive, so there is no mode to switch.
    ///
    /// The flag exists purely so an existing caller can be repointed at fleet by
    /// changing one environment variable, which is what makes the migration
    /// reversible. A caller that keeps passing it gets the same behaviour.
    #[arg(long, short = 'p')]
    print_mode: bool,
}

#[derive(Subcommand, Debug)]
enum HarnessCmd {
    /// List the harnesses that are live right now.
    List {
        /// Re-read every descriptor instead of using the fingerprint cache.
        #[arg(long)]
        reload: bool,
    },
    /// Show one harness in full, including whether it can run.
    Show {
        name: String,
    },
    /// Is it usable right now?
    Probe {
        name: String,
    },
    /// Install a descriptor by copying it into the harness directory.
    Add {
        file: PathBuf,
    },
    /// Write a descriptor from a template and install it.
    New {
        /// Template: omp, codex, opencode, claude, grok, gemini, verifier,
        /// openai-compat, blank.
        #[arg(default_value = "blank")]
        template: String,
        /// Name for the new harness (defaults to the template name).
        #[arg(long)]
        name: Option<String>,
        /// Base URL, for the openai-compat template.
        #[arg(long)]
        base_url: Option<String>,
        /// Key env var, for the openai-compat template.
        #[arg(long)]
        key_env: Option<String>,
        /// Write the file but do not install it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete a harness file.
    Rm {
        name: String,
    },
    /// Turn one on or off without deleting the file.
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProvidersCmd {
    /// List providers and what each one is blocked from.
    List,
    /// Add or update a provider.
    Add {
        name: String,
        #[arg(long)]
        base_url: String,
        #[arg(long)]
        key_env: String,
        /// Models this provider serves. Repeatable. Default `*`.
        #[arg(long)]
        model: Vec<String>,
        /// Harnesses that must never receive this provider. Repeatable.
        #[arg(long)]
        block: Vec<String>,
    },
    /// Remove a provider.
    Rm { name: String },
    /// Show which providers each harness may use, and why.
    Check {
        /// One harness; omit for all of them.
        harness: Option<String>,
    },
    /// Discover providers from the environment.
    Detect {
        /// Write what was found.
        #[arg(long)]
        write: bool,
    },
}

#[derive(Subcommand, Debug)]
enum PluginsCmd {
    /// List plugins and the events they handle.
    List,
    /// Install a plugin directory.
    Add { path: PathBuf },
    /// Remove a plugin.
    Rm { name: String },
    /// Fire one event by hand, to see what a plugin answers.
    Test {
        name: String,
        #[arg(long, default_value = "pre_call")]
        event: String,
        #[arg(long, default_value = "sample task")]
        task: String,
    },
    /// Write a working example plugin.
    Example,
    /// Where plugins live.
    Dir,
}

#[derive(Subcommand, Debug)]
enum PeersCmd {
    /// Configured peers and whether they answer.
    List,
    /// Add a peer. The transport is probed unless pinned.
    Add {
        name: String,
        #[arg(long)]
        host: String,
        /// Pin a transport: tailscale, ssh, reason.
        #[arg(long)]
        transport: Option<String>,
        /// Extra ssh args, e.g. -p 2222.
        #[arg(long)]
        ssh_arg: Vec<String>,
    },
    /// Remove a peer.
    Rm { name: String },
    /// Peers visible on this machine that are not configured yet.
    Discover,
    /// Send a message to a peer. Falls back to email when told to.
    Send {
        name: String,
        message: Vec<String>,
        /// Send by email instead of a direct transport.
        #[arg(long)]
        email: Option<String>,
    },
    /// Read a peer's inbox.
    Inbox { name: String },
    /// Run one harness on a peer.
    Run {
        name: String,
        harness: String,
        task: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SessionsCmd {
    /// Recent sessions with turn counts.
    List {
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Print a full transcript.
    Show {
        id: String,
        #[arg(long, default_value_t = 200)]
        limit: i64,
    },
    /// The context a resume would inject.
    Context {
        id: String,
        #[arg(long, default_value_t = 20)]
        turns: i64,
    },
}

#[derive(Subcommand, Debug)]
enum MemoryCmd {
    /// Entries in a scope, newest first.
    List {
        #[arg(long, default_value = "global")]
        scope: String,
        #[arg(long, default_value = "")]
        prefix: String,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Read one key.
    Get {
        key: String,
        #[arg(long, default_value = "global")]
        scope: String,
    },
    /// Write a key. Durable before it returns.
    Put {
        key: String,
        value: Vec<String>,
        #[arg(long, default_value = "global")]
        scope: String,
    },
    /// Substring search everywhere.
    Search {
        needle: String,
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Every superseded value of a key, newest first.
    History {
        key: String,
        #[arg(long, default_value = "global")]
        scope: String,
    },
    /// Which backend the fleet remembers with, and how to switch it.
    Backend {
        /// `sqlite` (default), `holographic`, `postgres`, or `http`.
        name: Option<String>,
        /// Where the backend lives.
        #[arg(long)]
        path: Option<String>,
        /// Endpoint, for an `http` backend.
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SelfmodCmd {
    /// Show the ceiling, the verifiers, and recent marks.
    Status,
    /// Create the ceiling file and the built-in verifier.
    Init,
    /// List installed verifiers.
    Verifiers,
    /// Digest an artifact and check it against the ceiling.
    Check { target: String },
    /// Ask for admission: frame check, then verifier, then a mark.
    Propose {
        target: String,
        #[arg(long)]
        reason: String,
        /// New content: inline, or `@file`.
        #[arg(long)]
        content: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum LessonsCmd {
    /// What the fleet has learned, most-observed first.
    List {
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Record one by hand.
    Add {
        text: Vec<String>,
        #[arg(long, default_value = "manual")]
        kind: String,
    },
}

#[derive(Args, Debug)]
struct LedgerArgs {
    #[arg(long, default_value_t = 40)]
    limit: usize,
    /// Filter by kind: route, call, plan, verify, mark, lesson, harness, run.
    #[arg(long)]
    kind: Option<String>,
}

#[derive(Args, Debug)]
struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8787)]
    port: u16,
}

#[derive(Args, Debug)]
struct DeployArgs {
    /// ssh://host[:port], tailscale://host, reason://peer, docker://context,
    /// file:./dir, or a bare host.
    #[arg(long)]
    to: String,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    #[arg(long, default_value = "/usr/local")]
    prefix: String,
    /// Build a release binary first.
    #[arg(long)]
    release: bool,
    /// Start it after installing.
    #[arg(long)]
    run: bool,
    /// Print the commands without executing them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand, Debug)]
enum TargetsCmd {
    /// List the targets available, built-in and plugin.
    List,
    /// Install a target directory (or a bare target.toml).
    Add { path: PathBuf },
    /// Remove a plugin target.
    Rm { scheme: String },
    /// Write a worked example target you can copy.
    Example,
    /// Ask a target whether it can run here, and show what it would do.
    Probe {
        /// The target, as it would be written: scheme://host
        target: String,
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// Where targets live.
    Dir,
}

#[derive(Args, Debug)]
struct OnboardArgs {
    /// Non-interactive: accept the detected defaults.
    #[arg(long, short = 'y')]
    yes: bool,
    /// Chooser: jev, http, static, or a path to a command.
    #[arg(long)]
    chooser: Option<String>,
    /// Provider base URL.
    #[arg(long)]
    base_url: Option<String>,
    /// Env var holding the provider key.
    #[arg(long)]
    key_env: Option<String>,
    /// Point a domain at this node and serve the web app on it. Give the name
    /// you want; `fleet domain check <name>` says what it costs first.
    #[arg(long)]
    domain: Option<String>,
    /// Skip the domain step entirely (it is optional).
    #[arg(long)]
    no_domain: bool,
}

#[derive(Subcommand, Debug)]
enum CredentialCmd {
    /// Resolve every provider's credential and report the source. Never prints
    /// a value — only whether one was obtained and where it came from.
    Check {
        /// One provider; omit for all of them.
        provider: Option<String>,
    },
    /// Forget a cached credential and resolve it again. This is what recovers a
    /// rotated OAuth token without restarting anything.
    Refresh {
        /// One provider; omit for all of them.
        provider: Option<String>,
    },
    /// Print a credential's value. For piping into another tool, not for reading.
    Show {
        provider: String,
        /// Print only the value, with no label.
        #[arg(long)]
        raw: bool,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Print the effective configuration.
    Show,
    /// Set the chooser.
    SetChooser {
        /// jev, http, static, or a command path.
        kind: String,
    },
    /// Set the fallback model and effort.
    SetDefault {
        model: String,
        #[arg(long)]
        effort: Option<String>,
    },
    /// What the fleet optimises for: cost, speed, quality, or balanced.
    /// With no argument, show the current choice and what it means.
    Priority {
        /// cost | speed | quality | balanced
        value: Option<String>,
        /// Set this priority for one harness only.
        #[arg(long)]
        harness: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum CatalogCmd {
    /// Everything the catalog declares, with its live state.
    List {
        /// Only entries whose tool is already on this machine.
        #[arg(long)]
        present: bool,
        /// Only entries whose tool is NOT on this machine.
        #[arg(long)]
        absent: bool,
        /// Filter by kind: harness, plugin, provider, target, runner, memory, integration.
        #[arg(long)]
        kind: Option<String>,
        /// Substring match on the name, description, or tags.
        #[arg(long)]
        search: Option<String>,
    },
    /// One entry in full: how to detect it, install it, and what keys it needs.
    Show { name: String },
    /// Wire an entry into the fleet (copy its artifact into the fleet home).
    Install {
        /// One entry, or `--all` for everything present but unwired.
        name: Option<String>,
        #[arg(long)]
        all: bool,
        /// Also install the upstream tool when the catalog knows how.
        #[arg(long)]
        upstream: bool,
        /// Show what would happen without doing it.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install the upstream tool a catalog entry names (npm/pip/cargo/brew).
    Upstream {
        name: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Where the catalog lives, and how to add an entry.
    Dir,
    /// Write a starter catalog entry you can edit.
    New {
        name: String,
        #[arg(long, default_value = "harness")]
        kind: String,
    },
}

#[derive(Args, Debug)]
struct DoctorArgs {
    /// Print what healing would do, without doing it.
    #[arg(long)]
    dry_run: bool,
    /// Only report; never change anything.
    #[arg(long)]
    check: bool,
    /// Machine-readable findings.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand, Debug)]
enum KeysCmd {
    /// The expected keys, grouped, with presence and source. Never a value.
    Show {
        /// Only one group: provider, tooling, messaging, voice, money, marketing.
        #[arg(long)]
        group: Option<String>,
        /// Only the ones that are missing.
        #[arg(long)]
        missing: bool,
    },
    /// Copy keys the machine already has into $FLEET_HOME/env.
    Import {
        /// Names to import; omit with --all to import every known key found.
        names: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        dry_run: bool,
    },
    /// Keys present but not in the expected list — where new integrations come from.
    Unexpected,
    /// Where the fleet reads keys from, in precedence order.
    Sources,
}

#[derive(Subcommand, Debug)]
enum DomainCmd {
    /// Every way the fleet can get a name, and what each costs.
    Providers,
    /// What a specific name would cost, and how it would be obtained.
    Check { name: String },
    /// Concrete free names for a label, so onboarding can offer one.
    Free {
        /// The word to build a name from, e.g. a project or client name.
        label: String,
    },
    /// DNS records for a zone.
    Dns {
        zone: String,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Create a DNS record.
    Point {
        zone: String,
        /// Record name, e.g. `app` or `@`.
        name: String,
        /// A, AAAA, or CNAME.
        #[arg(long, default_value = "A")]
        kind: String,
        /// The address or target.
        content: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value_t = 300)]
        ttl: u32,
    },
}

#[derive(Subcommand, Debug)]
enum ComputerCmd {
    /// Which computer-use lane is active, and what else is available.
    Status,
    /// Choose the lane: `ego` (the fleet default), `native`, or a harness name.
    Use {
        /// ego | native | <harness-name> | off
        lane: String,
    },
}

#[derive(Subcommand, Debug)]
enum CommsCmd {
    /// Which providers are configured for each channel.
    Providers,
    /// Send an SMS. A new recipient needs --confirm.
    Sms {
        #[arg(long)]
        to: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        provider: Option<String>,
        /// Required the first time a number is messaged.
        #[arg(long)]
        confirm: bool,
    },
    /// Place a call that speaks a script.
    Call {
        #[arg(long)]
        to: String,
        #[arg(long)]
        say: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        confirm: bool,
    },
    /// Send email.
    Email {
        #[arg(long)]
        to: String,
        #[arg(long)]
        subject: String,
        #[arg(long)]
        body: String,
    },
    /// What the fleet has sent, from shared memory.
    Log {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
}

#[derive(Subcommand, Debug)]
enum PayCmd {
    /// Read-only: what rails are configured and what they can see.
    Balances {
        /// Read a live balance from a named provider.
        #[arg(long)]
        provider: Option<String>,
    },
    /// What a payment would cost and whether it would be allowed. Moves nothing.
    Quote {
        #[arg(long)]
        amount: f64,
        #[arg(long, default_value = "USD")]
        currency: String,
        #[arg(long)]
        to: String,
    },
    /// Send money. Requires --approve, and a written policy.
    Send {
        #[arg(long)]
        amount: f64,
        #[arg(long, default_value = "USD")]
        currency: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        approve: Option<String>,
        /// Actually execute, once approved.
        #[arg(long)]
        execute: bool,
    },
    /// The fleet's own crypto wallet: show its address, or make one.
    Wallet {
        #[arg(long)]
        new: bool,
    },
    /// The spending policy: what is allowed without a fresh token.
    Policy,
}

#[derive(Subcommand, Debug)]
enum RunnersCmd {
    /// Known cloud runners, what they need, and whether they are reachable.
    List,
    /// One runner in full.
    Show { name: String },
    /// Provision a machine and install the fleet on it.
    Start {
        name: String,
        /// What to call this instance.
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Talk to a Maritime machine: the task goes in, the answer comes back.
    Chat {
        /// The machine, e.g. omp-worker-1.
        machine: String,
        task: Vec<String>,
    },
    /// Run a command on a Maritime machine.
    Exec {
        machine: String,
        command: Vec<String>,
    },
    /// The visible screen on a Maritime machine: a VM with a display.
    Desktop {
        /// The machine.
        machine: String,
        /// Show the current mode (the default).
        #[arg(long)]
        status: bool,
        /// Turn the desktop on.
        #[arg(long)]
        enable: bool,
        /// Turn it off.
        #[arg(long)]
        disable: bool,
        /// Hand the screen to the agent.
        #[arg(long)]
        take: bool,
        /// Hand it back to a human.
        #[arg(long)]
        release: bool,
        /// Why the takeover happened; recorded.
        #[arg(long, default_value = "fleet")]
        reason: String,
    },
    /// Put a Maritime machine to sleep, to stop paying for compute.
    Stop { machine: String },
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("fleet: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    // Before anything can need a credential: keys live in the fleet home, not in
    // the process environment, so every entry point gets the same behaviour.
    paths::load_env_file();
    let cli = Cli::parse();
    match cli.command {
        None => {
            // Bare `fleet` is the front door: if nothing is installed yet, that
            // is the one thing worth saying.
            let reg = registry::Registry::default_dir();
            let (all, _) = reg.all();
            if all.is_empty() {
                println!("{}", c2::onboard_hint());
                return Ok(());
            }
            print!("{}", c2::help_text(&reg));
            Ok(())
        }
        Some(Command::Onboard(a)) => c2::onboard(a, cli.json),
        Some(Command::Run(a)) => c1::run(a, cli.json),
        Some(Command::Tui(a)) => tui::tui(a, cli.json),
        Some(Command::Harness(c)) => c1::harness(c, cli.json),
        Some(Command::Providers(c)) => c1::providers(c, cli.json),
        Some(Command::Plugins(c)) => c2::plugins(c, cli.json),
        Some(Command::Peers(c)) => c2::peers(c, cli.json),
        Some(Command::Sessions(c)) => c2::sessions(c, cli.json),
        Some(Command::Memory(c)) => c2::memory(c, cli.json),
        Some(Command::Selfmod(c)) => c2::selfmod(c, cli.json),
        Some(Command::Lessons(c)) => c2::lessons(c, cli.json),
        Some(Command::Ledger(a)) => c2::ledger(a, cli.json),
        Some(Command::Serve(a)) => {
            paths::ensure_dirs()?;
            api::serve(&a.host, a.port)
        }
        Some(Command::Deploy(a)) => c2::deploy(a),
        Some(Command::Targets(c)) => c2::deploy_targets(c, cli.json),
        Some(Command::Credential(c)) => c2::credential(c, cli.json),
        Some(Command::Config(c)) => c2::config(c, cli.json),
        Some(Command::Catalog(c)) => c2::catalog(c, cli.json),
        Some(Command::Doctor(a)) => c2::doctor(a),
        Some(Command::Keys(c)) => c2::keys(c, cli.json),
        Some(Command::Runners(c)) => c2::runners(c, cli.json),
        Some(Command::Comms(c)) => c2::comms(c, cli.json),
        Some(Command::Pay(c)) => c2::pay(c, cli.json),
        Some(Command::Domain(c)) => c2::domain(c, cli.json),
        Some(Command::Computer(c)) => c2::computer(c, cli.json),
        Some(Command::Paths) => {
            c2::print_paths();
            Ok(())
        }
    }
}
