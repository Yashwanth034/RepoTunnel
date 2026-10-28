use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, fmt};

pub(crate) const MAX_BROWSER_TRANSACTION_STEPS: usize = 64;
pub(crate) const MAX_BROWSER_TRANSACTION_TEXT_BYTES: usize = 128 * 1024;
pub(crate) const MAX_BROWSER_TRANSACTION_STEP_TIMEOUT_MS: u64 = 10_000;
pub(crate) const MAX_BROWSER_TRANSACTION_TOTAL_WAIT_MS: u64 = 20_000;
pub(crate) const MAX_BROWSER_TRANSACTION_RUNTIME_MS: u64 = 25_000;
const _: () = assert!(MAX_BROWSER_TRANSACTION_TOTAL_WAIT_MS < MAX_BROWSER_TRANSACTION_RUNTIME_MS);
const MIN_BROWSER_TRANSACTION_STEP_TIMEOUT_MS: u64 = 50;
const MAX_BROWSER_TRANSACTION_ALIAS_LENGTH: usize = 64;
const MAX_BROWSER_TRANSACTION_SELECTOR_LENGTH: usize = 4 * 1024;
const MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH: usize = 4 * 1024;
const MAX_BROWSER_FORM_FIELDS: usize = 64;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserLocator {
    pub(crate) css: Option<String>,
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) label: Option<String>,
    pub(crate) aria_label: Option<String>,
    pub(crate) placeholder: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) query: Option<String>,
}

impl fmt::Debug for BrowserLocator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserLocator")
            .field("css", &self.css)
            .field("id", &self.id)
            .field("name", &self.name)
            .field("label", &self.label)
            .field("aria_label", &self.aria_label)
            .field("placeholder", &self.placeholder)
            .field("role", &self.role)
            .field("query", &self.query)
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub(crate) enum BrowserTarget {
    Alias(String),
    Locator(BrowserLocator),
}

impl fmt::Debug for BrowserTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alias(alias) => f.debug_tuple("Alias").field(alias).finish(),
            Self::Locator(locator) => f.debug_tuple("Locator").field(locator).finish(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind", content = "value")]
pub(crate) enum BrowserFormValue {
    Text(String),
    SelectValue(String),
    SelectLabel(String),
    Checked(bool),
}

impl fmt::Debug for BrowserFormValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text(value) => f
                .debug_struct("Text")
                .field("characters", &value.chars().count())
                .finish(),
            Self::SelectValue(value) => f
                .debug_struct("SelectValue")
                .field("characters", &value.chars().count())
                .finish(),
            Self::SelectLabel(value) => f
                .debug_struct("SelectLabel")
                .field("characters", &value.chars().count())
                .finish(),
            Self::Checked(value) => f.debug_tuple("Checked").field(value).finish(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserFormAssignment {
    pub(crate) name: Option<String>,
    pub(crate) target: BrowserTarget,
    pub(crate) value: BrowserFormValue,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "condition", content = "value")]
pub(crate) enum BrowserTransactionCondition {
    UrlEquals(String),
    UrlContains(String),
    DocumentChanged,
    ElementExists(BrowserTarget),
    ElementMissing(BrowserTarget),
    ElementEnabled(BrowserTarget),
    ValueEquals {
        target: BrowserTarget,
        value: String,
    },
    ValueContains {
        target: BrowserTarget,
        value: String,
    },
    CheckedEquals {
        target: BrowserTarget,
        checked: bool,
    },
    SelectedValueEquals {
        target: BrowserTarget,
        value: String,
    },
    SelectedLabelEquals {
        target: BrowserTarget,
        label: String,
    },
    TextContains {
        target: BrowserTarget,
        text: String,
    },
}

impl fmt::Debug for BrowserTransactionCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::UrlEquals(_) => "UrlEquals",
            Self::UrlContains(_) => "UrlContains",
            Self::DocumentChanged => "DocumentChanged",
            Self::ElementExists(_) => "ElementExists",
            Self::ElementMissing(_) => "ElementMissing",
            Self::ElementEnabled(_) => "ElementEnabled",
            Self::ValueEquals { .. } => "ValueEquals",
            Self::ValueContains { .. } => "ValueContains",
            Self::CheckedEquals { .. } => "CheckedEquals",
            Self::SelectedValueEquals { .. } => "SelectedValueEquals",
            Self::SelectedLabelEquals { .. } => "SelectedLabelEquals",
            Self::TextContains { .. } => "TextContains",
        };
        f.write_str(name)
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "operation")]
pub(crate) enum BrowserTransactionStep {
    Find {
        locator: BrowserLocator,
        as_name: String,
    },
    Fill {
        target: BrowserTarget,
        text: String,
        clear_first: bool,
    },
    SelectOption {
        target: BrowserTarget,
        value: Option<String>,
        label: Option<String>,
    },
    SetChecked {
        target: BrowserTarget,
        checked: bool,
    },
    Click {
        target: BrowserTarget,
    },
    UploadFile {
        target: BrowserTarget,
        workspace_relative_path: String,
    },
    WaitUntil {
        condition: BrowserTransactionCondition,
        timeout_ms: u64,
    },
    Verify {
        condition: BrowserTransactionCondition,
    },
    FillForm {
        fields: Vec<BrowserFormAssignment>,
    },
}

impl fmt::Debug for BrowserTransactionStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Find { .. } => "Find",
            Self::Fill { .. } => "Fill",
            Self::SelectOption { .. } => "SelectOption",
            Self::SetChecked { .. } => "SetChecked",
            Self::Click { .. } => "Click",
            Self::UploadFile { .. } => "UploadFile",
            Self::WaitUntil { .. } => "WaitUntil",
            Self::Verify { .. } => "Verify",
            Self::FillForm { .. } => "FillForm",
        };
        f.write_str(name)
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrowserTransactionPlan {
    pub(crate) transaction_id: String,
    pub(crate) tab_id: String,
    pub(crate) steps: Vec<BrowserTransactionStep>,
}

impl fmt::Debug for BrowserTransactionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrowserTransactionPlan")
            .field("transaction_id", &self.transaction_id)
            .field("tab_id", &self.tab_id)
            .field("step_count", &self.steps.len())
            .finish()
    }
}

pub(crate) fn validate_transaction_id(value: &str) -> Result<String, String> {
    validate_bounded_identifier(value, 128, "Browser transaction ID")
}

pub(crate) fn validate_transaction_alias(value: &str) -> Result<String, String> {
    validate_bounded_identifier(
        value,
        MAX_BROWSER_TRANSACTION_ALIAS_LENGTH,
        "Browser transaction alias",
    )
}

fn validate_bounded_identifier(value: &str, max_len: usize, label: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > max_len
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!(
            "{label} must contain 1..{max_len} letters, digits, '-' or '_'."
        ));
    }
    Ok(value.to_string())
}

fn validate_tab_id(value: &str) -> Result<String, String> {
    validate_bounded_identifier(value, 256, "Browser tab ID")
}

fn validate_locator(locator: &BrowserLocator) -> Result<(), String> {
    let mut present = 0usize;
    for (label, value, max_len) in [
        (
            "css",
            locator.css.as_deref(),
            MAX_BROWSER_TRANSACTION_SELECTOR_LENGTH,
        ),
        (
            "id",
            locator.id.as_deref(),
            MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH,
        ),
        (
            "name",
            locator.name.as_deref(),
            MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH,
        ),
        (
            "label",
            locator.label.as_deref(),
            MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH,
        ),
        (
            "aria_label",
            locator.aria_label.as_deref(),
            MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH,
        ),
        (
            "placeholder",
            locator.placeholder.as_deref(),
            MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH,
        ),
        (
            "role",
            locator.role.as_deref(),
            MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH,
        ),
        (
            "query",
            locator.query.as_deref(),
            MAX_BROWSER_TRANSACTION_LOCATOR_TEXT_LENGTH,
        ),
    ] {
        if let Some(value) = value {
            let value = value.trim();
            if value.is_empty() || value.len() > max_len || value.as_bytes().contains(&0) {
                return Err(format!(
                    "Browser locator {label} must contain 1..{max_len} non-NUL bytes."
                ));
            }
            present += 1;
        }
    }
    if present == 0 {
        return Err("Browser locator requires at least one selector field.".to_string());
    }
    Ok(())
}

fn validate_target(target: &BrowserTarget, aliases: &BTreeSet<String>) -> Result<(), String> {
    match target {
        BrowserTarget::Alias(alias) => {
            let alias = validate_transaction_alias(alias)?;
            if !aliases.contains(&alias) {
                return Err(format!(
                    "Browser transaction references unknown or forward alias '{alias}'."
                ));
            }
            Ok(())
        }
        BrowserTarget::Locator(locator) => validate_locator(locator),
    }
}

fn validate_text(value: &str, label: &str, allow_empty: bool) -> Result<(), String> {
    if value.as_bytes().contains(&0) || (!allow_empty && value.is_empty()) {
        let requirement = if allow_empty {
            "must not contain NUL bytes"
        } else {
            "must not be empty or contain NUL bytes"
        };
        return Err(format!("{label} {requirement}."));
    }
    Ok(())
}

fn validate_nonempty_text(value: &str, label: &str) -> Result<(), String> {
    validate_text(value, label, false)
}

fn validate_form_state_text(value: &str, label: &str) -> Result<(), String> {
    validate_text(value, label, true)
}

fn condition_text_bytes(condition: &BrowserTransactionCondition) -> usize {
    match condition {
        BrowserTransactionCondition::UrlEquals(value)
        | BrowserTransactionCondition::UrlContains(value) => value.len(),
        BrowserTransactionCondition::ValueEquals { value, .. }
        | BrowserTransactionCondition::ValueContains { value, .. }
        | BrowserTransactionCondition::SelectedValueEquals { value, .. } => value.len(),
        BrowserTransactionCondition::SelectedLabelEquals { label, .. } => label.len(),
        BrowserTransactionCondition::TextContains { text, .. } => text.len(),
        BrowserTransactionCondition::DocumentChanged
        | BrowserTransactionCondition::ElementExists(_)
        | BrowserTransactionCondition::ElementMissing(_)
        | BrowserTransactionCondition::ElementEnabled(_)
        | BrowserTransactionCondition::CheckedEquals { .. } => 0,
    }
}

fn validate_condition(
    condition: &BrowserTransactionCondition,
    aliases: &BTreeSet<String>,
) -> Result<(), String> {
    match condition {
        BrowserTransactionCondition::UrlEquals(value)
        | BrowserTransactionCondition::UrlContains(value) => {
            validate_nonempty_text(value, "Browser transaction URL condition")
        }
        BrowserTransactionCondition::DocumentChanged => Ok(()),
        BrowserTransactionCondition::ElementExists(target)
        | BrowserTransactionCondition::ElementMissing(target)
        | BrowserTransactionCondition::ElementEnabled(target) => validate_target(target, aliases),
        BrowserTransactionCondition::ValueEquals { target, value }
        | BrowserTransactionCondition::SelectedValueEquals { target, value } => {
            validate_target(target, aliases)?;
            validate_form_state_text(value, "Browser transaction condition value")
        }
        BrowserTransactionCondition::ValueContains { target, value } => {
            validate_target(target, aliases)?;
            validate_nonempty_text(value, "Browser transaction condition value")
        }
        BrowserTransactionCondition::CheckedEquals { target, .. } => {
            validate_target(target, aliases)
        }
        BrowserTransactionCondition::SelectedLabelEquals { target, label } => {
            validate_target(target, aliases)?;
            validate_nonempty_text(label, "Browser transaction selected label")
        }
        BrowserTransactionCondition::TextContains { target, text } => {
            validate_target(target, aliases)?;
            validate_nonempty_text(text, "Browser transaction condition text")
        }
    }
}

fn form_value_text_bytes(value: &BrowserFormValue) -> usize {
    match value {
        BrowserFormValue::Text(value)
        | BrowserFormValue::SelectValue(value)
        | BrowserFormValue::SelectLabel(value) => value.len(),
        BrowserFormValue::Checked(_) => 0,
    }
}

fn validate_form_assignment(
    field: &BrowserFormAssignment,
    aliases: &BTreeSet<String>,
) -> Result<(), String> {
    if let Some(name) = field.name.as_deref() {
        validate_transaction_alias(name)?;
    }
    validate_target(&field.target, aliases)?;
    match &field.value {
        BrowserFormValue::Text(value)
        | BrowserFormValue::SelectValue(value)
        | BrowserFormValue::SelectLabel(value) => {
            validate_form_state_text(value, "Browser form value")
        }
        BrowserFormValue::Checked(_) => Ok(()),
    }
}

pub(crate) fn validate_transaction_plan(
    mut plan: BrowserTransactionPlan,
) -> Result<BrowserTransactionPlan, String> {
    plan.transaction_id = validate_transaction_id(&plan.transaction_id)?;
    plan.tab_id = validate_tab_id(&plan.tab_id)?;
    if plan.steps.is_empty() || plan.steps.len() > MAX_BROWSER_TRANSACTION_STEPS {
        return Err(format!(
            "Browser transaction must contain 1..{MAX_BROWSER_TRANSACTION_STEPS} steps."
        ));
    }

    let mut aliases = BTreeSet::<String>::new();
    let mut total_wait_ms = 0_u64;
    let mut total_text_bytes = 0_usize;

    for (index, step) in plan.steps.iter().enumerate() {
        let step_number = index + 1;
        match step {
            BrowserTransactionStep::Find { locator, as_name } => {
                validate_locator(locator)?;
                let alias = validate_transaction_alias(as_name)?;
                if !aliases.insert(alias.clone()) {
                    return Err(format!(
                        "Browser transaction step {step_number} reuses alias '{alias}'."
                    ));
                }
            }
            BrowserTransactionStep::Fill {
                target,
                text,
                clear_first: _,
            } => {
                validate_target(target, &aliases)?;
                validate_form_state_text(text, "Browser transaction fill text")?;
                total_text_bytes = total_text_bytes.saturating_add(text.len());
            }
            BrowserTransactionStep::SelectOption {
                target,
                value,
                label,
            } => {
                validate_target(target, &aliases)?;
                if value.is_some() == label.is_some() {
                    return Err(format!(
                        "Browser transaction step {step_number} select_option requires exactly one of value or label."
                    ));
                }
                let selected = value.as_deref().or(label.as_deref()).unwrap_or_default();
                validate_form_state_text(selected, "Browser select option")?;
                total_text_bytes = total_text_bytes.saturating_add(selected.len());
            }
            BrowserTransactionStep::SetChecked { target, .. }
            | BrowserTransactionStep::Click { target } => validate_target(target, &aliases)?,
            BrowserTransactionStep::UploadFile {
                target,
                workspace_relative_path,
            } => {
                validate_target(target, &aliases)?;
                validate_nonempty_text(workspace_relative_path, "Browser upload workspace path")?;
                if workspace_relative_path.starts_with('/')
                    || workspace_relative_path.split('/').any(|part| part == "..")
                {
                    return Err(
                        "Browser upload path must remain workspace-relative without parent traversal."
                            .to_string(),
                    );
                }
            }
            BrowserTransactionStep::WaitUntil {
                condition,
                timeout_ms,
            } => {
                if !(MIN_BROWSER_TRANSACTION_STEP_TIMEOUT_MS
                    ..=MAX_BROWSER_TRANSACTION_STEP_TIMEOUT_MS)
                    .contains(timeout_ms)
                {
                    return Err(format!(
                        "Browser transaction step {step_number} wait timeout must be {MIN_BROWSER_TRANSACTION_STEP_TIMEOUT_MS}..{MAX_BROWSER_TRANSACTION_STEP_TIMEOUT_MS} ms."
                    ));
                }
                total_wait_ms = total_wait_ms.saturating_add(*timeout_ms);
                if total_wait_ms > MAX_BROWSER_TRANSACTION_TOTAL_WAIT_MS {
                    return Err(format!(
                        "Browser transaction total wait budget exceeds {MAX_BROWSER_TRANSACTION_TOTAL_WAIT_MS} ms."
                    ));
                }
                validate_condition(condition, &aliases)?;
                total_text_bytes = total_text_bytes.saturating_add(condition_text_bytes(condition));
            }
            BrowserTransactionStep::Verify { condition } => {
                validate_condition(condition, &aliases)?;
                total_text_bytes = total_text_bytes.saturating_add(condition_text_bytes(condition));
            }
            BrowserTransactionStep::FillForm { fields } => {
                if fields.is_empty() || fields.len() > MAX_BROWSER_FORM_FIELDS {
                    return Err(format!(
                        "Browser fill_form must contain 1..{MAX_BROWSER_FORM_FIELDS} fields."
                    ));
                }
                let mut field_names = BTreeSet::new();
                for field in fields {
                    validate_form_assignment(field, &aliases)?;
                    if let Some(name) = field.name.as_deref() {
                        let name = validate_transaction_alias(name)?;
                        if !field_names.insert(name.clone()) {
                            return Err(format!(
                                "Browser fill_form contains duplicate field name '{name}'."
                            ));
                        }
                    }
                    total_text_bytes =
                        total_text_bytes.saturating_add(form_value_text_bytes(&field.value));
                }
            }
        }

        if total_text_bytes > MAX_BROWSER_TRANSACTION_TEXT_BYTES {
            return Err(format!(
                "Browser transaction supplied text exceeds {MAX_BROWSER_TRANSACTION_TEXT_BYTES} UTF-8 bytes."
            ));
        }
    }

    Ok(plan)
}

pub(crate) fn fill_form_plan(
    tab_id: String,
    transaction_id: String,
    fields: Vec<BrowserFormAssignment>,
) -> Result<BrowserTransactionPlan, String> {
    validate_transaction_plan(BrowserTransactionPlan {
        transaction_id,
        tab_id,
        steps: vec![BrowserTransactionStep::FillForm { fields }],
    })
}

pub(crate) fn history_summary(plan: &BrowserTransactionPlan) -> String {
    let noun = if plan.steps.len() == 1 {
        "step"
    } else {
        "steps"
    };
    format!(
        "{} · {} transaction {noun} · transaction {}",
        plan.tab_id,
        plan.steps.len(),
        plan.transaction_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locator(name: &str) -> BrowserLocator {
        BrowserLocator {
            css: None,
            id: None,
            name: Some(name.to_string()),
            label: None,
            aria_label: None,
            placeholder: None,
            role: None,
            query: None,
        }
    }

    fn text_field(alias: &str, text: &str) -> BrowserFormAssignment {
        BrowserFormAssignment {
            name: Some(alias.to_string()),
            target: BrowserTarget::Locator(locator(alias)),
            value: BrowserFormValue::Text(text.to_string()),
        }
    }

    fn plan(steps: Vec<BrowserTransactionStep>) -> BrowserTransactionPlan {
        BrowserTransactionPlan {
            transaction_id: "tx_test-1".to_string(),
            tab_id: "tab-1".to_string(),
            steps,
        }
    }

    #[test]
    fn accepts_valid_transaction_aliases_and_rejects_invalid_ones() {
        for valid in ["a", "field_1", "submit-button", &"x".repeat(64)] {
            let validated = validate_transaction_alias(valid).expect("valid alias");
            assert_eq!(validated, valid);
        }
        for invalid in ["", "has space", "slash/name", &"x".repeat(65)] {
            assert!(validate_transaction_alias(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn requires_one_to_sixty_four_transaction_steps() {
        assert_eq!(MAX_BROWSER_TRANSACTION_STEPS, 64);
        assert!(validate_transaction_plan(plan(vec![])).is_err());
        let one = vec![BrowserTransactionStep::Verify {
            condition: BrowserTransactionCondition::UrlContains("example".to_string()),
        }];
        assert!(validate_transaction_plan(plan(one.clone())).is_ok());
        let sixty_four = std::iter::repeat_n(one[0].clone(), 64).collect();
        assert!(validate_transaction_plan(plan(sixty_four)).is_ok());
        let sixty_five = std::iter::repeat_n(one[0].clone(), 65).collect();
        assert!(validate_transaction_plan(plan(sixty_five)).is_err());
    }

    #[test]
    fn rejects_unknown_or_forward_alias_references() {
        let unknown = plan(vec![BrowserTransactionStep::Fill {
            target: BrowserTarget::Alias("missing".to_string()),
            text: "value".to_string(),
            clear_first: true,
        }]);
        assert!(validate_transaction_plan(unknown).is_err());

        let forward = plan(vec![
            BrowserTransactionStep::Click {
                target: BrowserTarget::Alias("later".to_string()),
            },
            BrowserTransactionStep::Find {
                locator: locator("later"),
                as_name: "later".to_string(),
            },
        ]);
        assert!(validate_transaction_plan(forward).is_err());
    }

    #[test]
    fn rejects_duplicate_find_aliases() {
        let duplicate = plan(vec![
            BrowserTransactionStep::Find {
                locator: locator("first"),
                as_name: "same".to_string(),
            },
            BrowserTransactionStep::Find {
                locator: locator("second"),
                as_name: "same".to_string(),
            },
        ]);
        assert!(validate_transaction_plan(duplicate).is_err());
    }

    #[test]
    fn enforces_transaction_text_byte_budget() {
        assert_eq!(MAX_BROWSER_TRANSACTION_TEXT_BYTES, 128 * 1024);
        let at_limit = "x".repeat(MAX_BROWSER_TRANSACTION_TEXT_BYTES);
        assert!(
            validate_transaction_plan(plan(vec![BrowserTransactionStep::Fill {
                target: BrowserTarget::Locator(locator("field")),
                text: at_limit,
                clear_first: true,
            }]))
            .is_ok()
        );
        let over_limit = "x".repeat(MAX_BROWSER_TRANSACTION_TEXT_BYTES + 1);
        assert!(
            validate_transaction_plan(plan(vec![BrowserTransactionStep::Fill {
                target: BrowserTarget::Locator(locator("field")),
                text: over_limit,
                clear_first: true,
            }]))
            .is_err()
        );
    }

    #[test]
    fn enforces_wait_and_total_wait_bounds() {
        assert_eq!(MAX_BROWSER_TRANSACTION_STEP_TIMEOUT_MS, 10_000);
        assert_eq!(MAX_BROWSER_TRANSACTION_TOTAL_WAIT_MS, 20_000);
        assert_eq!(MAX_BROWSER_TRANSACTION_RUNTIME_MS, 25_000);

        let too_short = plan(vec![BrowserTransactionStep::WaitUntil {
            condition: BrowserTransactionCondition::UrlContains("ready".to_string()),
            timeout_ms: 49,
        }]);
        assert!(validate_transaction_plan(too_short).is_err());

        let too_long = plan(vec![BrowserTransactionStep::WaitUntil {
            condition: BrowserTransactionCondition::UrlContains("ready".to_string()),
            timeout_ms: 10_001,
        }]);
        assert!(validate_transaction_plan(too_long).is_err());

        let total_over = plan(vec![
            BrowserTransactionStep::WaitUntil {
                condition: BrowserTransactionCondition::UrlContains("one".to_string()),
                timeout_ms: 10_000,
            },
            BrowserTransactionStep::WaitUntil {
                condition: BrowserTransactionCondition::UrlContains("two".to_string()),
                timeout_ms: 10_000,
            },
            BrowserTransactionStep::WaitUntil {
                condition: BrowserTransactionCondition::UrlContains("three".to_string()),
                timeout_ms: 50,
            },
        ]);
        assert!(validate_transaction_plan(total_over).is_err());
    }

    #[test]
    fn requires_exactly_one_select_option_identity() {
        let neither = plan(vec![BrowserTransactionStep::SelectOption {
            target: BrowserTarget::Locator(locator("category")),
            value: None,
            label: None,
        }]);
        assert!(validate_transaction_plan(neither).is_err());

        let both = plan(vec![BrowserTransactionStep::SelectOption {
            target: BrowserTarget::Locator(locator("category")),
            value: Some("dev".to_string()),
            label: Some("Developer Tools".to_string()),
        }]);
        assert!(validate_transaction_plan(both).is_err());

        let one = plan(vec![BrowserTransactionStep::SelectOption {
            target: BrowserTarget::Locator(locator("category")),
            value: None,
            label: Some("Developer Tools".to_string()),
        }]);
        assert!(validate_transaction_plan(one).is_ok());
    }

    #[test]
    fn permits_empty_values_when_empty_is_a_meaningful_form_state() {
        let clear = plan(vec![BrowserTransactionStep::Fill {
            target: BrowserTarget::Locator(locator("description")),
            text: String::new(),
            clear_first: true,
        }]);
        assert!(validate_transaction_plan(clear).is_ok());

        let empty_option = plan(vec![BrowserTransactionStep::SelectOption {
            target: BrowserTarget::Locator(locator("category")),
            value: Some(String::new()),
            label: None,
        }]);
        assert!(validate_transaction_plan(empty_option).is_ok());

        let verify_empty = plan(vec![BrowserTransactionStep::Verify {
            condition: BrowserTransactionCondition::ValueEquals {
                target: BrowserTarget::Locator(locator("description")),
                value: String::new(),
            },
        }]);
        assert!(validate_transaction_plan(verify_empty).is_ok());

        let fill_form = fill_form_plan(
            "tab-1".to_string(),
            "fill-empty".to_string(),
            vec![text_field("description", "")],
        );
        assert!(fill_form.is_ok());
    }

    #[test]
    fn fill_form_plan_never_contains_submit_operation() {
        let plan = fill_form_plan(
            "tab-1".to_string(),
            "fill-1".to_string(),
            vec![text_field("name", "RepoTunnel")],
        )
        .expect("fill form plan");
        assert_eq!(plan.steps.len(), 1);
        assert!(matches!(
            plan.steps[0],
            BrowserTransactionStep::FillForm { .. }
        ));
        let encoded = serde_json::to_string(&plan).expect("serialize plan");
        assert!(!encoded.contains("submit"));
    }

    #[test]
    fn transaction_debug_and_history_summary_do_not_contain_supplied_text() {
        let secret = "private form text that must not enter history";
        let plan = validate_transaction_plan(plan(vec![BrowserTransactionStep::Fill {
            target: BrowserTarget::Locator(locator("description")),
            text: secret.to_string(),
            clear_first: true,
        }]))
        .expect("valid plan");
        assert!(!format!("{plan:?}").contains(secret));
        assert!(!history_summary(&plan).contains(secret));
        assert!(history_summary(&plan).contains("1 transaction step"));
    }
}
