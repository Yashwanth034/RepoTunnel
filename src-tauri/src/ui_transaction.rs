use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::{
    latency::{LatencyPhase, LatencyTimings, LatencyTracker},
    semantic::{SemanticNode, SemanticSnapshot},
};

pub(crate) const MAX_UI_TRANSACTION_STEPS: usize = 64;
pub(crate) const MAX_UI_TRANSACTION_ALIASES: usize = 64;
pub(crate) const MAX_UI_TRANSACTION_TEXT_BYTES: usize = 128 * 1024;
const MAX_UI_TRANSACTION_TYPE_BYTES: usize = 32 * 1024;
pub(crate) const MAX_UI_TRANSACTION_WAIT_MS: u64 = 2_000;
pub(crate) const MAX_UI_TRANSACTION_TOTAL_WAIT_MS: u64 = 10_000;
pub(crate) const MIN_UI_TRANSACTION_CONDITION_TIMEOUT_MS: u64 = 50;
pub(crate) const MAX_UI_TRANSACTION_CONDITION_TIMEOUT_MS: u64 = 10_000;
pub(crate) const MAX_UI_TRANSACTION_TOTAL_CONDITION_WAIT_MS: u64 = 20_000;
pub(crate) const MAX_UI_TRANSACTION_RUNTIME_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum UiTransactionSurface {
    Desktop,
    AiWorkspace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UiTransactionQuery {
    pub(crate) query: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) action: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UiTransactionTarget {
    Alias(String),
    Query(UiTransactionQuery),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UiTransactionConditionKind {
    SemanticExists,
    SemanticMissing,
    SemanticEnabled,
    SemanticChecked,
    SemanticFocused,
    TextEquals,
    TextContains,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UiTransactionCondition {
    pub(crate) kind: UiTransactionConditionKind,
    pub(crate) target: UiTransactionTarget,
    pub(crate) expected_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UiTransactionStep {
    Find {
        query: UiTransactionQuery,
        as_name: String,
    },
    Click {
        target: UiTransactionTarget,
    },
    Type {
        target: UiTransactionTarget,
        text: String,
        clear_first: bool,
    },
    Wait {
        wait_ms: u64,
    },
    WaitUntil {
        condition: UiTransactionCondition,
        timeout_ms: u64,
    },
    Verify {
        condition: UiTransactionCondition,
    },
}

impl UiTransactionStep {
    pub(crate) fn operation_name(&self) -> &'static str {
        match self {
            Self::Find { .. } => "find",
            Self::Click { .. } => "click",
            Self::Type { .. } => "type",
            Self::Wait { .. } => "wait",
            Self::WaitUntil { .. } => "wait_until",
            Self::Verify { .. } => "verify",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UiTransactionPlan {
    pub(crate) surface: UiTransactionSurface,
    pub(crate) steps: Vec<UiTransactionStep>,
    pub(crate) return_semantic_snapshot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum UiTransactionErrorCode {
    InvalidPlan,
    TargetMissing,
    TargetAmbiguous,
    ActionUnsupported,
    SensitiveTarget,
    ConditionTimeout,
    VerificationFailed,
    MutationFailed,
    TransactionTimeout,
    FinalObservationFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiTransactionError {
    pub(crate) code: UiTransactionErrorCode,
    pub(crate) message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiTransactionStepReceipt {
    pub(crate) index: usize,
    pub(crate) operation: String,
    pub(crate) status: String,
    pub(crate) verified: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiTransactionResult {
    pub(crate) ok: bool,
    pub(crate) surface: UiTransactionSurface,
    pub(crate) total_steps: usize,
    pub(crate) completed_steps: usize,
    pub(crate) elapsed_ms: u64,
    pub(crate) timings: LatencyTimings,
    pub(crate) steps: Vec<UiTransactionStepReceipt>,
    pub(crate) failed_step: Option<usize>,
    pub(crate) failed_operation: Option<String>,
    pub(crate) error: Option<UiTransactionError>,
    pub(crate) final_snapshot: Option<SemanticSnapshot>,
    pub(crate) final_observation_error: Option<UiTransactionError>,
}

fn invalid_plan() -> UiTransactionError {
    UiTransactionError {
        code: UiTransactionErrorCode::InvalidPlan,
        message: "The UI transaction plan is invalid.".to_string(),
    }
}

fn query_is_meaningful(query: &UiTransactionQuery) -> bool {
    [
        query.query.as_deref(),
        query.role.as_deref(),
        query.name.as_deref(),
        query.state.as_deref(),
        query.action.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|value| !value.trim().is_empty())
}

fn alias_is_valid(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 64
        && alias
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

pub(crate) fn target_from_parts(
    alias: Option<String>,
    query: Option<UiTransactionQuery>,
) -> Result<UiTransactionTarget, UiTransactionError> {
    match (alias, query) {
        (Some(alias), None) => Ok(UiTransactionTarget::Alias(alias)),
        (None, Some(query)) => Ok(UiTransactionTarget::Query(query)),
        _ => Err(invalid_plan()),
    }
}

fn validate_target(
    target: &UiTransactionTarget,
    defined_aliases: &HashSet<String>,
) -> Result<(), UiTransactionError> {
    match target {
        UiTransactionTarget::Alias(alias) => {
            if !alias_is_valid(alias) || !defined_aliases.contains(alias) {
                return Err(invalid_plan());
            }
        }
        UiTransactionTarget::Query(query) => {
            if !query_is_meaningful(query) {
                return Err(invalid_plan());
            }
        }
    }
    Ok(())
}

fn validate_condition(
    condition: &UiTransactionCondition,
    defined_aliases: &HashSet<String>,
) -> Result<(), UiTransactionError> {
    validate_target(&condition.target, defined_aliases)?;
    let text_condition = matches!(
        condition.kind,
        UiTransactionConditionKind::TextEquals | UiTransactionConditionKind::TextContains
    );
    if text_condition != condition.expected_text.is_some() {
        return Err(invalid_plan());
    }
    Ok(())
}

pub(crate) fn validate_plan(
    plan: UiTransactionPlan,
) -> Result<UiTransactionPlan, UiTransactionError> {
    if plan.steps.is_empty() || plan.steps.len() > MAX_UI_TRANSACTION_STEPS {
        return Err(invalid_plan());
    }

    let mut defined_aliases = HashSet::new();
    let mut total_text_bytes = 0usize;
    let mut total_fixed_wait_ms = 0u64;
    let mut total_condition_wait_ms = 0u64;

    for step in &plan.steps {
        match step {
            UiTransactionStep::Find { query, as_name } => {
                if !query_is_meaningful(query)
                    || !alias_is_valid(as_name)
                    || defined_aliases.len() >= MAX_UI_TRANSACTION_ALIASES
                    || !defined_aliases.insert(as_name.clone())
                {
                    return Err(invalid_plan());
                }
            }
            UiTransactionStep::Click { target } => {
                validate_target(target, &defined_aliases)?;
            }
            UiTransactionStep::Type { target, text, .. } => {
                validate_target(target, &defined_aliases)?;
                if text.len() > MAX_UI_TRANSACTION_TYPE_BYTES || text.as_bytes().contains(&0) {
                    return Err(invalid_plan());
                }
                total_text_bytes = total_text_bytes
                    .checked_add(text.len())
                    .ok_or_else(invalid_plan)?;
                if total_text_bytes > MAX_UI_TRANSACTION_TEXT_BYTES {
                    return Err(invalid_plan());
                }
            }
            UiTransactionStep::Wait { wait_ms } => {
                if *wait_ms > MAX_UI_TRANSACTION_WAIT_MS {
                    return Err(invalid_plan());
                }
                total_fixed_wait_ms = total_fixed_wait_ms
                    .checked_add(*wait_ms)
                    .ok_or_else(invalid_plan)?;
                if total_fixed_wait_ms > MAX_UI_TRANSACTION_TOTAL_WAIT_MS {
                    return Err(invalid_plan());
                }
            }
            UiTransactionStep::WaitUntil {
                condition,
                timeout_ms,
            } => {
                validate_condition(condition, &defined_aliases)?;
                if !(MIN_UI_TRANSACTION_CONDITION_TIMEOUT_MS
                    ..=MAX_UI_TRANSACTION_CONDITION_TIMEOUT_MS)
                    .contains(timeout_ms)
                {
                    return Err(invalid_plan());
                }
                total_condition_wait_ms = total_condition_wait_ms
                    .checked_add(*timeout_ms)
                    .ok_or_else(invalid_plan)?;
                if total_condition_wait_ms > MAX_UI_TRANSACTION_TOTAL_CONDITION_WAIT_MS {
                    return Err(invalid_plan());
                }
            }
            UiTransactionStep::Verify { condition } => {
                validate_condition(condition, &defined_aliases)?;
            }
        }
    }

    Ok(plan)
}

#[cfg(test)]
fn history_summary(plan: &UiTransactionPlan) -> String {
    let surface = match plan.surface {
        UiTransactionSurface::Desktop => "desktop",
        UiTransactionSurface::AiWorkspace => "ai-workspace",
    };
    let operations = plan
        .steps
        .iter()
        .map(UiTransactionStep::operation_name)
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{surface} semantic transaction: {} step(s) [{operations}]",
        plan.steps.len()
    )
}

fn contains_typed_value(value: &str, typed_values: &[String]) -> bool {
    typed_values
        .iter()
        .any(|typed| !typed.is_empty() && value.contains(typed))
}

fn redact_typed_values_from_snapshot(snapshot: &mut SemanticSnapshot, typed_values: &[String]) {
    if typed_values.is_empty() {
        return;
    }
    for node in &mut snapshot.nodes {
        if contains_typed_value(&node.name, typed_values) {
            node.name.clear();
        }
        if contains_typed_value(&node.description, typed_values) {
            node.description.clear();
        }
        if node
            .text
            .as_deref()
            .is_some_and(|value| contains_typed_value(value, typed_values))
        {
            node.text = None;
        }
        if node
            .value
            .as_deref()
            .is_some_and(|value| contains_typed_value(value, typed_values))
        {
            node.value = None;
        }
    }
}

pub(crate) trait UiTransactionAdapter {
    fn observe(&mut self) -> Result<SemanticSnapshot, String>;
    fn find(
        &mut self,
        snapshot: &SemanticSnapshot,
        query: &UiTransactionQuery,
    ) -> Result<Vec<SemanticNode>, String>;
    fn click(&mut self, snapshot_id: &str, ref_id: &str) -> Result<(), String>;
    fn type_text(
        &mut self,
        snapshot_id: &str,
        ref_id: &str,
        text: &str,
        clear_first: bool,
    ) -> Result<(), String>;
}

struct TimedAdapter<'a, A: UiTransactionAdapter> {
    inner: &'a mut A,
    tracker: Rc<RefCell<LatencyTracker>>,
}

impl<A: UiTransactionAdapter> TimedAdapter<'_, A> {
    fn observe_final(&mut self) -> Result<SemanticSnapshot, String> {
        let started = Instant::now();
        let result = self.inner.observe();
        self.tracker
            .borrow_mut()
            .record(LatencyPhase::FinalObservation, started.elapsed());
        result
    }
}

impl<A: UiTransactionAdapter> UiTransactionAdapter for TimedAdapter<'_, A> {
    fn observe(&mut self) -> Result<SemanticSnapshot, String> {
        let started = Instant::now();
        let result = self.inner.observe();
        self.tracker
            .borrow_mut()
            .record(LatencyPhase::Observation, started.elapsed());
        result
    }

    fn find(
        &mut self,
        snapshot: &SemanticSnapshot,
        query: &UiTransactionQuery,
    ) -> Result<Vec<SemanticNode>, String> {
        let started = Instant::now();
        let result = self.inner.find(snapshot, query);
        self.tracker
            .borrow_mut()
            .record(LatencyPhase::Observation, started.elapsed());
        result
    }

    fn click(&mut self, snapshot_id: &str, ref_id: &str) -> Result<(), String> {
        let started = Instant::now();
        let result = self.inner.click(snapshot_id, ref_id);
        self.tracker
            .borrow_mut()
            .record(LatencyPhase::Mutation, started.elapsed());
        result
    }

    fn type_text(
        &mut self,
        snapshot_id: &str,
        ref_id: &str,
        text: &str,
        clear_first: bool,
    ) -> Result<(), String> {
        let started = Instant::now();
        let result = self.inner.type_text(snapshot_id, ref_id, text, clear_first);
        self.tracker
            .borrow_mut()
            .record(LatencyPhase::Mutation, started.elapsed());
        result
    }
}

fn safe_error(code: UiTransactionErrorCode) -> UiTransactionError {
    let message = match code {
        UiTransactionErrorCode::InvalidPlan => "The UI transaction plan is invalid.",
        UiTransactionErrorCode::TargetMissing => "The UI target could not be resolved uniquely.",
        UiTransactionErrorCode::TargetAmbiguous => "The UI target matched more than one element.",
        UiTransactionErrorCode::ActionUnsupported => {
            "The UI target does not support the requested action."
        }
        UiTransactionErrorCode::SensitiveTarget => {
            "The UI target is sensitive and cannot be typed into."
        }
        UiTransactionErrorCode::ConditionTimeout => {
            "The UI condition did not become true before its timeout."
        }
        UiTransactionErrorCode::VerificationFailed => {
            "The UI verification condition was not satisfied."
        }
        UiTransactionErrorCode::MutationFailed => "The UI mutation failed.",
        UiTransactionErrorCode::TransactionTimeout => {
            "The UI transaction exceeded its runtime budget."
        }
        UiTransactionErrorCode::FinalObservationFailed => {
            "The UI transaction completed, but the final observation could not be produced."
        }
    };
    UiTransactionError {
        code,
        message: message.to_string(),
    }
}

fn target_query<'a>(
    aliases: &'a HashMap<String, UiTransactionQuery>,
    target: &'a UiTransactionTarget,
) -> Result<&'a UiTransactionQuery, UiTransactionError> {
    match target {
        UiTransactionTarget::Alias(alias) => aliases
            .get(alias)
            .ok_or_else(|| safe_error(UiTransactionErrorCode::InvalidPlan)),
        UiTransactionTarget::Query(query) => Ok(query),
    }
}

fn observe_matches<A: UiTransactionAdapter>(
    adapter: &mut A,
    aliases: &HashMap<String, UiTransactionQuery>,
    target: &UiTransactionTarget,
) -> Result<(SemanticSnapshot, Vec<SemanticNode>), UiTransactionError> {
    let query = target_query(aliases, target)?;
    let snapshot = adapter
        .observe()
        .map_err(|_| safe_error(UiTransactionErrorCode::TargetMissing))?;
    let matches = adapter
        .find(&snapshot, query)
        .map_err(|_| safe_error(UiTransactionErrorCode::TargetMissing))?;
    Ok((snapshot, matches))
}

fn resolve_unique<A: UiTransactionAdapter>(
    adapter: &mut A,
    aliases: &HashMap<String, UiTransactionQuery>,
    target: &UiTransactionTarget,
) -> Result<(SemanticSnapshot, SemanticNode), UiTransactionError> {
    let (snapshot, mut matches) = observe_matches(adapter, aliases, target)?;
    match matches.len() {
        0 => Err(safe_error(UiTransactionErrorCode::TargetMissing)),
        1 => Ok((snapshot, matches.remove(0))),
        _ => Err(safe_error(UiTransactionErrorCode::TargetAmbiguous)),
    }
}

fn node_has_action(node: &SemanticNode, action: &str) -> bool {
    node.actions
        .iter()
        .any(|value| value.eq_ignore_ascii_case(action))
}

fn node_has_state(node: &SemanticNode, state: &str) -> bool {
    node.states
        .iter()
        .any(|value| value.eq_ignore_ascii_case(state))
}

fn evaluate_condition<A: UiTransactionAdapter>(
    adapter: &mut A,
    aliases: &HashMap<String, UiTransactionQuery>,
    condition: &UiTransactionCondition,
) -> Result<bool, UiTransactionError> {
    let (snapshot, matches) = observe_matches(adapter, aliases, &condition.target)?;
    if matches.len() > 1 {
        return Err(safe_error(UiTransactionErrorCode::TargetAmbiguous));
    }

    match condition.kind {
        UiTransactionConditionKind::SemanticExists => Ok(matches.len() == 1),
        UiTransactionConditionKind::SemanticMissing => {
            Ok(matches.is_empty() && !snapshot.truncated)
        }
        UiTransactionConditionKind::SemanticEnabled => {
            let node = matches
                .first()
                .ok_or_else(|| safe_error(UiTransactionErrorCode::TargetMissing))?;
            Ok(node_has_state(node, "enabled"))
        }
        UiTransactionConditionKind::SemanticChecked => {
            let node = matches
                .first()
                .ok_or_else(|| safe_error(UiTransactionErrorCode::TargetMissing))?;
            Ok(node_has_state(node, "checked"))
        }
        UiTransactionConditionKind::SemanticFocused => {
            let node = matches
                .first()
                .ok_or_else(|| safe_error(UiTransactionErrorCode::TargetMissing))?;
            Ok(node_has_state(node, "focused"))
        }
        UiTransactionConditionKind::TextEquals | UiTransactionConditionKind::TextContains => {
            let node = matches
                .first()
                .ok_or_else(|| safe_error(UiTransactionErrorCode::TargetMissing))?;
            if node.sensitive {
                return Err(safe_error(UiTransactionErrorCode::SensitiveTarget));
            }
            let expected = condition.expected_text.as_deref().unwrap_or_default();
            let values = [node.value.as_deref(), node.text.as_deref()]
                .into_iter()
                .flatten();
            Ok(match condition.kind {
                UiTransactionConditionKind::TextEquals => {
                    values.into_iter().any(|value| value == expected)
                }
                UiTransactionConditionKind::TextContains => {
                    values.into_iter().any(|value| value.contains(expected))
                }
                _ => unreachable!(),
            })
        }
    }
}

fn step_receipt(
    index: usize,
    operation: &str,
    status: &str,
    verified: Option<bool>,
) -> UiTransactionStepReceipt {
    UiTransactionStepReceipt {
        index,
        operation: operation.to_string(),
        status: status.to_string(),
        verified,
    }
}

struct FailedResultContext<'a> {
    surface: UiTransactionSurface,
    total_steps: usize,
    completed_steps: usize,
    receipts: Vec<UiTransactionStepReceipt>,
    failed_step: Option<usize>,
    failed_operation: Option<&'a str>,
}

fn failed_result(
    mut context: FailedResultContext<'_>,
    error: UiTransactionError,
    verified: Option<bool>,
    timings: LatencyTimings,
    started: Option<Instant>,
) -> UiTransactionResult {
    if let (Some(index), Some(operation)) = (context.failed_step, context.failed_operation) {
        context
            .receipts
            .push(step_receipt(index, operation, "failed", verified));
    }
    UiTransactionResult {
        ok: false,
        surface: context.surface,
        total_steps: context.total_steps,
        completed_steps: context.completed_steps,
        elapsed_ms: started
            .map(|instant| instant.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0),
        timings,
        steps: context.receipts,
        failed_step: context.failed_step,
        failed_operation: context.failed_operation.map(str::to_string),
        error: Some(error),
        final_snapshot: None,
        final_observation_error: None,
    }
}

pub(crate) fn execute_transaction<A: UiTransactionAdapter>(
    adapter: &mut A,
    plan: UiTransactionPlan,
) -> UiTransactionResult {
    execute_transaction_with_runtime_budget(
        adapter,
        plan,
        Duration::from_millis(MAX_UI_TRANSACTION_RUNTIME_MS),
    )
}

fn execute_transaction_with_runtime_budget<A: UiTransactionAdapter>(
    adapter: &mut A,
    plan: UiTransactionPlan,
    runtime_budget: Duration,
) -> UiTransactionResult {
    let surface = plan.surface;
    let total_steps = plan.steps.len();
    let tracker = Rc::new(RefCell::new(LatencyTracker::new()));
    let preflight_started = Instant::now();
    let plan = match validate_plan(plan) {
        Ok(plan) => plan,
        Err(error) => {
            tracker
                .borrow_mut()
                .record(LatencyPhase::Preflight, preflight_started.elapsed());
            return failed_result(
                FailedResultContext {
                    surface,
                    total_steps,
                    completed_steps: 0,
                    receipts: Vec::new(),
                    failed_step: None,
                    failed_operation: None,
                },
                error,
                None,
                tracker.borrow().snapshot(),
                None,
            );
        }
    };

    let typed_values_to_redact = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            UiTransactionStep::Type { text, .. } if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    tracker
        .borrow_mut()
        .record(LatencyPhase::Preflight, preflight_started.elapsed());
    let started = Instant::now();
    let mut timed_adapter = TimedAdapter {
        inner: adapter,
        tracker: tracker.clone(),
    };
    let adapter = &mut timed_adapter;
    let mut aliases = HashMap::<String, UiTransactionQuery>::new();
    let mut receipts = Vec::with_capacity(plan.steps.len());
    let mut completed_steps = 0usize;

    for (index, step) in plan.steps.iter().enumerate() {
        let operation = step.operation_name();
        if started.elapsed() >= runtime_budget {
            return failed_result(
                FailedResultContext {
                    surface,
                    total_steps,
                    completed_steps,
                    receipts,
                    failed_step: Some(index),
                    failed_operation: Some(operation),
                },
                safe_error(UiTransactionErrorCode::TransactionTimeout),
                None,
                tracker.borrow().snapshot(),
                Some(started),
            );
        }

        let outcome: Result<Option<bool>, UiTransactionError> = match step {
            UiTransactionStep::Find { query, as_name } => {
                let resolved = resolve_unique(
                    adapter,
                    &aliases,
                    &UiTransactionTarget::Query(query.clone()),
                );
                if started.elapsed() >= runtime_budget {
                    Err(safe_error(UiTransactionErrorCode::TransactionTimeout))
                } else {
                    match resolved {
                        Ok(_) => {
                            aliases.insert(as_name.clone(), query.clone());
                            Ok(None)
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            UiTransactionStep::Click { target } => {
                let resolved = resolve_unique(adapter, &aliases, target);
                if started.elapsed() >= runtime_budget {
                    Err(safe_error(UiTransactionErrorCode::TransactionTimeout))
                } else {
                    match resolved {
                        Ok((snapshot, node)) => {
                            if !node_has_action(&node, "click") {
                                Err(safe_error(UiTransactionErrorCode::ActionUnsupported))
                            } else {
                                adapter
                                    .click(&snapshot.snapshot_id, &node.ref_id)
                                    .map(|_| None)
                                    .map_err(|_| safe_error(UiTransactionErrorCode::MutationFailed))
                            }
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            UiTransactionStep::Type {
                target,
                text,
                clear_first,
            } => {
                let resolved = resolve_unique(adapter, &aliases, target);
                if started.elapsed() >= runtime_budget {
                    Err(safe_error(UiTransactionErrorCode::TransactionTimeout))
                } else {
                    match resolved {
                        Ok((snapshot, node)) => {
                            if node.sensitive {
                                Err(safe_error(UiTransactionErrorCode::SensitiveTarget))
                            } else if !node_has_action(&node, "type") {
                                Err(safe_error(UiTransactionErrorCode::ActionUnsupported))
                            } else {
                                adapter
                                    .type_text(
                                        &snapshot.snapshot_id,
                                        &node.ref_id,
                                        text,
                                        *clear_first,
                                    )
                                    .map(|_| None)
                                    .map_err(|_| safe_error(UiTransactionErrorCode::MutationFailed))
                            }
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            UiTransactionStep::Wait { wait_ms } => {
                let remaining = runtime_budget.saturating_sub(started.elapsed());
                let requested = Duration::from_millis(*wait_ms);
                let wait_started = Instant::now();
                let outcome = if requested > remaining {
                    if !remaining.is_zero() {
                        thread::sleep(remaining);
                    }
                    Err(safe_error(UiTransactionErrorCode::TransactionTimeout))
                } else {
                    thread::sleep(requested);
                    Ok(None)
                };
                tracker
                    .borrow_mut()
                    .record(LatencyPhase::Wait, wait_started.elapsed());
                outcome
            }
            UiTransactionStep::Verify { condition } => {
                let evaluated = evaluate_condition(adapter, &aliases, condition);
                if started.elapsed() >= runtime_budget {
                    Err(safe_error(UiTransactionErrorCode::TransactionTimeout))
                } else {
                    match evaluated {
                        Ok(true) => Ok(Some(true)),
                        Ok(false) => Err(safe_error(UiTransactionErrorCode::VerificationFailed)),
                        Err(error) => Err(error),
                    }
                }
            }
            UiTransactionStep::WaitUntil {
                condition,
                timeout_ms,
            } => {
                let condition_started = Instant::now();
                let timeout = Duration::from_millis(*timeout_ms);
                loop {
                    if started.elapsed() >= runtime_budget {
                        break Err(safe_error(UiTransactionErrorCode::TransactionTimeout));
                    }
                    if condition_started.elapsed() >= timeout {
                        break Err(safe_error(UiTransactionErrorCode::ConditionTimeout));
                    }
                    let evaluated = evaluate_condition(adapter, &aliases, condition);
                    if started.elapsed() >= runtime_budget {
                        break Err(safe_error(UiTransactionErrorCode::TransactionTimeout));
                    }
                    if condition_started.elapsed() >= timeout {
                        break Err(safe_error(UiTransactionErrorCode::ConditionTimeout));
                    }
                    match evaluated {
                        Ok(true) => break Ok(Some(true)),
                        Ok(false) => {
                            let condition_remaining =
                                timeout.saturating_sub(condition_started.elapsed());
                            let runtime_remaining =
                                runtime_budget.saturating_sub(started.elapsed());
                            let sleep_for = Duration::from_millis(60)
                                .min(condition_remaining)
                                .min(runtime_remaining);
                            if sleep_for.is_zero() {
                                continue;
                            }
                            let wait_started = Instant::now();
                            thread::sleep(sleep_for);
                            tracker
                                .borrow_mut()
                                .record(LatencyPhase::Wait, wait_started.elapsed());
                        }
                        Err(error) => break Err(error),
                    }
                }
            }
        };

        match outcome {
            Ok(verified) => {
                receipts.push(step_receipt(index, operation, "completed", verified));
                completed_steps += 1;
            }
            Err(error) => {
                let verified = matches!(
                    error.code,
                    UiTransactionErrorCode::ConditionTimeout
                        | UiTransactionErrorCode::VerificationFailed
                )
                .then_some(false);
                return failed_result(
                    FailedResultContext {
                        surface,
                        total_steps,
                        completed_steps,
                        receipts,
                        failed_step: Some(index),
                        failed_operation: Some(operation),
                    },
                    error,
                    verified,
                    tracker.borrow().snapshot(),
                    Some(started),
                );
            }
        }
    }

    let mut result = UiTransactionResult {
        ok: true,
        surface,
        total_steps,
        completed_steps,
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        timings: tracker.borrow().snapshot(),
        steps: receipts,
        failed_step: None,
        failed_operation: None,
        error: None,
        final_snapshot: None,
        final_observation_error: None,
    };

    if plan.return_semantic_snapshot {
        if started.elapsed() >= runtime_budget {
            result.final_observation_error =
                Some(safe_error(UiTransactionErrorCode::FinalObservationFailed));
        } else {
            match adapter.observe_final() {
                Ok(mut snapshot) if started.elapsed() <= runtime_budget => {
                    redact_typed_values_from_snapshot(&mut snapshot, &typed_values_to_redact);
                    result.final_snapshot = Some(snapshot);
                }
                Ok(_) | Err(_) => {
                    result.final_observation_error =
                        Some(safe_error(UiTransactionErrorCode::FinalObservationFailed));
                }
            }
        }
        result.elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    }
    result.timings = tracker.borrow().snapshot();

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(label: &str) -> UiTransactionQuery {
        UiTransactionQuery {
            query: Some(label.to_string()),
            role: None,
            name: None,
            state: None,
            action: None,
        }
    }

    fn click_step(label: &str) -> UiTransactionStep {
        UiTransactionStep::Click {
            target: UiTransactionTarget::Query(query(label)),
        }
    }

    fn plan(steps: Vec<UiTransactionStep>) -> UiTransactionPlan {
        UiTransactionPlan {
            surface: UiTransactionSurface::Desktop,
            steps,
            return_semantic_snapshot: true,
        }
    }

    fn invalid_code(
        result: Result<UiTransactionPlan, UiTransactionError>,
    ) -> UiTransactionErrorCode {
        result.expect_err("plan should be rejected").code
    }

    #[test]
    fn accepts_one_to_sixty_four_steps_and_rejects_other_counts() {
        assert_eq!(
            invalid_code(validate_plan(plan(vec![]))),
            UiTransactionErrorCode::InvalidPlan
        );
        assert!(validate_plan(plan(vec![click_step("one")])).is_ok());
        assert!(validate_plan(plan(
            (0..MAX_UI_TRANSACTION_STEPS)
                .map(|index| click_step(&format!("button-{index}")))
                .collect()
        ))
        .is_ok());
        assert_eq!(
            invalid_code(validate_plan(plan(
                (0..=MAX_UI_TRANSACTION_STEPS)
                    .map(|index| click_step(&format!("button-{index}")))
                    .collect()
            ))),
            UiTransactionErrorCode::InvalidPlan
        );
    }

    #[test]
    fn accepts_valid_aliases_and_rejects_invalid_duplicate_or_forward_aliases() {
        let valid = plan(vec![
            UiTransactionStep::Find {
                query: query("Open"),
                as_name: "open_button".to_string(),
            },
            UiTransactionStep::Click {
                target: UiTransactionTarget::Alias("open_button".to_string()),
            },
        ]);
        assert!(validate_plan(valid).is_ok());

        for alias in [
            String::new(),
            "space alias".to_string(),
            "slash/alias".to_string(),
            "a".repeat(65),
        ] {
            let bad = plan(vec![UiTransactionStep::Find {
                query: query("Open"),
                as_name: alias,
            }]);
            assert_eq!(
                invalid_code(validate_plan(bad)),
                UiTransactionErrorCode::InvalidPlan
            );
        }

        let duplicate = plan(vec![
            UiTransactionStep::Find {
                query: query("Open"),
                as_name: "same".to_string(),
            },
            UiTransactionStep::Find {
                query: query("Save"),
                as_name: "same".to_string(),
            },
        ]);
        assert_eq!(
            invalid_code(validate_plan(duplicate)),
            UiTransactionErrorCode::InvalidPlan
        );

        let forward = plan(vec![
            UiTransactionStep::Click {
                target: UiTransactionTarget::Alias("later".to_string()),
            },
            UiTransactionStep::Find {
                query: query("Later"),
                as_name: "later".to_string(),
            },
        ]);
        assert_eq!(
            invalid_code(validate_plan(forward)),
            UiTransactionErrorCode::InvalidPlan
        );
    }

    #[test]
    fn requires_exactly_one_alias_or_inline_query_for_targets() {
        assert!(matches!(
            target_from_parts(Some("saved".to_string()), None),
            Ok(UiTransactionTarget::Alias(alias)) if alias == "saved"
        ));
        assert!(matches!(
            target_from_parts(None, Some(query("Save"))),
            Ok(UiTransactionTarget::Query(_))
        ));
        assert_eq!(
            target_from_parts(None, None)
                .expect_err("missing target should fail")
                .code,
            UiTransactionErrorCode::InvalidPlan
        );
        assert_eq!(
            target_from_parts(Some("saved".to_string()), Some(query("Save")))
                .expect_err("two target sources should fail")
                .code,
            UiTransactionErrorCode::InvalidPlan
        );
    }

    #[test]
    fn requires_at_least_one_nonempty_query_filter() {
        let empty = UiTransactionQuery {
            query: Some("   ".to_string()),
            role: None,
            name: Some(String::new()),
            state: None,
            action: None,
        };
        assert_eq!(
            invalid_code(validate_plan(plan(vec![UiTransactionStep::Find {
                query: empty,
                as_name: "empty".to_string(),
            }]))),
            UiTransactionErrorCode::InvalidPlan
        );
        assert!(validate_plan(plan(vec![UiTransactionStep::Find {
            query: UiTransactionQuery {
                query: None,
                role: Some(" button ".to_string()),
                name: None,
                state: None,
                action: None,
            },
            as_name: "button".to_string(),
        }]))
        .is_ok());
    }

    #[test]
    fn rejects_typed_text_over_128_kib() {
        let accepted_steps = (0..4)
            .map(|index| UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query(&format!("Field {index}"))),
                text: "x".repeat(32_768),
                clear_first: true,
            })
            .collect::<Vec<_>>();
        assert!(validate_plan(plan(accepted_steps)).is_ok());

        let mut rejected_steps = (0..4)
            .map(|index| UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query(&format!("Field {index}"))),
                text: "x".repeat(32_768),
                clear_first: true,
            })
            .collect::<Vec<_>>();
        rejected_steps.push(UiTransactionStep::Type {
            target: UiTransactionTarget::Query(query("Overflow")),
            text: "x".to_string(),
            clear_first: true,
        });
        assert_eq!(
            invalid_code(validate_plan(plan(rejected_steps))),
            UiTransactionErrorCode::InvalidPlan
        );
    }

    #[test]
    fn preflights_existing_semantic_type_limits_before_any_mutation() {
        assert_eq!(
            invalid_code(validate_plan(plan(vec![UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query("Name")),
                text: "x".repeat(32_769),
                clear_first: true,
            }]))),
            UiTransactionErrorCode::InvalidPlan
        );

        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "click",
                vec![semantic_node(
                    "e1",
                    "Open",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "type",
                vec![semantic_node(
                    "e2",
                    "Name",
                    &["type"],
                    &["editable"],
                    None,
                    None,
                    false,
                )],
                false,
            ),
        ]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![
                click_step("Open"),
                UiTransactionStep::Type {
                    target: UiTransactionTarget::Query(query("Name")),
                    text: "bad\0text".to_string(),
                    clear_first: true,
                },
            ]),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 0);
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::InvalidPlan
        );
        assert_eq!(adapter.observe_calls, 0);
        assert!(adapter.clicks.is_empty());
    }

    #[test]
    fn bounds_fixed_and_condition_wait_budgets() {
        assert!(validate_plan(plan(vec![UiTransactionStep::Wait {
            wait_ms: MAX_UI_TRANSACTION_WAIT_MS,
        }]))
        .is_ok());
        assert_eq!(
            invalid_code(validate_plan(plan(vec![UiTransactionStep::Wait {
                wait_ms: MAX_UI_TRANSACTION_WAIT_MS + 1,
            }]))),
            UiTransactionErrorCode::InvalidPlan
        );
        assert_eq!(
            invalid_code(validate_plan(plan(vec![
                UiTransactionStep::Wait { wait_ms: 2_000 },
                UiTransactionStep::Wait { wait_ms: 2_000 },
                UiTransactionStep::Wait { wait_ms: 2_000 },
                UiTransactionStep::Wait { wait_ms: 2_000 },
                UiTransactionStep::Wait { wait_ms: 2_000 },
                UiTransactionStep::Wait { wait_ms: 1 },
            ]))),
            UiTransactionErrorCode::InvalidPlan
        );

        let condition = UiTransactionCondition {
            kind: UiTransactionConditionKind::SemanticExists,
            target: UiTransactionTarget::Query(query("Ready")),
            expected_text: None,
        };
        assert!(validate_plan(plan(vec![UiTransactionStep::WaitUntil {
            condition: condition.clone(),
            timeout_ms: MIN_UI_TRANSACTION_CONDITION_TIMEOUT_MS,
        }]))
        .is_ok());
        assert_eq!(
            invalid_code(validate_plan(plan(vec![UiTransactionStep::WaitUntil {
                condition: condition.clone(),
                timeout_ms: MIN_UI_TRANSACTION_CONDITION_TIMEOUT_MS - 1,
            }]))),
            UiTransactionErrorCode::InvalidPlan
        );
        assert_eq!(
            invalid_code(validate_plan(plan(vec![
                UiTransactionStep::WaitUntil {
                    condition: condition.clone(),
                    timeout_ms: MAX_UI_TRANSACTION_CONDITION_TIMEOUT_MS,
                },
                UiTransactionStep::WaitUntil {
                    condition: condition.clone(),
                    timeout_ms: MAX_UI_TRANSACTION_CONDITION_TIMEOUT_MS,
                },
                UiTransactionStep::WaitUntil {
                    condition,
                    timeout_ms: MIN_UI_TRANSACTION_CONDITION_TIMEOUT_MS,
                },
            ]))),
            UiTransactionErrorCode::InvalidPlan
        );
    }

    #[test]
    fn rejects_condition_expected_text_mismatches_but_preserves_empty_text_equals() {
        let text_equals_empty = UiTransactionCondition {
            kind: UiTransactionConditionKind::TextEquals,
            target: UiTransactionTarget::Query(query("Value")),
            expected_text: Some(String::new()),
        };
        assert!(validate_plan(plan(vec![UiTransactionStep::Verify {
            condition: text_equals_empty,
        }]))
        .is_ok());

        let missing_expected = UiTransactionCondition {
            kind: UiTransactionConditionKind::TextContains,
            target: UiTransactionTarget::Query(query("Value")),
            expected_text: None,
        };
        assert_eq!(
            invalid_code(validate_plan(plan(vec![UiTransactionStep::Verify {
                condition: missing_expected,
            }]))),
            UiTransactionErrorCode::InvalidPlan
        );

        let unexpected_expected = UiTransactionCondition {
            kind: UiTransactionConditionKind::SemanticEnabled,
            target: UiTransactionTarget::Query(query("Save")),
            expected_text: Some("secret".to_string()),
        };
        assert_eq!(
            invalid_code(validate_plan(plan(vec![UiTransactionStep::Verify {
                condition: unexpected_expected,
            }]))),
            UiTransactionErrorCode::InvalidPlan
        );
    }

    #[test]
    fn public_error_codes_serialize_to_exact_contract() {
        let cases = [
            (UiTransactionErrorCode::InvalidPlan, "INVALID_PLAN"),
            (UiTransactionErrorCode::TargetMissing, "TARGET_MISSING"),
            (UiTransactionErrorCode::TargetAmbiguous, "TARGET_AMBIGUOUS"),
            (
                UiTransactionErrorCode::ActionUnsupported,
                "ACTION_UNSUPPORTED",
            ),
            (UiTransactionErrorCode::SensitiveTarget, "SENSITIVE_TARGET"),
            (
                UiTransactionErrorCode::ConditionTimeout,
                "CONDITION_TIMEOUT",
            ),
            (
                UiTransactionErrorCode::VerificationFailed,
                "VERIFICATION_FAILED",
            ),
            (UiTransactionErrorCode::MutationFailed, "MUTATION_FAILED"),
            (
                UiTransactionErrorCode::TransactionTimeout,
                "TRANSACTION_TIMEOUT",
            ),
            (
                UiTransactionErrorCode::FinalObservationFailed,
                "FINAL_OBSERVATION_FAILED",
            ),
        ];
        for (code, expected) in cases {
            assert_eq!(serde_json::to_value(code).unwrap(), expected);
        }
    }

    #[test]
    fn history_summary_and_public_result_shapes_do_not_expose_typed_text_or_backend_ids() {
        const SECRET: &str = "SUPER_SECRET_TYPED_VALUE";
        let transaction_plan = plan(vec![UiTransactionStep::Type {
            target: UiTransactionTarget::Query(query("secret-field")),
            text: SECRET.to_string(),
            clear_first: true,
        }]);
        let summary = history_summary(&transaction_plan);
        assert!(!summary.contains(SECRET));
        assert!(!summary.contains("secret-field"));

        let result = UiTransactionResult {
            ok: false,
            surface: UiTransactionSurface::Desktop,
            total_steps: 1,
            completed_steps: 0,
            elapsed_ms: 1,
            timings: LatencyTimings::default(),
            steps: vec![UiTransactionStepReceipt {
                index: 0,
                operation: "type".to_string(),
                status: "failed".to_string(),
                verified: None,
            }],
            failed_step: Some(0),
            failed_operation: Some("type".to_string()),
            error: Some(UiTransactionError {
                code: UiTransactionErrorCode::MutationFailed,
                message: "The UI mutation failed.".to_string(),
            }),
            final_snapshot: None,
            final_observation_error: None,
        };
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains(SECRET));
        assert!(!serialized.contains("backend"));
    }

    #[test]
    fn transaction_timings_are_fixed_numeric_redacted_fields() {
        const SECRET: &str = "transaction-secret-never-echo";
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "type",
            vec![semantic_node(
                "e1",
                "Name",
                &["type"],
                &["editable"],
                None,
                None,
                false,
            )],
            false,
        )]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query("Name")),
                text: SECRET.to_string(),
                clear_first: true,
            }]),
        );
        let timings = serde_json::to_value(&result.timings).expect("serialize timings");
        let object = timings.as_object().expect("timings object");
        assert_eq!(
            object
                .get("externalMcpRoundTripIncluded")
                .and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(object
            .values()
            .all(|value| value.is_number() || value.is_boolean()));
        assert!(!serde_json::to_string(&timings).unwrap().contains(SECRET));
        for key in [
            "preflightMs",
            "observationMs",
            "mutationMs",
            "waitMs",
            "finalObservationMs",
            "repoTunnelInternalMs",
        ] {
            assert!(object.contains_key(key), "missing timing field {key}");
        }
    }

    use std::{collections::VecDeque, time::Duration};

    use crate::semantic::{SemanticNode, SemanticSnapshot, SemanticSurface};

    fn semantic_node(
        ref_id: &str,
        name: &str,
        actions: &[&str],
        states: &[&str],
        text: Option<&str>,
        value: Option<&str>,
        sensitive: bool,
    ) -> SemanticNode {
        SemanticNode {
            ref_id: ref_id.to_string(),
            role: "button".to_string(),
            name: name.to_string(),
            description: String::new(),
            text: text.map(str::to_string),
            value: value.map(str::to_string),
            states: states.iter().map(|value| (*value).to_string()).collect(),
            actions: actions.iter().map(|value| (*value).to_string()).collect(),
            bounds: None,
            sensitive,
            parent_ref: None,
            child_refs: Vec::new(),
        }
    }

    fn semantic_snapshot(
        snapshot_id: &str,
        nodes: Vec<SemanticNode>,
        truncated: bool,
    ) -> SemanticSnapshot {
        SemanticSnapshot {
            snapshot_id: snapshot_id.to_string(),
            version: 1,
            hash: format!("hash-{snapshot_id}"),
            surface: SemanticSurface::Desktop,
            target_id: "app.test".to_string(),
            document_identity: format!("doc-{snapshot_id}"),
            created_at: 1,
            nodes,
            truncated,
            unchanged: false,
        }
    }

    fn matches_query(node: &SemanticNode, query: &UiTransactionQuery) -> bool {
        let contains = |value: &str, wanted: &str| {
            value
                .to_ascii_lowercase()
                .contains(&wanted.trim().to_ascii_lowercase())
        };
        if let Some(role) = query.role.as_deref().filter(|v| !v.trim().is_empty()) {
            if !contains(&node.role, role) {
                return false;
            }
        }
        if let Some(name) = query.name.as_deref().filter(|v| !v.trim().is_empty()) {
            if !contains(&node.name, name) {
                return false;
            }
        }
        if let Some(state) = query.state.as_deref().filter(|v| !v.trim().is_empty()) {
            if !node
                .states
                .iter()
                .any(|value| value.eq_ignore_ascii_case(state.trim()))
            {
                return false;
            }
        }
        if let Some(action) = query.action.as_deref().filter(|v| !v.trim().is_empty()) {
            if !node
                .actions
                .iter()
                .any(|value| value.eq_ignore_ascii_case(action.trim()))
            {
                return false;
            }
        }
        if let Some(wanted) = query.query.as_deref().filter(|v| !v.trim().is_empty()) {
            if ![
                Some(node.role.as_str()),
                Some(node.name.as_str()),
                Some(node.description.as_str()),
                node.text.as_deref(),
                node.value.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|value| contains(value, wanted))
            {
                return false;
            }
        }
        true
    }

    #[derive(Default)]
    struct MockAdapter {
        observations: VecDeque<Result<SemanticSnapshot, String>>,
        last_snapshot: Option<SemanticSnapshot>,
        observe_calls: usize,
        observe_delay: Duration,
        find_snapshot_ids: Vec<String>,
        clicks: Vec<(String, String)>,
        types: Vec<(String, String, String, bool)>,
        click_results: VecDeque<Result<(), String>>,
        type_results: VecDeque<Result<(), String>>,
    }

    impl MockAdapter {
        fn with_snapshots(snapshots: Vec<SemanticSnapshot>) -> Self {
            Self {
                observations: snapshots.into_iter().map(Ok).collect(),
                ..Self::default()
            }
        }

        fn push_observation_error(&mut self, message: &str) {
            self.observations.push_back(Err(message.to_string()));
        }
    }

    impl UiTransactionAdapter for MockAdapter {
        fn observe(&mut self) -> Result<SemanticSnapshot, String> {
            self.observe_calls += 1;
            if !self.observe_delay.is_zero() {
                thread::sleep(self.observe_delay);
            }
            match self.observations.pop_front() {
                Some(Ok(snapshot)) => {
                    self.last_snapshot = Some(snapshot.clone());
                    Ok(snapshot)
                }
                Some(Err(error)) => Err(error),
                None => self
                    .last_snapshot
                    .clone()
                    .ok_or_else(|| "no observation available".to_string()),
            }
        }

        fn find(
            &mut self,
            snapshot: &SemanticSnapshot,
            query: &UiTransactionQuery,
        ) -> Result<Vec<SemanticNode>, String> {
            self.find_snapshot_ids.push(snapshot.snapshot_id.clone());
            Ok(snapshot
                .nodes
                .iter()
                .filter(|node| matches_query(node, query))
                .cloned()
                .collect())
        }

        fn click(&mut self, snapshot_id: &str, ref_id: &str) -> Result<(), String> {
            self.clicks
                .push((snapshot_id.to_string(), ref_id.to_string()));
            self.click_results.pop_front().unwrap_or(Ok(()))
        }

        fn type_text(
            &mut self,
            snapshot_id: &str,
            ref_id: &str,
            text: &str,
            clear_first: bool,
        ) -> Result<(), String> {
            self.types.push((
                snapshot_id.to_string(),
                ref_id.to_string(),
                text.to_string(),
                clear_first,
            ));
            self.type_results.pop_front().unwrap_or(Ok(()))
        }
    }

    fn execution_plan(steps: Vec<UiTransactionStep>) -> UiTransactionPlan {
        UiTransactionPlan {
            surface: UiTransactionSurface::Desktop,
            steps,
            return_semantic_snapshot: false,
        }
    }

    fn verify_condition(kind: UiTransactionConditionKind, label: &str) -> UiTransactionStep {
        UiTransactionStep::Verify {
            condition: UiTransactionCondition {
                kind,
                target: UiTransactionTarget::Query(query(label)),
                expected_text: None,
            },
        }
    }

    #[test]
    fn find_requires_exactly_one_match() {
        let mut missing =
            MockAdapter::with_snapshots(vec![semantic_snapshot("missing", vec![], false)]);
        let missing_result = execute_transaction(
            &mut missing,
            execution_plan(vec![UiTransactionStep::Find {
                query: query("Open"),
                as_name: "open".to_string(),
            }]),
        );
        assert_eq!(
            missing_result.error.unwrap().code,
            UiTransactionErrorCode::TargetMissing
        );

        let mut ambiguous = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "ambiguous",
            vec![
                semantic_node("e1", "Open", &["click"], &[], None, None, false),
                semantic_node("e2", "Open settings", &["click"], &[], None, None, false),
            ],
            false,
        )]);
        let ambiguous_result = execute_transaction(
            &mut ambiguous,
            execution_plan(vec![UiTransactionStep::Find {
                query: query("Open"),
                as_name: "open".to_string(),
            }]),
        );
        assert_eq!(
            ambiguous_result.error.unwrap().code,
            UiTransactionErrorCode::TargetAmbiguous
        );
    }

    #[test]
    fn alias_is_query_based_and_reresolves_after_ui_change() {
        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "before",
                vec![semantic_node(
                    "e1",
                    "Open",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "after",
                vec![semantic_node(
                    "e9",
                    "Open",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
        ]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![
                UiTransactionStep::Find {
                    query: query("Open"),
                    as_name: "open".to_string(),
                },
                UiTransactionStep::Click {
                    target: UiTransactionTarget::Alias("open".to_string()),
                },
            ]),
        );
        assert!(result.ok);
        assert_eq!(adapter.observe_calls, 2);
        assert_eq!(adapter.find_snapshot_ids, vec!["before", "after"]);
        assert_eq!(
            adapter.clicks,
            vec![("after".to_string(), "e9".to_string())]
        );
    }

    #[test]
    fn alias_that_becomes_ambiguous_fails_closed() {
        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "before",
                vec![semantic_node(
                    "e1",
                    "Open",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "after",
                vec![
                    semantic_node("e9", "Open", &["click"], &[], None, None, false),
                    semantic_node("e10", "Open recent", &["click"], &[], None, None, false),
                ],
                false,
            ),
        ]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![
                UiTransactionStep::Find {
                    query: query("Open"),
                    as_name: "open".to_string(),
                },
                UiTransactionStep::Click {
                    target: UiTransactionTarget::Alias("open".to_string()),
                },
            ]),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 1);
        assert_eq!(result.failed_step, Some(1));
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::TargetAmbiguous
        );
        assert!(adapter.clicks.is_empty());
    }

    #[test]
    fn truncated_zero_match_never_proves_semantic_missing() {
        let mut adapter =
            MockAdapter::with_snapshots(vec![semantic_snapshot("truncated", vec![], true)]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![UiTransactionStep::Verify {
                condition: UiTransactionCondition {
                    kind: UiTransactionConditionKind::SemanticMissing,
                    target: UiTransactionTarget::Query(query("Save")),
                    expected_text: None,
                },
            }]),
        );
        assert!(!result.ok);
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::VerificationFailed
        );
    }

    #[test]
    fn click_requires_advertised_click_action() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "no-click",
            vec![semantic_node("e1", "Save", &[], &[], None, None, false)],
            false,
        )]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![UiTransactionStep::Click {
                target: UiTransactionTarget::Query(query("Save")),
            }]),
        );
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::ActionUnsupported
        );
        assert!(adapter.clicks.is_empty());
    }

    #[test]
    fn sensitive_type_is_rejected_before_adapter_mutation() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "sensitive",
            vec![semantic_node(
                "e1",
                "Password",
                &["type"],
                &["editable"],
                None,
                None,
                true,
            )],
            false,
        )]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query("Password")),
                text: "do-not-send".to_string(),
                clear_first: true,
            }]),
        );
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::SensitiveTarget
        );
        assert!(adapter.types.is_empty());
    }

    #[test]
    fn state_and_text_conditions_require_one_target() {
        for condition in [
            UiTransactionCondition {
                kind: UiTransactionConditionKind::SemanticEnabled,
                target: UiTransactionTarget::Query(query("Missing")),
                expected_text: None,
            },
            UiTransactionCondition {
                kind: UiTransactionConditionKind::SemanticChecked,
                target: UiTransactionTarget::Query(query("Missing")),
                expected_text: None,
            },
            UiTransactionCondition {
                kind: UiTransactionConditionKind::SemanticFocused,
                target: UiTransactionTarget::Query(query("Missing")),
                expected_text: None,
            },
            UiTransactionCondition {
                kind: UiTransactionConditionKind::TextEquals,
                target: UiTransactionTarget::Query(query("Missing")),
                expected_text: Some("value".to_string()),
            },
            UiTransactionCondition {
                kind: UiTransactionConditionKind::TextContains,
                target: UiTransactionTarget::Query(query("Missing")),
                expected_text: Some("value".to_string()),
            },
        ] {
            let mut adapter =
                MockAdapter::with_snapshots(vec![semantic_snapshot("missing", vec![], false)]);
            let result = execute_transaction(
                &mut adapter,
                execution_plan(vec![UiTransactionStep::Verify { condition }]),
            );
            assert!(!result.ok);
            assert_eq!(result.completed_steps, 0);
            assert_eq!(result.failed_step, Some(0));
            assert_eq!(
                result.error.unwrap().code,
                UiTransactionErrorCode::TargetMissing
            );
        }
    }

    #[test]
    fn verify_supports_exists_enabled_checked_focused_and_text_conditions() {
        let snapshots = vec![
            semantic_snapshot(
                "exists",
                vec![semantic_node("e1", "Thing", &[], &[], None, None, false)],
                false,
            ),
            semantic_snapshot(
                "enabled",
                vec![semantic_node(
                    "e2",
                    "Thing",
                    &[],
                    &["enabled"],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "checked",
                vec![semantic_node(
                    "e3",
                    "Thing",
                    &[],
                    &["checked"],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "focused",
                vec![semantic_node(
                    "e4",
                    "Thing",
                    &[],
                    &["focused"],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "text",
                vec![semantic_node(
                    "e5",
                    "Thing",
                    &[],
                    &[],
                    Some("hello world"),
                    None,
                    false,
                )],
                false,
            ),
        ];
        let mut adapter = MockAdapter::with_snapshots(snapshots);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![
                verify_condition(UiTransactionConditionKind::SemanticExists, "Thing"),
                verify_condition(UiTransactionConditionKind::SemanticEnabled, "Thing"),
                verify_condition(UiTransactionConditionKind::SemanticChecked, "Thing"),
                verify_condition(UiTransactionConditionKind::SemanticFocused, "Thing"),
                UiTransactionStep::Verify {
                    condition: UiTransactionCondition {
                        kind: UiTransactionConditionKind::TextContains,
                        target: UiTransactionTarget::Query(query("Thing")),
                        expected_text: Some("world".to_string()),
                    },
                },
            ]),
        );
        assert!(result.ok);
        assert_eq!(result.completed_steps, 5);
        assert_eq!(adapter.observe_calls, 5);
        assert!(result
            .steps
            .iter()
            .all(|receipt| receipt.verified == Some(true)));
    }

    #[test]
    fn text_equals_empty_string_is_meaningful() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "empty",
            vec![semantic_node(
                "e1",
                "Value",
                &[],
                &[],
                None,
                Some(""),
                false,
            )],
            false,
        )]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![UiTransactionStep::Verify {
                condition: UiTransactionCondition {
                    kind: UiTransactionConditionKind::TextEquals,
                    target: UiTransactionTarget::Query(query("Value")),
                    expected_text: Some(String::new()),
                },
            }]),
        );
        assert!(result.ok);
    }

    #[test]
    fn semantic_missing_distinguishes_zero_one_and_many_matches() {
        let missing_step = || UiTransactionStep::Verify {
            condition: UiTransactionCondition {
                kind: UiTransactionConditionKind::SemanticMissing,
                target: UiTransactionTarget::Query(query("Dialog")),
                expected_text: None,
            },
        };

        let mut zero = MockAdapter::with_snapshots(vec![semantic_snapshot("zero", vec![], false)]);
        assert!(execute_transaction(&mut zero, execution_plan(vec![missing_step()])).ok);

        let mut one = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "one",
            vec![semantic_node("e1", "Dialog", &[], &[], None, None, false)],
            false,
        )]);
        assert_eq!(
            execute_transaction(&mut one, execution_plan(vec![missing_step()]))
                .error
                .unwrap()
                .code,
            UiTransactionErrorCode::VerificationFailed
        );

        let mut many = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "many",
            vec![
                semantic_node("e1", "Dialog", &[], &[], None, None, false),
                semantic_node("e2", "Dialog secondary", &[], &[], None, None, false),
            ],
            false,
        )]);
        assert_eq!(
            execute_transaction(&mut many, execution_plan(vec![missing_step()]))
                .error
                .unwrap()
                .code,
            UiTransactionErrorCode::TargetAmbiguous
        );
    }

    #[test]
    fn wait_until_refreshes_until_match_or_timeout() {
        let condition = UiTransactionCondition {
            kind: UiTransactionConditionKind::SemanticExists,
            target: UiTransactionTarget::Query(query("Ready")),
            expected_text: None,
        };
        let mut ready = MockAdapter::with_snapshots(vec![
            semantic_snapshot("not-yet", vec![], false),
            semantic_snapshot(
                "ready",
                vec![semantic_node("e1", "Ready", &[], &[], None, None, false)],
                false,
            ),
        ]);
        let result = execute_transaction(
            &mut ready,
            execution_plan(vec![UiTransactionStep::WaitUntil {
                condition: condition.clone(),
                timeout_ms: 250,
            }]),
        );
        assert!(result.ok);
        assert_eq!(ready.observe_calls, 2);

        let mut never =
            MockAdapter::with_snapshots(vec![semantic_snapshot("never", vec![], false)]);
        let timed_out = execute_transaction(
            &mut never,
            execution_plan(vec![UiTransactionStep::WaitUntil {
                condition,
                timeout_ms: 50,
            }]),
        );
        assert_eq!(
            timed_out.error.unwrap().code,
            UiTransactionErrorCode::ConditionTimeout
        );
    }

    #[test]
    fn condition_timeout_reports_completed_steps_without_rollback_claim() {
        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "click",
                vec![semantic_node(
                    "e1",
                    "Next",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot("waiting", vec![], false),
        ]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![
                UiTransactionStep::Click {
                    target: UiTransactionTarget::Query(query("Next")),
                },
                UiTransactionStep::WaitUntil {
                    condition: UiTransactionCondition {
                        kind: UiTransactionConditionKind::SemanticExists,
                        target: UiTransactionTarget::Query(query("Done")),
                        expected_text: None,
                    },
                    timeout_ms: 50,
                },
            ]),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 1);
        assert_eq!(result.failed_step, Some(1));
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::ConditionTimeout
        );
        assert_eq!(adapter.clicks.len(), 1);
    }

    #[test]
    fn mutation_failure_reports_exact_failed_step_and_completed_count() {
        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "one",
                vec![semantic_node(
                    "e1",
                    "One",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "two",
                vec![semantic_node(
                    "e2",
                    "Two",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
        ]);
        adapter.click_results = VecDeque::from([Ok(()), Err("raw helper failure".to_string())]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![click_step("One"), click_step("Two")]),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 1);
        assert_eq!(result.failed_step, Some(1));
        let error = result.error.unwrap();
        assert_eq!(error.code, UiTransactionErrorCode::MutationFailed);
        assert!(!error.message.contains("raw helper failure"));
    }

    #[test]
    fn transaction_runtime_guard_stops_later_steps() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "late",
            vec![semantic_node(
                "e1",
                "Late",
                &["click"],
                &[],
                None,
                None,
                false,
            )],
            false,
        )]);
        let result = execute_transaction_with_runtime_budget(
            &mut adapter,
            execution_plan(vec![
                UiTransactionStep::Wait { wait_ms: 2 },
                click_step("Late"),
            ]),
            Duration::from_millis(1),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 0);
        assert_eq!(result.failed_step, Some(0));
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::TransactionTimeout
        );
        assert!(adapter.clicks.is_empty());
    }

    #[test]
    fn runtime_expiry_during_target_observation_blocks_click() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "slow-observation",
            vec![semantic_node(
                "e1",
                "Save",
                &["click"],
                &[],
                None,
                None,
                false,
            )],
            false,
        )]);
        adapter.observe_delay = Duration::from_millis(5);
        let result = execute_transaction_with_runtime_budget(
            &mut adapter,
            execution_plan(vec![click_step("Save")]),
            Duration::from_millis(1),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 0);
        assert_eq!(result.failed_step, Some(0));
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::TransactionTimeout
        );
        assert!(adapter.clicks.is_empty());
    }

    #[test]
    fn runtime_expiry_during_verify_observation_fails_current_step() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "slow-verify",
            vec![semantic_node("e1", "Ready", &[], &[], None, None, false)],
            false,
        )]);
        adapter.observe_delay = Duration::from_millis(5);
        let result = execute_transaction_with_runtime_budget(
            &mut adapter,
            execution_plan(vec![verify_condition(
                UiTransactionConditionKind::SemanticExists,
                "Ready",
            )]),
            Duration::from_millis(1),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 0);
        assert_eq!(result.failed_step, Some(0));
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::TransactionTimeout
        );
    }

    #[test]
    fn condition_timeout_expiring_during_observation_cannot_succeed_wait_until() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "slow-ready",
            vec![semantic_node("e1", "Ready", &[], &[], None, None, false)],
            false,
        )]);
        adapter.observe_delay = Duration::from_millis(60);
        let result = execute_transaction_with_runtime_budget(
            &mut adapter,
            execution_plan(vec![UiTransactionStep::WaitUntil {
                condition: UiTransactionCondition {
                    kind: UiTransactionConditionKind::SemanticExists,
                    target: UiTransactionTarget::Query(query("Ready")),
                    expected_text: None,
                },
                timeout_ms: 50,
            }]),
            Duration::from_millis(250),
        );
        assert!(!result.ok);
        assert_eq!(result.completed_steps, 0);
        assert_eq!(result.failed_step, Some(0));
        assert_eq!(
            result.error.unwrap().code,
            UiTransactionErrorCode::ConditionTimeout
        );
    }

    #[test]
    fn successful_transaction_can_return_fresh_final_snapshot() {
        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "action",
                vec![semantic_node(
                    "e1",
                    "Save",
                    &["click"],
                    &[],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "final",
                vec![semantic_node("e2", "Saved", &[], &[], None, None, false)],
                false,
            ),
        ]);
        let result = execute_transaction(&mut adapter, plan(vec![click_step("Save")]));
        assert!(result.ok);
        assert_eq!(result.final_snapshot.unwrap().snapshot_id, "final");
        assert!(result.final_observation_error.is_none());
    }

    #[test]
    fn final_snapshot_never_echoes_text_supplied_to_type_steps() {
        const TYPED: &str = "VISIBLE_TYPED_VALUE_94731";
        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "type",
                vec![semantic_node(
                    "e1",
                    "Name",
                    &["type"],
                    &["editable"],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "final",
                vec![SemanticNode {
                    ref_id: "e2".to_string(),
                    role: "textbox".to_string(),
                    name: format!("Current value {TYPED}"),
                    description: format!("Entered {TYPED}"),
                    text: Some(format!("Preview: {TYPED}")),
                    value: Some(TYPED.to_string()),
                    states: vec!["editable".to_string()],
                    actions: vec!["type".to_string()],
                    bounds: None,
                    sensitive: false,
                    parent_ref: None,
                    child_refs: Vec::new(),
                }],
                false,
            ),
        ]);
        let result = execute_transaction(
            &mut adapter,
            plan(vec![UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query("Name")),
                text: TYPED.to_string(),
                clear_first: true,
            }]),
        );
        assert!(result.ok);
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains(TYPED));
    }

    #[test]
    fn final_snapshot_never_echoes_resolved_absolute_path_supplied_to_type() {
        const RESOLVED_PATH: &str = "/home/private/Projects/RepoTunnel/report.txt";
        let mut adapter = MockAdapter::with_snapshots(vec![
            semantic_snapshot(
                "type-path",
                vec![semantic_node(
                    "e1",
                    "File name",
                    &["type"],
                    &["editable"],
                    None,
                    None,
                    false,
                )],
                false,
            ),
            semantic_snapshot(
                "final-path",
                vec![semantic_node(
                    "e2",
                    "File name",
                    &["type"],
                    &["editable"],
                    None,
                    Some(RESOLVED_PATH),
                    false,
                )],
                false,
            ),
        ]);
        let result = execute_transaction(
            &mut adapter,
            plan(vec![UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query("File name")),
                text: RESOLVED_PATH.to_string(),
                clear_first: true,
            }]),
        );
        assert!(result.ok);
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains(RESOLVED_PATH));
    }

    #[test]
    fn final_observation_failure_preserves_successful_mutations() {
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "action",
            vec![semantic_node(
                "e1",
                "Save",
                &["click"],
                &[],
                None,
                None,
                false,
            )],
            false,
        )]);
        adapter.push_observation_error("private host error /home/user/secret");
        let result = execute_transaction(&mut adapter, plan(vec![click_step("Save")]));
        assert!(result.ok);
        assert_eq!(result.completed_steps, 1);
        assert!(result.final_snapshot.is_none());
        let error = result.final_observation_error.unwrap();
        assert_eq!(error.code, UiTransactionErrorCode::FinalObservationFailed);
        assert!(!error.message.contains("/home/user/secret"));
    }

    #[test]
    fn result_receipts_never_echo_type_text() {
        const SECRET: &str = "TRANSACTION_TYPED_SECRET";
        let mut adapter = MockAdapter::with_snapshots(vec![semantic_snapshot(
            "type",
            vec![semantic_node(
                "e1",
                "Name",
                &["type"],
                &["editable"],
                None,
                None,
                false,
            )],
            false,
        )]);
        let result = execute_transaction(
            &mut adapter,
            execution_plan(vec![UiTransactionStep::Type {
                target: UiTransactionTarget::Query(query("Name")),
                text: SECRET.to_string(),
                clear_first: true,
            }]),
        );
        assert!(result.ok);
        let serialized = serde_json::to_string(&result).unwrap();
        assert!(!serialized.contains(SECRET));
        assert_eq!(adapter.types.len(), 1);
    }
}
