use bm25::{Document, Language, SearchEngineBuilder};
use browser_use_protocol::{FreeformToolFormat, ToolSpec};
use browser_use_providers::ModelShellType;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) mod command;
pub(crate) mod files;

const APPLY_PATCH_LARK_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?

filename: /(.+)/
add_line: "+" /(.*)/ LF -> line

change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF

%import common.LF
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolHandlerKind {
    Done,
    Browser,
    BrowserScript,
    Python,
    ShellCommand,
    ExecCommand,
    WriteStdin,
    ApplyPatch,
    ReadFile,
    SearchFiles,
    ListFiles,
    ViewImage,
    UpdatePlan,
    RequestUserInput,
    SpawnAgent,
    SpawnAgentV1,
    WaitAgent,
    WaitAgentV1,
    SendInputV1,
    ResumeAgentV1,
    SendMessage,
    FollowupTask,
    ListAgents,
    CloseAgent,
    CloseAgentV1,
    ToolSearch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolExposure {
    Direct,
    Deferred,
    DirectModelOnly,
    Hidden,
}

#[derive(Clone, Debug)]
pub(crate) struct RegisteredTool {
    spec: ToolSpec,
    handler: ToolHandlerKind,
    exposure: ToolExposure,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    allow_login_shell: bool,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            allow_login_shell: true,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MultiAgentToolSpecConfig {
    pub(crate) family: MultiAgentToolFamily,
    pub(crate) hide_spawn_agent_metadata: bool,
    pub(crate) wait_default_timeout_ms: i64,
    pub(crate) wait_min_timeout_ms: i64,
    pub(crate) wait_max_timeout_ms: i64,
    pub(crate) usage_hint_enabled: bool,
    pub(crate) usage_hint_text: Option<String>,
    pub(crate) max_concurrent_threads_per_session: usize,
    pub(crate) tool_namespace: Option<String>,
    pub(crate) non_code_mode_only: bool,
    pub(crate) request_user_input_default_mode_enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MultiAgentToolFamily {
    Disabled,
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ShellToolSpecConfig {
    pub(crate) shell_tool_enabled: bool,
    pub(crate) unified_exec_enabled: bool,
    pub(crate) model_shell_type: ModelShellType,
    pub(crate) allow_login_shell: bool,
}

impl Default for ShellToolSpecConfig {
    fn default() -> Self {
        Self {
            shell_tool_enabled: true,
            unified_exec_enabled: !cfg!(windows),
            model_shell_type: ModelShellType::ShellCommand,
            allow_login_shell: true,
        }
    }
}

impl ShellToolSpecConfig {
    fn resolved_shell_type(self) -> ModelShellType {
        if !self.shell_tool_enabled {
            return ModelShellType::Disabled;
        }
        if self.unified_exec_enabled {
            return ModelShellType::UnifiedExec;
        }
        match self.model_shell_type {
            ModelShellType::UnifiedExec | ModelShellType::Default | ModelShellType::Local => {
                ModelShellType::ShellCommand
            }
            other => other,
        }
    }
}

impl Default for MultiAgentToolSpecConfig {
    fn default() -> Self {
        Self {
            family: MultiAgentToolFamily::V2,
            hide_spawn_agent_metadata: false,
            wait_default_timeout_ms: 30_000,
            wait_min_timeout_ms: 10_000,
            wait_max_timeout_ms: 3_600_000,
            usage_hint_enabled: true,
            usage_hint_text: None,
            max_concurrent_threads_per_session: 4,
            tool_namespace: None,
            non_code_mode_only: false,
            request_user_input_default_mode_enabled: false,
        }
    }
}

const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";
const MULTI_AGENT_V1_NAMESPACE_DESCRIPTION: &str = "Tools for spawning and managing sub-agents.";
const MULTI_AGENT_V2_NAMESPACE_DESCRIPTION: &str = "Tools for spawning and managing sub-agents.";
const TOOL_SEARCH_TOOL_NAME: &str = "tool_search";
const TOOL_SEARCH_DEFAULT_LIMIT: usize = 8;
const TOOL_SEARCH_SOURCE_NAME_MULTI_AGENT: &str = "Multi-agent tools";
const TOOL_SEARCH_SOURCE_DESCRIPTION_MULTI_AGENT: &str = "Spawn and manage sub-agents.";

#[derive(Clone, Debug)]
struct ToolSearchEntry {
    search_text: String,
    spec: ToolSpec,
}

impl ToolRegistry {
    #[cfg(test)]
    pub(crate) fn browser_agent() -> Self {
        Self::browser_agent_with_agent_type_description(
            default_spawn_agent_type_description(),
            false,
            false,
        )
    }

    #[cfg(test)]
    pub(crate) fn browser_agent_with_agent_type_description(
        agent_type_description: String,
        hide_spawn_agent_metadata: bool,
        can_request_original_image_detail: bool,
    ) -> Self {
        Self::browser_agent_with_agent_type_description_and_model_description(
            agent_type_description,
            hide_spawn_agent_metadata,
            can_request_original_image_detail,
            browser_use_providers::spawn_agent_model_overrides_description(),
        )
    }

    #[cfg(test)]
    pub(crate) fn browser_agent_with_agent_type_description_and_model_description(
        agent_type_description: String,
        hide_spawn_agent_metadata: bool,
        can_request_original_image_detail: bool,
        model_overrides_description: String,
    ) -> Self {
        let config = MultiAgentToolSpecConfig {
            hide_spawn_agent_metadata,
            ..MultiAgentToolSpecConfig::default()
        };
        Self::browser_agent_with_agent_type_description_and_model_description_and_multi_agent_config(
            agent_type_description,
            config,
            ShellToolSpecConfig::default(),
            can_request_original_image_detail,
            model_overrides_description,
        )
    }

    pub(crate) fn browser_agent_with_agent_type_description_and_model_description_and_multi_agent_config(
        agent_type_description: String,
        multi_agent_config: MultiAgentToolSpecConfig,
        shell_config: ShellToolSpecConfig,
        can_request_original_image_detail: bool,
        model_overrides_description: String,
    ) -> Self {
        let mut registry = Self::default();
        registry.allow_login_shell = shell_config.allow_login_shell;
        match shell_config.resolved_shell_type() {
            ModelShellType::UnifiedExec => {
                registry.register(
                    exec_command_tool_spec(shell_config.allow_login_shell),
                    ToolHandlerKind::ExecCommand,
                );
                registry.register(write_stdin_tool_spec(), ToolHandlerKind::WriteStdin);
                registry.register_with_exposure(
                    shell_command_tool_spec(shell_config.allow_login_shell),
                    ToolHandlerKind::ShellCommand,
                    ToolExposure::Hidden,
                );
            }
            ModelShellType::Disabled => {}
            ModelShellType::Default | ModelShellType::Local | ModelShellType::ShellCommand => {
                registry.register(
                    shell_command_tool_spec(shell_config.allow_login_shell),
                    ToolHandlerKind::ShellCommand,
                );
            }
        }
        registry.register(apply_patch_tool_spec(), ToolHandlerKind::ApplyPatch);
        registry.register(
            view_image_tool_spec(can_request_original_image_detail),
            ToolHandlerKind::ViewImage,
        );
        registry.register(update_plan_tool_spec(), ToolHandlerKind::UpdatePlan);
        registry.register(
            request_user_input_tool_spec(
                multi_agent_config.request_user_input_default_mode_enabled,
            ),
            ToolHandlerKind::RequestUserInput,
        );
        registry.register(browser_tool_spec(), ToolHandlerKind::Browser);
        registry.register(browser_script_tool_spec(), ToolHandlerKind::BrowserScript);
        registry.register(done_tool_spec(), ToolHandlerKind::Done);
        match multi_agent_config.family {
            MultiAgentToolFamily::Disabled => {}
            MultiAgentToolFamily::V1 => {
                registry.register_with_exposure(
                    spawn_agent_v1_tool_spec(
                        &agent_type_description,
                        &multi_agent_config,
                        &model_overrides_description,
                    ),
                    ToolHandlerKind::SpawnAgentV1,
                    ToolExposure::Deferred,
                );
                registry.register_with_exposure(
                    send_input_v1_tool_spec(),
                    ToolHandlerKind::SendInputV1,
                    ToolExposure::Deferred,
                );
                registry.register_with_exposure(
                    resume_agent_v1_tool_spec(),
                    ToolHandlerKind::ResumeAgentV1,
                    ToolExposure::Deferred,
                );
                registry.register_with_exposure(
                    wait_agent_v1_tool_spec(&multi_agent_config),
                    ToolHandlerKind::WaitAgentV1,
                    ToolExposure::Deferred,
                );
                registry.register_with_exposure(
                    close_agent_v1_tool_spec(),
                    ToolHandlerKind::CloseAgentV1,
                    ToolExposure::Deferred,
                );
                registry.register_with_exposure(
                    tool_search_tool_spec(&[(
                        TOOL_SEARCH_SOURCE_NAME_MULTI_AGENT,
                        Some(TOOL_SEARCH_SOURCE_DESCRIPTION_MULTI_AGENT),
                    )]),
                    ToolHandlerKind::ToolSearch,
                    ToolExposure::Hidden,
                );
            }
            MultiAgentToolFamily::V2 => {
                let exposure = if multi_agent_config.non_code_mode_only {
                    ToolExposure::DirectModelOnly
                } else {
                    ToolExposure::Direct
                };
                registry.register(
                    spawn_agent_tool_spec(
                        &agent_type_description,
                        &multi_agent_config,
                        &model_overrides_description,
                    ),
                    ToolHandlerKind::SpawnAgent,
                );
                registry.register(
                    wait_agent_tool_spec(&multi_agent_config),
                    ToolHandlerKind::WaitAgent,
                );
                registry.register(
                    send_message_tool_spec(&multi_agent_config),
                    ToolHandlerKind::SendMessage,
                );
                registry.register(
                    followup_task_tool_spec(&multi_agent_config),
                    ToolHandlerKind::FollowupTask,
                );
                registry.register(
                    list_agents_tool_spec(&multi_agent_config),
                    ToolHandlerKind::ListAgents,
                );
                registry.register(
                    close_agent_tool_spec(&multi_agent_config),
                    ToolHandlerKind::CloseAgent,
                );
                if exposure == ToolExposure::DirectModelOnly {
                    for tool in &mut registry.tools {
                        if matches!(
                            tool.handler,
                            ToolHandlerKind::SpawnAgent
                                | ToolHandlerKind::WaitAgent
                                | ToolHandlerKind::SendMessage
                                | ToolHandlerKind::FollowupTask
                                | ToolHandlerKind::ListAgents
                                | ToolHandlerKind::CloseAgent
                        ) {
                            tool.exposure = exposure;
                        }
                    }
                }
            }
        }
        registry
    }

    pub(crate) fn register(&mut self, spec: ToolSpec, handler: ToolHandlerKind) {
        self.register_with_exposure(spec, handler, ToolExposure::Direct);
    }

    pub(crate) fn register_with_exposure(
        &mut self,
        spec: ToolSpec,
        handler: ToolHandlerKind,
        exposure: ToolExposure,
    ) {
        self.tools.push(RegisteredTool {
            spec,
            handler,
            exposure,
        });
    }

    pub(crate) fn allow_login_shell(&self) -> bool {
        self.allow_login_shell
    }

    #[cfg(test)]
    pub(crate) fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|tool| tool.exposure != ToolExposure::Hidden)
            .map(|tool| tool.spec.clone())
            .collect()
    }

    pub(crate) fn specs_for_model(
        &self,
        tool_search_supported: bool,
        namespace_tools_supported: bool,
    ) -> Vec<ToolSpec> {
        let use_tool_search =
            tool_search_supported && namespace_tools_supported && self.has_deferred_tools();
        let mut specs = self
            .tools
            .iter()
            .filter(|tool| match tool.exposure {
                ToolExposure::Hidden => false,
                ToolExposure::Deferred => !use_tool_search,
                ToolExposure::Direct | ToolExposure::DirectModelOnly => true,
            })
            .map(|tool| tool.spec.clone())
            .collect::<Vec<_>>();
        if use_tool_search {
            specs.push(tool_search_tool_spec(&[(
                TOOL_SEARCH_SOURCE_NAME_MULTI_AGENT,
                Some(TOOL_SEARCH_SOURCE_DESCRIPTION_MULTI_AGENT),
            )]));
        }
        specs
    }

    fn has_deferred_tools(&self) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.exposure == ToolExposure::Deferred)
    }

    pub(crate) fn handler_for(&self, name: &str) -> Option<ToolHandlerKind> {
        self.tools
            .iter()
            .find(|tool| tool.spec.name == name && tool.spec.namespace.is_none())
            .map(|tool| tool.handler)
    }

    pub(crate) fn handler_for_namespaced(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Option<ToolHandlerKind> {
        match namespace {
            Some(namespace) => self
                .tools
                .iter()
                .find(|tool| {
                    tool.spec.name == name && tool.spec.namespace.as_deref() == Some(namespace)
                })
                .map(|tool| tool.handler),
            None => self.handler_for(name),
        }
    }

    pub(crate) fn direct_handler_for_namespaced(
        &self,
        namespace: Option<&str>,
        name: &str,
    ) -> Option<ToolHandlerKind> {
        self.tools
            .iter()
            .find(|tool| {
                tool.exposure != ToolExposure::Hidden
                    && tool.spec.name == name
                    && tool.spec.namespace.as_deref() == namespace
            })
            .map(|tool| tool.handler)
    }

    pub(crate) fn search_deferred_tools(&self, query: &str, limit: usize) -> Vec<Value> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Vec::new();
        }
        let entries = self.deferred_tool_search_entries();
        if entries.is_empty() {
            return Vec::new();
        }
        let documents = entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| Document::new(idx, entry.search_text.clone()))
            .collect::<Vec<_>>();
        let search_engine =
            SearchEngineBuilder::<usize>::with_documents(Language::English, documents).build();
        let matches = search_engine
            .search(query, limit)
            .into_iter()
            .filter_map(|result| entries.get(result.document.id));
        coalesce_loadable_tool_specs(matches.map(|entry| deferred_tool_json(&entry.spec)))
    }

    fn deferred_tool_search_entries(&self) -> Vec<ToolSearchEntry> {
        self.tools
            .iter()
            .filter(|tool| tool.exposure == ToolExposure::Deferred)
            .map(|tool| ToolSearchEntry {
                search_text: multi_agent_v1_tool_search_text(tool.handler)
                    .unwrap_or_else(|| tool.spec.name.as_str())
                    .to_string(),
                spec: tool.spec.clone(),
            })
            .collect()
    }
}

fn multi_agent_v1_tool_search_text(handler: ToolHandlerKind) -> Option<&'static str> {
    match handler {
        ToolHandlerKind::SpawnAgentV1 => Some(
            "spawn_agent spawn agent subagent sub-agent delegate delegation parallel work worker explorer no-apps fork model reasoning",
        ),
        ToolHandlerKind::SendInputV1 => Some(
            "send_input send message existing agent subagent follow up interrupt redirect queue target",
        ),
        ToolHandlerKind::ResumeAgentV1 => {
            Some("resume_agent resume reopen closed agent subagent thread id target")
        }
        ToolHandlerKind::WaitAgentV1 => {
            Some("wait_agent wait agent subagent status final result complete timeout targets")
        }
        ToolHandlerKind::CloseAgentV1 => {
            Some("close_agent close shutdown stop agent subagent thread status target")
        }
        _ => None,
    }
}

fn tool_search_tool_spec(searchable_sources: &[(&str, Option<&str>)]) -> ToolSpec {
    let mut source_descriptions = BTreeMap::<String, Option<String>>::new();
    for (name, description) in searchable_sources {
        source_descriptions
            .entry((*name).to_string())
            .and_modify(|existing| {
                if existing.is_none() {
                    *existing = description.map(str::to_string);
                }
            })
            .or_insert_with(|| description.map(str::to_string));
    }
    let source_descriptions = if source_descriptions.is_empty() {
        "None currently enabled.".to_string()
    } else {
        source_descriptions
            .into_iter()
            .map(|(name, description)| match description {
                Some(description) => format!("- {name}: {description}"),
                None => format!("- {name}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    ToolSpec {
        name: TOOL_SEARCH_TOOL_NAME.to_string(),
        namespace: None,
        namespace_description: None,
        description: format!(
            "# Tool discovery\n\nSearches over deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nYou have access to tools from the following sources:\n{source_descriptions}\nSome of the tools may not have been provided to you upfront, and you should use this tool (`{TOOL_SEARCH_TOOL_NAME}`) to search for the required tools. For MCP tool discovery, always use `{TOOL_SEARCH_TOOL_NAME}` instead of `list_mcp_resources` or `list_mcp_resource_templates`."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query for deferred tools.",
                },
                "limit": {
                    "type": "number",
                    "description": format!(
                        "Maximum number of tools to return (defaults to {TOOL_SEARCH_DEFAULT_LIMIT})."
                    ),
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
        output_schema: None,
        freeform: None,
    }
}

fn deferred_tool_json(spec: &ToolSpec) -> Value {
    let function_tool = if let Some(format) = &spec.freeform {
        serde_json::json!({
            "type": "custom",
            "name": spec.name.clone(),
            "description": spec.description.clone(),
            "defer_loading": true,
            "format": {
                "type": format.kind.clone(),
                "syntax": format.syntax.clone(),
                "definition": format.definition.clone(),
            },
        })
    } else {
        serde_json::json!({
            "type": "function",
            "name": spec.name.clone(),
            "description": spec.description.clone(),
            "strict": false,
            "defer_loading": true,
            "parameters": spec.input_schema.clone(),
        })
    };
    if let Some(namespace) = spec.namespace.as_deref() {
        serde_json::json!({
            "type": "namespace",
            "name": namespace,
            "description": spec
                .namespace_description
                .clone()
                .unwrap_or_else(|| format!("Tools in the {namespace} namespace.")),
            "tools": [function_tool],
        })
    } else {
        function_tool
    }
}

fn coalesce_loadable_tool_specs(specs: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut output = Vec::<Value>::new();
    let mut namespace_indices = BTreeMap::<String, usize>::new();
    for spec in specs {
        let Some(namespace) = spec
            .get("name")
            .and_then(Value::as_str)
            .filter(|_| spec.get("type").and_then(Value::as_str) == Some("namespace"))
            .map(str::to_string)
        else {
            output.push(spec);
            continue;
        };
        if let Some(index) = namespace_indices.get(&namespace).copied() {
            let new_tools = spec
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if let Some(tools) = output[index].get_mut("tools").and_then(Value::as_array_mut) {
                tools.extend(new_tools);
            }
            continue;
        }
        namespace_indices.insert(namespace, output.len());
        output.push(spec);
    }
    for spec in &mut output {
        if let Some(tools) = spec.get_mut("tools").and_then(Value::as_array_mut) {
            tools.sort_by(|left, right| {
                left.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .cmp(
                        right
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
            });
        }
    }
    output
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpawnAgentRoleDescription {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) config_file: Option<PathBuf>,
}

pub(crate) fn spawn_agent_type_description_for_roles(
    user_roles: impl IntoIterator<Item = SpawnAgentRoleDescription>,
) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let mut formatted_roles = Vec::new();
    for role in user_roles {
        if seen.insert(role.name.clone()) {
            formatted_roles.push(format_agent_role(&role));
        }
    }
    for (name, description) in built_in_agent_role_descriptions() {
        if seen.insert(name.to_string()) {
            formatted_roles.push(format_agent_role(&SpawnAgentRoleDescription {
                name: name.to_string(),
                description: Some(description.to_string()),
                config_file: None,
            }));
        }
    }
    format!(
        "Optional type name for the new agent. If omitted, `default` is used.\nAvailable roles:\n{}",
        formatted_roles.join("\n")
    )
}

#[cfg(test)]
fn default_spawn_agent_type_description() -> String {
    spawn_agent_type_description_for_roles(std::iter::empty::<SpawnAgentRoleDescription>())
}

fn built_in_agent_role_descriptions() -> [(&'static str, &'static str); 3] {
    [
        ("default", "Default agent."),
        (
            "explorer",
            r#"Use `explorer` for specific codebase questions.
Explorers are fast and authoritative.
They must be used to ask specific, well-scoped questions on the codebase.
Rules:
- In order to avoid redundant work, you should avoid exploring the same problem that explorers have already covered. Typically, you should trust the explorer results without additional verification. You are still allowed to inspect the code yourself to gain the needed context!
- You are encouraged to spawn up multiple explorers in parallel when you have multiple distinct questions to ask about the codebase that can be answered independently. This allows you to get more information faster without waiting for one question to finish before asking the next. While waiting for the explorer results, you can continue working on other local tasks that do not depend on those results. This parallelism is a key advantage of delegation, so use it whenever you have multiple questions to ask.
- Reuse existing explorers for related questions."#,
        ),
        (
            "worker",
            r#"Use for execution and production work.
Typical tasks:
- Implement part of a feature
- Fix tests or bugs
- Split large refactors into independent chunks
Rules:
- Explicitly assign **ownership** of the task (files / responsibility). When the subtask involves code changes, you should clearly specify which files or modules the worker is responsible for. This helps avoid merge conflicts and ensures accountability. For example, you can say "Worker 1 is responsible for updating the authentication module, while Worker 2 will handle the database layer." By defining clear ownership, you can delegate more effectively and reduce coordination overhead.
- Always tell workers they are **not alone in the codebase**, and they should not revert the edits made by others, and they should adjust their implementation to accommodate the changes made by others. This is important because there may be multiple workers making changes in parallel, and they need to be aware of each other's work to avoid conflicts and ensure a cohesive final product."#,
        ),
    ]
}

fn format_agent_role(role: &SpawnAgentRoleDescription) -> String {
    match role.description.as_deref() {
        Some(description) => {
            let locked_settings_note = role
                .config_file
                .as_deref()
                .and_then(locked_agent_role_settings_note)
                .unwrap_or_default();
            format!(
                "{}: {{\n{}{}\n}}",
                role.name, description, locked_settings_note
            )
        }
        None => format!("{}: no description", role.name),
    }
}

fn locked_agent_role_settings_note(config_file: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(config_file).ok()?;
    let role_toml = contents.parse::<toml::Value>().ok()?;
    let model = role_toml.get("model").and_then(toml::Value::as_str);
    let reasoning_effort = role_toml
        .get("model_reasoning_effort")
        .and_then(toml::Value::as_str);
    let service_tier = role_toml.get("service_tier").and_then(toml::Value::as_str);

    let model_and_reasoning_note = match (model, reasoning_effort) {
        (Some(model), Some(reasoning_effort)) => format!(
            "\n- This role's model is set to `{model}` and its reasoning effort is set to `{reasoning_effort}`. These settings cannot be changed."
        ),
        (Some(model), None) => {
            format!("\n- This role's model is set to `{model}` and cannot be changed.")
        }
        (None, Some(reasoning_effort)) => {
            format!(
                "\n- This role's reasoning effort is set to `{reasoning_effort}` and cannot be changed."
            )
        }
        (None, None) => String::new(),
    };
    let service_tier_note = service_tier
        .map(|service_tier| {
            format!(
                "\n- This role's service tier is set to `{service_tier}`. If it is supported by the resolved model, it takes precedence over a valid spawn request service tier."
            )
        })
        .unwrap_or_default();
    Some(format!("{model_and_reasoning_note}{service_tier_note}"))
}

fn unified_exec_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "chunk_id": {
                "type": "string",
                "description": "Chunk identifier included when the response reports one."
            },
            "wall_time_seconds": {
                "type": "number",
                "description": "Elapsed wall time spent waiting for output in seconds."
            },
            "exit_code": {
                "type": "number",
                "description": "Process exit code when the command finished during this call."
            },
            "session_id": {
                "type": "number",
                "description": "Session identifier to pass to write_stdin when the process is still running."
            },
            "original_token_count": {
                "type": "number",
                "description": "Approximate token count before output truncation."
            },
            "output": {
                "type": "string",
                "description": "Command output text, possibly truncated."
            }
        },
        "required": ["wall_time_seconds", "output"],
        "additionalProperties": false
    })
}

fn view_image_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "image_url": {
                "type": "string",
                "description": "Data URL for the loaded image."
            },
            "detail": {
                "type": "string",
                "enum": ["high", "original"],
                "description": "Image detail hint returned by view_image. Returns `high` for default resized behavior or `original` when original resolution is preserved."
            }
        },
        "required": ["image_url", "detail"],
        "additionalProperties": false
    })
}

fn agent_status_output_schema() -> Value {
    serde_json::json!({
        "oneOf": [
            {
                "type": "string",
                "enum": ["pending_init", "running", "interrupted", "shutdown", "not_found"]
            },
            {
                "type": "object",
                "properties": {
                    "completed": {
                        "type": ["string", "null"]
                    }
                },
                "required": ["completed"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "errored": {
                        "type": "string"
                    }
                },
                "required": ["errored"],
                "additionalProperties": false
            }
        ]
    })
}

fn spawn_agent_output_schema(hide_agent_metadata: bool) -> Value {
    if hide_agent_metadata {
        return serde_json::json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "description": "Canonical task name for the spawned agent."
                }
            },
            "required": ["task_name"],
            "additionalProperties": false
        });
    }

    serde_json::json!({
        "type": "object",
        "properties": {
            "task_name": {
                "type": "string",
                "description": "Canonical task name for the spawned agent."
            },
            "nickname": {
                "type": ["string", "null"],
                "description": "User-facing nickname for the spawned agent when available."
            }
        },
        "required": ["task_name", "nickname"],
        "additionalProperties": false
    })
}

fn spawn_agent_v1_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "description": "Thread identifier for the spawned agent."
            },
            "nickname": {
                "type": ["string", "null"],
                "description": "User-facing nickname for the spawned agent when available."
            }
        },
        "required": ["agent_id", "nickname"],
        "additionalProperties": false
    })
}

fn send_input_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "submission_id": {
                "type": "string",
                "description": "Identifier for the queued input submission."
            }
        },
        "required": ["submission_id"],
        "additionalProperties": false
    })
}

fn resume_agent_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": agent_status_output_schema()
        },
        "required": ["status"],
        "additionalProperties": false
    })
}

fn wait_agent_v1_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "object",
                "description": "Final statuses keyed by agent id.",
                "additionalProperties": agent_status_output_schema()
            },
            "timed_out": {
                "type": "boolean",
                "description": "Whether the wait call returned due to timeout before any agent reached a final status."
            }
        },
        "required": ["status", "timed_out"],
        "additionalProperties": false
    })
}

fn wait_agent_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Brief wait summary without the agent's final content."
            },
            "timed_out": {
                "type": "boolean",
                "description": "Whether the wait call returned because no mailbox update arrived before the timeout."
            }
        },
        "required": ["message", "timed_out"],
        "additionalProperties": false
    })
}

fn list_agents_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "agents": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "agent_name": {
                            "type": "string",
                            "description": "Canonical task name for the agent when available, otherwise the agent id."
                        },
                        "agent_status": {
                            "description": "Last known status of the agent.",
                            "allOf": [agent_status_output_schema()]
                        },
                        "last_task_message": {
                            "type": ["string", "null"],
                            "description": "Most recent user or inter-agent instruction received by the agent, when available."
                        }
                    },
                    "required": ["agent_name", "agent_status", "last_task_message"],
                    "additionalProperties": false
                },
                "description": "Live agents visible in the current root thread tree."
            }
        },
        "required": ["agents"],
        "additionalProperties": false
    })
}

fn close_agent_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "previous_status": {
                "description": "The agent status observed before shutdown was requested.",
                "allOf": [agent_status_output_schema()]
            }
        },
        "required": ["previous_status"],
        "additionalProperties": false
    })
}

fn exec_command_tool_spec(allow_login_shell: bool) -> ToolSpec {
    let mut properties = serde_json::json!({
        "cmd": {
            "type": "string",
            "description": "Shell command to execute."
        },
        "workdir": {
            "type": "string",
            "description": "Optional working directory to run the command in; defaults to the turn cwd."
        },
        "shell": {
            "type": "string",
            "description": "Shell binary to launch. Defaults to the user's default shell."
        },
        "tty": {
            "type": "boolean",
            "description": "Whether to allocate a TTY for the command. Defaults to false (plain pipes); set to true to open a PTY and access TTY process."
        },
        "yield_time_ms": {
            "type": "integer",
            "description": "How long to wait (in milliseconds) for output before yielding."
        },
        "max_output_tokens": {
            "type": "integer",
            "description": "Maximum number of tokens to return. Excess output will be truncated."
        },
        "sandbox_permissions": {
            "type": "string",
            "description": "Sandbox permissions for the command. Set to \"require_escalated\" to request running without sandbox restrictions; defaults to \"use_default\"."
        },
        "justification": {
            "type": "string",
            "description": "Only set if sandbox_permissions is \"require_escalated\". Request approval from the user to run this command outside the sandbox. Phrased as a simple question that summarizes the purpose of the command as it relates to the task at hand - e.g. 'Do you want to fetch and pull the latest version of this git branch?'"
        },
        "prefix_rule": {
            "type": "array",
            "description": "Only specify when sandbox_permissions is `require_escalated`. Suggest a prefix command pattern that will allow you to fulfill similar requests from the user in the future. Should be a short but reasonable prefix, e.g. [\"git\", \"pull\"] or [\"uv\", \"run\"] or [\"pytest\"].",
            "items": { "type": "string" }
        }
    });
    if allow_login_shell {
        properties["login"] = serde_json::json!({
            "type": "boolean",
            "description": "Whether to run the shell with -l/-i semantics. Defaults to true."
        });
    }
    ToolSpec {
        name: "exec_command".to_string(),
        namespace: None,
        namespace_description: None,
        description:
            "Runs a command in a PTY, returning output or a session ID for ongoing interaction."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["cmd"],
            "additionalProperties": false
        }),
        output_schema: Some(unified_exec_output_schema()),
        freeform: None,
    }
}

fn write_stdin_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "write_stdin".to_string(),
        namespace: None,
        namespace_description: None,
        description:
            "Writes characters to an existing unified exec session and returns recent output."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "number",
                    "description": "Identifier of the running unified exec session."
                },
                "chars": {
                    "type": "string",
                    "description": "Bytes to write to stdin (may be empty to poll)."
                },
                "yield_time_ms": {
                    "type": "integer",
                    "description": "How long to wait (in milliseconds) for output before yielding."
                },
                "max_output_tokens": {
                    "type": "integer",
                    "description": "Maximum number of tokens to return. Excess output will be truncated."
                }
            },
            "required": ["session_id"],
            "additionalProperties": false
        }),
        output_schema: Some(unified_exec_output_schema()),
        freeform: None,
    }
}

fn shell_command_tool_spec(allow_login_shell: bool) -> ToolSpec {
    let mut properties = serde_json::json!({
        "command": {
            "type": "string",
            "description": "The shell script to execute in the user's default shell"
        },
        "workdir": {
            "type": "string",
            "description": "The working directory to execute the command in"
        },
        "timeout_ms": {
            "type": "number",
            "description": "The timeout for the command in milliseconds"
        },
        "sandbox_permissions": {
            "type": "string",
            "description": "Sandbox permissions for the command. Set to \"require_escalated\" to request running without sandbox restrictions; defaults to \"use_default\"."
        },
        "justification": {
            "type": "string",
            "description": "Only set if sandbox_permissions is \"require_escalated\". Request approval from the user to run this command outside the sandbox. Phrased as a simple question that summarizes the purpose of the command as it relates to the task at hand - e.g. 'Do you want to fetch and pull the latest version of this git branch?'"
        },
        "prefix_rule": {
            "type": "array",
            "description": "Only specify when sandbox_permissions is `require_escalated`. Suggest a prefix command pattern that will allow you to fulfill similar requests from the user in the future. Should be a short but reasonable prefix, e.g. [\"git\", \"pull\"] or [\"uv\", \"run\"] or [\"pytest\"].",
            "items": { "type": "string" }
        }
    });
    if allow_login_shell {
        properties["login"] = serde_json::json!({
            "type": "boolean",
            "description": "Whether to run the shell with login shell semantics. Defaults to true."
        });
    }
    ToolSpec {
        name: "shell_command".to_string(),
        namespace: None,
        namespace_description: None,
        description:
            "Runs a shell command and returns its output.\n- Always set the `workdir` param when using the shell_command function. Do not use `cd` unless absolutely necessary."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["command"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn apply_patch_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "apply_patch".to_string(),
        namespace: None,
        namespace_description: None,
        description:
            "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": "Codex-style patch body."
                }
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: Some(FreeformToolFormat {
            kind: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: APPLY_PATCH_LARK_GRAMMAR.to_string(),
        }),
    }
}

fn view_image_tool_spec(can_request_original_image_detail: bool) -> ToolSpec {
    let mut properties = serde_json::json!({
        "path": {
            "type": "string",
            "description": "Image path to inspect. Relative paths resolve from the task cwd."
        }
    });
    if can_request_original_image_detail {
        properties["detail"] = serde_json::json!({
            "type": "string",
            "enum": ["high", "original"],
            "description": "Optional detail override. Supported values are `high` and `original`; omit this field for default high resized behavior. Use `original` to preserve the file's original resolution instead of resizing to fit. This is important when high-fidelity image perception or precise localization is needed, especially for CUA agents."
        });
    }
    ToolSpec {
        name: "view_image".to_string(),
        namespace: None,
        namespace_description: None,
        description: concat!(
            "Sequential local image inspection tool. Use this only for images already saved ",
            "on disk, such as screenshots or artifacts. This tool is not parallel-safe: visual ",
            "context should be inspected in order with the browser actions that produced it. ",
            "It must not be called in parallel with browser actions or other image views. ",
            "It is not a browser screenshot command; use browser_script screenshot helpers ",
            "to create browser screenshots first."
        )
        .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["path"],
            "additionalProperties": false
        }),
        output_schema: Some(view_image_output_schema()),
        freeform: None,
    }
}

fn browser_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "browser".to_string(),
        namespace: None,
        namespace_description: None,
        description: include_str!("../../../../prompts/browser-tool-description.md")
            .trim()
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "CLI-like browser command. The leading word `browser` is optional, e.g. `status --json` or `browser connect local`."
                }
            },
            "required": ["cmd"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn browser_script_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "browser_script".to_string(),
        namespace: None,
        namespace_description: None,
        description: include_str!("../../../../prompts/browser-script-tool-description.md")
            .trim()
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Python code to run in a fresh process with browser helpers preimported."
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn done_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "done".to_string(),
        namespace: None,
        namespace_description: None,
        description: "Finish the browser task with a final user-facing result.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "result": {
                    "type": "string",
                    "description": "Final answer for the user. If the task requests an exact inline format such as JSON, CSV, a table, markdown, or a schema-shaped response, put that content here. When both result and result_file are supplied, result remains the final answer."
                },
                "result_file": {
                    "type": "string",
                    "description": "Optional path to a text/JSON/CSV result file saved as an artifact. Relative paths resolve against the current working directory. Use this by itself only when a file pointer or artifact summary is an acceptable final answer."
                },
                "finish_and_close_children": {
                    "type": "boolean",
                    "description": "If true, cancel active child agents before finishing. Otherwise done() is rejected while children are active."
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn update_plan_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "update_plan".to_string(),
        namespace: None,
        namespace_description: None,
        description: concat!(
            "Updates the task plan.\n",
            "Provide an optional explanation and a list of plan items, each with a step and status.\n",
            "At most one step can be in_progress at a time.\n",
        )
        .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "explanation": {
                    "type": "string"
                },
                "plan": {
                    "type": "array",
                    "description": "The list of steps",
                    "items": {
                        "type": "object",
                        "properties": {
                            "step": {
                                "type": "string"
                            },
                            "status": {
                                "type": "string",
                                "description": "One of: pending, in_progress, completed"
                            }
                        },
                        "required": ["step", "status"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["plan"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn request_user_input_tool_spec(default_mode_enabled: bool) -> ToolSpec {
    let mode_description = if default_mode_enabled {
        "Default or Plan mode"
    } else {
        "Plan mode"
    };
    ToolSpec {
        name: "request_user_input".to_string(),
        namespace: None,
        namespace_description: None,
        description: format!(
            "Request user input for one to three short questions and wait for the response. This tool is only available in {mode_description}."
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "Questions to show the user. Prefer 1 and do not exceed 3",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable identifier for mapping answers (snake_case)."
                            },
                            "header": {
                                "type": "string",
                                "description": "Short header label shown in the UI (12 or fewer chars)."
                            },
                            "question": {
                                "type": "string",
                                "description": "Single-sentence prompt shown to the user."
                            },
                            "options": {
                                "type": "array",
                                "description": "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "User-facing label (1-5 words)."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "One short sentence explaining impact/tradeoff if selected."
                                        }
                                    },
                                    "required": ["label", "description"],
                                    "additionalProperties": false
                                }
                            }
                        },
                        "required": ["id", "header", "question", "options"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn spawn_agent_tool_spec(
    agent_type_description: &str,
    multi_agent_config: &MultiAgentToolSpecConfig,
    model_overrides_description: &str,
) -> ToolSpec {
    let model_overrides_description = if multi_agent_config.hide_spawn_agent_metadata {
        String::new()
    } else {
        format!("{model_overrides_description}\n")
    };
    let concurrency_guidance = format!(
        "This session is configured with `max_concurrent_threads_per_session = {}` for concurrently open agent threads.",
        multi_agent_config.max_concurrent_threads_per_session
    );
    let usage_hint = if !multi_agent_config.usage_hint_enabled {
        String::new()
    } else if let Some(usage_hint_text) = multi_agent_config.usage_hint_text.as_deref() {
        format!("\n{usage_hint_text}")
    } else {
        format!("\n{}", default_spawn_agent_usage_hint())
    };
    let mut input_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Initial plain-text task for the new agent."
            },
            "task_name": {
                "type": "string",
                "description": "Task name for the new agent. Use lowercase letters, digits, and underscores."
            },
            "agent_type": {
                "type": "string",
                "description": agent_type_description
            },
            "fork_turns": {
                "type": "string",
                "description": "Optional number of turns to fork. Defaults to `all`. Use `none`, `all`, or a positive integer string such as `3` to fork only the most recent turns."
            },
            "model": {
                "type": "string",
                "description": "Optional model override for the new agent. Leave unset to inherit the same model as the parent, which is the preferred default. Only set this when the user explicitly asks for a different model or the task clearly requires one."
            },
            "reasoning_effort": {
                "type": "string",
                "description": "Optional reasoning effort override for the new agent. Replaces the inherited reasoning effort."
            },
            "service_tier": {
                "type": "string",
                "description": "Optional service tier override for the new agent. Leave unset unless the user explicitly asks for one."
            }
        },
        "required": ["task_name", "message"],
        "additionalProperties": false
    });
    if multi_agent_config.hide_spawn_agent_metadata {
        if let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            for field in ["agent_type", "model", "reasoning_effort", "service_tier"] {
                properties.remove(field);
            }
        }
    }
    ToolSpec {
        name: "spawn_agent".to_string(),
        namespace: multi_agent_config.tool_namespace.clone(),
        namespace_description: multi_agent_config
            .tool_namespace
            .as_ref()
            .map(|_| MULTI_AGENT_V2_NAMESPACE_DESCRIPTION.to_string()),
        description: format!(
            "{model_overrides_description}Spawns an agent to work on the specified task. \
If your current task is `/root/task1` and you spawn_agent with task_name \"task_3\" the agent will have canonical task name `/root/task1/task_3`. \
You are then able to refer to this agent as `task_3` or `/root/task1/task_3` interchangeably. However an agent `/root/task2/task_3` would only be able to communicate with this agent via its canonical name `/root/task1/task_3`. \
The spawned agent will have the same tools as you and the ability to spawn its own subagents. \
Spawned agents inherit your current model by default. Omit `model` to use that preferred default; set `model` only when an explicit override is needed. \
It will be able to send you and other running agents messages, and its final answer will be provided to you when it finishes. \
The new agent's canonical task name will be provided to it along with the message. \
{concurrency_guidance}{usage_hint}"
        ),
        input_schema,
        output_schema: Some(spawn_agent_output_schema(
            multi_agent_config.hide_spawn_agent_metadata,
        )),
        freeform: None,
    }
}

fn spawn_agent_v1_tool_spec(
    agent_type_description: &str,
    multi_agent_config: &MultiAgentToolSpecConfig,
    model_overrides_description: &str,
) -> ToolSpec {
    let model_overrides_description = if multi_agent_config.hide_spawn_agent_metadata {
        String::new()
    } else {
        format!("{model_overrides_description}\n")
    };
    let usage_hint = if !multi_agent_config.usage_hint_enabled {
        String::new()
    } else if let Some(usage_hint_text) = multi_agent_config.usage_hint_text.as_deref() {
        format!("\n{usage_hint_text}")
    } else {
        format!("\n{}", default_spawn_agent_usage_hint())
    };
    let mut input_schema = serde_json::json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Initial plain-text task for the new agent. Use either message or items."
            },
            "items": collab_input_items_schema(),
            "agent_type": {
                "type": "string",
                "description": agent_type_description
            },
            "fork_context": {
                "type": "boolean",
                "description": "When true, fork the current thread history into the new agent before sending the initial prompt. This must be used when you want the new agent to have exactly the same context as you."
            },
            "model": {
                "type": "string",
                "description": "Optional model override for the new agent. Leave unset to inherit the same model as the parent, which is the preferred default. Only set this when the user explicitly asks for a different model or there is a clear task-specific reason."
            },
            "reasoning_effort": {
                "type": "string",
                "description": "Optional reasoning effort override for the new agent. Replaces the inherited reasoning effort."
            },
            "service_tier": {
                "type": "string",
                "description": "Optional service tier override for the new agent. Leave unset unless the user explicitly asks for one."
            }
        },
        "additionalProperties": false
    });
    if multi_agent_config.hide_spawn_agent_metadata {
        if let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            for field in ["agent_type", "model", "reasoning_effort", "service_tier"] {
                properties.remove(field);
            }
        }
    }
    ToolSpec {
        name: "spawn_agent".to_string(),
        namespace: Some(MULTI_AGENT_V1_NAMESPACE.to_string()),
        namespace_description: Some(MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string()),
        description: format!(
            "{model_overrides_description}Spawn a sub-agent for a well-scoped task. \
Returns the spawned agent id plus the user-facing nickname when available. \
Spawned agents inherit your current model by default. Omit `model` to use that preferred default; set `model` only when an explicit override is needed.{usage_hint}"
        ),
        input_schema,
        output_schema: Some(spawn_agent_v1_output_schema()),
        freeform: None,
    }
}

fn default_spawn_agent_usage_hint() -> &'static str {
    "This spawn_agent tool provides you access to sub-agents that inherit your current model by default. Do not set the `model` field unless the user explicitly asks for a different model or there is a clear task-specific reason. You should follow the rules and guidelines below to use this tool.\n\n\
Only use `spawn_agent` if and only if the user explicitly asks for sub-agents, delegation, or parallel agent work.\n\
Requests for depth, thoroughness, research, investigation, or detailed codebase analysis do not count as permission to spawn.\n\
Agent-role guidance below only helps choose which agent to use after spawning is already authorized; it never authorizes spawning by itself.\n\n\
### When to delegate vs. do the subtask yourself\n\
- First, quickly analyze the overall user task and form a succinct high-level plan. Identify which tasks are immediate blockers on the critical path, and which tasks are sidecar tasks that are needed but can run in parallel without blocking the next local step. As part of that plan, explicitly decide what immediate task you should do locally right now. Do this planning step before delegating to agents so you do not hand off the immediate blocking task to a submodel and then waste time waiting on it.\n\
- Use a subagent when a subtask is easy enough for it to handle and can run in parallel with your local work. Prefer delegating concrete, bounded sidecar tasks that materially advance the main task without blocking your immediate next local step.\n\
- Do not delegate urgent blocking work when your immediate next step depends on that result. If the very next action is blocked on that task, the main rollout should usually do it locally to keep the critical path moving.\n\
- Keep work local when the subtask is too difficult to delegate well and when it is tightly coupled, urgent, or likely to block your immediate next step.\n\n\
### Designing delegated subtasks\n\
- Subtasks must be concrete, well-defined, and self-contained.\n\
- Delegated subtasks must materially advance the main task.\n\
- Do not duplicate work between the main rollout and delegated subtasks.\n\
- Avoid issuing multiple delegate calls on the same unresolved thread unless the new delegated task is genuinely different and necessary.\n\
- Narrow the delegated ask to the concrete output you need next.\n\
- For coding tasks, prefer delegating concrete code-change worker subtasks over read-only explorer analysis when the subagent can make a bounded patch in a clear write scope.\n\
- When delegating coding work, instruct the submodel to edit files directly in its forked workspace and list the file paths it changed in the final answer.\n\
- For code-edit subtasks, decompose work so each delegated task has a disjoint write set.\n\n\
### After you delegate\n\
- Call wait_agent very sparingly. Only call wait_agent when you need the result immediately for the next critical-path step and you are blocked until it returns.\n\
- Do not redo delegated subagent tasks yourself; focus on integrating results or tackling non-overlapping work.\n\
- While the subagent is running in the background, do meaningful non-overlapping work immediately.\n\
- Do not repeatedly wait by reflex.\n\
- When a delegated coding task returns, quickly review the uploaded changes, then integrate or refine them.\n\n\
### Parallel delegation patterns\n\
- Run multiple independent information-seeking subtasks in parallel when you have distinct questions that can be answered independently.\n\
- Split implementation into disjoint codebase slices and spawn multiple agents for them in parallel when the write scopes do not overlap.\n\
- Delegate verification only when it can run in parallel with ongoing implementation and is likely to catch a concrete risk before final integration.\n\
- The key is to find opportunities to spawn multiple independent subtasks in parallel within the same round, while ensuring each subtask is well-defined, self-contained, and materially advances the main task."
}

fn collab_input_items_schema() -> Value {
    serde_json::json!({
        "type": "array",
        "description": "Structured input items. Use this to pass explicit mentions (for example app:// connector paths).",
        "items": {
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "description": "Input item type: text, image, local_image, skill, or mention."
                },
                "text": {
                    "type": "string",
                    "description": "Text content when type is text."
                },
                "image_url": {
                    "type": "string",
                    "description": "Image URL when type is image."
                },
                "detail": {
                    "type": "string",
                    "description": "Image detail level when type is image or local_image.",
                    "enum": ["auto", "low", "high"]
                },
                "path": {
                    "type": "string",
                    "description": "Path when type is local_image/skill, or structured mention target such as app://<connector-id> or plugin://<plugin-name>@<marketplace-name> when type is mention."
                },
                "name": {
                    "type": "string",
                    "description": "Display name when type is skill or mention."
                }
            },
            "additionalProperties": false
        }
    })
}

fn send_input_v1_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "send_input".to_string(),
        namespace: Some(MULTI_AGENT_V1_NAMESPACE.to_string()),
        namespace_description: Some(MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string()),
        description: "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Agent id to message (from spawn_agent)."
                },
                "message": {
                    "type": "string",
                    "description": "Legacy plain-text message to send to the agent. Use either message or items."
                },
                "items": collab_input_items_schema(),
                "interrupt": {
                    "type": "boolean",
                    "description": "When true, stop the agent's current task and handle this immediately. When false (default), queue this message."
                }
            },
            "required": ["target"],
            "additionalProperties": false
        }),
        output_schema: Some(send_input_output_schema()),
        freeform: None,
    }
}

fn resume_agent_v1_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "resume_agent".to_string(),
        namespace: Some(MULTI_AGENT_V1_NAMESPACE.to_string()),
        namespace_description: Some(MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string()),
        description: "Resume a previously closed agent by id so it can receive send_input and wait_agent calls."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Agent id to resume."
                }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        output_schema: Some(resume_agent_output_schema()),
        freeform: None,
    }
}

fn wait_agent_v1_tool_spec(multi_agent_config: &MultiAgentToolSpecConfig) -> ToolSpec {
    ToolSpec {
        name: "wait_agent".to_string(),
        namespace: Some(MULTI_AGENT_V1_NAMESPACE.to_string()),
        namespace_description: Some(MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string()),
        description: "Wait for agents to reach a final status. Completed statuses may include the agent's final message. Returns empty status when timed out. Once the agent reaches a final status, a notification message will be received containing the same completed status."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "targets": {
                    "type": "array",
                    "description": "Agent ids to wait on. Pass multiple ids to wait for whichever finishes first.",
                    "items": {
                        "type": "string"
                    }
                },
                "timeout_ms": {
                    "type": "number",
                    "description": format!(
                        "Optional timeout in milliseconds. Defaults to {}, min {}, max {}. Prefer longer waits (minutes) to avoid busy polling.",
                        multi_agent_config.wait_default_timeout_ms,
                        multi_agent_config.wait_min_timeout_ms,
                        multi_agent_config.wait_max_timeout_ms,
                    )
                }
            },
            "required": ["targets"],
            "additionalProperties": false
        }),
        output_schema: Some(wait_agent_v1_output_schema()),
        freeform: None,
    }
}

fn close_agent_v1_tool_spec() -> ToolSpec {
    ToolSpec {
        name: "close_agent".to_string(),
        namespace: Some(MULTI_AGENT_V1_NAMESPACE.to_string()),
        namespace_description: Some(MULTI_AGENT_V1_NAMESPACE_DESCRIPTION.to_string()),
        description: "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Don't keep agents open for too long if they are not needed anymore."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Agent id to close (from spawn_agent)."
                }
            },
            "required": ["target"],
            "additionalProperties": false
        }),
        output_schema: Some(close_agent_output_schema()),
        freeform: None,
    }
}

fn wait_agent_tool_spec(multi_agent_config: &MultiAgentToolSpecConfig) -> ToolSpec {
    ToolSpec {
        name: "wait_agent".to_string(),
        namespace: multi_agent_config.tool_namespace.clone(),
        namespace_description: multi_agent_config
            .tool_namespace
            .as_ref()
            .map(|_| MULTI_AGENT_V2_NAMESPACE_DESCRIPTION.to_string()),
        description: "Wait for a mailbox update from any live agent, including queued messages and final-status notifications. Does not return the content; returns either a summary of which agents have updates (if any), or a timeout summary if no mailbox update arrives before the deadline."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "timeout_ms": {
                    "type": "number",
                    "description": format!(
                        "Optional timeout in milliseconds. Defaults to {}, min {}, max {}.",
                        multi_agent_config.wait_default_timeout_ms,
                        multi_agent_config.wait_min_timeout_ms,
                        multi_agent_config.wait_max_timeout_ms,
                    )
                }
            },
            "required": [],
            "additionalProperties": false
        }),
        output_schema: Some(wait_agent_output_schema()),
        freeform: None,
    }
}

fn multi_agent_namespace_description(config: &MultiAgentToolSpecConfig) -> Option<String> {
    config
        .tool_namespace
        .as_ref()
        .map(|_| MULTI_AGENT_V2_NAMESPACE_DESCRIPTION.to_string())
}

fn send_message_tool_spec(multi_agent_config: &MultiAgentToolSpecConfig) -> ToolSpec {
    ToolSpec {
        name: "send_message".to_string(),
        namespace: multi_agent_config.tool_namespace.clone(),
        namespace_description: multi_agent_namespace_description(multi_agent_config),
        description: "Send a message to an existing agent. The message will be delivered promptly. Does not trigger a new turn."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Relative or canonical task name to message (from spawn_agent)."
                },
                "message": {
                    "type": "string",
                    "description": "Message text to queue on the target agent."
                }
            },
            "required": ["target", "message"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn followup_task_tool_spec(multi_agent_config: &MultiAgentToolSpecConfig) -> ToolSpec {
    ToolSpec {
        name: "followup_task".to_string(),
        namespace: multi_agent_config.tool_namespace.clone(),
        namespace_description: multi_agent_namespace_description(multi_agent_config),
        description: "Send a message to an existing non-root target agent and trigger a turn in that target. If the target is currently mid-turn, the message is queued and will be used to start the target's next turn, after the current turn completes."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Agent id or canonical task name to message (from spawn_agent)."
                },
                "message": {
                    "type": "string",
                    "description": "Message text to send to the target agent."
                }
            },
            "required": ["target", "message"],
            "additionalProperties": false
        }),
        output_schema: None,
        freeform: None,
    }
}

fn list_agents_tool_spec(multi_agent_config: &MultiAgentToolSpecConfig) -> ToolSpec {
    ToolSpec {
        name: "list_agents".to_string(),
        namespace: multi_agent_config.tool_namespace.clone(),
        namespace_description: multi_agent_namespace_description(multi_agent_config),
        description:
            "List live agents in the current root thread tree. Optionally filter by task-path prefix."
                .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path_prefix": {
                    "type": "string",
                    "description": "Optional task-path prefix (not ending with trailing slash). Accepts the same relative or absolute task-path syntax."
                }
            },
            "additionalProperties": false
        }),
        output_schema: Some(list_agents_output_schema()),
        freeform: None,
    }
}

fn close_agent_tool_spec(multi_agent_config: &MultiAgentToolSpecConfig) -> ToolSpec {
    ToolSpec {
        name: "close_agent".to_string(),
        namespace: multi_agent_config.tool_namespace.clone(),
        namespace_description: multi_agent_namespace_description(multi_agent_config),
        description: "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Don't keep agents open for too long if they are not needed anymore."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Agent id or canonical task name to close (from spawn_agent)."
                }
            },
            "required": ["target"],
            "additionalProperties": false
        }),
        output_schema: Some(close_agent_output_schema()),
        freeform: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_script_tool_description_preserves_raw_cdp_contract() {
        let description = browser_script_tool_spec().description;
        for expected in [
            "CDP is the source of truth",
            "browser interaction tool",
            "new_tab(url)",
            "coordinate clicks",
            "click_at_xy",
            "screenshot(label)",
            "The user does not see those pixels inline",
            "cdp(...)",
            "Do not import Playwright",
            "audit_artifact",
        ] {
            assert!(
                description.contains(expected),
                "missing {expected:?} from browser_script tool description:\n{description}"
            );
        }
    }

    #[test]
    fn browser_tool_description_preserves_llm_control_plane_contract() {
        let description = browser_tool_spec().description;
        for expected in [
            "browser control plane",
            "The input is a single CLI-like command string",
            "Remote start means start and connect",
            "Nothing reloads, relaunches, closes, or switches tabs silently",
            "browser connect local",
            "browser connect managed",
            "browser remote start",
            "browser doctor --json",
            "browser recover reconnect-websocket",
            "browser runtime ownership --json",
            "External user Chrome is never killed or relaunched",
        ] {
            assert!(
                description.contains(expected),
                "missing {expected:?} from browser tool description:\n{description}"
            );
        }
    }

    #[test]
    fn view_image_tool_description_marks_sequential_contract() {
        let description = view_image_tool_spec(true).description;
        assert!(description.to_ascii_lowercase().contains("sequential"));
        assert!(description.contains("not parallel-safe"));
        assert!(description.contains("must not be called in parallel"));
    }

    #[test]
    fn view_image_detail_schema_is_model_capability_gated_like_codex() {
        let without_original = view_image_tool_spec(false);
        assert!(without_original.input_schema["properties"]
            .get("detail")
            .is_none());

        let with_original = view_image_tool_spec(true);
        assert_eq!(
            with_original.input_schema["properties"]["detail"]["enum"],
            serde_json::json!(["high", "original"])
        );
        assert_eq!(
            with_original.output_schema.as_ref().unwrap()["required"],
            serde_json::json!(["image_url", "detail"])
        );
    }

    #[test]
    fn v1_structured_input_schema_exposes_image_detail_like_codex() {
        let schema = collab_input_items_schema();

        assert_eq!(
            schema["items"]["properties"]["detail"]["enum"],
            serde_json::json!(["auto", "low", "high"])
        );
    }

    #[test]
    fn spawn_agent_tool_description_matches_codex_delegation_gate() {
        let description = default_spawn_agent_type_description();
        let model_description = browser_use_providers::spawn_agent_model_overrides_description();
        let spec = spawn_agent_tool_spec(
            &description,
            &MultiAgentToolSpecConfig::default(),
            &model_description,
        );
        assert!(spec.description.contains(
            "Available model overrides (optional; inherited parent model is preferred):"
        ));
        assert!(spec.description.contains(
            "- `gpt-5.5`: Frontier model for complex coding, research, and real-world work. Reasoning efforts: low, medium (default), high, xhigh. Service tiers: priority."
        ));
        assert!(spec.description.contains(
            "- `gpt-5.4-mini`: Small, fast, and cost-efficient model for simpler coding tasks. Reasoning efforts: low, medium (default), high, xhigh."
        ));
        assert!(!spec.description.contains("`codex-auto-review`"));
        assert!(spec
            .description
            .contains("Only use `spawn_agent` if and only if the user explicitly asks"));
        assert!(spec
            .description
            .contains("detailed codebase analysis do not count as permission to spawn"));
        assert!(spec
            .description
            .contains("Do not delegate urgent blocking work"));
        assert!(spec
            .description
            .contains("max_concurrent_threads_per_session = 4"));
        assert!(!spec
            .description
            .contains("spawn a read-only helper with role \"explorer\" before answering"));
        assert_eq!(
            spec.input_schema["required"],
            serde_json::json!(["task_name", "message"])
        );
        assert!(spec.input_schema["properties"].get("fork_mode").is_none());
        assert!(spec.input_schema["properties"].get("nickname").is_none());
        assert!(spec.input_schema["properties"]["fork_turns"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Defaults to `all`"));
        assert!(spec.input_schema["properties"]["model"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Optional model override"));
        assert!(
            spec.input_schema["properties"]["reasoning_effort"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Optional reasoning effort override")
        );
        assert!(
            spec.input_schema["properties"]["service_tier"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Optional service tier override")
        );
        assert!(spec.input_schema["properties"]["agent_type"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Available roles:"));
        assert!(spec.input_schema["properties"]["agent_type"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("worker: {"));
    }

    #[test]
    fn spawn_agent_tool_hides_metadata_fields_like_codex_when_configured() {
        let description = default_spawn_agent_type_description();
        let spec = spawn_agent_tool_spec(
            &description,
            &MultiAgentToolSpecConfig {
                hide_spawn_agent_metadata: true,
                ..MultiAgentToolSpecConfig::default()
            },
            "",
        );
        let properties = spec.input_schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("message"));
        assert!(properties.contains_key("task_name"));
        assert!(properties.contains_key("fork_turns"));
        assert!(!spec.description.contains("Available model overrides"));
        assert!(!properties.contains_key("agent_type"));
        assert!(!properties.contains_key("model"));
        assert!(!properties.contains_key("reasoning_effort"));
        assert!(!properties.contains_key("service_tier"));
    }

    #[test]
    fn multi_agent_v2_management_tool_specs_match_codex_shape() {
        let registry = ToolRegistry::browser_agent();
        let specs = registry.specs();
        assert!(!specs.iter().any(|spec| spec.name == "send_input"));

        let wait = specs
            .iter()
            .find(|spec| spec.name == "wait_agent")
            .expect("wait_agent spec");
        assert!(wait.description.contains("Does not return the content"));
        let wait_properties = wait.input_schema["properties"].as_object().unwrap();
        assert!(wait_properties.contains_key("timeout_ms"));
        assert!(!wait_properties.contains_key("target"));
        assert!(!wait_properties.contains_key("targets"));
        assert_eq!(wait.input_schema["required"], serde_json::json!([]));
        assert_eq!(
            wait_properties["timeout_ms"]["description"],
            "Optional timeout in milliseconds. Defaults to 30000, min 10000, max 3600000."
        );

        let send = specs
            .iter()
            .find(|spec| spec.name == "send_message")
            .expect("send_message spec");
        assert!(send.description.contains("Does not trigger a new turn"));
        let send_properties = send.input_schema["properties"].as_object().unwrap();
        assert!(send_properties.contains_key("target"));
        assert!(send_properties.contains_key("message"));
        assert!(!send_properties.contains_key("items"));
        assert!(!send_properties.contains_key("interrupt"));

        let followup = specs
            .iter()
            .find(|spec| spec.name == "followup_task")
            .expect("followup_task spec");
        assert!(followup
            .description
            .contains("existing non-root target agent"));
        let followup_properties = followup.input_schema["properties"].as_object().unwrap();
        assert!(followup_properties.contains_key("target"));
        assert!(followup_properties.contains_key("message"));
        assert!(!followup_properties.contains_key("items"));

        let list = specs
            .iter()
            .find(|spec| spec.name == "list_agents")
            .expect("list_agents spec");
        assert!(list.description.contains("current root thread tree"));
        assert_eq!(
            list.input_schema["properties"]["path_prefix"]["description"],
            "Optional task-path prefix (not ending with trailing slash). Accepts the same relative or absolute task-path syntax."
        );

        let close = specs
            .iter()
            .find(|spec| spec.name == "close_agent")
            .expect("close_agent spec");
        assert!(close
            .description
            .contains("return the target agent's previous status"));
        let close_properties = close.input_schema["properties"].as_object().unwrap();
        assert!(close_properties.contains_key("target"));
        assert!(!close_properties.contains_key("reason"));
    }

    #[test]
    fn v1_multi_agent_tools_defer_behind_tool_search_when_supported_like_codex() {
        let registry =
            ToolRegistry::browser_agent_with_agent_type_description_and_model_description_and_multi_agent_config(
                default_spawn_agent_type_description(),
                MultiAgentToolSpecConfig {
                    family: MultiAgentToolFamily::V1,
                    ..MultiAgentToolSpecConfig::default()
                },
                ShellToolSpecConfig::default(),
                false,
                browser_use_providers::spawn_agent_model_overrides_description(),
            );

        let direct_specs = registry.specs_for_model(true, true);
        assert!(direct_specs.iter().any(|spec| spec.name == "tool_search"));
        assert!(!direct_specs.iter().any(|spec| {
            spec.namespace.as_deref() == Some("multi_agent_v1") && spec.name == "spawn_agent"
        }));

        let fallback_specs = registry.specs_for_model(false, true);
        assert!(!fallback_specs.iter().any(|spec| spec.name == "tool_search"));
        assert!(fallback_specs.iter().any(|spec| {
            spec.namespace.as_deref() == Some("multi_agent_v1") && spec.name == "spawn_agent"
        }));

        let loaded = registry.search_deferred_tools("spawn subagent", 8);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["type"], "namespace");
        assert_eq!(loaded[0]["name"], "multi_agent_v1");
        assert!(loaded[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| { tool["name"] == "spawn_agent" && tool["defer_loading"] == true }));
    }

    #[test]
    fn codex_tool_output_schemas_are_preserved_in_registry() {
        let registry = ToolRegistry::browser_agent_with_agent_type_description(
            default_spawn_agent_type_description(),
            false,
            true,
        );
        let specs = registry.specs();
        let exec = specs
            .iter()
            .find(|spec| spec.name == "exec_command")
            .unwrap();
        assert_eq!(
            exec.output_schema.as_ref().unwrap()["required"],
            serde_json::json!(["wall_time_seconds", "output"])
        );
        let write = specs
            .iter()
            .find(|spec| spec.name == "write_stdin")
            .unwrap();
        assert_eq!(write.output_schema, exec.output_schema);
        let spawn = specs
            .iter()
            .find(|spec| spec.name == "spawn_agent")
            .unwrap();
        assert_eq!(
            spawn.output_schema.as_ref().unwrap()["required"],
            serde_json::json!(["task_name", "nickname"])
        );
        let wait = specs.iter().find(|spec| spec.name == "wait_agent").unwrap();
        assert_eq!(
            wait.output_schema.as_ref().unwrap()["required"],
            serde_json::json!(["message", "timed_out"])
        );
        let list = specs
            .iter()
            .find(|spec| spec.name == "list_agents")
            .unwrap();
        assert_eq!(
            list.output_schema.as_ref().unwrap()["required"],
            serde_json::json!(["agents"])
        );
        let close = specs
            .iter()
            .find(|spec| spec.name == "close_agent")
            .unwrap();
        assert_eq!(
            close.output_schema.as_ref().unwrap()["required"],
            serde_json::json!(["previous_status"])
        );
        let send = specs
            .iter()
            .find(|spec| spec.name == "send_message")
            .unwrap();
        assert!(send.output_schema.is_none());
        let followup = specs
            .iter()
            .find(|spec| spec.name == "followup_task")
            .unwrap();
        assert!(followup.output_schema.is_none());
    }

    #[test]
    fn spawn_agent_role_description_notes_locked_settings_like_codex() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let both = temp.path().join("both.toml");
        let model = temp.path().join("model.toml");
        let reasoning = temp.path().join("reasoning.toml");
        let tier = temp.path().join("tier.toml");
        std::fs::write(
            &both,
            "model = \"gpt-5.4\"\nmodel_reasoning_effort = \"high\"\nservice_tier = \"priority\"\n",
        )?;
        std::fs::write(&model, "model = \"gpt-5.4\"\n")?;
        std::fs::write(&reasoning, "model_reasoning_effort = \"low\"\n")?;
        std::fs::write(&tier, "service_tier = \"priority\"\n")?;

        let description = spawn_agent_type_description_for_roles([
            SpawnAgentRoleDescription {
                name: "both".to_string(),
                description: Some("Both role.".to_string()),
                config_file: Some(both),
            },
            SpawnAgentRoleDescription {
                name: "model_only".to_string(),
                description: Some("Model role.".to_string()),
                config_file: Some(model),
            },
            SpawnAgentRoleDescription {
                name: "reasoning_only".to_string(),
                description: Some("Reasoning role.".to_string()),
                config_file: Some(reasoning),
            },
            SpawnAgentRoleDescription {
                name: "tier_only".to_string(),
                description: Some("Tier role.".to_string()),
                config_file: Some(tier),
            },
        ]);

        assert!(description.contains(
            "both: {\nBoth role.\n- This role's model is set to `gpt-5.4` and its reasoning effort is set to `high`. These settings cannot be changed.\n- This role's service tier is set to `priority`. If it is supported by the resolved model, it takes precedence over a valid spawn request service tier.\n}"
        ));
        assert!(description.contains(
            "model_only: {\nModel role.\n- This role's model is set to `gpt-5.4` and cannot be changed.\n}"
        ));
        assert!(description.contains(
            "reasoning_only: {\nReasoning role.\n- This role's reasoning effort is set to `low` and cannot be changed.\n}"
        ));
        assert!(description.contains(
            "tier_only: {\nTier role.\n- This role's service tier is set to `priority`. If it is supported by the resolved model, it takes precedence over a valid spawn request service tier.\n}"
        ));
        Ok(())
    }

    #[test]
    fn spawn_agent_role_description_ignores_unreadable_or_bad_lock_files_like_codex(
    ) -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let bad = temp.path().join("bad.toml");
        std::fs::write(&bad, "model = ")?;
        let missing = temp.path().join("missing.toml");

        let description = spawn_agent_type_description_for_roles([
            SpawnAgentRoleDescription {
                name: "bad".to_string(),
                description: Some("Bad role.".to_string()),
                config_file: Some(bad),
            },
            SpawnAgentRoleDescription {
                name: "missing".to_string(),
                description: Some("Missing role.".to_string()),
                config_file: Some(missing),
            },
        ]);

        assert!(description.contains("bad: {\nBad role.\n}"));
        assert!(description.contains("missing: {\nMissing role.\n}"));
        assert!(!description.contains("cannot be changed"));
        assert!(!description.contains("service tier is set"));
        Ok(())
    }

    #[test]
    fn exec_command_tool_spec_exposes_codex_approval_metadata() {
        let spec = exec_command_tool_spec(true);
        assert_eq!(
            spec.description,
            "Runs a command in a PTY, returning output or a session ID for ongoing interaction."
        );
        let properties = &spec.input_schema["properties"];
        assert_eq!(
            properties["workdir"]["description"],
            "Optional working directory to run the command in; defaults to the turn cwd."
        );
        assert_eq!(
            properties["tty"]["description"],
            "Whether to allocate a TTY for the command. Defaults to false (plain pipes); set to true to open a PTY and access TTY process."
        );
        assert_eq!(
            properties["sandbox_permissions"]["description"],
            "Sandbox permissions for the command. Set to \"require_escalated\" to request running without sandbox restrictions; defaults to \"use_default\"."
        );
        assert!(properties["justification"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Only set if sandbox_permissions is"));
        assert!(properties["prefix_rule"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("Only specify when sandbox_permissions"));
        assert_eq!(properties["prefix_rule"]["items"]["type"], "string");
        assert!(properties.get("login").is_some());
        assert!(properties.get("additional_permissions").is_none());
    }

    #[test]
    fn shell_tool_specs_hide_login_when_login_shell_disabled_like_codex() {
        let exec = exec_command_tool_spec(false);
        assert!(exec.input_schema["properties"].get("login").is_none());

        let shell = shell_command_tool_spec(false);
        assert!(shell.input_schema["properties"].get("login").is_none());
    }

    #[test]
    fn write_stdin_tool_spec_uses_codex_unified_exec_wording() {
        let spec = write_stdin_tool_spec();
        assert_eq!(
            spec.description,
            "Writes characters to an existing unified exec session and returns recent output."
        );
        let properties = &spec.input_schema["properties"];
        assert_eq!(properties["session_id"]["type"], "number");
        assert_eq!(
            properties["session_id"]["description"],
            "Identifier of the running unified exec session."
        );
        assert_eq!(
            properties["chars"]["description"],
            "Bytes to write to stdin (may be empty to poll)."
        );
        assert_eq!(
            properties["yield_time_ms"]["description"],
            "How long to wait (in milliseconds) for output before yielding."
        );
    }

    #[test]
    fn apply_patch_tool_spec_matches_codex_freeform_shape() {
        let spec = apply_patch_tool_spec();
        assert_eq!(
            spec.description,
            "Use the `apply_patch` tool to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON."
        );
        let freeform = spec.freeform.as_ref().expect("freeform format");
        assert_eq!(freeform.kind, "grammar");
        assert_eq!(freeform.syntax, "lark");
        assert!(freeform
            .definition
            .contains("start: begin_patch hunk+ end_patch"));
        assert!(freeform
            .definition
            .contains("eof_line: \"*** End of File\" LF"));
        assert_eq!(spec.input_schema["properties"]["patch"]["type"], "string");
    }

    #[test]
    fn browser_registry_exposes_browser_interfaces_not_legacy_python() {
        let names = ToolRegistry::browser_agent()
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"browser".to_string()));
        assert!(names.contains(&"browser_script".to_string()));
        assert!(!names.contains(&"python".to_string()));
        assert!(!names.contains(&"read_file".to_string()));
        assert!(!names.contains(&"search_files".to_string()));
        assert!(!names.contains(&"list_files".to_string()));
    }
}
