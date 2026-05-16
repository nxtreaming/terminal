use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use browser_use_core::{
    record_python_response_final_event, record_python_worker_event, run_agent_from_config,
    run_existing_session_from_config, run_existing_session_with_provider, run_fake_agent,
    AgentRunOptions, FakeAgentOptions, ProviderBackend, ProviderRunConfig,
};
use browser_use_protocol::{
    browser_summary_from_events, failure_from_events, result_from_events,
    sanitized_agent_context_from_events, task_from_events,
};
use browser_use_providers::{
    claude_code_oauth_authorize_url, claude_code_oauth_pkce,
    exchange_claude_code_authorization_code, load_codex_auth, load_codex_auth_file,
    parse_claude_code_authorization_input, refresh_claude_code_oauth, AnthropicMessagesProvider,
    ClaudeCodeOAuthCredential, CodexAuth, CodexResponsesProvider, FakeProvider, ModelProvider,
    OpenAICompatibleChatProvider, OpenAIResponsesProvider, CLAUDE_CODE_CALLBACK_HOST,
    CLAUDE_CODE_CALLBACK_PATH, CLAUDE_CODE_CALLBACK_PORT,
};
use browser_use_python_worker::PythonWorker;
use browser_use_store::{now_ms, Store};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "browser-use-terminal", bin_name = "browser-use-terminal")]
#[command(about = "Rust browser-use task control")]
struct Args {
    #[arg(long, default_value = ".browser-use-terminal")]
    state_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start {
        text: String,
    },
    RunFake {
        text: String,
        #[arg(long)]
        python_code: Option<String>,
    },
    RunOpenai {
        text: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
    RunCodex {
        text: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
    RunAnthropic {
        text: String,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
    },
    RunOpenrouter {
        text: String,
        #[arg(long, default_value = "openai/gpt-5.5")]
        model: String,
    },
    RunOpenaiSession {
        task_id: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
    RunCodexSession {
        task_id: String,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
    },
    RunAnthropicSession {
        task_id: String,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
    },
    RunOpenrouterSession {
        task_id: String,
        #[arg(long, default_value = "openai/gpt-5.5")]
        model: String,
    },
    Followup {
        task_id: String,
        text: String,
    },
    Finish {
        task_id: String,
        #[arg(long)]
        result: String,
    },
    Fail {
        task_id: String,
        #[arg(long)]
        error: String,
    },
    Cancel {
        task_id: String,
        #[arg(long, default_value = "user requested cancellation")]
        reason: String,
    },
    #[command(alias = "session")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    History,
    Show {
        task_id: String,
    },
    Events {
        task_id: String,
    },
    Python {
        task_id: String,
        code: String,
    },
    Export {
        task_id: String,
        output_dir: PathBuf,
    },
    Import {
        input: PathBuf,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Diagnostics,
    Trace {
        task_id: String,
        output: Option<PathBuf>,
    },
    SpawnAgent {
        parent_id: String,
        message: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        nickname: Option<String>,
        #[arg(long)]
        role: Option<String>,
    },
    ListAgents {
        parent_id: String,
    },
    CloseAgent {
        child_id: String,
        #[arg(long, default_value = "closed by user")]
        reason: String,
    },
    SendAgentMessage {
        author_id: String,
        target_id: String,
        message: String,
        #[arg(long)]
        trigger_turn: bool,
    },
    WaitAgent {
        target_id: String,
    },
    DatasetList,
    DatasetSample {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    DatasetReport {
        run_id_or_path: String,
    },
    DatasetRunFake {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 1)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunOpenai {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunCodex {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunAnthropic {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
    DatasetRunOpenrouter {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long = "task-id")]
        task_ids: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value = "openai/gpt-5.5")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        resume: bool,
        #[arg(long)]
        skip_failed: bool,
        #[arg(long)]
        stop_on_failure: bool,
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        #[arg(long)]
        browser_mode: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Init,
    Show,
    Set { key: String, value: String },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Status,
    Login {
        account: AuthAccount,
        #[arg(long)]
        api_key: Option<String>,
        #[arg(long)]
        access_token: Option<String>,
        #[arg(long)]
        account_id: Option<String>,
        #[arg(long)]
        code: Option<String>,
        #[arg(long)]
        no_browser: bool,
    },
    ImportCodex {
        #[arg(long = "from")]
        input: Option<PathBuf>,
    },
    Logout {
        account: AuthAccount,
    },
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    List,
    Show {
        task_id: String,
    },
    Cancel {
        task_id: String,
        #[arg(long, default_value = "user requested cancellation")]
        reason: String,
    },
    Trace {
        task_id: String,
        output: Option<PathBuf>,
    },
    Export {
        task_id: String,
        output_dir: PathBuf,
    },
    Import {
        input: PathBuf,
    },
    Events {
        task_id: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AuthAccount {
    Codex,
    ClaudeCode,
    BrowserUseCloud,
    Openai,
    Anthropic,
    Openrouter,
}

#[derive(Clone, Debug, Serialize)]
struct DatasetCase {
    dataset: String,
    path: String,
    task_id: String,
    confirmed_task: String,
    raw: Value,
}

#[derive(Clone, Debug)]
struct DatasetRunOptions {
    count: usize,
    task_ids: Vec<String>,
    all: bool,
    run_id: Option<String>,
    resume: bool,
    skip_failed: bool,
    stop_on_failure: bool,
    max_attempts: usize,
    concurrency: usize,
    browser_mode: Option<String>,
}

#[derive(Clone, Debug)]
struct DatasetProviderConfig {
    provider: String,
    model: String,
    browser_mode: String,
    max_turns: usize,
    python_timeout_seconds: u64,
}

#[derive(Clone, Debug, Serialize)]
struct DatasetTaskPaths {
    root: PathBuf,
    cwd: PathBuf,
    outputs: PathBuf,
    artifacts: PathBuf,
    agent_workspace: PathBuf,
    logs: PathBuf,
    runtime: PathBuf,
    tmp: PathBuf,
}

fn main() -> Result<()> {
    load_dotenv()?;
    let args = Args::parse();
    let store = Store::open(&args.state_dir)?;
    match args.command {
        Command::Start { text } => start(&store, text),
        Command::RunFake { text, python_code } => run_fake(&store, text, python_code),
        Command::RunOpenai { text, model } => run_openai(&store, text, model),
        Command::RunCodex { text, model } => run_codex(&store, text, model),
        Command::RunAnthropic { text, model } => run_anthropic(&store, text, model),
        Command::RunOpenrouter { text, model } => run_openrouter(&store, text, model),
        Command::RunOpenaiSession { task_id, model } => run_openai_session(&store, &task_id, model),
        Command::RunCodexSession { task_id, model } => run_codex_session(&store, &task_id, model),
        Command::RunAnthropicSession { task_id, model } => {
            run_anthropic_session(&store, &task_id, model)
        }
        Command::RunOpenrouterSession { task_id, model } => {
            run_openrouter_session(&store, &task_id, model)
        }
        Command::Followup { task_id, text } => followup(&store, &task_id, text),
        Command::Finish { task_id, result } => finish(&store, &task_id, result),
        Command::Fail { task_id, error } => fail(&store, &task_id, error),
        Command::Cancel { task_id, reason } => cancel(&store, &task_id, &reason),
        Command::Sessions { command } => sessions(&store, command),
        Command::History => history(&store),
        Command::Show { task_id } => show(&store, &task_id),
        Command::Events { task_id } => events(&store, &task_id),
        Command::Python { task_id, code } => python(&store, &task_id, code),
        Command::Export {
            task_id,
            output_dir,
        } => export(&store, &task_id, output_dir),
        Command::Import { input } => import(&store, input),
        Command::Config { command } => config(&store, command),
        Command::Auth { command } => auth(&store, command),
        Command::Diagnostics => diagnostics(&store),
        Command::Trace { task_id, output } => trace(&store, &task_id, output),
        Command::SpawnAgent {
            parent_id,
            message,
            path,
            nickname,
            role,
        } => spawn_agent(&store, &parent_id, message, path, nickname, role),
        Command::ListAgents { parent_id } => list_agents(&store, &parent_id),
        Command::CloseAgent { child_id, reason } => close_agent(&store, &child_id, &reason),
        Command::SendAgentMessage {
            author_id,
            target_id,
            message,
            trigger_turn,
        } => send_agent_message(&store, &author_id, &target_id, &message, trigger_turn),
        Command::WaitAgent { target_id } => wait_agent(&store, &target_id),
        Command::DatasetList => dataset_list(),
        Command::DatasetSample {
            dataset,
            count,
            task_ids,
            all,
        } => dataset_sample(&dataset, count, task_ids, all),
        Command::DatasetReport { run_id_or_path } => dataset_report(&store, &run_id_or_path),
        Command::DatasetRunFake {
            dataset,
            count,
            task_ids,
            all,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_fake(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
        ),
        Command::DatasetRunOpenai {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_openai(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
        ),
        Command::DatasetRunCodex {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_codex(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
        ),
        Command::DatasetRunAnthropic {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_anthropic(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
        ),
        Command::DatasetRunOpenrouter {
            dataset,
            count,
            task_ids,
            all,
            model,
            max_turns,
            python_timeout_seconds,
            run_id,
            resume,
            skip_failed,
            stop_on_failure,
            max_attempts,
            concurrency,
            browser_mode,
        } => dataset_run_openrouter(
            &store,
            &dataset,
            DatasetRunOptions {
                count,
                task_ids,
                all,
                run_id,
                resume,
                skip_failed,
                stop_on_failure,
                max_attempts,
                concurrency,
                browser_mode,
            },
            model,
            max_turns,
            python_timeout_seconds,
        ),
    }
}

fn load_dotenv() -> Result<()> {
    let path = Path::new(".env");
    if !path.exists() {
        return Ok(());
    }
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let value = unquote_env_value(value.trim());
        unsafe {
            std::env::set_var(key, value);
        }
    }
    Ok(())
}

fn unquote_env_value(value: &str) -> String {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn sessions(store: &Store, command: SessionsCommand) -> Result<()> {
    match command {
        SessionsCommand::List => history(store),
        SessionsCommand::Show { task_id } => show(store, &task_id),
        SessionsCommand::Cancel { task_id, reason } => cancel(store, &task_id, &reason),
        SessionsCommand::Trace { task_id, output } => trace(store, &task_id, output),
        SessionsCommand::Export {
            task_id,
            output_dir,
        } => export(store, &task_id, output_dir),
        SessionsCommand::Import { input } => import(store, input),
        SessionsCommand::Events { task_id } => events(store, &task_id),
    }
}

fn start(store: &Store, text: String) -> Result<()> {
    let task = store.create_session(None, std::env::current_dir()?)?;
    store.append_event(
        &task.id,
        "session.input",
        serde_json::json!({ "text": text }),
    )?;
    println!("{}", task.id);
    Ok(())
}

fn run_fake(store: &Store, text: String, python_code: Option<String>) -> Result<()> {
    let session_id = run_fake_agent(
        store,
        &text,
        std::env::current_dir()?,
        FakeAgentOptions {
            python_code: python_code.as_deref(),
        },
    )?;
    println!("{session_id}");
    Ok(())
}

fn cli_agent_options() -> AgentRunOptions {
    AgentRunOptions::default().with_browser_mode(cli_browser_mode())
}

fn cli_browser_mode() -> String {
    std::env::var("LLM_BROWSER_BROWSER_MODE")
        .ok()
        .filter(|mode| !mode.trim().is_empty())
        .unwrap_or_else(|| "headless".to_string())
}

fn dataset_browser_mode(options: &DatasetRunOptions) -> String {
    options
        .browser_mode
        .as_deref()
        .filter(|mode| !mode.trim().is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(cli_browser_mode)
        .to_ascii_lowercase()
        .replace(['_', ' '], "-")
}

fn run_openai(store: &Store, text: String, model: String) -> Result<()> {
    let config =
        ProviderRunConfig::new(ProviderBackend::Openai, model).with_options(cli_agent_options());
    let session_id = run_agent_from_config(store, &text, std::env::current_dir()?, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_codex(store: &Store, text: String, model: String) -> Result<()> {
    let config =
        ProviderRunConfig::new(ProviderBackend::Codex, model).with_options(cli_agent_options());
    let session_id = run_agent_from_config(store, &text, std::env::current_dir()?, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_anthropic(store: &Store, text: String, model: String) -> Result<()> {
    let config =
        ProviderRunConfig::new(ProviderBackend::Anthropic, model).with_options(cli_agent_options());
    let session_id = run_agent_from_config(store, &text, std::env::current_dir()?, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_openrouter(store: &Store, text: String, model: String) -> Result<()> {
    let config = ProviderRunConfig::new(ProviderBackend::Openrouter, model)
        .with_options(cli_agent_options());
    let session_id = run_agent_from_config(store, &text, std::env::current_dir()?, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_openai_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config =
        ProviderRunConfig::new(ProviderBackend::Openai, model).with_options(cli_agent_options());
    let session_id = run_existing_session_from_config(store, task_id, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_codex_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config =
        ProviderRunConfig::new(ProviderBackend::Codex, model).with_options(cli_agent_options());
    let session_id = run_existing_session_from_config(store, task_id, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_anthropic_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config =
        ProviderRunConfig::new(ProviderBackend::Anthropic, model).with_options(cli_agent_options());
    let session_id = run_existing_session_from_config(store, task_id, config)?;
    println!("{session_id}");
    Ok(())
}

fn run_openrouter_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let config = ProviderRunConfig::new(ProviderBackend::Openrouter, model)
        .with_options(cli_agent_options());
    let session_id = run_existing_session_from_config(store, task_id, config)?;
    println!("{session_id}");
    Ok(())
}

fn followup(store: &Store, task_id: &str, text: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "session.followup",
        serde_json::json!({ "text": text }),
    )?;
    println!("followup {task_id}");
    Ok(())
}

fn finish(store: &Store, task_id: &str, result: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "session.done",
        serde_json::json!({ "result": result.clone() }),
    )?;
    notify_parent_agent_done(
        store,
        &task,
        "done",
        serde_json::json!({ "result": result }),
    )?;
    println!("done {task_id}");
    Ok(())
}

fn cancel(store: &Store, task_id: &str, reason: &str) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    store.request_cancel(task_id, reason)?;
    notify_parent_agent_done(
        store,
        &task,
        "cancelled",
        serde_json::json!({ "reason": reason }),
    )?;
    println!("cancelled {task_id}");
    Ok(())
}

fn fail(store: &Store, task_id: &str, error: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "session.failed",
        serde_json::json!({ "error": error.clone() }),
    )?;
    notify_parent_agent_done(
        store,
        &task,
        "failed",
        serde_json::json!({ "error": error }),
    )?;
    println!("failed {task_id}");
    Ok(())
}

fn history(store: &Store) -> Result<()> {
    let tasks = store.list_sessions()?;
    if tasks.is_empty() {
        println!("No previous work yet.");
        return Ok(());
    }
    for task in tasks {
        let events = store.events_for_session(&task.id)?;
        let title = task_from_events(&events).unwrap_or_else(|| "untitled task".to_string());
        println!("{}  {:<9}  {}", task.id, task.status.as_str(), title);
    }
    Ok(())
}

fn show(store: &Store, task_id: &str) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    let events = store.events_for_session(task_id)?;
    let title = task_from_events(&events).unwrap_or_else(|| "untitled task".to_string());
    let browser = browser_summary_from_events(&events, "local chrome");
    println!("Task: {title}");
    println!("Status: {}", task.status.as_str());
    if let Some(url) = browser.url {
        println!("Browser: {url}");
    }
    if let Some(result) = result_from_events(&events) {
        println!();
        println!("Result");
        println!("{result}");
    }
    if let Some(error) = failure_from_events(&events) {
        println!();
        println!("Failure");
        println!("{error}");
    }
    Ok(())
}

fn events(store: &Store, task_id: &str) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    for event in store.events_for_session(task_id)? {
        println!("{}", serde_json::to_string(&event)?);
    }
    Ok(())
}

fn python(store: &Store, task_id: &str, code: String) -> Result<()> {
    let task = ensure_task_exists(store, task_id)?;
    store.append_event(
        task_id,
        "tool.started",
        serde_json::json!({
            "name": "python",
            "arguments": { "code": code.clone() },
        }),
    )?;
    let mut worker = PythonWorker::start()?;
    let mut stream_error = None;
    let response =
        worker.run_with_events(task_id, &task.cwd, &task.artifact_root, &code, |event| {
            if stream_error.is_none() {
                if let Err(err) = record_python_worker_event(store, task_id, &event) {
                    stream_error = Some(err);
                }
            }
        })?;
    if let Some(err) = stream_error {
        return Err(err);
    }
    record_python_response_final_event(store, task_id, &response)?;
    if response.ok {
        store.append_event(
            task_id,
            "tool.finished",
            serde_json::json!({ "name": "python" }),
        )?;
        print!("{}", response.text);
        return Ok(());
    }
    store.append_event(
        task_id,
        "tool.failed",
        serde_json::json!({
            "name": "python",
            "error": response.error,
        }),
    )?;
    bail!(
        "{}",
        response
            .error
            .unwrap_or_else(|| "python worker failed".to_string())
    )
}

fn export(store: &Store, task_id: &str, output_dir: PathBuf) -> Result<()> {
    store.export_legacy_session(task_id, &output_dir)?;
    println!("{}", output_dir.display());
    Ok(())
}

fn import(store: &Store, input: PathBuf) -> Result<()> {
    let session = store.import_legacy_session(input)?;
    println!("{}", session.id);
    Ok(())
}

fn config(store: &Store, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Init => {
            for (key, value) in default_settings() {
                if store.get_setting(key)?.is_none() {
                    store.set_setting(key, value)?;
                }
            }
            println!(
                "initialized {}",
                store.state_dir().join("state.db").display()
            );
            Ok(())
        }
        ConfigCommand::Show => {
            let mut settings = default_settings()
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string(), true))
                .collect::<Vec<_>>();
            for (key, value) in store.list_settings()? {
                if let Some(existing) = settings.iter_mut().find(|(name, _, _)| name == &key) {
                    existing.1 = value;
                    existing.2 = false;
                } else {
                    settings.push((key, value, false));
                }
            }
            for (key, value, is_default) in settings {
                let suffix = if is_default { " (default)" } else { "" };
                let shown = if is_secret_setting(&key) {
                    "<stored>"
                } else {
                    value.as_str()
                };
                println!("{key}={shown}{suffix}");
            }
            Ok(())
        }
        ConfigCommand::Set { key, value } => {
            store.set_setting(&key, &value)?;
            println!("{key}={value}");
            Ok(())
        }
    }
}

fn default_settings() -> Vec<(&'static str, &'static str)> {
    vec![
        ("account", "Codex login"),
        ("model", "GPT-5.5"),
        ("provider.model", "gpt-5.5"),
        ("browser", "Browser Use cloud"),
        ("agent.backend", "codex"),
        ("setup.complete", "0"),
    ]
}

fn is_secret_setting(key: &str) -> bool {
    key.starts_with("auth.")
        && (key.ends_with(".api_key")
            || key.ends_with(".access_token")
            || key.ends_with(".refresh_token")
            || key.ends_with(".auth_token"))
}

const BROWSER_USE_CLOUD_API_KEY_SETTING: &str = "auth.browser_use_cloud.api_key";

fn auth(store: &Store, command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Status => {
            print_api_key_status(
                store,
                "Browser Use cloud key",
                BROWSER_USE_CLOUD_API_KEY_SETTING,
                &["BROWSER_USE_API_KEY"],
            )?;
            print_api_key_status(
                store,
                "OpenAI API key",
                "auth.openai.api_key",
                &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
            )?;
            print_codex_status(store)?;
            print_api_key_status(
                store,
                "Anthropic API key",
                "auth.anthropic.api_key",
                &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
            )?;
            print_api_key_status(
                store,
                "OpenRouter API key",
                "auth.openrouter.api_key",
                &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
            )?;
            print_claude_code_status(store)?;
            Ok(())
        }
        AuthCommand::Login {
            account,
            api_key,
            access_token,
            account_id,
            code,
            no_browser,
        } => auth_login(
            store,
            account,
            api_key,
            access_token,
            account_id,
            code,
            no_browser,
        ),
        AuthCommand::ImportCodex { input } => {
            let auth = if let Some(input) = input {
                load_codex_auth_file(input)?
            } else {
                load_codex_auth().context("load external Codex auth for import")?
            };
            store_codex_auth(store, &auth)?;
            println!("Codex login: imported account {}", auth.account_id);
            Ok(())
        }
        AuthCommand::Logout { account } => {
            auth_logout(store, account)?;
            println!("{}: logged out", auth_account_label(account));
            Ok(())
        }
    }
}

fn env_any(names: &[&str]) -> bool {
    names
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

fn print_auth_line(label: &str, connected: bool) {
    let status = if connected {
        "connected"
    } else {
        "not connected"
    };
    println!("{label}: {status}");
}

fn auth_login(
    store: &Store,
    account: AuthAccount,
    api_key: Option<String>,
    access_token: Option<String>,
    account_id: Option<String>,
    code: Option<String>,
    no_browser: bool,
) -> Result<()> {
    match account {
        AuthAccount::BrowserUseCloud => {
            let api_key =
                read_required_secret(api_key, &format!("{} API key", auth_account_label(account)))?;
            let key = api_key_setting(account).context("account does not use an API key")?;
            store.set_setting(key, api_key.trim())?;
            store.set_setting("browser", "Browser Use cloud")?;
            println!("{}: connected (stored)", auth_account_label(account));
            Ok(())
        }
        AuthAccount::Openai | AuthAccount::Anthropic | AuthAccount::Openrouter => {
            let api_key =
                read_required_secret(api_key, &format!("{} API key", auth_account_label(account)))?;
            let key = api_key_setting(account).context("account does not use an API key")?;
            store.set_setting(key, api_key.trim())?;
            store.set_setting("account", auth_account_label(account))?;
            println!("{}: connected (stored)", auth_account_label(account));
            Ok(())
        }
        AuthAccount::Codex => {
            let auth = if access_token.is_some() || account_id.is_some() {
                CodexAuth {
                    access_token: access_token
                        .context("auth login codex requires --access-token with --account-id")?,
                    account_id: account_id
                        .context("auth login codex requires --account-id with --access-token")?,
                }
            } else {
                load_codex_auth().context("load external Codex auth for login")?
            };
            store_codex_auth(store, &auth)?;
            store.set_setting("account", "Codex login")?;
            println!("Codex login: connected account {}", auth.account_id);
            Ok(())
        }
        AuthAccount::ClaudeCode => {
            let credential = claude_code_login(access_token, code, !no_browser)?;
            store_claude_code_oauth(store, &credential)?;
            store.set_setting("account", "Claude Code login")?;
            println!("Claude Code login: connected (stored OAuth credential)");
            Ok(())
        }
    }
}

fn auth_logout(store: &Store, account: AuthAccount) -> Result<()> {
    match account {
        AuthAccount::Codex => {
            store.delete_setting("auth.codex.access_token")?;
            store.delete_setting("auth.codex.account_id")?;
        }
        AuthAccount::Openai | AuthAccount::Anthropic | AuthAccount::Openrouter => {
            if let Some(key) = api_key_setting(account) {
                store.delete_setting(key)?;
            }
        }
        AuthAccount::BrowserUseCloud => {
            store.delete_setting(BROWSER_USE_CLOUD_API_KEY_SETTING)?;
        }
        AuthAccount::ClaudeCode => {
            store.delete_setting("auth.claude_code.access_token")?;
            store.delete_setting("auth.claude_code.refresh_token")?;
            store.delete_setting("auth.claude_code.expires_ms")?;
            store.delete_setting("auth.claude_code.auth_token")?;
        }
    }
    Ok(())
}

fn read_required_secret(value: Option<String>, prompt: &str) -> Result<String> {
    if let Some(value) = value {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            bail!("{prompt} cannot be empty");
        }
        return Ok(trimmed);
    }
    eprint!("{prompt}: ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        bail!("{prompt} cannot be empty");
    }
    Ok(trimmed)
}

fn claude_code_login(
    access_token: Option<String>,
    code: Option<String>,
    open_browser: bool,
) -> Result<ClaudeCodeOAuthCredential> {
    let (verifier, challenge) = claude_code_oauth_pkce();
    if let Some(access_token) = access_token {
        let access_token = access_token.trim().to_string();
        if access_token.is_empty() {
            bail!("Claude Code OAuth token cannot be empty");
        }
        return Ok(ClaudeCodeOAuthCredential {
            access_token,
            refresh_token: String::new(),
            expires_ms: 0,
        });
    }

    if let Some(input) = code {
        let parsed = parse_claude_code_authorization_input(&input);
        let auth_code = parsed
            .code
            .context("Claude Code authorization code was missing")?;
        let state = parsed.state.unwrap_or_else(|| verifier.clone());
        return exchange_claude_code_authorization_code(&auth_code, &state, &verifier);
    }

    let (tx, rx) = mpsc::channel();
    let _callback = start_claude_code_callback_server(verifier.clone(), tx)?;
    let url = claude_code_oauth_authorize_url(&verifier, &challenge);
    println!("Open this URL to login with Anthropic Claude Code:\n");
    println!("{url}");
    println!("\nWaiting for browser callback on http://localhost:{CLAUDE_CODE_CALLBACK_PORT}{CLAUDE_CODE_CALLBACK_PATH} ...");
    if open_browser {
        if let Err(error) = open::that(&url) {
            eprintln!("Could not open browser automatically: {error}");
        }
    }
    let parsed = rx
        .recv_timeout(Duration::from_secs(900))
        .context("timed out waiting for Anthropic browser callback")??;
    let auth_code = parsed
        .code
        .context("Claude Code authorization code was missing")?;
    let state = parsed.state.unwrap_or_default();
    if state != verifier {
        bail!("Claude Code OAuth state mismatch");
    }
    exchange_claude_code_authorization_code(&auth_code, &state, &verifier)
}

struct CallbackServerHandle {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for CallbackServerHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn start_claude_code_callback_server(
    expected_state: String,
    sender: mpsc::Sender<Result<browser_use_providers::ClaudeCodeAuthorization>>,
) -> Result<CallbackServerHandle> {
    let listener = TcpListener::bind((CLAUDE_CODE_CALLBACK_HOST, CLAUDE_CODE_CALLBACK_PORT))
        .with_context(|| {
            format!(
                "bind Claude Code OAuth callback on {CLAUDE_CODE_CALLBACK_HOST}:{CLAUDE_CODE_CALLBACK_PORT}"
            )
        })?;
    listener
        .set_nonblocking(true)
        .context("configure Claude Code OAuth callback listener")?;
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(900);
        loop {
            if stop_rx.try_recv().is_ok() || Instant::now() >= deadline {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let result = handle_claude_code_callback(&mut stream, &expected_state);
                    let _ = sender.send(result);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    let _ = sender.send(Err(error).context("accept Claude Code OAuth callback"));
                    break;
                }
            }
        }
    });
    Ok(CallbackServerHandle {
        stop: stop_tx,
        thread: Some(thread),
    })
}

fn handle_claude_code_callback(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<browser_use_providers::ClaudeCodeAuthorization> {
    let mut request = [0_u8; 4096];
    let read = stream
        .read(&mut request)
        .context("read Claude Code OAuth callback")?;
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("parse Claude Code OAuth callback request")?;
    let parsed = parse_claude_code_authorization_input(path);
    let status = if !path.starts_with(CLAUDE_CODE_CALLBACK_PATH) {
        404
    } else if parsed.code.is_none() || parsed.state.as_deref() != Some(expected_state) {
        400
    } else {
        200
    };
    let text = match status {
        200 => "Anthropic authentication completed. You can close this window.",
        400 => "Anthropic authentication failed: missing code or state mismatch.",
        _ => "Anthropic callback route not found.",
    };
    let body = format!("<html><body><p>{text}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).ok();
    if status == 200 {
        Ok(parsed)
    } else {
        bail!("{text}")
    }
}

fn store_codex_auth(store: &Store, auth: &CodexAuth) -> Result<()> {
    store.set_setting("auth.codex.access_token", auth.access_token.trim())?;
    store.set_setting("auth.codex.account_id", auth.account_id.trim())?;
    Ok(())
}

fn store_claude_code_oauth(store: &Store, credential: &ClaudeCodeOAuthCredential) -> Result<()> {
    store.set_setting(
        "auth.claude_code.access_token",
        credential.access_token.trim(),
    )?;
    if credential.refresh_token.trim().is_empty() {
        store.delete_setting("auth.claude_code.refresh_token")?;
    } else {
        store.set_setting(
            "auth.claude_code.refresh_token",
            credential.refresh_token.trim(),
        )?;
    }
    if credential.expires_ms > 0 {
        store.set_setting(
            "auth.claude_code.expires_ms",
            &credential.expires_ms.to_string(),
        )?;
    } else {
        store.delete_setting("auth.claude_code.expires_ms")?;
    }
    store.delete_setting("auth.claude_code.auth_token")?;
    Ok(())
}

fn print_api_key_status(
    store: &Store,
    label: &str,
    setting_key: &str,
    env_names: &[&str],
) -> Result<()> {
    if store
        .get_setting(setting_key)?
        .is_some_and(|value| !value.trim().is_empty())
    {
        println!("{label}: connected (stored)");
    } else if env_any(env_names) {
        println!("{label}: connected (environment)");
    } else {
        print_auth_line(label, false);
    }
    Ok(())
}

fn print_codex_status(store: &Store) -> Result<()> {
    if let Some(auth) = stored_codex_auth(store)? {
        println!(
            "Codex login: connected account {} (stored)",
            auth.account_id
        );
        return Ok(());
    }
    match load_codex_auth() {
        Ok(auth) => println!(
            "Codex login: connected account {} (external)",
            auth.account_id
        ),
        Err(error) => println!("Codex login: not connected ({error})"),
    }
    Ok(())
}

fn print_claude_code_status(store: &Store) -> Result<()> {
    if store
        .get_setting("auth.claude_code.access_token")?
        .is_some_and(|value| !value.trim().is_empty())
    {
        println!("Claude Code login: connected (stored OAuth credential)");
        return Ok(());
    }
    if store
        .get_setting("auth.claude_code.auth_token")?
        .is_some_and(|value| !value.trim().is_empty())
    {
        println!("Claude Code login: connected (stored legacy OAuth token)");
        return Ok(());
    }
    if env_any(&[
        "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
        "ANTHROPIC_OAUTH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
    ]) {
        println!("Claude Code login: connected (environment OAuth token)");
        return Ok(());
    }
    match claude_code_cli_status() {
        Ok(Some(summary)) => println!("Claude Code CLI: connected ({summary})"),
        Ok(None) => print_auth_line("Claude Code login", false),
        Err(error) => println!("Claude Code login: not connected ({error})"),
    }
    Ok(())
}

fn claude_code_cli_status() -> Result<Option<String>> {
    let output = std::process::Command::new("claude")
        .args(["auth", "status", "--json"])
        .output()
        .context("run `claude auth status --json`")?;
    if !output.status.success() {
        return Ok(None);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parse Claude Code auth status")?;
    if value
        .get("loggedIn")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let email = value
            .get("email")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown email");
        let subscription = value
            .get("subscriptionType")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown plan");
        return Ok(Some(format!("{email}, {subscription}")));
    }
    Ok(None)
}

fn stored_codex_auth(store: &Store) -> Result<Option<CodexAuth>> {
    let Some(access_token) = store.get_setting("auth.codex.access_token")? else {
        return Ok(None);
    };
    let Some(account_id) = store.get_setting("auth.codex.account_id")? else {
        return Ok(None);
    };
    if access_token.trim().is_empty() || account_id.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(CodexAuth {
        access_token,
        account_id,
    }))
}

fn stored_or_env(store: &Store, setting_key: &str, env_names: &[&str]) -> Result<Option<String>> {
    if let Some(value) = store.get_setting(setting_key)? {
        if !value.trim().is_empty() {
            return Ok(Some(value));
        }
    }
    Ok(env_names
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty()))
}

fn setting_or_env_or_default(
    store: &Store,
    setting_key: &str,
    env_names: &[&str],
    default: &str,
) -> Result<String> {
    Ok(stored_or_env(store, setting_key, env_names)?.unwrap_or_else(|| default.to_string()))
}

fn openai_provider(store: &Store, model: String) -> Result<OpenAIResponsesProvider> {
    let api_key = stored_or_env(
        store,
        "auth.openai.api_key",
        &["LLM_BROWSER_OPENAI_API_KEY", "OPENAI_API_KEY"],
    )?
    .context("run `auth login openai --api-key ...` or set LLM_BROWSER_OPENAI_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.openai.base_url",
        &["LLM_BROWSER_OPENAI_BASE_URL"],
        "https://api.openai.com/v1",
    )?;
    Ok(OpenAIResponsesProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn codex_provider(store: &Store, model: String) -> Result<CodexResponsesProvider> {
    let auth = match stored_codex_auth(store)? {
        Some(auth) => auth,
        None => load_codex_auth()?,
    };
    let base_url = setting_or_env_or_default(
        store,
        "auth.codex.base_url",
        &["LLM_BROWSER_CODEX_BASE_URL"],
        "https://chatgpt.com/backend-api",
    )?;
    Ok(CodexResponsesProvider::with_base_url(auth, model, base_url))
}

fn anthropic_provider(store: &Store, model: String) -> Result<AnthropicMessagesProvider> {
    let base_url = setting_or_env_or_default(
        store,
        "auth.anthropic.base_url",
        &["LLM_BROWSER_ANTHROPIC_BASE_URL"],
        "https://api.anthropic.com/v1",
    )?;
    if store
        .get_setting("account")?
        .as_deref()
        .is_some_and(is_claude_code_account)
    {
        let auth_token = claude_code_access_token(store)?;
        return Ok(AnthropicMessagesProvider::with_auth_token(
            auth_token, model, base_url,
        ));
    }
    let api_key = stored_or_env(
        store,
        "auth.anthropic.api_key",
        &["LLM_BROWSER_ANTHROPIC_API_KEY", "ANTHROPIC_API_KEY"],
    )?
    .context("run `auth login anthropic --api-key ...` or set LLM_BROWSER_ANTHROPIC_API_KEY")?;
    Ok(AnthropicMessagesProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn claude_code_access_token(store: &Store) -> Result<String> {
    if let Some(refresh_token) = store.get_setting("auth.claude_code.refresh_token")? {
        let expires_ms = store
            .get_setting("auth.claude_code.expires_ms")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        if !refresh_token.trim().is_empty() && expires_ms <= now_ms() + 60_000 {
            let credential = refresh_claude_code_oauth(refresh_token.trim())
                .context("refresh Claude Code OAuth token")?;
            store_claude_code_oauth(store, &credential)?;
            return Ok(credential.access_token);
        }
    }
    if let Some(access_token) = stored_or_env(
        store,
        "auth.claude_code.access_token",
        &[
            "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ],
    )? {
        return Ok(access_token);
    }
    stored_or_env(
        store,
        "auth.claude_code.auth_token",
        &[
            "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "LLM_BROWSER_ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ],
    )?
    .context(
        "run `auth login claude-code` to sign in with Claude Code, or set CLAUDE_CODE_OAUTH_TOKEN",
    )
}

fn is_claude_code_account(account: &str) -> bool {
    matches!(account, "Claude Code login" | "Claude Code subscription")
}

fn openrouter_provider(store: &Store, model: String) -> Result<OpenAICompatibleChatProvider> {
    let api_key = stored_or_env(
        store,
        "auth.openrouter.api_key",
        &["LLM_BROWSER_OPENAI_COMPAT_API_KEY", "OPENROUTER_API_KEY"],
    )?
    .context("run `auth login openrouter --api-key ...` or set OPENROUTER_API_KEY")?;
    let base_url = setting_or_env_or_default(
        store,
        "auth.openrouter.base_url",
        &["LLM_BROWSER_OPENAI_COMPAT_BASE_URL", "OPENROUTER_BASE_URL"],
        "https://openrouter.ai/api/v1",
    )?;
    Ok(OpenAICompatibleChatProvider::with_base_url(
        api_key, model, base_url,
    ))
}

fn api_key_setting(account: AuthAccount) -> Option<&'static str> {
    match account {
        AuthAccount::Openai => Some("auth.openai.api_key"),
        AuthAccount::Anthropic => Some("auth.anthropic.api_key"),
        AuthAccount::Openrouter => Some("auth.openrouter.api_key"),
        AuthAccount::BrowserUseCloud => Some(BROWSER_USE_CLOUD_API_KEY_SETTING),
        AuthAccount::Codex | AuthAccount::ClaudeCode => None,
    }
}

fn auth_account_label(account: AuthAccount) -> &'static str {
    match account {
        AuthAccount::Codex => "Codex login",
        AuthAccount::ClaudeCode => "Claude Code login",
        AuthAccount::BrowserUseCloud => "Browser Use cloud",
        AuthAccount::Openai => "OpenAI API key",
        AuthAccount::Anthropic => "Anthropic API key",
        AuthAccount::Openrouter => "OpenRouter API key",
    }
}

fn diagnostics(store: &Store) -> Result<()> {
    let sessions = store.list_sessions()?;
    let event_count = sessions.iter().try_fold(0usize, |count, session| {
        Ok::<usize, anyhow::Error>(count + store.events_for_session(&session.id)?.len())
    })?;
    println!("state_dir: {}", store.state_dir().display());
    println!("database: {}", store.state_dir().join("state.db").display());
    println!("sessions: {}", sessions.len());
    println!("events: {event_count}");
    println!("settings: {}", store.list_settings()?.len());

    let mut worker = PythonWorker::start()?;
    let artifact_dir = store.state_dir().join("artifacts").join("__diagnostics__");
    let response = worker.run(
        "__diagnostics__",
        std::env::current_dir()?,
        artifact_dir,
        "result = {'browser_harness_available': browser_harness_available, 'browser_harness_error': browser_harness_error}",
    )?;
    println!(
        "browser_harness: {}",
        if response.browser_harness_available {
            "available"
        } else {
            "not available"
        }
    );
    if let Some(error) = response.browser_harness_error {
        if !error.trim().is_empty() {
            println!("browser_harness_error: {error}");
        }
    }
    Ok(())
}

fn trace(store: &Store, task_id: &str, output: Option<PathBuf>) -> Result<()> {
    let session = ensure_task_exists(store, task_id)?;
    let events = store.events_for_session(task_id)?;
    let artifacts = store.artifacts_for_session(task_id)?;
    let bundle = serde_json::json!({
        "session": session,
        "events": events,
        "artifacts": artifacts,
    });
    if let Some(output) = output {
        if output.extension().is_some() {
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            std::fs::write(
                &output,
                format!("{}\n", serde_json::to_string_pretty(&bundle)?),
            )
            .with_context(|| format!("write {}", output.display()))?;
            println!("{}", output.display());
        } else {
            std::fs::create_dir_all(&output)
                .with_context(|| format!("create {}", output.display()))?;
            let path = output.join("trace.json");
            std::fs::write(
                &path,
                format!("{}\n", serde_json::to_string_pretty(&bundle)?),
            )
            .with_context(|| format!("write {}", path.display()))?;
            println!("{}", path.display());
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&bundle)?);
    }
    Ok(())
}

fn spawn_agent(
    store: &Store,
    parent_id: &str,
    message: String,
    path: Option<String>,
    nickname: Option<String>,
    role: Option<String>,
) -> Result<()> {
    let parent_events = store.events_for_session(parent_id)?;
    let inherited_context = sanitized_agent_context_from_events(&parent_events);
    let child = store.create_child_session(
        parent_id,
        std::env::current_dir()?,
        path.as_deref(),
        nickname.as_deref(),
        role.as_deref(),
    )?;
    store.append_event(
        &child.id,
        "agent.context",
        serde_json::json!({
            "from_session_id": parent_id,
            "context": inherited_context,
        }),
    )?;
    store.append_event(
        &child.id,
        "session.input",
        serde_json::json!({ "text": message }),
    )?;
    store.append_event(
        parent_id,
        "agent.spawned",
        serde_json::json!({
            "child_session_id": child.id,
            "agent_path": path,
            "nickname": nickname,
            "role": role,
        }),
    )?;
    println!("{}", child.id);
    Ok(())
}

fn list_agents(store: &Store, parent_id: &str) -> Result<()> {
    let agents = store.list_child_agents(parent_id)?;
    if agents.is_empty() {
        println!("No child agents.");
        return Ok(());
    }
    for agent in agents {
        println!(
            "{}  {:<7}  {}  {}",
            agent.child_session_id,
            agent.status,
            agent.agent_path.unwrap_or_else(|| "-".to_string()),
            agent.agent_role.unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(())
}

fn close_agent(store: &Store, child_id: &str, reason: &str) -> Result<()> {
    store.close_child_agent(child_id, reason)?;
    println!("closed {child_id}");
    Ok(())
}

fn send_agent_message(
    store: &Store,
    author_id: &str,
    target_id: &str,
    message: &str,
    trigger_turn: bool,
) -> Result<()> {
    let msg = store.send_agent_message(author_id, target_id, message, trigger_turn)?;
    store.append_event(
        author_id,
        "agent.message",
        serde_json::json!({
            "id": msg.id,
            "author_session_id": msg.author_session_id,
            "target_session_id": msg.target_session_id,
            "content": msg.content,
            "trigger_turn": msg.trigger_turn,
        }),
    )?;
    println!("{}", msg.id);
    Ok(())
}

fn wait_agent(store: &Store, target_id: &str) -> Result<()> {
    let session = ensure_task_exists(store, target_id)?;
    println!("{}  {}", session.id, session.status.as_str());
    for message in store.messages_for_agent(target_id)? {
        println!(
            "message {} from {} trigger={} {}",
            message.id, message.author_session_id, message.trigger_turn, message.content
        );
    }
    Ok(())
}

fn dataset_list() -> Result<()> {
    let mut datasets = vec![
        serde_json::json!({
            "name": "real_v14_short",
            "path": "datasets/real_v14_short.json",
            "description": "10-task current smoke dataset",
        }),
        serde_json::json!({
            "name": "real_v14",
            "path": "datasets/real_v14_short.json",
            "description": "alias for real_v14_short in this repository",
        }),
        serde_json::json!({
            "name": "real_v8",
            "path": "datasets/real_v8.json",
            "description": "100-task baseline dataset",
        }),
    ];
    let dir = PathBuf::from("datasets");
    if dir.exists() {
        for entry in std::fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if datasets
                .iter()
                .any(|item| item.get("name").and_then(Value::as_str) == Some(name))
            {
                continue;
            }
            datasets.push(serde_json::json!({
                "name": name,
                "path": path.display().to_string(),
            }));
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "datasets": datasets }))?
    );
    Ok(())
}

fn dataset_sample(dataset: &str, count: usize, task_ids: Vec<String>, all: bool) -> Result<()> {
    let cases = load_dataset_cases(dataset)?;
    let selected = select_dataset_cases(
        cases,
        &DatasetRunOptions {
            count,
            task_ids,
            all,
            run_id: None,
            resume: false,
            skip_failed: false,
            stop_on_failure: false,
            max_attempts: 1,
            concurrency: 1,
            browser_mode: None,
        },
    )?;
    let sample = selected
        .iter()
        .map(dataset_case_manifest)
        .collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&sample)?);
    Ok(())
}

fn dataset_report(store: &Store, run_id_or_path: &str) -> Result<()> {
    let manifest = load_dataset_manifest(store, run_id_or_path)?;
    let mut summary = summarize_dataset_manifest(&manifest);
    summary["artifact_salvage"] = dataset_artifact_salvage_report(store, &manifest)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn dataset_run_fake(store: &Store, dataset: &str, options: DatasetRunOptions) -> Result<()> {
    let provider = FakeProvider::with_text("Fake dataset case completed.");
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        &provider,
        DatasetProviderConfig {
            provider: "fake".to_string(),
            model: "fake".to_string(),
            browser_mode,
            max_turns: 80,
            python_timeout_seconds: 120,
        },
    )
}

fn create_dataset_session(
    store: &Store,
    run_id: &str,
    case: &DatasetCase,
    attempt: usize,
) -> Result<(String, DatasetTaskPaths)> {
    let paths = dataset_task_paths(store, run_id, case, attempt);
    create_dataset_task_dirs(&paths)?;
    let session = store.create_session_with_artifact_root(None, &paths.cwd, &paths.artifacts)?;
    let prompt = build_dataset_prompt(case);
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({ "text": prompt }),
    )?;
    store.append_event(
        &session.id,
        "dataset.case",
        serde_json::json!({
            "dataset": case.dataset,
            "path": case.path,
            "task_id": case.task_id,
            "attempt": attempt,
            "workspace": paths.cwd.display().to_string(),
            "task_root": paths.root.display().to_string(),
            "outputs": paths.outputs.display().to_string(),
            "agent_workspace": paths.agent_workspace.display().to_string(),
            "runtime": paths.runtime.display().to_string(),
        }),
    )?;
    Ok((session.id, paths))
}

fn dataset_run_openai(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let provider = openai_provider(store, model.clone())?;
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        &provider,
        DatasetProviderConfig {
            provider: "openai".to_string(),
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_codex(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let provider = codex_provider(store, model.clone())?;
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        &provider,
        DatasetProviderConfig {
            provider: "codex".to_string(),
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_anthropic(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let provider = anthropic_provider(store, model.clone())?;
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        &provider,
        DatasetProviderConfig {
            provider: "anthropic".to_string(),
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_openrouter(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let provider = openrouter_provider(store, model.clone())?;
    let browser_mode = dataset_browser_mode(&options);
    dataset_run_provider(
        store,
        dataset,
        options,
        &provider,
        DatasetProviderConfig {
            provider: "openrouter".to_string(),
            model: model.clone(),
            browser_mode,
            max_turns,
            python_timeout_seconds,
        },
    )
}

fn dataset_run_provider<P>(
    store: &Store,
    dataset: &str,
    options: DatasetRunOptions,
    provider: &P,
    config: DatasetProviderConfig,
) -> Result<()>
where
    P: ModelProvider + Clone + Send + Sync + 'static,
{
    let all_cases = load_dataset_cases(dataset)?;
    let run_id = options
        .run_id
        .clone()
        .unwrap_or_else(|| dataset_run_id(dataset));
    let manifest_path = dataset_manifest_path(store, &run_id);
    let resume_manifest = options.resume && manifest_path.exists();
    let selected = if resume_manifest {
        cases_from_manifest_selection(&all_cases, &load_dataset_manifest(store, &run_id)?)?
    } else {
        select_dataset_cases(all_cases, &options)?
    };
    if selected.is_empty() {
        println!("No dataset cases selected.");
        return Ok(());
    }

    let mut manifest = if resume_manifest {
        load_dataset_manifest(store, &run_id)?
    } else {
        new_dataset_manifest(&run_id, dataset, &selected, &options, &config)
    };
    let skip_ids = if options.resume {
        resume_skip_ids(&manifest, options.skip_failed)
    } else {
        HashSet::new()
    };
    write_dataset_manifest(store, &run_id, &manifest)?;

    let selected = selected
        .into_iter()
        .filter(|case| {
            if skip_ids.contains(&case.task_id) {
                println!("{}  skipped", case.task_id);
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>();

    let mut pending = VecDeque::from(selected);
    let mut active = 0_usize;
    let mut stop_launching = false;
    let concurrency = options.concurrency.max(1);
    let max_attempts = options.max_attempts.max(1);
    let state_dir = store.state_dir().to_path_buf();
    let (tx, rx) = mpsc::channel::<(String, Result<Value>)>();

    while active > 0 || (!pending.is_empty() && !stop_launching) {
        while active < concurrency && !pending.is_empty() && !stop_launching {
            let case = pending.pop_front().expect("pending checked");
            let task_id = case.task_id.clone();
            let run_id = run_id.clone();
            let config = config.clone();
            let provider = provider.clone();
            let state_dir = state_dir.clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Value> {
                        let store = Store::open(&state_dir)?;
                        run_dataset_case_with_attempts(
                            &store,
                            &provider,
                            &run_id,
                            &case,
                            config,
                            max_attempts,
                        )
                    }))
                    .unwrap_or_else(|panic| {
                        Err(anyhow::anyhow!(
                            "dataset worker panicked: {}",
                            panic_payload_message(panic)
                        ))
                    });
                let _ = tx.send((task_id, result));
            });
            active += 1;
        }
        if active == 0 {
            break;
        }
        let (task_id, result) = rx.recv().context("dataset worker channel closed")?;
        active -= 1;
        let result = match result {
            Ok(result) => result,
            Err(error) => serde_json::json!({
                "task_id": task_id,
                "ok": false,
                "error_type": "runner",
                "error": format!("{error:#}"),
            }),
        };
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        manifest_sessions_mut(&mut manifest)?.push(result);
        manifest["summary"] = summarize_dataset_manifest(&manifest);
        write_dataset_manifest(store, &run_id, &manifest)?;
        if options.stop_on_failure && !ok {
            stop_launching = true;
            pending.clear();
        }
    }

    for case in pending {
        if skip_ids.contains(&case.task_id) {
            println!("{}  skipped", case.task_id);
            continue;
        }
        println!("{}  pending", case.task_id);
    }

    manifest["summary"] = summarize_dataset_manifest(&manifest);
    write_dataset_manifest(store, &run_id, &manifest)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    if !dataset_manifest_exit_ok(&manifest) {
        bail!("dataset run has failures or pending tasks");
    }
    Ok(())
}

fn run_dataset_case_with_attempts<P: ModelProvider>(
    store: &Store,
    provider: &P,
    run_id: &str,
    case: &DatasetCase,
    config: DatasetProviderConfig,
    max_attempts: usize,
) -> Result<Value> {
    let mut retry_history = Vec::new();
    for attempt in 1..=max_attempts {
        let mut result =
            run_dataset_case_with_provider(store, provider, run_id, case, config.clone(), attempt)?;
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if ok {
            if !retry_history.is_empty() {
                result["retry_history"] = Value::Array(retry_history);
            }
            return Ok(result);
        }
        let should_retry = attempt < max_attempts && is_transient_provider_failure(&result);
        if !should_retry {
            if is_permanent_provider_failure(&result) {
                result["retry_classification"] = Value::String("permanent".to_string());
            } else if attempt < max_attempts {
                result["retry_classification"] = Value::String("not_transient".to_string());
            }
            if !retry_history.is_empty() {
                result["retry_history"] = Value::Array(retry_history);
            }
            return Ok(result);
        }
        retry_history.push(result);
    }
    bail!("unreachable dataset retry loop")
}

fn run_dataset_case_with_provider<P: ModelProvider>(
    store: &Store,
    provider: &P,
    run_id: &str,
    case: &DatasetCase,
    config: DatasetProviderConfig,
    attempt: usize,
) -> Result<Value> {
    let (session_id, paths) = create_dataset_session(store, run_id, case, attempt)?;
    println!("{}  {}", case.task_id, session_id);
    io::stdout().flush()?;
    let agent_options = AgentRunOptions {
        max_turns: config.max_turns,
        max_context_chars: AgentRunOptions::default().max_context_chars,
        browser_mode: Some(config.browser_mode.clone()),
        python_tool_timeout_seconds: config.python_timeout_seconds,
        python_env: dataset_python_env(run_id, case, attempt, &paths, &config),
        child_agent_runner: None,
    };
    let run_error = run_existing_session_with_provider(store, provider, &session_id, agent_options)
        .err()
        .map(|error| format!("{error:#}"));
    dataset_attempt_result(store, case, &session_id, config, attempt, run_error)
}

fn dataset_attempt_result(
    store: &Store,
    case: &DatasetCase,
    session_id: &str,
    config: DatasetProviderConfig,
    attempt: usize,
    run_error: Option<String>,
) -> Result<Value> {
    let session = ensure_task_exists(store, session_id)?;
    let events = store.events_for_session(session_id)?;
    let final_result = result_from_events(&events);
    let final_result_chars = final_result.as_deref().map(str::len).unwrap_or(0);
    let usage = usage_summary_from_events(&events);
    let session_failure = failure_from_events(&events);
    let artifacts =
        dataset_artifacts_for_paths(Path::new(&session.cwd), Path::new(&session.artifact_root))?;
    let error = run_error.clone().or(session_failure.clone());
    let error_type = if run_error.is_some() {
        Value::String("provider".to_string())
    } else if session_failure.is_some() {
        Value::String("session".to_string())
    } else {
        Value::Null
    };
    let ok = run_error.is_none()
        && session.status.as_str() == "done"
        && error.is_none()
        && final_result.is_some();
    Ok(serde_json::json!({
        "task_id": case.task_id,
        "dataset": case.dataset,
        "path": case.path,
        "ok": ok,
        "attempt_number": attempt,
        "provider": config.provider,
        "model": config.model,
        "usage": usage,
        "final_result": final_result,
        "final_result_chars": final_result_chars,
        "artifacts": artifacts,
        "error_type": error_type,
        "error": error,
        "session": {
            "id": session.id,
            "status": session.status.as_str(),
            "cwd": session.cwd,
            "artifact_root": session.artifact_root,
        },
    }))
}

fn load_dataset_cases(dataset: &str) -> Result<Vec<DatasetCase>> {
    let path = resolve_dataset_path(dataset);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let raw: Value =
        serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))?;
    let array = raw
        .as_array()
        .with_context(|| format!("{} must contain a JSON array", path.display()))?;
    let dataset_name = dataset_name_for_path(dataset, &path);
    array
        .iter()
        .enumerate()
        .map(|(idx, value)| parse_dataset_case(&dataset_name, &path, idx, value.clone()))
        .collect()
}

fn resolve_dataset_path(dataset: &str) -> PathBuf {
    let direct = PathBuf::from(dataset);
    if direct.exists() {
        return direct;
    }
    match dataset {
        "real_v14" | "real_v14_short" => return PathBuf::from("datasets/real_v14_short.json"),
        "real_v8" => return PathBuf::from("datasets/real_v8.json"),
        _ => {}
    }
    let with_ext = PathBuf::from("datasets").join(format!("{dataset}.json"));
    if with_ext.exists() {
        return with_ext;
    }
    direct
}

fn parse_dataset_case(
    dataset: &str,
    path: &std::path::Path,
    idx: usize,
    raw: Value,
) -> Result<DatasetCase> {
    raw.as_object()
        .with_context(|| format!("dataset row {} must be an object", idx + 1))?;
    let task_id = raw
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| (idx + 1).to_string());
    let confirmed_task = ["confirmed_task", "task", "text", "prompt"]
        .iter()
        .find_map(|key| raw.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .with_context(|| format!("dataset row {task_id} has no task text"))?;
    Ok(DatasetCase {
        dataset: dataset.to_string(),
        path: path.display().to_string(),
        task_id,
        confirmed_task,
        raw,
    })
}

fn dataset_name_for_path(dataset: &str, path: &std::path::Path) -> String {
    match dataset {
        "real_v14" | "real_v14_short" => "real_v14_short".to_string(),
        "real_v8" => "real_v8".to_string(),
        _ => path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(dataset)
            .to_string(),
    }
}

fn select_dataset_cases(
    cases: Vec<DatasetCase>,
    options: &DatasetRunOptions,
) -> Result<Vec<DatasetCase>> {
    if !options.task_ids.is_empty() {
        let requested = options
            .task_ids
            .iter()
            .cloned()
            .collect::<HashSet<String>>();
        let selected = cases
            .into_iter()
            .filter(|case| requested.contains(&case.task_id))
            .collect::<Vec<_>>();
        let found = selected
            .iter()
            .map(|case| case.task_id.clone())
            .collect::<HashSet<_>>();
        let missing = requested
            .difference(&found)
            .cloned()
            .collect::<Vec<String>>();
        if !missing.is_empty() {
            bail!("dataset task id(s) not found: {}", missing.join(", "));
        }
        return Ok(selected);
    }
    if options.all {
        return Ok(cases);
    }
    Ok(cases.into_iter().take(options.count).collect())
}

fn cases_from_manifest_selection(
    cases: &[DatasetCase],
    manifest: &Value,
) -> Result<Vec<DatasetCase>> {
    let empty = Vec::new();
    let ids = manifest
        .get("selection")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .iter()
        .filter_map(|case| case.get("task_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        bail!("resume manifest has no selection");
    }
    let by_id = cases
        .iter()
        .map(|case| (case.task_id.clone(), case.clone()))
        .collect::<HashMap<_, _>>();
    ids.into_iter()
        .map(|id| {
            by_id
                .get(&id)
                .cloned()
                .with_context(|| format!("resume task id {id} no longer exists in dataset"))
        })
        .collect()
}

fn build_dataset_prompt(case: &DatasetCase) -> String {
    include_str!("../../../prompts/dataset-case-user.md")
        .trim()
        .replace("{{dataset}}", &case.dataset)
        .replace("{{task_id}}", &case.task_id)
        .replace("{{task}}", &case.confirmed_task)
}

fn dataset_case_manifest(case: &DatasetCase) -> Value {
    serde_json::json!({
        "dataset": case.dataset,
        "path": case.path,
        "task_id": case.task_id,
        "confirmed_task": case.confirmed_task,
        "raw": case.raw,
    })
}

fn new_dataset_manifest(
    run_id: &str,
    dataset: &str,
    cases: &[DatasetCase],
    options: &DatasetRunOptions,
    config: &DatasetProviderConfig,
) -> Value {
    let mut datasets = HashMap::<String, usize>::new();
    for case in cases {
        *datasets.entry(case.dataset.clone()).or_default() += 1;
    }
    serde_json::json!({
        "run_id": run_id,
        "dataset": dataset,
        "created_ms": now_ms(),
        "provider": config.provider,
        "model": config.model,
        "concurrency": options.concurrency.max(1),
        "max_attempts": options.max_attempts.max(1),
        "max_turns": config.max_turns,
        "python_timeout_seconds": config.python_timeout_seconds,
        "headless": config.browser_mode != "cloud",
        "browser": config.browser_mode,
        "selection": cases.iter().map(dataset_case_manifest).collect::<Vec<_>>(),
        "summary": {
            "count": cases.len(),
            "datasets": datasets,
            "passed": 0,
            "failed": 0,
            "pending": cases.len(),
            "usage": empty_usage_summary(),
        },
        "sessions": [],
    })
}

fn dataset_run_id(dataset: &str) -> String {
    let mut safe = dataset
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if safe.is_empty() {
        safe.push_str("dataset");
    }
    format!("{safe}-{}", now_ms())
}

fn dataset_manifest_path(store: &Store, run_id: &str) -> PathBuf {
    store
        .state_dir()
        .join("dataset-runs")
        .join(format!("{run_id}.json"))
}

fn dataset_run_files_path(store: &Store, run_id: &str) -> PathBuf {
    store
        .state_dir()
        .join("dataset-run-files")
        .join(safe_path_segment(run_id))
}

fn dataset_task_paths(
    store: &Store,
    run_id: &str,
    case: &DatasetCase,
    attempt: usize,
) -> DatasetTaskPaths {
    let root = dataset_run_files_path(store, run_id).join(format!(
        "task-{}-attempt-{}",
        safe_path_segment(&case.task_id),
        attempt
    ));
    let runtime_base = PathBuf::from("/tmp")
        .join("lbe")
        .join(stable_short_hash(run_id, 12))
        .join(format!(
            "t{}a{}",
            stable_short_hash(&case.task_id, 10),
            attempt
        ));
    DatasetTaskPaths {
        cwd: root.join("cwd"),
        outputs: root.join("outputs"),
        artifacts: root.join("artifacts"),
        agent_workspace: root.join("agent-workspace"),
        logs: root.join("logs"),
        runtime: runtime_base.join("r"),
        tmp: root.join("tmp"),
        root,
    }
}

fn create_dataset_task_dirs(paths: &DatasetTaskPaths) -> Result<()> {
    for path in [
        &paths.root,
        &paths.cwd,
        &paths.outputs,
        &paths.artifacts,
        &paths.agent_workspace,
        &paths.logs,
        &paths.runtime,
        &paths.tmp,
    ] {
        std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    }
    let helper = paths.agent_workspace.join("agent_helpers.py");
    if !helper.exists() {
        std::fs::write(&helper, "").with_context(|| format!("write {}", helper.display()))?;
    }
    Ok(())
}

fn dataset_python_env(
    run_id: &str,
    case: &DatasetCase,
    attempt: usize,
    paths: &DatasetTaskPaths,
    config: &DatasetProviderConfig,
) -> Vec<(String, String)> {
    let task_name = format!(
        "bu{}{}a{}",
        stable_short_hash(run_id, 8),
        stable_short_hash(&case.task_id, 8),
        attempt
    );
    let mut env = vec![
        ("BU_NAME".to_string(), task_name),
        (
            "BH_RUNTIME_DIR".to_string(),
            paths.runtime.display().to_string(),
        ),
        ("BH_TMP_DIR".to_string(), paths.tmp.display().to_string()),
        (
            "BH_AGENT_WORKSPACE".to_string(),
            paths.agent_workspace.display().to_string(),
        ),
        (
            "LLM_BROWSER_VIRTUAL_HOME".to_string(),
            paths.root.display().to_string(),
        ),
        (
            "LLM_BROWSER_OUTPUTS_DIR".to_string(),
            paths.outputs.display().to_string(),
        ),
        (
            "LLM_BROWSER_BROWSER_MODE".to_string(),
            config.browser_mode.clone(),
        ),
        (
            "LLM_BROWSER_OPEN_CLOUD_LIVE_VIEW".to_string(),
            "0".to_string(),
        ),
    ];
    if config.browser_mode == "cloud" {
        env.push(("LLM_BROWSER_AUTO_CHROME".to_string(), "0".to_string()));
        env.push(("BU_CDP_URL".to_string(), "".to_string()));
        env.push(("BU_CDP_WS".to_string(), "".to_string()));
        env.push(("BU_BROWSER_ID".to_string(), "".to_string()));
    }
    env
}

fn safe_path_segment(value: &str) -> String {
    let mut safe = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if safe.is_empty() {
        safe.push_str("case");
    }
    safe
}

fn stable_short_hash(value: &str, len: usize) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let hex = format!("{hash:016x}");
    hex.chars().take(len.min(hex.len())).collect()
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

fn load_dataset_manifest(store: &Store, run_id_or_path: &str) -> Result<Value> {
    let direct = PathBuf::from(run_id_or_path);
    let path = if direct.exists() {
        direct
    } else {
        dataset_manifest_path(store, run_id_or_path)
    };
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))
}

fn write_dataset_manifest(store: &Store, run_id: &str, manifest: &Value) -> Result<()> {
    let path = dataset_manifest_path(store, run_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(
        &path,
        format!("{}\n", serde_json::to_string_pretty(manifest)?),
    )
    .with_context(|| format!("write {}", path.display()))
}

fn manifest_sessions_mut(manifest: &mut Value) -> Result<&mut Vec<Value>> {
    if !manifest.get("sessions").is_some_and(Value::is_array) {
        manifest["sessions"] = Value::Array(Vec::new());
    }
    manifest
        .get_mut("sessions")
        .and_then(Value::as_array_mut)
        .context("manifest sessions must be an array")
}

fn summarize_dataset_manifest(manifest: &Value) -> Value {
    let selection = manifest
        .get("selection")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let selected_ids = selection
        .iter()
        .filter_map(|case| case.get("task_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut datasets = HashMap::<String, usize>::new();
    for case in &selection {
        if let Some(dataset) = case.get("dataset").and_then(Value::as_str) {
            *datasets.entry(dataset.to_string()).or_default() += 1;
        }
    }
    let mut latest = HashMap::<String, Value>::new();
    let mut attempts = HashMap::<String, usize>::new();
    for session in manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(task_id) = session.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        *attempts.entry(task_id.to_string()).or_default() += 1;
        latest.insert(task_id.to_string(), session.clone());
    }
    let mut passed_ids = Vec::new();
    let mut failed_ids = Vec::new();
    let mut pending_ids = Vec::new();
    for task_id in &selected_ids {
        match latest.get(task_id) {
            Some(session) if session.get("ok").and_then(Value::as_bool).unwrap_or(false) => {
                passed_ids.push(task_id.clone());
            }
            Some(_) => failed_ids.push(task_id.clone()),
            None => pending_ids.push(task_id.clone()),
        }
    }
    serde_json::json!({
        "run_id": manifest.get("run_id").cloned().unwrap_or(Value::Null),
        "dataset": manifest.get("dataset").cloned().unwrap_or(Value::Null),
        "provider": manifest.get("provider").cloned().unwrap_or(Value::Null),
        "model": manifest.get("model").cloned().unwrap_or(Value::Null),
        "count": selected_ids.len(),
        "datasets": datasets,
        "passed": passed_ids.len(),
        "failed": failed_ids.len(),
        "pending": pending_ids.len(),
        "passed_ids": passed_ids,
        "failed_ids": failed_ids,
        "pending_ids": pending_ids,
        "attempts_by_task": attempts,
        "usage": usage_summary_from_manifest(manifest),
    })
}

fn dataset_artifact_salvage_report(store: &Store, manifest: &Value) -> Result<Value> {
    let summary = summarize_dataset_manifest(manifest);
    let sessions = manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let sessions_with_artifacts = sessions
        .iter()
        .filter(|session| {
            session
                .get("artifacts")
                .and_then(|artifacts| artifacts.get("found"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .count();
    let failed_with_artifacts = sessions
        .iter()
        .filter(|session| !session.get("ok").and_then(Value::as_bool).unwrap_or(false))
        .filter(|session| {
            session
                .get("artifacts")
                .and_then(|artifacts| artifacts.get("found"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|session| session.get("task_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();

    let run_id = manifest.get("run_id").and_then(Value::as_str);
    let mut pending_with_artifacts = Vec::new();
    if let Some(run_id) = run_id {
        for task_id in summary
            .get("pending_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let attempts = pending_artifact_attempts(store, run_id, task_id)?;
            if !attempts.is_empty() {
                pending_with_artifacts.push(serde_json::json!({
                    "task_id": task_id,
                    "attempts": attempts,
                }));
            }
        }
    }

    Ok(serde_json::json!({
        "note": "Artifact presence is reported for manual review only; it does not mark a task successful.",
        "sessions_with_artifacts": sessions_with_artifacts,
        "failed_with_artifacts": failed_with_artifacts,
        "pending_with_artifacts": pending_with_artifacts,
    }))
}

fn pending_artifact_attempts(store: &Store, run_id: &str, task_id: &str) -> Result<Vec<Value>> {
    let base = dataset_run_files_path(store, run_id);
    if !base.exists() {
        return Ok(Vec::new());
    }
    let prefix = format!("task-{}-attempt-", safe_path_segment(task_id));
    let mut attempts = Vec::new();
    for entry in std::fs::read_dir(&base).with_context(|| format!("read {}", base.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(attempt) = name.strip_prefix(&prefix) else {
            continue;
        };
        let artifacts = dataset_artifacts_for_paths(&path.join("cwd"), &path.join("artifacts"))?;
        if artifacts
            .get("found")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            attempts.push(serde_json::json!({
                "attempt": attempt,
                "task_root": path.display().to_string(),
                "artifacts": artifacts,
            }));
        }
    }
    attempts.sort_by(|left, right| {
        left.get("attempt")
            .and_then(Value::as_str)
            .cmp(&right.get("attempt").and_then(Value::as_str))
    });
    Ok(attempts)
}

fn dataset_artifacts_for_paths(cwd: &Path, artifact_root: &Path) -> Result<Value> {
    let task_root = cwd.parent().unwrap_or(cwd);
    let outputs = task_root.join("outputs");
    let final_answer = artifact_root.join(".final_answer.json");

    let final_answer_summary = if final_answer.exists() {
        summarize_artifact_file(&final_answer)
    } else {
        Value::Null
    };
    let output_summaries = summarize_output_dir(&outputs)?;
    let found = !final_answer_summary.is_null() || !output_summaries.is_empty();

    Ok(serde_json::json!({
        "found": found,
        "final_answer": final_answer_summary,
        "outputs": output_summaries,
    }))
}

fn summarize_output_dir(outputs: &Path) -> Result<Vec<Value>> {
    if !outputs.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in
        std::fs::read_dir(outputs).with_context(|| format!("read {}", outputs.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files
        .into_iter()
        .take(20)
        .map(|path| summarize_artifact_file(&path))
        .collect())
}

fn summarize_artifact_file(path: &Path) -> Value {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return serde_json::json!({
                "path": path.display().to_string(),
                "error": format!("{error:#}"),
            });
        }
    };
    let mut summary = serde_json::json!({
        "path": path.display().to_string(),
        "bytes": metadata.len(),
    });
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if extension == "json" && metadata.len() <= 10_000_000 {
        match std::fs::read_to_string(path)
            .map_err(anyhow::Error::from)
            .and_then(|content| {
                serde_json::from_str::<Value>(&content).map_err(anyhow::Error::from)
            }) {
            Ok(value) => {
                summary["json"] = summarize_json_artifact(&value);
            }
            Err(error) => {
                summary["json_error"] = Value::String(format!("{error:#}"));
            }
        }
    } else if extension == "csv" {
        summary["kind"] = Value::String("csv".to_string());
    } else {
        summary["kind"] = Value::String(if extension.is_empty() {
            "file".to_string()
        } else {
            extension
        });
    }
    summary
}

fn summarize_json_artifact(value: &Value) -> Value {
    match value {
        Value::Array(items) => serde_json::json!({
            "kind": "array",
            "length": items.len(),
        }),
        Value::Object(object) => {
            let keys = object.keys().take(20).cloned().collect::<Vec<_>>();
            let array_lengths = object
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .as_array()
                        .map(|items| (key.clone(), Value::from(items.len())))
                })
                .collect::<serde_json::Map<_, _>>();
            serde_json::json!({
                "kind": "object",
                "keys": keys,
                "array_lengths": array_lengths,
            })
        }
        _ => serde_json::json!({
            "kind": "scalar",
        }),
    }
}

fn usage_summary_from_events(events: &[browser_use_protocol::EventRecord]) -> Value {
    let mut input_tokens = 0_i64;
    let mut input_cached_tokens = 0_i64;
    let mut input_cache_creation_tokens = 0_i64;
    let mut output_tokens = 0_i64;
    let mut total_tokens = 0_i64;
    let mut input_cost_usd = 0.0_f64;
    let mut input_cached_cost_usd = 0.0_f64;
    let mut input_cache_creation_cost_usd = 0.0_f64;
    let mut output_cost_usd = 0.0_f64;
    let mut cost_usd = 0.0_f64;
    let mut invocation_count = 0_i64;
    let mut cost_known_invocation_count = 0_i64;
    let mut cost_estimated_invocation_count = 0_i64;
    let mut cost_missing_invocation_count = 0_i64;

    for event in events {
        if event.event_type != "model.usage" {
            continue;
        }
        invocation_count += 1;
        if event
            .payload
            .get("cost_usd")
            .and_then(Value::as_f64)
            .is_some()
        {
            cost_known_invocation_count += 1;
            if event.payload.get("cost_source").and_then(Value::as_str) == Some("estimated") {
                cost_estimated_invocation_count += 1;
            }
        } else {
            cost_missing_invocation_count += 1;
        }
        input_tokens += json_i64(&event.payload, "input_tokens");
        input_cached_tokens += json_i64(&event.payload, "input_cached_tokens");
        input_cache_creation_tokens += json_i64(&event.payload, "input_cache_creation_tokens");
        output_tokens += json_i64(&event.payload, "output_tokens");
        total_tokens += json_i64(&event.payload, "total_tokens");
        input_cost_usd += json_f64(&event.payload, "input_cost_usd");
        input_cached_cost_usd += json_f64(&event.payload, "input_cached_cost_usd");
        input_cache_creation_cost_usd += json_f64(&event.payload, "input_cache_creation_cost_usd");
        output_cost_usd += json_f64(&event.payload, "output_cost_usd");
        cost_usd += json_f64(&event.payload, "cost_usd");
    }

    serde_json::json!({
        "input_tokens": input_tokens,
        "input_cached_tokens": input_cached_tokens,
        "input_cache_creation_tokens": input_cache_creation_tokens,
        "output_tokens": output_tokens,
        "total_tokens": total_tokens,
        "input_cost_usd": input_cost_usd,
        "input_cached_cost_usd": input_cached_cost_usd,
        "input_cache_creation_cost_usd": input_cache_creation_cost_usd,
        "output_cost_usd": output_cost_usd,
        "cost_usd": cost_usd,
        "cost_known_invocation_count": cost_known_invocation_count,
        "cost_estimated_invocation_count": cost_estimated_invocation_count,
        "cost_missing_invocation_count": cost_missing_invocation_count,
        "cost_status": usage_cost_status(invocation_count, cost_known_invocation_count, cost_missing_invocation_count),
        "invocation_count": invocation_count,
    })
}

fn usage_summary_from_manifest(manifest: &Value) -> Value {
    let mut summary = empty_usage_summary();
    for session in manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let usage = session.get("usage").unwrap_or(&Value::Null);
        for key in [
            "input_tokens",
            "input_cached_tokens",
            "input_cache_creation_tokens",
            "output_tokens",
            "total_tokens",
            "invocation_count",
        ] {
            summary[key] = Value::from(json_i64(&summary, key) + json_i64(usage, key));
        }
        for key in [
            "input_cost_usd",
            "input_cached_cost_usd",
            "input_cache_creation_cost_usd",
            "output_cost_usd",
            "cost_usd",
        ] {
            summary[key] = Value::from(json_f64(&summary, key) + json_f64(usage, key));
        }
        for key in [
            "cost_known_invocation_count",
            "cost_estimated_invocation_count",
            "cost_missing_invocation_count",
        ] {
            summary[key] = Value::from(json_i64(&summary, key) + json_i64(usage, key));
        }
    }
    let invocation_count = json_i64(&summary, "invocation_count");
    let known_count = json_i64(&summary, "cost_known_invocation_count");
    let missing_count = json_i64(&summary, "cost_missing_invocation_count");
    summary["cost_status"] = Value::String(usage_cost_status(
        invocation_count,
        known_count,
        missing_count,
    ));
    summary
}

fn empty_usage_summary() -> Value {
    serde_json::json!({
        "input_tokens": 0,
        "input_cached_tokens": 0,
        "input_cache_creation_tokens": 0,
        "output_tokens": 0,
        "total_tokens": 0,
        "input_cost_usd": 0.0,
        "input_cached_cost_usd": 0.0,
        "input_cache_creation_cost_usd": 0.0,
        "output_cost_usd": 0.0,
        "cost_usd": 0.0,
        "cost_known_invocation_count": 0,
        "cost_estimated_invocation_count": 0,
        "cost_missing_invocation_count": 0,
        "cost_status": "missing",
        "invocation_count": 0,
    })
}

fn usage_cost_status(invocation_count: i64, known_count: i64, missing_count: i64) -> String {
    if invocation_count <= 0 {
        return "missing".to_string();
    }
    if known_count == invocation_count {
        return "known".to_string();
    }
    if missing_count == invocation_count {
        return "missing".to_string();
    }
    "partial".to_string()
}

fn json_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn json_f64(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn resume_skip_ids(manifest: &Value, skip_failed: bool) -> HashSet<String> {
    let mut skip = HashSet::new();
    for session in manifest
        .get("sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(task_id) = session.get("task_id").and_then(Value::as_str) else {
            continue;
        };
        let ok = session.get("ok").and_then(Value::as_bool).unwrap_or(false);
        if ok || skip_failed {
            skip.insert(task_id.to_string());
        }
    }
    skip
}

fn dataset_manifest_exit_ok(manifest: &Value) -> bool {
    let summary = summarize_dataset_manifest(manifest);
    summary.get("failed").and_then(Value::as_u64).unwrap_or(1) == 0
        && summary.get("pending").and_then(Value::as_u64).unwrap_or(1) == 0
}

fn is_transient_provider_failure(result: &Value) -> bool {
    let Some(error) = result.get("error").and_then(Value::as_str) else {
        return false;
    };
    let error = error.to_ascii_lowercase();
    if is_permanent_provider_error(&error) {
        return false;
    }
    if [
        "incorrect api key",
        "401 unauthorized",
        "403 forbidden",
        "400 bad request",
        "content was flagged",
        "cybersecurity risk",
        "invalid_request_error",
    ]
    .iter()
    .any(|needle| error.contains(needle))
    {
        return false;
    }
    [
        "read codex sse line",
        "stream error",
        "stream disconnected",
        "connection reset",
        "connection closed",
        "connection aborted",
        "operation timed out",
        "rate limit",
        "too many requests",
        "overloaded",
        "temporarily",
        "timeout",
        "timed out",
        "eof",
        "gateway",
        "502",
        "503",
        "504",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn is_permanent_provider_failure(result: &Value) -> bool {
    result
        .get("error")
        .and_then(Value::as_str)
        .map(is_permanent_provider_error)
        .unwrap_or(false)
}

fn is_permanent_provider_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "no endpoints found that support image input",
        "context length exceeded",
        "maximum context length",
        "context_length_exceeded",
        "tool schema",
        "schema mismatch",
        "invalid_request_error",
        "incorrect api key",
        "401 unauthorized",
        "403 forbidden",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn ensure_task_exists(store: &Store, task_id: &str) -> Result<browser_use_protocol::SessionMeta> {
    store
        .load_session(task_id)?
        .with_context(|| format!("unknown task id: {task_id}"))
}

fn notify_parent_agent_done(
    store: &Store,
    task: &browser_use_protocol::SessionMeta,
    status: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let Some(parent_id) = task.parent_id.as_deref() else {
        return Ok(());
    };
    store.set_child_agent_status(&task.id, status)?;
    let event_type = match status {
        "done" => "agent.completed",
        "failed" => "agent.failed",
        "cancelled" => "agent.cancelled",
        _ => "agent.updated",
    };
    store.append_event(
        parent_id,
        event_type,
        serde_json::json!({
            "child_session_id": task.id,
            "status": status,
            "payload": payload,
        }),
    )?;
    Ok(())
}
