# Phase-F repoint map (tui/cli → browser-use-agent)

Generated 2026-05-31 by the f-map agent. See conversation for full detail.

## DIRECT swaps (just change crate path)
AgentRunOptions, ProviderRunConfig, ProviderBackend, ConfigOverrides, RunConfigValueSource,
parse_config_overrides (config_overrides::); CollaborationModeKind (prompts::);
collect_agent_tree, resolve_agent_reference_in_tree, canonical_agent_reference (subagents::tree),
update_parent_from_child_run (subagents::parent_link); install_process_crypto_provider,
UnifiedExecShutdownCleanup, record_python_response_final_event, record_python_worker_event,
record_browser_script_response_events, review_prompt_{uncommitted_changes,base_branch,commit,custom} (infra::);
MessageHistoryConfig, MessageHistoryPersistence (history::).

## ADAPT (signature/name differs)
- run_existing_session_from_config / run_agent_from_config / run_existing_session_with_provider / run_fake_agent
  → entrypoint::run_session_with_config (ASYNC; store = Arc<Mutex<Store>>; returns session id String).
- message_history_config_for_cwd_with_options → now takes MessageHistorySettings, not &AgentRunOptions.
- append_workspace_context_event_with_options → context helpers (async; session_id not SessionMeta).
- root_session_id → subagents::tree::root_session (registry-based, name changed).
- agent-tree fns are REGISTRY/path-based now, not Store-based — callers need an AgentRegistry.
- product_analytics::* → infra::analytics::{capture_async,capture_blocking}.

## MISSING (claimed not exported) — VERIFY each before building
append_user_shell_command_context_event, append_workspace_context_event,
canonical_agent_path_from_task_name, cleanup_agent_runtime_state_for_agent_subtree,
configured_model_provider_id_for_cwd_with_options, configured_model_for_cwd_with_options,
default_model_for_cwd_with_options, display_agent_path_for_session, final_statuses_for_v1_wait,
last_task_message_for_agent, local_agent_status_value, model_catalog_for_cwd_with_options,
start_review_session, typed_user_input_payload_from_text_for_cwd,
typed_user_input_payload_from_items_for_cwd, FakeAgentOptions.

## Codex removal sites
CLI main.rs: 1308,1323,1412,1417,1508,1513,1953,1960,3341,3348,3364,5227,5244,5254,5274,5353
  (RunCodex/RunCodexSession/DatasetRunCodex command variants + helpers).
TUI settings.rs:46,117; main.rs:1706,5201,9013,9021,9053,9116,9168 (AgentBackend::Codex enum + tests).

## Cargo edits
tui + cli Cargo.toml: swap browser-use-core → browser-use-agent; ensure browser-use-protocol present.
