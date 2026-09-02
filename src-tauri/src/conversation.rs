use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Emitter, Manager};
use tokio::sync::watch;

use crate::{
    changes,
    model_hub::{self, LocalChatErrorKind, LocalChatMessage, ModelSelection},
    models::Workspace,
    project_context::{self, ContextHistoryInput, ContextSource, ProjectContextRequest},
};

const CONVERSATIONS_FILE: &str = "home-conversations.json";
pub(crate) const GENERAL_WORKSPACE_ID: &str = "__general__";
const GENERAL_WORKSPACE_NAME: &str = "General";
const STREAM_EVENT: &str = "repotunnel://home-chat-stream";
const MAX_CONVERSATIONS: usize = 40;
const MAX_MESSAGES_PER_CONVERSATION: usize = 80;
const MAX_MESSAGE_CHARS: usize = 160_000;
const MAX_QUESTION_CHARS: usize = 12_000;
const MAX_TITLE_CHARS: usize = 64;
const MAX_HOME_CHANGE_ACTIONS: usize = 8;
const HOME_CHANGE_FENCE: &str = "```repotunnel-changes";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HomeChangePlan {
    #[serde(default)]
    actions: Vec<HomeChangeAction>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum HomeChangeAction {
    CreateFile {
        path: String,
        content: String,
    },
    PatchFile {
        path: String,
        expected: String,
        replacement: String,
    },
}

static ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static ACTIVE_GENERATIONS: OnceLock<Mutex<HashMap<String, ActiveGeneration>>> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationMessage {
    pub(crate) id: String,
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) created_at: u64,
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) context_sources: Vec<ContextSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HomeConversation {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) selected_model: Option<ModelSelection>,
    pub(crate) title: String,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) messages: Vec<ConversationMessage>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationSummary {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) workspace_name: String,
    pub(crate) selected_model: Option<ModelSelection>,
    pub(crate) title: String,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) message_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HomeChatStartResult {
    pub(crate) generation_id: String,
    pub(crate) conversation: HomeConversation,
    pub(crate) context_sources: Vec<ContextSource>,
    pub(crate) context_warnings: Vec<String>,
    pub(crate) context_reduced: bool,
    pub(crate) context_budget_chars: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HomeChatStreamEvent {
    generation_id: String,
    conversation_id: String,
    kind: String,
    delta: Option<String>,
    message: Option<String>,
    context_sources: Vec<ContextSource>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct ConversationStore {
    conversations: Vec<HomeConversation>,
}

struct ActiveGeneration {
    conversation_id: String,
    cancel: watch::Sender<bool>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn new_id(prefix: &str) -> String {
    let sequence = ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:x}-{sequence:x}", now_millis())
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.to_string()
    } else {
        let mut output = value
            .chars()
            .take(limit.saturating_sub(1))
            .collect::<String>();
        output.push('…');
        output
    }
}

fn conversation_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(CONVERSATIONS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve Home conversation storage: {error}"))
}

fn load_store_path(path: &Path) -> Result<ConversationStore, String> {
    if !path.exists() {
        return Ok(ConversationStore::default());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("Could not read Home conversations: {error}"))?;
    if contents.trim().is_empty() {
        return Ok(ConversationStore::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| format!("Saved Home conversations are invalid: {error}"))
}

fn save_store_path(path: &Path, store: &ConversationStore) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve Home conversation storage directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;
    let contents = serde_json::to_string_pretty(store)
        .map_err(|error| format!("Could not serialize Home conversations: {error}"))?;
    fs::write(path, contents).map_err(|error| format!("Could not save Home conversations: {error}"))
}

fn with_store<T>(
    app: &AppHandle,
    mut operation: impl FnMut(&mut ConversationStore) -> Result<T, String>,
) -> Result<T, String> {
    let lock = STORE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|_| "Home conversation storage lock is unavailable.".to_string())?;
    let path = conversation_path(app)?;
    let mut store = load_store_path(&path)?;
    let result = operation(&mut store)?;
    store
        .conversations
        .sort_by_key(|conversation| std::cmp::Reverse(conversation.updated_at));
    store.conversations.truncate(MAX_CONVERSATIONS);
    for conversation in &mut store.conversations {
        if conversation.messages.len() > MAX_MESSAGES_PER_CONVERSATION {
            let keep_from = conversation.messages.len() - MAX_MESSAGES_PER_CONVERSATION;
            conversation.messages.drain(..keep_from);
        }
        for message in &mut conversation.messages {
            message.content = truncate_chars(&message.content, MAX_MESSAGE_CHARS);
        }
    }
    save_store_path(&path, &store)?;
    Ok(result)
}

fn title_from_question(question: &str) -> String {
    let first_line = question.lines().next().unwrap_or(question).trim();
    let title = truncate_chars(first_line, MAX_TITLE_CHARS);
    if title.is_empty() {
        "New conversation".to_string()
    } else {
        title
    }
}

fn summary(conversation: &HomeConversation) -> ConversationSummary {
    ConversationSummary {
        id: conversation.id.clone(),
        workspace_id: conversation.workspace_id.clone(),
        workspace_name: conversation.workspace_name.clone(),
        selected_model: conversation.selected_model.clone(),
        title: conversation.title.clone(),
        created_at: conversation.created_at,
        updated_at: conversation.updated_at,
        message_count: conversation.messages.len(),
    }
}

pub(crate) fn list(
    app: &AppHandle,
    workspace_id: Option<&str>,
) -> Result<Vec<ConversationSummary>, String> {
    with_store(app, |store| {
        Ok(store
            .conversations
            .iter()
            .filter(|conversation| {
                workspace_id.is_none_or(|workspace_id| conversation.workspace_id == workspace_id)
            })
            .map(summary)
            .collect())
    })
}

pub(crate) fn get(app: &AppHandle, conversation_id: &str) -> Result<HomeConversation, String> {
    with_store(app, |store| {
        store
            .conversations
            .iter()
            .find(|conversation| conversation.id == conversation_id)
            .cloned()
            .ok_or_else(|| "That Home conversation no longer exists.".to_string())
    })
}

pub(crate) fn delete(app: &AppHandle, conversation_id: &str) -> Result<(), String> {
    if let Some(registry) = ACTIVE_GENERATIONS.get() {
        let registry = registry
            .lock()
            .map_err(|_| "Home generation registry is unavailable.".to_string())?;
        if registry
            .values()
            .any(|generation| generation.conversation_id == conversation_id)
        {
            return Err("Stop the active response before deleting this conversation.".to_string());
        }
    }

    with_store(app, |store| {
        let original_len = store.conversations.len();
        store
            .conversations
            .retain(|conversation| conversation.id != conversation_id);
        if store.conversations.len() == original_len {
            return Err("That Home conversation no longer exists.".to_string());
        }
        Ok(())
    })
}

pub(crate) fn create(
    app: &AppHandle,
    workspace: Option<&Workspace>,
) -> Result<HomeConversation, String> {
    let selected_model = model_hub::selected_model(app)?;
    let timestamp = now_millis();
    let conversation = HomeConversation {
        id: new_id("conversation"),
        workspace_id: workspace
            .map(|workspace| workspace.id.clone())
            .unwrap_or_else(|| GENERAL_WORKSPACE_ID.to_string()),
        workspace_name: workspace
            .map(|workspace| workspace.name.clone())
            .unwrap_or_else(|| GENERAL_WORKSPACE_NAME.to_string()),
        selected_model,
        title: "New conversation".to_string(),
        created_at: timestamp,
        updated_at: timestamp,
        messages: Vec::new(),
    };
    with_store(app, |store| {
        store.conversations.push(conversation.clone());
        Ok(conversation.clone())
    })
}

fn update_conversation(
    app: &AppHandle,
    conversation_id: &str,
    operation: impl FnOnce(&mut HomeConversation) -> Result<(), String>,
) -> Result<HomeConversation, String> {
    let mut operation = Some(operation);
    with_store(app, |store| {
        let conversation = store
            .conversations
            .iter_mut()
            .find(|conversation| conversation.id == conversation_id)
            .ok_or_else(|| "That Home conversation no longer exists.".to_string())?;
        operation
            .take()
            .ok_or_else(|| "Conversation update was already applied.".to_string())?(
            conversation
        )?;
        conversation.updated_at = now_millis();
        Ok(conversation.clone())
    })
}

fn history_inputs(conversation: &HomeConversation) -> Vec<ContextHistoryInput> {
    conversation
        .messages
        .iter()
        .filter(|message| {
            matches!(message.role.as_str(), "user" | "assistant")
                && matches!(message.state.as_str(), "complete" | "cancelled")
        })
        .map(|message| ContextHistoryInput {
            role: message.role.clone(),
            content: message.content.clone(),
        })
        .collect()
}

fn system_instruction(
    workspace_name: Option<&str>,
    project_context: &str,
    allow_changes: bool,
) -> String {
    let change_instruction = if allow_changes {
        "The user explicitly enabled Home Chat edits for this message. You may propose at most 8 file changes, but only with the strict change block described below. RepoTunnel—not you—validates and applies or queues every edit through the project's existing change policy, history, and undo system. Never claim an edit succeeded until RepoTunnel reports it. Supported actions are createFile and patchFile only. Use workspace-relative paths, never protected files, and at most one action per path. For patchFile, expected must be exact existing text from supplied context and specific enough to occur once. Put the block at the very end of the response:\n```repotunnel-changes\n{\"actions\":[{\"type\":\"patchFile\",\"path\":\"src/example.ts\",\"expected\":\"exact old text\",\"replacement\":\"new text\"}]}\n```\nIf context is insufficient for a safe exact edit, do not emit a change block; explain what context is needed."
    } else {
        "Edits are disabled for this message. Explain, diagnose, and propose changes in prose only. Do not emit a repotunnel-changes block and do not claim that files were changed."
    };
    match workspace_name {
        Some(workspace_name) => format!(
            "You are RepoTunnel Home AI assisting with the approved local project \"{workspace_name}\". \
Use the supplied project context and conversation for project-specific claims. Distinguish evidence from assumptions and say when context is insufficient. For project-explanation questions, summarize architecture, important entry points, data flow, and relevant files from the evidence instead of giving generic advice. \
You cannot run commands, launch processes, browse, write Git state, or start autonomous agents. {change_instruction} \
Never request, reveal, or infer protected secrets. Give concise engineering explanations, architecture reasoning, debugging guidance, and implementation plans.\n\nProject context supplied by RepoTunnel:{project_context}"
        ),
        None => "You are RepoTunnel Home AI in general-chat mode. No local project is selected. Answer general questions using the local model's knowledge and the conversation history. Do not invent claims about the user's projects or files. Home Chat is read-only and cannot edit files, run commands, launch processes, browse, write Git state, or start agents. Never claim that you performed those actions, and never request or reveal protected secrets.".to_string(),
    }
}

fn extract_home_change_plan(content: &str) -> Result<(String, Option<HomeChangePlan>), String> {
    let Some(start) = content.rfind(HOME_CHANGE_FENCE) else {
        return Ok((content.trim().to_string(), None));
    };
    let after_marker = start + HOME_CHANGE_FENCE.len();
    let remainder = &content[after_marker..];
    let Some(end_offset) = remainder.find("```") else {
        return Err(
            "Home Chat returned an incomplete RepoTunnel change block, so no edits were applied."
                .to_string(),
        );
    };
    if !remainder[end_offset + 3..].trim().is_empty() {
        return Err(
            "RepoTunnel change blocks must be the final content in a Home Chat response."
                .to_string(),
        );
    }
    let json = remainder[..end_offset].trim();
    let plan: HomeChangePlan = serde_json::from_str(json).map_err(|error| {
        format!("Home Chat returned an invalid RepoTunnel change block: {error}")
    })?;
    if plan.actions.is_empty() {
        return Err(
            "Home Chat returned an empty change plan, so no edits were applied.".to_string(),
        );
    }
    if plan.actions.len() > MAX_HOME_CHANGE_ACTIONS {
        return Err(format!(
            "Home Chat proposed too many edits. RepoTunnel allows at most {MAX_HOME_CHANGE_ACTIONS} per message."
        ));
    }
    Ok((content[..start].trim_end().to_string(), Some(plan)))
}

fn apply_home_change_plan(
    app: &AppHandle,
    workspace: &Workspace,
    generation_id: &str,
    plan: HomeChangePlan,
) -> Result<String, String> {
    let mut paths = HashSet::new();
    for action in &plan.actions {
        let path = match action {
            HomeChangeAction::CreateFile { path, .. }
            | HomeChangeAction::PatchFile { path, .. } => path,
        };
        if !paths.insert(path.clone()) {
            return Err("Home Chat proposed more than one edit for the same path, so the full change plan was refused before any edit was submitted.".to_string());
        }
    }

    let mut applied = 0usize;
    let mut queued = 0usize;
    let edit_group = format!("home-chat-{generation_id}");
    for action in plan.actions {
        let outcome = match action {
            HomeChangeAction::CreateFile { path, content } => {
                changes::create_file(app, workspace, path, content, Some(&edit_group))?
            }
            HomeChangeAction::PatchFile {
                path,
                expected,
                replacement,
            } => changes::patch_file(
                app,
                workspace,
                path,
                expected,
                replacement,
                Some(&edit_group),
            )?,
        };
        if outcome.applied {
            applied = applied.saturating_add(1);
        } else {
            queued = queued.saturating_add(1);
        }
    }
    let summary = match (applied, queued) {
        (0, queued) => format!("RepoTunnel queued {queued} change(s) for local review."),
        (applied, 0) => format!("RepoTunnel safely applied {applied} change(s). They are recorded in History and can be undone."),
        (applied, queued) => format!("RepoTunnel safely applied {applied} change(s) and queued {queued} change(s) for local review."),
    };
    Ok(summary)
}

fn finalize_home_assistant(
    app: &AppHandle,
    workspace: Option<&Workspace>,
    generation_id: &str,
    assistant: String,
    allow_changes: bool,
) -> String {
    let (mut visible, plan) = match extract_home_change_plan(&assistant) {
        Ok(result) => result,
        Err(error) => {
            return format!(
                "{}\n\n> RepoTunnel did not apply changes: {error}",
                assistant.trim()
            );
        }
    };
    if let Some(plan) = plan {
        let change_result = if !allow_changes {
            Err("Allow edits was not enabled for this message.".to_string())
        } else if let Some(workspace) = workspace {
            apply_home_change_plan(app, workspace, generation_id, plan)
        } else {
            Err("No approved project is selected.".to_string())
        };
        let note = match change_result {
            Ok(summary) => summary,
            Err(error) => format!("RepoTunnel did not apply all proposed changes: {error}"),
        };
        if !visible.is_empty() {
            visible.push_str("\n\n");
        }
        visible.push_str("> ");
        visible.push_str(&note);
    }
    visible
}

fn register_generation(
    generation_id: &str,
    conversation_id: &str,
) -> Result<watch::Receiver<bool>, String> {
    let registry = ACTIVE_GENERATIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .map_err(|_| "Home generation registry is unavailable.".to_string())?;
    if registry
        .values()
        .any(|generation| generation.conversation_id == conversation_id)
    {
        return Err("This conversation already has an active local model generation.".to_string());
    }
    let (cancel, receiver) = watch::channel(false);
    registry.insert(
        generation_id.to_string(),
        ActiveGeneration {
            conversation_id: conversation_id.to_string(),
            cancel,
        },
    );
    Ok(receiver)
}

fn finish_generation(generation_id: &str) {
    if let Some(registry) = ACTIVE_GENERATIONS.get() {
        if let Ok(mut registry) = registry.lock() {
            registry.remove(generation_id);
        }
    }
}

pub(crate) fn cancel(generation_id: &str) -> Result<bool, String> {
    let registry = ACTIVE_GENERATIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let registry = registry
        .lock()
        .map_err(|_| "Home generation registry is unavailable.".to_string())?;
    let Some(generation) = registry.get(generation_id) else {
        return Ok(false);
    };
    generation
        .cancel
        .send(true)
        .map_err(|_| "The local model generation already ended.".to_string())?;
    Ok(true)
}

fn emit_stream_event(app: &AppHandle, event: HomeChatStreamEvent) -> Result<(), String> {
    app.emit(STREAM_EVENT, event)
        .map_err(|error| format!("Could not deliver Home chat stream event: {error}"))
}

fn append_assistant_message(
    app: &AppHandle,
    conversation_id: &str,
    content: String,
    state: &str,
    sources: Vec<ContextSource>,
) -> Result<HomeConversation, String> {
    if content.trim().is_empty() {
        return get(app, conversation_id);
    }
    update_conversation(app, conversation_id, move |conversation| {
        conversation.messages.push(ConversationMessage {
            id: new_id("message"),
            role: "assistant".to_string(),
            content: truncate_chars(&content, MAX_MESSAGE_CHARS),
            created_at: now_millis(),
            state: state.to_string(),
            context_sources: sources.clone(),
        });
        Ok(())
    })
}

pub(crate) async fn begin(
    app: AppHandle,
    workspace: Option<Workspace>,
    conversation_id: String,
    question: String,
    mut context_request: ProjectContextRequest,
    allow_changes: bool,
) -> Result<HomeChatStartResult, String> {
    let question = question.trim().to_string();
    if question.is_empty() {
        return Err("Enter a question for the local model.".to_string());
    }
    if question.chars().count() > MAX_QUESTION_CHARS {
        return Err(format!(
            "Home questions are limited to {MAX_QUESTION_CHARS} characters to protect local model resources."
        ));
    }

    let selection = model_hub::selected_model(&app)?.ok_or_else(|| {
        "Start or select a local model before sending a Home AI question.".to_string()
    })?;
    let configured_endpoint = model_hub::configured_endpoint(&app, selection.provider)?;
    let selected_endpoint = model_hub::validate_loopback_endpoint(&selection.endpoint)?;
    if configured_endpoint != selected_endpoint {
        return Err(
            "The selected model no longer matches its local runtime. Wait for automatic model discovery and select it again."
                .to_string(),
        );
    }

    let existing = get(&app, &conversation_id)?;
    let expected_workspace_id = workspace
        .as_ref()
        .map(|workspace| workspace.id.as_str())
        .unwrap_or(GENERAL_WORKSPACE_ID);
    if existing.workspace_id != expected_workspace_id {
        return Err("That conversation belongs to a different Home chat scope.".to_string());
    }
    context_request.history = history_inputs(&existing);

    let built = if let Some(build_workspace) = workspace.clone() {
        let build_question = question.clone();
        tauri::async_runtime::spawn_blocking(move || {
            project_context::build(&build_workspace, &build_question, &context_request)
        })
        .await
        .map_err(|error| format!("Project context task could not complete: {error}"))??
    } else {
        project_context::build_general(&question, &context_request)
    };

    let generation_id = new_id("generation");
    let cancel_receiver = register_generation(&generation_id, &conversation_id)?;
    let selected_for_store = selection.clone();
    let question_for_store = question.clone();
    let conversation = match update_conversation(&app, &conversation_id, move |conversation| {
        conversation.selected_model = Some(selected_for_store.clone());
        if conversation.messages.is_empty() || conversation.title == "New conversation" {
            conversation.title = title_from_question(&question_for_store);
        }
        conversation.messages.push(ConversationMessage {
            id: new_id("message"),
            role: "user".to_string(),
            content: question_for_store.clone(),
            created_at: now_millis(),
            state: "complete".to_string(),
            context_sources: Vec::new(),
        });
        Ok(())
    }) {
        Ok(conversation) => conversation,
        Err(error) => {
            finish_generation(&generation_id);
            return Err(error);
        }
    };

    let mut messages = Vec::new();
    messages.push(LocalChatMessage {
        role: "system".to_string(),
        content: system_instruction(
            workspace.as_ref().map(|workspace| workspace.name.as_str()),
            &built.text,
            allow_changes && workspace.is_some(),
        ),
    });
    for message in &built.history {
        messages.push(LocalChatMessage {
            role: message.role.clone(),
            content: message.content.clone(),
        });
    }
    messages.push(LocalChatMessage {
        role: "user".to_string(),
        content: question,
    });

    let task_app = app.clone();
    let task_generation_id = generation_id.clone();
    let task_conversation_id = conversation_id.clone();
    let task_sources = built.sources.clone();
    let task_workspace = workspace.clone();
    let task_allow_changes = allow_changes;
    tauri::async_runtime::spawn(async move {
        let mut assistant = String::new();
        let event_app = task_app.clone();
        let event_generation = task_generation_id.clone();
        let event_conversation = task_conversation_id.clone();
        let stream_result = model_hub::stream_chat(selection, messages, cancel_receiver, |chunk| {
            assistant.push_str(chunk);
            if task_allow_changes {
                // Keep RepoTunnel's internal structured change plan out of the visible stream.
                // The final sanitized response is loaded after validation/application completes.
                Ok(())
            } else {
                emit_stream_event(
                    &event_app,
                    HomeChatStreamEvent {
                        generation_id: event_generation.clone(),
                        conversation_id: event_conversation.clone(),
                        kind: "chunk".to_string(),
                        delta: Some(chunk.to_string()),
                        message: None,
                        context_sources: Vec::new(),
                    },
                )
            }
        })
        .await;

        let final_event = match stream_result {
            Ok(()) if assistant.trim().is_empty() => HomeChatStreamEvent {
                generation_id: task_generation_id.clone(),
                conversation_id: task_conversation_id.clone(),
                kind: "failed".to_string(),
                delta: None,
                message: Some("The local model completed without returning text.".to_string()),
                context_sources: task_sources.clone(),
            },
            Ok(()) => {
                let assistant = finalize_home_assistant(
                    &task_app,
                    task_workspace.as_ref(),
                    &task_generation_id,
                    assistant,
                    task_allow_changes,
                );
                match append_assistant_message(
                    &task_app,
                    &task_conversation_id,
                    assistant,
                    "complete",
                    task_sources.clone(),
                ) {
                    Ok(_) => HomeChatStreamEvent {
                        generation_id: task_generation_id.clone(),
                        conversation_id: task_conversation_id.clone(),
                        kind: "complete".to_string(),
                        delta: None,
                        message: None,
                        context_sources: task_sources.clone(),
                    },
                    Err(error) => HomeChatStreamEvent {
                        generation_id: task_generation_id.clone(),
                        conversation_id: task_conversation_id.clone(),
                        kind: "failed".to_string(),
                        delta: None,
                        message: Some(format!(
                            "The response was generated but could not be saved locally: {error}"
                        )),
                        context_sources: task_sources.clone(),
                    },
                }
            }
            Err(error) if error.kind == LocalChatErrorKind::Cancelled => {
                let _ = append_assistant_message(
                    &task_app,
                    &task_conversation_id,
                    assistant,
                    "cancelled",
                    task_sources.clone(),
                );
                HomeChatStreamEvent {
                    generation_id: task_generation_id.clone(),
                    conversation_id: task_conversation_id.clone(),
                    kind: "cancelled".to_string(),
                    delta: None,
                    message: Some("Generation stopped.".to_string()),
                    context_sources: task_sources.clone(),
                }
            }
            Err(error) => {
                let _ = append_assistant_message(
                    &task_app,
                    &task_conversation_id,
                    assistant,
                    "failed",
                    task_sources.clone(),
                );
                let user_message = match error.kind {
                    LocalChatErrorKind::Timeout => "The model did not respond in time.".to_string(),
                    LocalChatErrorKind::Unreachable => error.message,
                    LocalChatErrorKind::ModelUnavailable => {
                        "The selected model is no longer available.".to_string()
                    }
                    LocalChatErrorKind::TooLarge => {
                        "The local model response exceeded RepoTunnel's safety limit.".to_string()
                    }
                    LocalChatErrorKind::Rejected | LocalChatErrorKind::InvalidStream => {
                        error.message
                    }
                    LocalChatErrorKind::Cancelled => "Generation stopped.".to_string(),
                };
                HomeChatStreamEvent {
                    generation_id: task_generation_id.clone(),
                    conversation_id: task_conversation_id.clone(),
                    kind: "failed".to_string(),
                    delta: None,
                    message: Some(user_message),
                    context_sources: task_sources.clone(),
                }
            }
        };
        let _ = emit_stream_event(&task_app, final_event);
        finish_generation(&task_generation_id);
    });

    Ok(HomeChatStartResult {
        generation_id,
        conversation,
        context_sources: built.sources,
        context_warnings: built.warnings,
        context_reduced: built.reduced,
        context_budget_chars: built.budget_chars,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        extract_home_change_plan, load_store_path, save_store_path, ConversationMessage,
        ConversationStore, HomeChangeAction, HomeConversation, MAX_HOME_CHANGE_ACTIONS,
    };

    fn temp_path() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("repotunnel-home-conversations-{nonce}.json"))
    }

    #[test]
    fn conversation_store_persists_and_switches_conversations() {
        let path = temp_path();
        let first = HomeConversation {
            id: "one".to_string(),
            workspace_id: "workspace".to_string(),
            workspace_name: "Fixture".to_string(),
            selected_model: None,
            title: "Direct HTTPS architecture".to_string(),
            created_at: 1,
            updated_at: 2,
            messages: vec![ConversationMessage {
                id: "message-one".to_string(),
                role: "user".to_string(),
                content: "Explain Direct HTTPS".to_string(),
                created_at: 1,
                state: "complete".to_string(),
                context_sources: Vec::new(),
            }],
        };
        let second = HomeConversation {
            id: "two".to_string(),
            workspace_id: "workspace".to_string(),
            workspace_name: "Fixture".to_string(),
            selected_model: None,
            title: "Rust lifetime issue".to_string(),
            created_at: 3,
            updated_at: 4,
            messages: Vec::new(),
        };
        save_store_path(
            &path,
            &ConversationStore {
                conversations: vec![first.clone(), second.clone()],
            },
        )
        .unwrap();
        let restored = load_store_path(&path).unwrap();
        assert_eq!(restored.conversations.len(), 2);
        assert_eq!(restored.conversations[0], first);
        assert_eq!(restored.conversations[1], second);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn home_change_plan_is_stripped_and_parsed() {
        let response = r#"I can make that focused change.

```repotunnel-changes
{"actions":[{"type":"patchFile","path":"src/example.ts","expected":"old value","replacement":"new value"}]}
```"#;
        let (visible, plan) = extract_home_change_plan(response).unwrap();
        assert_eq!(visible, "I can make that focused change.");
        let plan = plan.expect("change plan");
        assert_eq!(plan.actions.len(), 1);
        match &plan.actions[0] {
            HomeChangeAction::PatchFile {
                path,
                expected,
                replacement,
            } => {
                assert_eq!(path, "src/example.ts");
                assert_eq!(expected, "old value");
                assert_eq!(replacement, "new value");
            }
            _ => panic!("expected patch action"),
        }
    }

    #[test]
    fn home_change_plan_rejects_trailing_content() {
        let response = r#"Proposal
```repotunnel-changes
{"actions":[{"type":"createFile","path":"src/new.ts","content":"ok"}]}
```
extra"#;
        assert!(extract_home_change_plan(response)
            .unwrap_err()
            .contains("final content"));
    }

    #[test]
    fn home_change_plan_enforces_action_limit() {
        let actions = (0..=MAX_HOME_CHANGE_ACTIONS)
            .map(|index| {
                format!(r#"{{"type":"createFile","path":"src/file-{index}.txt","content":"x"}}"#)
            })
            .collect::<Vec<_>>()
            .join(",");
        let response = format!("Proposal\n```repotunnel-changes\n{{\"actions\":[{actions}]}}\n```");
        assert!(extract_home_change_plan(&response)
            .unwrap_err()
            .contains("too many edits"));
    }
}
