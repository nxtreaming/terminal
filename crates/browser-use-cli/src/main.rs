use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use browser_use_core::{
    record_python_response_final_event, record_python_worker_event, run_agent_with_provider,
    run_existing_session_with_provider, run_fake_agent, AgentRunOptions, FakeAgentOptions,
};
use browser_use_protocol::{
    browser_summary_from_events, failure_from_events, result_from_events,
    sanitized_agent_context_from_events, task_from_events,
};
use browser_use_providers::{
    load_codex_auth, load_codex_auth_file, AnthropicMessagesProvider, CodexAuth,
    CodexResponsesProvider, OpenAICompatibleChatProvider, OpenAIResponsesProvider,
};
use browser_use_python_worker::PythonWorker;
use browser_use_store::Store;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;

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
    DatasetRunFake {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
    },
    DatasetRunOpenai {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
    },
    DatasetRunCodex {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long, default_value = "gpt-5.5")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
    },
    DatasetRunAnthropic {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long, default_value = "claude-sonnet-4-6")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
    },
    DatasetRunOpenrouter {
        dataset: String,
        #[arg(long, default_value_t = 1)]
        count: usize,
        #[arg(long, default_value = "openai/gpt-5.5")]
        model: String,
        #[arg(long, default_value_t = 80)]
        max_turns: usize,
        #[arg(long, default_value_t = 120)]
        python_timeout_seconds: u64,
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
    },
    ImportCodex {
        #[arg(long = "from")]
        input: Option<PathBuf>,
    },
    Logout {
        account: AuthAccount,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AuthAccount {
    Codex,
    ClaudeCode,
    Openai,
    Anthropic,
    Openrouter,
}

#[derive(Debug, Deserialize)]
struct DatasetCase {
    task_id: String,
    confirmed_task: String,
}

fn main() -> Result<()> {
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
        Command::DatasetRunFake { dataset, count } => dataset_run_fake(&store, &dataset, count),
        Command::DatasetRunOpenai {
            dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        } => dataset_run_openai(
            &store,
            &dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        ),
        Command::DatasetRunCodex {
            dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        } => dataset_run_codex(
            &store,
            &dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        ),
        Command::DatasetRunAnthropic {
            dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        } => dataset_run_anthropic(
            &store,
            &dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        ),
        Command::DatasetRunOpenrouter {
            dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        } => dataset_run_openrouter(
            &store,
            &dataset,
            count,
            model,
            max_turns,
            python_timeout_seconds,
        ),
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
    AgentRunOptions::default().with_browser_mode("headless")
}

fn cli_agent_options_with(max_turns: usize, python_timeout_seconds: u64) -> AgentRunOptions {
    let mut options = cli_agent_options();
    options.max_turns = max_turns;
    options.python_tool_timeout_seconds = python_timeout_seconds;
    options
}

fn run_openai(store: &Store, text: String, model: String) -> Result<()> {
    let provider = openai_provider(store, model)?;
    let session_id = run_agent_with_provider(
        store,
        &provider,
        &text,
        std::env::current_dir()?,
        cli_agent_options(),
    )?;
    println!("{session_id}");
    Ok(())
}

fn run_codex(store: &Store, text: String, model: String) -> Result<()> {
    let provider = codex_provider(store, model)?;
    let session_id = run_agent_with_provider(
        store,
        &provider,
        &text,
        std::env::current_dir()?,
        cli_agent_options(),
    )?;
    println!("{session_id}");
    Ok(())
}

fn run_anthropic(store: &Store, text: String, model: String) -> Result<()> {
    let provider = anthropic_provider(store, model)?;
    let session_id = run_agent_with_provider(
        store,
        &provider,
        &text,
        std::env::current_dir()?,
        cli_agent_options(),
    )?;
    println!("{session_id}");
    Ok(())
}

fn run_openrouter(store: &Store, text: String, model: String) -> Result<()> {
    let provider = openrouter_provider(store, model)?;
    let session_id = run_agent_with_provider(
        store,
        &provider,
        &text,
        std::env::current_dir()?,
        cli_agent_options(),
    )?;
    println!("{session_id}");
    Ok(())
}

fn run_openai_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let provider = openai_provider(store, model)?;
    let session_id =
        run_existing_session_with_provider(store, &provider, task_id, cli_agent_options())?;
    println!("{session_id}");
    Ok(())
}

fn run_codex_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let provider = codex_provider(store, model)?;
    let session_id =
        run_existing_session_with_provider(store, &provider, task_id, cli_agent_options())?;
    println!("{session_id}");
    Ok(())
}

fn run_anthropic_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let provider = anthropic_provider(store, model)?;
    let session_id =
        run_existing_session_with_provider(store, &provider, task_id, cli_agent_options())?;
    println!("{session_id}");
    Ok(())
}

fn run_openrouter_session(store: &Store, task_id: &str, model: String) -> Result<()> {
    ensure_task_exists(store, task_id)?;
    let provider = openrouter_provider(store, model)?;
    let session_id =
        run_existing_session_with_provider(store, &provider, task_id, cli_agent_options())?;
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
        ("browser", "Local Chrome"),
        ("agent.backend", "codex"),
        ("setup.complete", "0"),
    ]
}

fn is_secret_setting(key: &str) -> bool {
    key.starts_with("auth.")
        && (key.ends_with(".api_key")
            || key.ends_with(".access_token")
            || key.ends_with(".auth_token"))
}

fn auth(store: &Store, command: AuthCommand) -> Result<()> {
    match command {
        AuthCommand::Status => {
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
        } => auth_login(store, account, api_key, access_token, account_id),
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
) -> Result<()> {
    match account {
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
            let auth_token = read_optional_secret_from_env(
                access_token,
                &[
                    "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    "ANTHROPIC_AUTH_TOKEN",
                ],
                "Claude Code OAuth token",
            )?;
            store.set_setting("auth.claude_code.auth_token", auth_token.trim())?;
            store.set_setting("account", "Claude Code login")?;
            println!("Claude Code login: connected (stored OAuth token)");
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
        AuthAccount::ClaudeCode => {
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

fn read_optional_secret_from_env(
    value: Option<String>,
    env_names: &[&str],
    prompt: &str,
) -> Result<String> {
    if let Some(value) = value {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            bail!("{prompt} cannot be empty");
        }
        return Ok(trimmed);
    }
    for name in env_names {
        if let Ok(value) = std::env::var(name) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Ok(trimmed);
            }
        }
    }
    bail!(
        "{prompt} not found; run `claude setup-token` and pass it with --access-token, or set CLAUDE_CODE_OAUTH_TOKEN"
    )
}

fn store_codex_auth(store: &Store, auth: &CodexAuth) -> Result<()> {
    store.set_setting("auth.codex.access_token", auth.access_token.trim())?;
    store.set_setting("auth.codex.account_id", auth.account_id.trim())?;
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
        .get_setting("auth.claude_code.auth_token")?
        .is_some_and(|value| !value.trim().is_empty())
    {
        println!("Claude Code login: connected (stored OAuth token)");
        return Ok(());
    }
    if env_any(&[
        "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "ANTHROPIC_AUTH_TOKEN",
    ]) {
        println!("Claude Code login: connected (environment OAuth token)");
        return Ok(());
    }
    match claude_code_cli_status() {
        Ok(Some(summary)) => println!(
            "Claude Code login: connected in Claude Code CLI ({summary}; run `claude setup-token` to make it usable here)"
        ),
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
    if store.get_setting("account")?.as_deref() == Some("Claude Code login") {
        let auth_token = stored_or_env(
            store,
            "auth.claude_code.auth_token",
            &[
                "LLM_BROWSER_CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "ANTHROPIC_AUTH_TOKEN",
            ],
        )?
        .context(
            "run `claude setup-token`, then `auth login claude-code --access-token ...`, or set CLAUDE_CODE_OAUTH_TOKEN",
        )?;
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
        AuthAccount::Codex | AuthAccount::ClaudeCode => None,
    }
}

fn auth_account_label(account: AuthAccount) -> &'static str {
    match account {
        AuthAccount::Codex => "Codex login",
        AuthAccount::ClaudeCode => "Claude Code login",
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

fn dataset_run_fake(store: &Store, dataset: &str, count: usize) -> Result<()> {
    let cases = load_dataset_cases(dataset)?;
    if cases.is_empty() || count == 0 {
        println!("No dataset cases selected.");
        return Ok(());
    }
    for case in cases.into_iter().take(count) {
        let session_id = run_fake_agent(
            store,
            &case.confirmed_task,
            std::env::current_dir()?,
            FakeAgentOptions { python_code: None },
        )?;
        store.append_event(
            &session_id,
            "dataset.case",
            serde_json::json!({
                "dataset": dataset,
                "task_id": case.task_id,
            }),
        )?;
        println!("{}  {}", case.task_id, session_id);
    }
    Ok(())
}

fn create_dataset_session(store: &Store, dataset: &str, case: &DatasetCase) -> Result<String> {
    let session = store.create_session(None, std::env::current_dir()?)?;
    store.append_event(
        &session.id,
        "session.input",
        serde_json::json!({ "text": case.confirmed_task }),
    )?;
    store.append_event(
        &session.id,
        "dataset.case",
        serde_json::json!({
            "dataset": dataset,
            "task_id": case.task_id,
        }),
    )?;
    Ok(session.id)
}

fn dataset_run_openai(
    store: &Store,
    dataset: &str,
    count: usize,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let cases = load_dataset_cases(dataset)?;
    if cases.is_empty() || count == 0 {
        println!("No dataset cases selected.");
        return Ok(());
    }
    let provider = openai_provider(store, model)?;
    for case in cases.into_iter().take(count) {
        let session_id = create_dataset_session(store, dataset, &case)?;
        println!("{}  {}", case.task_id, session_id);
        io::stdout().flush()?;
        run_existing_session_with_provider(
            store,
            &provider,
            &session_id,
            cli_agent_options_with(max_turns, python_timeout_seconds),
        )?;
    }
    Ok(())
}

fn dataset_run_codex(
    store: &Store,
    dataset: &str,
    count: usize,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let cases = load_dataset_cases(dataset)?;
    if cases.is_empty() || count == 0 {
        println!("No dataset cases selected.");
        return Ok(());
    }
    let provider = codex_provider(store, model)?;
    for case in cases.into_iter().take(count) {
        let session_id = create_dataset_session(store, dataset, &case)?;
        println!("{}  {}", case.task_id, session_id);
        io::stdout().flush()?;
        run_existing_session_with_provider(
            store,
            &provider,
            &session_id,
            cli_agent_options_with(max_turns, python_timeout_seconds),
        )?;
    }
    Ok(())
}

fn dataset_run_anthropic(
    store: &Store,
    dataset: &str,
    count: usize,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let cases = load_dataset_cases(dataset)?;
    if cases.is_empty() || count == 0 {
        println!("No dataset cases selected.");
        return Ok(());
    }
    let provider = anthropic_provider(store, model)?;
    for case in cases.into_iter().take(count) {
        let session_id = create_dataset_session(store, dataset, &case)?;
        println!("{}  {}", case.task_id, session_id);
        io::stdout().flush()?;
        run_existing_session_with_provider(
            store,
            &provider,
            &session_id,
            cli_agent_options_with(max_turns, python_timeout_seconds),
        )?;
    }
    Ok(())
}

fn dataset_run_openrouter(
    store: &Store,
    dataset: &str,
    count: usize,
    model: String,
    max_turns: usize,
    python_timeout_seconds: u64,
) -> Result<()> {
    let cases = load_dataset_cases(dataset)?;
    if cases.is_empty() || count == 0 {
        println!("No dataset cases selected.");
        return Ok(());
    }
    let provider = openrouter_provider(store, model)?;
    for case in cases.into_iter().take(count) {
        let session_id = create_dataset_session(store, dataset, &case)?;
        println!("{}  {}", case.task_id, session_id);
        io::stdout().flush()?;
        run_existing_session_with_provider(
            store,
            &provider,
            &session_id,
            cli_agent_options_with(max_turns, python_timeout_seconds),
        )?;
    }
    Ok(())
}

fn load_dataset_cases(dataset: &str) -> Result<Vec<DatasetCase>> {
    let path = resolve_dataset_path(dataset);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("parse {}", path.display()))
}

fn resolve_dataset_path(dataset: &str) -> PathBuf {
    let direct = PathBuf::from(dataset);
    if direct.exists() {
        return direct;
    }
    let with_ext = PathBuf::from("datasets").join(format!("{dataset}.json"));
    if with_ext.exists() {
        return with_ext;
    }
    direct
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
