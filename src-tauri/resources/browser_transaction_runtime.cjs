"use strict";

function privateFieldHint(value) {
  const text = String(value || "").toLowerCase();
  return [
    "password", "passwd", "passcode", "passphrase", "pin", "secret",
    "credential", "token", "api key", "api-key", "apikey", "access key", "private key",
    "verification code", "one-time", "one time", "otp", "2fa", "mfa",
  ].some((hint) => text.includes(hint));
}

function transactionError(code, message, detail) {
  const error = new Error(`${code}: ${message}`);
  error.code = code;
  if (detail !== undefined) error.detail = detail;
  return error;
}

function formatTransactionError(error) {
  const message = error instanceof Error ? error.message : String(error);
  const transactionId = typeof error?.transactionId === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(error.transactionId)
    ? error.transactionId
    : null;
  const completedSteps = Number.isSafeInteger(error?.completedSteps) && error.completedSteps >= 0
    ? error.completedSteps
    : null;
  const failedStep = Number.isSafeInteger(error?.failedStep) && error.failedStep >= 1
    ? error.failedStep
    : null;
  if (transactionId === null && completedSteps === null && failedStep === null) return message;
  const context = [];
  if (transactionId !== null) context.push(`transactionId=${transactionId}`);
  if (completedSteps !== null) context.push(`completedSteps=${completedSteps}`);
  if (failedStep !== null) context.push(`failedStep=${failedStep}`);
  return `${message} [${context.join("; ")}]`;
}

function normalizeText(value) {
  return String(value ?? "").replace(/\s+/g, " ").trim();
}

function exactAttributeCandidates(document, attr, value) {
  return Array.from(document.querySelectorAll(`[${attr}]`))
    .filter((element) => element.getAttribute(attr) === value);
}

function labelCandidates(document, requested) {
  const wanted = normalizeText(requested);
  const matches = [];
  for (const label of Array.from(document.querySelectorAll("label"))) {
    const clone = label.cloneNode(true);
    for (const child of Array.from(clone.querySelectorAll("input,textarea,select,button"))) {
      child.remove();
    }
    if (normalizeText(clone.textContent) !== wanted) continue;
    const forId = label.getAttribute("for");
    if (forId) {
      const target = document.getElementById(forId);
      if (target) matches.push(target);
      continue;
    }
    const nested = label.querySelector("input,textarea,select,button,[contenteditable='true']");
    if (nested) matches.push(nested);
  }
  return matches;
}

function uniqueElements(elements) {
  return Array.from(new Set(elements));
}

function candidateRole(element) {
  const explicit = normalizeText(element.getAttribute?.("role")).toLowerCase();
  if (explicit) return explicit;
  const tag = String(element.tagName || "").toLowerCase();
  const type = normalizeText(element.getAttribute?.("type")).toLowerCase();
  if (tag === "textarea") return "textbox";
  if (tag === "select") return "combobox";
  if (tag === "button") return "button";
  if (tag === "a" && element.hasAttribute?.("href")) return "link";
  if (tag === "input") {
    if (type === "checkbox") return "checkbox";
    if (type === "radio") return "radio";
    if (type === "button" || type === "submit" || type === "reset") return "button";
    return "textbox";
  }
  if (element.isContentEditable) return "textbox";
  return "";
}

function semanticTextParts(element) {
  const document = element.ownerDocument;
  const referenced = (attribute) => normalizeText(element.getAttribute?.(attribute))
    .split(/\s+/)
    .filter(Boolean)
    .map((id) => normalizeText(document?.getElementById(id)?.textContent));
  return uniqueElements([
    element.getAttribute?.("aria-label"),
    element.getAttribute?.("placeholder"),
    element.getAttribute?.("title"),
    element.getAttribute?.("alt"),
    element.getAttribute?.("name"),
    element.getAttribute?.("id"),
    ...Array.from(element.labels || []).map((label) => label.textContent),
    ...referenced("aria-labelledby"),
    normalizeText(element.textContent),
  ].map(normalizeText).filter(Boolean));
}

function semanticCandidateUniverse(document) {
  return Array.from(document.querySelectorAll(
    "input,textarea,select,button,a[href],[role],[contenteditable='true']",
  ));
}

function isSensitiveElement(element) {
  const type = normalizeText(element.getAttribute?.("type")).toLowerCase();
  const autocomplete = normalizeText(element.getAttribute?.("autocomplete")).toLowerCase();
  if (type === "password") return true;
  if (autocomplete.includes("password") || autocomplete.includes("one-time-code")) return true;
  const referencedText = (attribute) => normalizeText(element.getAttribute?.(attribute))
    .split(/\s+/)
    .filter(Boolean)
    .map((id) => normalizeText(element.ownerDocument?.getElementById(id)?.textContent))
    .filter(Boolean);
  const hints = [
    candidateRole(element),
    element.getAttribute?.("name"),
    element.getAttribute?.("id"),
    element.getAttribute?.("aria-label"),
    element.getAttribute?.("placeholder"),
    element.getAttribute?.("autocomplete"),
    ...Array.from(element.labels || []).map((label) => normalizeText(label.textContent)),
    ...referencedText("aria-labelledby"),
    ...referencedText("aria-describedby"),
  ];
  return hints.some(privateFieldHint);
}

function resolveElement(document, locator) {
  if (!document || !locator || typeof locator !== "object") {
    throw transactionError("INVALID_ARGUMENT", "A browser DOM locator is required.");
  }

  let candidates = null;
  const intersect = (next) => {
    const unique = uniqueElements(next);
    if (candidates === null) {
      candidates = unique;
    } else {
      const allowed = new Set(unique);
      candidates = candidates.filter((element) => allowed.has(element));
    }
  };

  if (locator.css) {
    let matches;
    try {
      matches = Array.from(document.querySelectorAll(String(locator.css)));
    } catch {
      throw transactionError("INVALID_LOCATOR", "CSS selector is invalid.");
    }
    intersect(matches);
  }
  if (locator.id) {
    const element = document.getElementById(String(locator.id));
    intersect(element ? [element] : []);
  }
  if (locator.name) intersect(exactAttributeCandidates(document, "name", String(locator.name)));
  if (locator.label) intersect(labelCandidates(document, locator.label));
  if (locator.ariaLabel) {
    intersect(exactAttributeCandidates(document, "aria-label", String(locator.ariaLabel)));
  }
  if (locator.placeholder) {
    intersect(exactAttributeCandidates(document, "placeholder", String(locator.placeholder)));
  }

  if (candidates === null && (locator.role || locator.query)) {
    candidates = semanticCandidateUniverse(document);
  }
  if (candidates === null) {
    throw transactionError(
      "FALLBACK_REQUIRED",
      "DOM identity was not specific enough; semantic or visual grounding is required.",
      { role: locator.role || null, query: locator.query || null },
    );
  }

  if (locator.role) {
    const role = normalizeText(locator.role).toLowerCase();
    candidates = candidates.filter((element) => candidateRole(element) === role);
  }
  if (locator.query) {
    const query = normalizeText(locator.query).toLowerCase();
    const withParts = candidates.map((element) => ({
      element,
      parts: semanticTextParts(element).map((part) => part.toLowerCase()),
    }));
    const exact = withParts.filter(({ parts }) => parts.some((part) => part === query));
    const matched = exact.length > 0
      ? exact
      : withParts.filter(({ parts }) => parts.some((part) => part.includes(query)));
    candidates = matched.map(({ element }) => element);
  }

  candidates = uniqueElements(candidates);
  if (candidates.length === 0) {
    throw transactionError(
      "FALLBACK_REQUIRED",
      "No unique DOM target matched; semantic or visual grounding is required.",
      { role: locator.role || null, query: locator.query || null },
    );
  }
  if (candidates.length > 1) {
    throw transactionError(
      "AMBIGUOUS_TARGET",
      `${candidates.length} DOM elements matched the locator.`,
      { count: candidates.length },
    );
  }

  const element = candidates[0];
  if (isSensitiveElement(element)) {
    throw transactionError("SENSITIVE_FIELD", "RepoTunnel blocks sensitive credential fields.");
  }
  return element;
}

function safeElementSummary(element) {
  return {
    ok: true,
    tag: String(element.tagName || "").toLowerCase(),
    id: element.id || null,
    name: element.getAttribute?.("name") || null,
    role: candidateRole(element) || null,
  };
}

function isDisabled(element) {
  return element.disabled === true ||
    element.hasAttribute?.("disabled") ||
    normalizeText(element.getAttribute?.("aria-disabled")).toLowerCase() === "true";
}

function isReadonly(element) {
  return element.readOnly === true ||
    element.hasAttribute?.("readonly") ||
    normalizeText(element.getAttribute?.("aria-readonly")).toLowerCase() === "true";
}

function dispatchValueEvents(window, element) {
  element.dispatchEvent(new window.Event("input", { bubbles: true }));
  element.dispatchEvent(new window.Event("change", { bubbles: true }));
}

function nativeSetter(window, element, property, value) {
  const constructors = [];
  if (element instanceof window.HTMLInputElement) constructors.push(window.HTMLInputElement);
  if (element instanceof window.HTMLTextAreaElement) constructors.push(window.HTMLTextAreaElement);
  if (element instanceof window.HTMLSelectElement) constructors.push(window.HTMLSelectElement);
  for (const ctor of constructors) {
    const descriptor = Object.getOwnPropertyDescriptor(ctor.prototype, property);
    if (descriptor?.set) {
      descriptor.set.call(element, value);
      return;
    }
  }
  element[property] = value;
}

function preflightFormAssignment(document, field) {
  if (!field || typeof field !== "object") {
    throw transactionError("INVALID_ARGUMENT", "Browser form assignment is invalid.");
  }
  if (!field.target || field.target.kind !== "locator" || !field.target.value) {
    throw transactionError("INVALID_ARGUMENT", "Bulk form fields require a DOM locator target.");
  }
  const element = resolveElement(document, field.target.value);
  if (isDisabled(element)) {
    throw transactionError("NOT_ACTIONABLE", "Browser form target is disabled.");
  }
  if (isReadonly(element)) {
    throw transactionError("NOT_ACTIONABLE", "Browser form target is read-only.");
  }

  const value = field.value || {};
  const kind = String(value.kind || "");
  const tag = String(element.tagName || "").toLowerCase();
  const inputType = normalizeText(element.getAttribute?.("type") || "text").toLowerCase();

  if (kind === "text") {
    const editableInput = tag === "input" && ![
      "button", "submit", "reset", "checkbox", "radio", "file", "hidden", "image",
    ].includes(inputType);
    const editable = editableInput || tag === "textarea" || element.isContentEditable ||
      normalizeText(element.getAttribute?.("contenteditable")).toLowerCase() === "true";
    if (!editable) {
      throw transactionError("UNSUPPORTED_CONTROL", "Text assignment requires a native editable control.");
    }
    if ((element.isContentEditable || element.getAttribute?.("contenteditable") === "true") && element.children.length > 0) {
      throw transactionError("FALLBACK_REQUIRED", "Rich contenteditable controls require semantic or visual handling.");
    }
    return { field, element, kind, desired: String(value.value ?? "") };
  }

  if (kind === "selectValue" || kind === "selectLabel") {
    if (tag !== "select") {
      throw transactionError("UNSUPPORTED_CONTROL", "Select assignment requires a native select element.");
    }
    const wanted = String(value.value ?? "");
    let option = null;
    if (kind === "selectValue") {
      option = Array.from(element.options).find((candidate) => candidate.value === wanted) || null;
    } else {
      const normalized = normalizeText(wanted);
      option = Array.from(element.options).find((candidate) => normalizeText(candidate.textContent) === normalized) || null;
    }
    if (!option) {
      throw transactionError("OPTION_NOT_FOUND", "Requested select option was not found.");
    }
    if (option.disabled) {
      throw transactionError("NOT_ACTIONABLE", "Requested select option is disabled.");
    }
    return { field, element, kind, desired: option.value, selectedLabel: normalizeText(option.textContent) };
  }

  if (kind === "checked") {
    if (tag !== "input" || !["checkbox", "radio"].includes(inputType)) {
      throw transactionError("UNSUPPORTED_CONTROL", "Checked assignment requires a native checkbox or radio input.");
    }
    return { field, element, kind, desired: value.value === true };
  }

  throw transactionError("INVALID_ARGUMENT", `Unsupported browser form value kind: ${kind || "<empty>"}.`);
}

function applyFormAssignment(window, prepared) {
  const { element, kind, desired } = prepared;
  if (kind === "text") {
    if (element.isContentEditable || normalizeText(element.getAttribute?.("contenteditable")).toLowerCase() === "true") {
      element.focus?.();
      element.textContent = desired;
    } else {
      nativeSetter(window, element, "value", desired);
      element.focus?.();
    }
    dispatchValueEvents(window, element);
    const actual = element.isContentEditable || normalizeText(element.getAttribute?.("contenteditable")).toLowerCase() === "true"
      ? String(element.textContent || "")
      : String(element.value ?? "");
    if (actual !== desired) {
      throw transactionError("POSTCONDITION_FAILED", "Browser text field did not retain the requested value.");
    }
    return { operation: "text", verified: true, characterCount: desired.length };
  }

  if (kind === "selectValue" || kind === "selectLabel") {
    nativeSetter(window, element, "value", desired);
    dispatchValueEvents(window, element);
    if (String(element.value) !== desired) {
      throw transactionError("POSTCONDITION_FAILED", "Browser select did not retain the requested option.");
    }
    return { operation: kind, verified: true };
  }

  if (kind === "checked") {
    if (Boolean(element.checked) !== desired) {
      nativeSetter(window, element, "checked", desired);
      element.dispatchEvent(new window.Event("click", { bubbles: true }));
      dispatchValueEvents(window, element);
    }
    if (element.checked !== desired) {
      throw transactionError("POSTCONDITION_FAILED", "Browser checked control did not retain the requested state.");
    }
    return { operation: "checked", verified: true, checked: desired };
  }

  throw transactionError("INVALID_ARGUMENT", "Unsupported prepared browser form operation.");
}

function verifyLiveFormAssignment(document, prepared) {
  const element = resolveElement(document, prepared.field.target.value);
  const { kind, desired } = prepared;
  let matches = false;
  if (kind === "text") {
    const contentEditable = element.isContentEditable || normalizeText(element.getAttribute?.("contenteditable")).toLowerCase() === "true";
    matches = (contentEditable ? String(element.textContent || "") : String(element.value ?? "")) === desired;
  } else if (kind === "selectValue" || kind === "selectLabel") {
    matches = String(element.value ?? "") === desired;
  } else if (kind === "checked") {
    matches = Boolean(element.checked) === desired;
  }
  if (!matches) {
    throw transactionError("POSTCONDITION_FAILED", "Browser form target did not retain the requested live DOM state.");
  }
}

function fillForm(document, window, assignments) {
  if (!Array.isArray(assignments) || assignments.length < 1 || assignments.length > 64) {
    throw transactionError("INVALID_ARGUMENT", "Browser fill_form requires 1..64 assignments.");
  }

  // All-target static preflight is intentionally complete before the first mutation.
  assignments.map((field) => preflightFormAssignment(document, field));
  const applied = [];
  const fields = assignments.map((field) => {
    // Earlier field events may replace later controls, so resolve and preflight again immediately before mutation.
    const current = preflightFormAssignment(document, field);
    const result = applyFormAssignment(window, current);
    verifyLiveFormAssignment(document, current);
    applied.push(current);
    return {
      name: current.field.name || null,
      ...result,
    };
  });
  // A later field event can replace an earlier control, so verify every final live target again.
  for (const entry of applied) verifyLiveFormAssignment(document, entry);
  return { ok: true, verified: true, fields };
}

const TRANSACTION_ID_RE = /^[A-Za-z0-9_-]{1,128}$/;
const TRANSACTION_ALIAS_RE = /^[A-Za-z0-9_-]{1,64}$/;
const TRANSACTION_MAX_STEPS = 64;
const TRANSACTION_MAX_TEXT_BYTES = 128 * 1024;
const TRANSACTION_MIN_WAIT_MS = 50;
const TRANSACTION_MAX_WAIT_MS = 10_000;
const TRANSACTION_MAX_TOTAL_WAIT_MS = 20_000;
const TRANSACTION_MAX_RUNTIME_MS = 25_000;

function utf8Bytes(value) {
  return Buffer.byteLength(String(value ?? ""), "utf8");
}

function hasDeterministicLocator(locator) {
  return Boolean(locator && (locator.css || locator.id || locator.name || locator.label || locator.ariaLabel || locator.placeholder || locator.role || locator.query));
}

function validateTargetSyntax(target, aliases) {
  if (!target || typeof target !== "object") {
    throw transactionError("INVALID_ARGUMENT", "Browser transaction target is required.");
  }
  if (target.kind === "alias") {
    const alias = String(target.value || "");
    if (!TRANSACTION_ALIAS_RE.test(alias)) {
      throw transactionError("INVALID_ARGUMENT", "Browser transaction alias is invalid.");
    }
    if (!aliases.has(alias)) {
      throw transactionError("INVALID_ARGUMENT", `Browser transaction references unknown or forward alias '${alias}'.`);
    }
    return;
  }
  if (target.kind === "locator" && target.value && typeof target.value === "object") {
    const locator = target.value;
    if (!hasDeterministicLocator(locator) && !locator.role && !locator.query) {
      throw transactionError("INVALID_ARGUMENT", "Browser transaction locator requires at least one locator field.");
    }
    return;
  }
  throw transactionError("INVALID_ARGUMENT", "Browser transaction target must be an alias or locator.");
}

function conditionTextBytes(condition) {
  const kind = String(condition?.condition || "");
  const value = condition?.value;
  if (["urlEquals", "urlContains"].includes(kind)) return utf8Bytes(value);
  if (["valueEquals", "valueContains", "selectedValueEquals"].includes(kind)) return utf8Bytes(value?.value);
  if (kind === "selectedLabelEquals") return utf8Bytes(value?.label);
  if (kind === "textContains") return utf8Bytes(value?.text);
  return 0;
}

function validateConditionSyntax(condition, aliases) {
  if (!condition || typeof condition !== "object") {
    throw transactionError("INVALID_ARGUMENT", "Browser transaction condition is required.");
  }
  const kind = String(condition.condition || "");
  const value = condition.value;
  if (["urlEquals", "urlContains"].includes(kind)) {
    if (typeof value !== "string" || value.length === 0) {
      throw transactionError("INVALID_ARGUMENT", `${kind} requires a non-empty string.`);
    }
    return;
  }
  if (kind === "documentChanged") return;
  if (["elementExists", "elementMissing", "elementEnabled"].includes(kind)) {
    validateTargetSyntax(value, aliases);
    return;
  }
  if (["valueEquals", "selectedValueEquals"].includes(kind)) {
    validateTargetSyntax(value?.target, aliases);
    if (typeof value?.value !== "string" || value.value.includes("\0")) {
      throw transactionError("INVALID_ARGUMENT", `${kind} requires a string without NUL bytes.`);
    }
    return;
  }
  if (kind === "valueContains") {
    validateTargetSyntax(value?.target, aliases);
    if (typeof value?.value !== "string" || value.value.length === 0 || value.value.includes("\0")) {
      throw transactionError("INVALID_ARGUMENT", "valueContains requires a non-empty string without NUL bytes.");
    }
    return;
  }
  if (kind === "checkedEquals") {
    validateTargetSyntax(value?.target, aliases);
    if (typeof value?.checked !== "boolean") {
      throw transactionError("INVALID_ARGUMENT", "checkedEquals requires a boolean checked value.");
    }
    return;
  }
  if (kind === "selectedLabelEquals") {
    validateTargetSyntax(value?.target, aliases);
    if (typeof value?.label !== "string" || value.label.length === 0) {
      throw transactionError("INVALID_ARGUMENT", "selectedLabelEquals requires a non-empty label.");
    }
    return;
  }
  if (kind === "textContains") {
    validateTargetSyntax(value?.target, aliases);
    if (typeof value?.text !== "string" || value.text.length === 0) {
      throw transactionError("INVALID_ARGUMENT", "textContains requires non-empty text.");
    }
    return;
  }
  throw transactionError("INVALID_ARGUMENT", `Unsupported browser transaction condition: ${kind || "<empty>"}.`);
}

function validateFormValueSyntax(value) {
  const kind = String(value?.kind || "");
  if (["text", "selectValue", "selectLabel"].includes(kind)) {
    if (typeof value?.value !== "string" || value.value.includes("\0")) {
      throw transactionError("INVALID_ARGUMENT", `Browser form ${kind} requires a string without NUL bytes.`);
    }
    return utf8Bytes(value.value);
  }
  if (kind === "checked") {
    if (typeof value?.value !== "boolean") {
      throw transactionError("INVALID_ARGUMENT", "Browser form checked requires a boolean.");
    }
    return 0;
  }
  throw transactionError("INVALID_ARGUMENT", `Unsupported browser form value kind: ${kind || "<empty>"}.`);
}

function validateTransactionPlan(plan) {
  if (!plan || typeof plan !== "object") {
    throw transactionError("INVALID_ARGUMENT", "Browser transaction plan is required.");
  }
  const transactionId = String(plan.transactionId || "");
  if (!TRANSACTION_ID_RE.test(transactionId)) {
    throw transactionError("INVALID_ARGUMENT", "Browser transaction ID is invalid.");
  }
  if (!Array.isArray(plan.steps) || plan.steps.length < 1 || plan.steps.length > TRANSACTION_MAX_STEPS) {
    throw transactionError("INVALID_ARGUMENT", "Browser transaction requires 1..64 steps.");
  }

  const aliases = new Set();
  let totalWaitMs = 0;
  let totalTextBytes = 0;
  for (let index = 0; index < plan.steps.length; index += 1) {
    const step = plan.steps[index] || {};
    const operation = String(step.operation || "");
    const stepNumber = index + 1;
    if (operation === "find") {
      const alias = String(step.asName || "");
      if (!TRANSACTION_ALIAS_RE.test(alias)) {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} has an invalid find alias.`);
      }
      if (aliases.has(alias)) {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} reuses alias '${alias}'.`);
      }
      validateTargetSyntax({ kind: "locator", value: step.locator }, aliases);
      aliases.add(alias);
    } else if (operation === "fill") {
      validateTargetSyntax(step.target, aliases);
      if (typeof step.text !== "string" || step.text.includes("\0")) {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} fill requires a string without NUL bytes.`);
      }
      totalTextBytes += utf8Bytes(step.text);
    } else if (operation === "selectOption") {
      validateTargetSyntax(step.target, aliases);
      const hasValue = typeof step.value === "string";
      const hasLabel = typeof step.label === "string";
      if (hasValue === hasLabel || (hasValue && step.value.includes("\0")) || (hasLabel && step.label.includes("\0"))) {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} selectOption requires exactly one string value or label without NUL bytes.`);
      }
      totalTextBytes += utf8Bytes(hasValue ? step.value : step.label);
    } else if (operation === "setChecked") {
      validateTargetSyntax(step.target, aliases);
      if (typeof step.checked !== "boolean") {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} setChecked requires a boolean.`);
      }
    } else if (operation === "click") {
      validateTargetSyntax(step.target, aliases);
    } else if (operation === "uploadFile") {
      validateTargetSyntax(step.target, aliases);
      if (typeof step.workspaceRelativePath !== "string" || step.workspaceRelativePath.length === 0) {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} uploadFile requires a workspace-relative path.`);
      }
    } else if (operation === "waitUntil") {
      const timeoutMs = Number(step.timeoutMs);
      if (!Number.isInteger(timeoutMs) || timeoutMs < TRANSACTION_MIN_WAIT_MS || timeoutMs > TRANSACTION_MAX_WAIT_MS) {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} wait timeout must be 50..10000 ms.`);
      }
      totalWaitMs += timeoutMs;
      if (totalWaitMs > TRANSACTION_MAX_TOTAL_WAIT_MS) {
        throw transactionError("INVALID_ARGUMENT", "Browser transaction total wait budget exceeds 20000 ms.");
      }
      validateConditionSyntax(step.condition, aliases);
      totalTextBytes += conditionTextBytes(step.condition);
    } else if (operation === "verify") {
      validateConditionSyntax(step.condition, aliases);
      totalTextBytes += conditionTextBytes(step.condition);
    } else if (operation === "fillForm") {
      if (!Array.isArray(step.fields) || step.fields.length < 1 || step.fields.length > 64) {
        throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} fillForm requires 1..64 fields.`);
      }
      const names = new Set();
      for (const field of step.fields) {
        if (field?.name != null) {
          const name = String(field.name);
          if (!TRANSACTION_ALIAS_RE.test(name) || names.has(name)) {
            throw transactionError("INVALID_ARGUMENT", `Browser transaction step ${stepNumber} has an invalid or duplicate form field name.`);
          }
          names.add(name);
        }
        validateTargetSyntax(field?.target, aliases);
        totalTextBytes += validateFormValueSyntax(field?.value);
      }
    } else {
      throw transactionError("INVALID_ARGUMENT", `Unsupported browser transaction operation: ${operation || "<empty>"}.`);
    }

    if (totalTextBytes > TRANSACTION_MAX_TEXT_BYTES) {
      throw transactionError("INVALID_ARGUMENT", "Browser transaction supplied text exceeds 131072 UTF-8 bytes.");
    }
  }
  return plan;
}

function resolveTransactionTarget(document, target, aliases, getDocumentIdentity) {
  if (target.kind === "locator") return resolveElement(document, target.value);
  const alias = aliases.get(String(target.value || ""));
  if (!alias) {
    throw transactionError("INVALID_ARGUMENT", `Browser transaction alias '${String(target.value || "")}' is unavailable.`);
  }
  const currentIdentity = String(getDocumentIdentity() || "");
  if (currentIdentity !== alias.documentIdentity) {
    throw transactionError("STALE_DOCUMENT", "Browser document changed after this alias was found.");
  }
  return resolveElement(document, alias.locator);
}

function safeOperationResult(operation, element, extra = {}) {
  const summary = element ? safeElementSummary(element) : {};
  delete summary.ok;
  return { operation, verified: true, ...summary, ...extra };
}

function preflightTextMutation(document, target, aliases, getDocumentIdentity, text, clearFirst = true) {
  const element = resolveTransactionTarget(document, target, aliases, getDocumentIdentity);
  const field = {
    name: null,
    target: { kind: "locator", value: { id: element.id || undefined, name: element.getAttribute?.("name") || undefined } },
    value: { kind: "text", value: String(text ?? "") },
  };
  // Validate the already-resolved element directly rather than resolving a generated weak locator again.
  if (isDisabled(element)) throw transactionError("NOT_ACTIONABLE", "Browser text target is disabled.");
  if (isReadonly(element)) throw transactionError("NOT_ACTIONABLE", "Browser text target is read-only.");
  const tag = String(element.tagName || "").toLowerCase();
  const inputType = normalizeText(element.getAttribute?.("type") || "text").toLowerCase();
  const editableInput = tag === "input" && !["button", "submit", "reset", "checkbox", "radio", "file", "hidden", "image"].includes(inputType);
  const contentEditable = element.isContentEditable || normalizeText(element.getAttribute?.("contenteditable")).toLowerCase() === "true";
  if (!editableInput && tag !== "textarea" && !contentEditable) {
    throw transactionError("SEMANTIC_FALLBACK_REQUIRED", "Text target is not a native editable control.");
  }
  if (contentEditable && element.children.length > 0) {
    throw transactionError("SEMANTIC_FALLBACK_REQUIRED", "Rich text target requires semantic or visual handling.");
  }
  const current = contentEditable ? String(element.textContent || "") : String(element.value ?? "");
  const incoming = String(text ?? "");
  return { field, element, kind: "text", desired: clearFirst ? incoming : current + incoming };
}

function preflightSelectMutation(document, step, aliases, getDocumentIdentity) {
  const element = resolveTransactionTarget(document, step.target, aliases, getDocumentIdentity);
  if (isDisabled(element)) throw transactionError("NOT_ACTIONABLE", "Browser select target is disabled.");
  if (String(element.tagName || "").toLowerCase() !== "select") {
    throw transactionError("SEMANTIC_FALLBACK_REQUIRED", "Custom combobox requires semantic or visual handling.");
  }
  const kind = typeof step.value === "string" ? "selectValue" : "selectLabel";
  const wanted = kind === "selectValue" ? step.value : step.label;
  const option = kind === "selectValue"
    ? Array.from(element.options).find((candidate) => candidate.value === wanted)
    : Array.from(element.options).find((candidate) => normalizeText(candidate.textContent) === normalizeText(wanted));
  if (!option) throw transactionError("OPTION_NOT_FOUND", "Requested select option was not found.");
  if (option.disabled) throw transactionError("NOT_ACTIONABLE", "Requested select option is disabled.");
  return { element, kind, desired: option.value };
}

function preflightCheckedMutation(document, step, aliases, getDocumentIdentity) {
  const element = resolveTransactionTarget(document, step.target, aliases, getDocumentIdentity);
  if (isDisabled(element)) throw transactionError("NOT_ACTIONABLE", "Browser checked target is disabled.");
  const tag = String(element.tagName || "").toLowerCase();
  const type = normalizeText(element.getAttribute?.("type")).toLowerCase();
  if (tag !== "input" || !["checkbox", "radio"].includes(type)) {
    throw transactionError("SEMANTIC_FALLBACK_REQUIRED", "Custom checked control requires semantic or visual handling.");
  }
  return { element, kind: "checked", desired: step.checked === true };
}

function preflightClickMutation(document, target, aliases, getDocumentIdentity) {
  const element = resolveTransactionTarget(document, target, aliases, getDocumentIdentity);
  if (isDisabled(element)) throw transactionError("NOT_ACTIONABLE", "Browser click target is disabled.");
  const tag = String(element.tagName || "").toLowerCase();
  const type = normalizeText(element.getAttribute?.("type")).toLowerCase();
  const role = candidateRole(element);
  const nativeClickable = tag === "button" || (tag === "a" && element.hasAttribute?.("href")) ||
    (tag === "input" && ["button", "submit", "reset", "checkbox", "radio"].includes(type));
  if (!nativeClickable && ["button", "link", "checkbox", "radio", "switch", "tab", "menuitem", "option"].includes(role)) {
    throw transactionError("SEMANTIC_FALLBACK_REQUIRED", "Custom clickable control requires semantic pointer handling.");
  }
  if (!nativeClickable && typeof element.click !== "function") {
    throw transactionError("SEMANTIC_FALLBACK_REQUIRED", "Target requires semantic or visual click handling.");
  }
  return element;
}

function safeResolveForCondition(document, target, aliases, getDocumentIdentity) {
  try {
    return { found: true, element: resolveTransactionTarget(document, target, aliases, getDocumentIdentity) };
  } catch (error) {
    if (error?.code === "FALLBACK_REQUIRED" && target?.kind === "locator" && hasDeterministicLocator(target.value)) {
      return { found: false, element: null };
    }
    throw error;
  }
}

function evaluateCondition(document, condition, aliases, getUrl, getDocumentIdentity, initialDocumentIdentity) {
  const kind = String(condition.condition || "");
  const value = condition.value;
  if (kind === "urlEquals") return String(getUrl() || "") === String(value || "");
  if (kind === "urlContains") return String(getUrl() || "").includes(String(value || ""));
  if (kind === "documentChanged") return String(getDocumentIdentity() || "") !== initialDocumentIdentity;
  if (kind === "elementExists") return safeResolveForCondition(document, value, aliases, getDocumentIdentity).found;
  if (kind === "elementMissing") return !safeResolveForCondition(document, value, aliases, getDocumentIdentity).found;
  if (kind === "elementEnabled") {
    const resolved = safeResolveForCondition(document, value, aliases, getDocumentIdentity);
    return resolved.found && !isDisabled(resolved.element);
  }

  const target = value?.target;
  const resolved = safeResolveForCondition(document, target, aliases, getDocumentIdentity);
  if (!resolved.found) return false;
  const element = resolved.element;
  if (kind === "valueEquals") return String(element.value ?? "") === String(value.value ?? "");
  if (kind === "valueContains") return String(element.value ?? "").includes(String(value.value ?? ""));
  if (kind === "checkedEquals") return Boolean(element.checked) === Boolean(value.checked);
  if (kind === "selectedValueEquals") return String(element.value ?? "") === String(value.value ?? "");
  if (kind === "selectedLabelEquals") {
    const label = normalizeText(element.selectedOptions?.[0]?.textContent || "");
    return label === normalizeText(value.label);
  }
  if (kind === "textContains") return normalizeText(element.textContent).includes(normalizeText(value.text));
  throw transactionError("INVALID_ARGUMENT", `Unsupported browser transaction condition: ${kind}.`);
}

function preflightTransactionFormAssignment(document, field, aliases, getDocumentIdentity) {
  const element = resolveTransactionTarget(document, field.target, aliases, getDocumentIdentity);
  if (isDisabled(element)) throw transactionError("NOT_ACTIONABLE", "Browser form target is disabled.");
  if (isReadonly(element)) throw transactionError("NOT_ACTIONABLE", "Browser form target is read-only.");
  const value = field.value || {};
  const kind = String(value.kind || "");
  if (kind === "text") {
    return { field, ...preflightTextMutation(document, field.target, aliases, getDocumentIdentity, value.value) };
  }
  if (kind === "selectValue" || kind === "selectLabel") {
    const step = {
      target: field.target,
      value: kind === "selectValue" ? String(value.value ?? "") : null,
      label: kind === "selectLabel" ? String(value.value ?? "") : null,
    };
    return { field, ...preflightSelectMutation(document, step, aliases, getDocumentIdentity) };
  }
  if (kind === "checked") {
    return { field, ...preflightCheckedMutation(document, { target: field.target, checked: value.value === true }, aliases, getDocumentIdentity) };
  }
  throw transactionError("INVALID_ARGUMENT", `Unsupported browser form value kind: ${kind || "<empty>"}.`);
}

function attachExecutionContext(error, transactionId, completedSteps, failedStep, partialSteps) {
  const wrapped = error instanceof Error ? error : transactionError("TRANSACTION_FAILED", String(error));
  if (!wrapped.code) wrapped.code = "TRANSACTION_FAILED";
  wrapped.transactionId = transactionId;
  wrapped.completedSteps = completedSteps;
  wrapped.failedStep = failedStep;
  wrapped.partialSteps = partialSteps;
  return wrapped;
}

function createBrowserTransactionRuntime(env = {}) {
  const document = env.document;
  if (!document) {
    throw transactionError("INVALID_ENVIRONMENT", "Browser transaction runtime requires a document adapter.");
  }

  const window = env.window || document.defaultView;
  if (!window) {
    throw transactionError("INVALID_ENVIRONMENT", "Browser transaction runtime requires a window adapter.");
  }

  const getUrl = typeof env.getUrl === "function" ? env.getUrl : () => String(window.location?.href || "");
  const getDocumentIdentity = typeof env.getDocumentIdentity === "function"
    ? env.getDocumentIdentity
    : () => String(document.URL || window.location?.href || "document");
  const now = typeof env.now === "function" ? env.now : Date.now;
  const sleep = typeof env.sleep === "function"
    ? env.sleep
    : (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const uploadFile = typeof env.uploadFile === "function" ? env.uploadFile : null;
  const activeTransactions = new Map();

  function cancellationCheck(token, startedAt, transactionId, completedSteps, failedStep, partialSteps) {
    if (token.cancelled) {
      throw attachExecutionContext(
        transactionError("TRANSACTION_CANCELLED", "Browser transaction was cancelled."),
        transactionId,
        completedSteps,
        failedStep,
        partialSteps,
      );
    }
    if (Number(now()) - startedAt > TRANSACTION_MAX_RUNTIME_MS) {
      throw attachExecutionContext(
        transactionError("TRANSACTION_TIMEOUT", "Browser transaction exceeded 25000 ms."),
        transactionId,
        completedSteps,
        failedStep,
        partialSteps,
      );
    }
  }

  async function waitUntilCondition(condition, timeoutMs, aliases, initialDocumentIdentity, token, context) {
    const waitStarted = Number(now());
    while (true) {
      cancellationCheck(token, context.startedAt, context.transactionId, context.completedSteps, context.failedStep, context.partialSteps);
      if (evaluateCondition(document, condition, aliases, getUrl, getDocumentIdentity, initialDocumentIdentity)) {
        return;
      }
      const elapsed = Number(now()) - waitStarted;
      if (elapsed >= timeoutMs) {
        throw transactionError("CONDITION_TIMEOUT", `Browser transaction condition '${condition.condition}' did not become true within ${timeoutMs} ms.`);
      }
      await sleep(Math.min(50, Math.max(1, timeoutMs - elapsed)));
    }
  }

  async function execute(plan) {
    validateTransactionPlan(plan);
    const transactionId = String(plan.transactionId);
    if (activeTransactions.has(transactionId)) {
      throw transactionError("TRANSACTION_ACTIVE", "A browser transaction with this ID is already active.");
    }

    const token = { cancelled: false };
    activeTransactions.set(transactionId, token);
    const startedAt = Number(now());
    const initialDocumentIdentity = String(getDocumentIdentity() || "");
    const initialUrl = String(getUrl() || "");
    const aliases = new Map();
    const receipts = [];
    let completedSteps = 0;

    try {
      for (let index = 0; index < plan.steps.length; index += 1) {
        const step = plan.steps[index];
        const stepNumber = index + 1;
        const operation = String(step.operation || "");
        const stepStarted = Number(now());
        cancellationCheck(token, startedAt, transactionId, completedSteps, stepNumber, receipts);

        try {
          let result;
          if (operation === "find") {
            const element = resolveElement(document, step.locator);
            aliases.set(step.asName, {
              locator: { ...step.locator },
              documentIdentity: String(getDocumentIdentity() || ""),
            });
            result = safeOperationResult("find", element, { alias: step.asName });
          } else if (operation === "fill") {
            const prepared = preflightTextMutation(
              document,
              step.target,
              aliases,
              getDocumentIdentity,
              step.text,
              step.clearFirst !== false,
            );
            result = applyFormAssignment(window, prepared);
            result.target = step.target.kind === "alias" ? step.target.value : null;
          } else if (operation === "selectOption") {
            const prepared = preflightSelectMutation(document, step, aliases, getDocumentIdentity);
            result = applyFormAssignment(window, prepared);
            result.target = step.target.kind === "alias" ? step.target.value : null;
          } else if (operation === "setChecked") {
            const prepared = preflightCheckedMutation(document, step, aliases, getDocumentIdentity);
            result = applyFormAssignment(window, prepared);
            result.target = step.target.kind === "alias" ? step.target.value : null;
          } else if (operation === "click") {
            const element = preflightClickMutation(document, step.target, aliases, getDocumentIdentity);
            element.click();
            result = safeOperationResult("click", element);
          } else if (operation === "uploadFile") {
            const element = resolveTransactionTarget(document, step.target, aliases, getDocumentIdentity);
            const tag = String(element.tagName || "").toLowerCase();
            const type = normalizeText(element.getAttribute?.("type")).toLowerCase();
            if (tag !== "input" || type !== "file") {
              throw transactionError("UNSUPPORTED_CONTROL", "File upload requires a native input[type=file].");
            }
            if (!uploadFile) {
              throw transactionError("UNSUPPORTED_OPERATION", "Browser transaction upload adapter is unavailable.");
            }
            await uploadFile({ element, workspaceRelativePath: step.workspaceRelativePath });
            result = safeOperationResult("uploadFile", element);
          } else if (operation === "waitUntil") {
            const context = {
              startedAt,
              transactionId,
              completedSteps,
              failedStep: stepNumber,
              partialSteps: receipts,
            };
            await waitUntilCondition(step.condition, Number(step.timeoutMs), aliases, initialDocumentIdentity, token, context);
            cancellationCheck(token, startedAt, transactionId, completedSteps, stepNumber, receipts);
            result = { operation: "waitUntil", condition: step.condition.condition, verified: true, timeoutMs: Number(step.timeoutMs) };
          } else if (operation === "verify") {
            const verified = evaluateCondition(document, step.condition, aliases, getUrl, getDocumentIdentity, initialDocumentIdentity);
            if (!verified) {
              throw transactionError("POSTCONDITION_FAILED", `Browser transaction condition '${step.condition.condition}' is false.`);
            }
            result = { operation: "verify", condition: step.condition.condition, verified: true };
          } else if (operation === "fillForm") {
            // Resolve and validate every requested field before mutating the first field.
            const prepared = step.fields.map((field) => preflightTransactionFormAssignment(document, field, aliases, getDocumentIdentity));
            const fields = prepared.map((entry) => ({
              name: entry.field.name || null,
              ...applyFormAssignment(window, entry),
            }));
            result = { operation: "fillForm", verified: fields.every((field) => field.verified), fields };
          } else {
            throw transactionError("INVALID_ARGUMENT", `Unsupported browser transaction operation: ${operation || "<empty>"}.`);
          }

          cancellationCheck(token, startedAt, transactionId, completedSteps, stepNumber, receipts);
          const elapsedMs = Math.max(0, Number(now()) - stepStarted);
          receipts.push({ step: stepNumber, operation, elapsedMs, result });
          completedSteps += 1;
        } catch (error) {
          if (error?.transactionId === transactionId) throw error;
          throw attachExecutionContext(error, transactionId, completedSteps, stepNumber, receipts);
        }
      }

      const finalDocumentIdentity = String(getDocumentIdentity() || "");
      const finalUrl = String(getUrl() || "");
      return {
        ok: true,
        transactionId,
        completedSteps,
        totalSteps: plan.steps.length,
        steps: receipts,
        finalUrl,
        finalDocumentIdentity,
        documentChanged: finalDocumentIdentity !== initialDocumentIdentity || finalUrl !== initialUrl,
        timings: {
          repoTunnelInternalMs: Math.max(0, Number(now()) - startedAt),
          externalMcpRoundTripIncluded: false,
        },
        fallback: null,
      };
    } finally {
      if (activeTransactions.get(transactionId) === token) activeTransactions.delete(transactionId);
    }
  }

  function cancel(transactionId) {
    const id = String(transactionId || "");
    if (!TRANSACTION_ID_RE.test(id)) {
      throw transactionError("INVALID_ARGUMENT", "Browser transaction ID is invalid.");
    }
    const token = activeTransactions.get(id);
    if (!token) return { transactionId: id, cancelled: false, active: false };
    token.cancelled = true;
    return { transactionId: id, cancelled: true, active: true };
  }

  return {
    async resolve(locator) {
      return safeElementSummary(resolveElement(document, locator));
    },
    async fillForm(assignments) {
      return fillForm(document, window, assignments);
    },
    execute,
    cancel,
  };
}

function browserTransactionPageCommand(operation, payload = {}) {
  const normalize = (value) => String(value ?? "").replace(/\s+/g, " ").trim();
  const privateHint = (value) => {
    const text = String(value || "").toLowerCase();
    return [
      "password", "passwd", "passcode", "passphrase", "pin", "secret", "credential", "token",
      "api key", "api-key", "apikey", "access key", "private key", "verification code",
      "one-time", "one time", "otp", "2fa", "mfa",
    ].some((hint) => text.includes(hint));
  };
  const failResult = (code, message, detail) => ({ ok: false, code, message, ...(detail === undefined ? {} : { detail }) });
  const candidateRole = (element) => {
    const explicit = normalize(element?.getAttribute?.("role")).toLowerCase();
    if (explicit) return explicit;
    const tag = String(element?.tagName || "").toLowerCase();
    const type = normalize(element?.getAttribute?.("type")).toLowerCase();
    if (tag === "textarea") return "textbox";
    if (tag === "select") return "combobox";
    if (tag === "button") return "button";
    if (tag === "a" && element?.hasAttribute?.("href")) return "link";
    if (tag === "input") {
      if (type === "checkbox") return "checkbox";
      if (type === "radio") return "radio";
      if (["button", "submit", "reset"].includes(type)) return "button";
      return "textbox";
    }
    if (element?.isContentEditable) return "textbox";
    return "";
  };
  const isSensitive = (element) => {
    const type = normalize(element?.getAttribute?.("type")).toLowerCase();
    const autocomplete = normalize(element?.getAttribute?.("autocomplete")).toLowerCase();
    if (type === "password" || autocomplete.includes("password") || autocomplete.includes("one-time-code")) return true;
    const referencedText = (attribute) => normalize(element?.getAttribute?.(attribute))
      .split(/\s+/)
      .filter(Boolean)
      .map((id) => normalize(document.getElementById(id)?.textContent))
      .filter(Boolean);
    return [
      candidateRole(element), element?.getAttribute?.("name"), element?.getAttribute?.("id"),
      element?.getAttribute?.("aria-label"), element?.getAttribute?.("placeholder"),
      element?.getAttribute?.("autocomplete"),
      ...Array.from(element?.labels || []).map((label) => normalize(label.textContent)),
      ...referencedText("aria-labelledby"), ...referencedText("aria-describedby"),
    ].some(privateHint);
  };
  const unique = (elements) => Array.from(new Set(elements));
  const labelMatches = (requested) => {
    const wanted = normalize(requested);
    const matches = [];
    for (const label of Array.from(document.querySelectorAll("label"))) {
      const clone = label.cloneNode(true);
      for (const child of Array.from(clone.querySelectorAll("input,textarea,select,button"))) child.remove();
      if (normalize(clone.textContent) !== wanted) continue;
      const forId = label.getAttribute("for");
      if (forId) {
        const target = document.getElementById(forId);
        if (target) matches.push(target);
      } else {
        const nested = label.querySelector("input,textarea,select,button,[contenteditable='true']");
        if (nested) matches.push(nested);
      }
    }
    return matches;
  };
  const semanticParts = (element) => {
    const referenced = (attribute) => normalize(element?.getAttribute?.(attribute))
      .split(/\s+/)
      .filter(Boolean)
      .map((id) => normalize(document.getElementById(id)?.textContent));
    return unique([
      element?.getAttribute?.("aria-label"), element?.getAttribute?.("placeholder"),
      element?.getAttribute?.("title"), element?.getAttribute?.("alt"),
      element?.getAttribute?.("name"), element?.getAttribute?.("id"),
      ...Array.from(element?.labels || []).map((label) => label.textContent),
      ...referenced("aria-labelledby"), normalize(element?.textContent),
    ].map(normalize).filter(Boolean));
  };
  const semanticUniverse = () => Array.from(document.querySelectorAll(
    "input,textarea,select,button,a[href],[role],[contenteditable='true']",
  ));
  const resolve = (locator) => {
    if (!locator || typeof locator !== "object") throw Object.assign(new Error("Locator required."), { code: "INVALID_ARGUMENT" });
    let candidates = null;
    const intersect = (next) => {
      const values = unique(next);
      if (candidates === null) candidates = values;
      else {
        const allowed = new Set(values);
        candidates = candidates.filter((element) => allowed.has(element));
      }
    };
    if (locator.css) {
      let matches;
      try { matches = Array.from(document.querySelectorAll(String(locator.css))); }
      catch { throw Object.assign(new Error("Invalid CSS selector."), { code: "INVALID_LOCATOR" }); }
      intersect(matches);
    }
    if (locator.id) {
      const element = document.getElementById(String(locator.id));
      intersect(element ? [element] : []);
    }
    if (locator.name) intersect(Array.from(document.querySelectorAll("[name]")).filter((element) => element.getAttribute("name") === String(locator.name)));
    if (locator.label) intersect(labelMatches(locator.label));
    if (locator.ariaLabel) intersect(Array.from(document.querySelectorAll("[aria-label]")).filter((element) => element.getAttribute("aria-label") === String(locator.ariaLabel)));
    if (locator.placeholder) intersect(Array.from(document.querySelectorAll("[placeholder]")).filter((element) => element.getAttribute("placeholder") === String(locator.placeholder)));
    if (candidates === null && (locator.role || locator.query)) candidates = semanticUniverse();
    if (candidates === null) throw Object.assign(new Error("DOM identity is insufficient."), { code: "FALLBACK_REQUIRED" });
    if (locator.role) {
      const role = normalize(locator.role).toLowerCase();
      candidates = candidates.filter((element) => candidateRole(element) === role);
    }
    if (locator.query) {
      const query = normalize(locator.query).toLowerCase();
      const withParts = candidates.map((element) => ({ element, parts: semanticParts(element).map((part) => part.toLowerCase()) }));
      const exact = withParts.filter(({ parts }) => parts.some((part) => part === query));
      const matched = exact.length > 0 ? exact : withParts.filter(({ parts }) => parts.some((part) => part.includes(query)));
      candidates = matched.map(({ element }) => element);
    }
    candidates = unique(candidates);
    if (candidates.length === 0) throw Object.assign(new Error("No DOM target matched."), { code: "FALLBACK_REQUIRED" });
    if (candidates.length > 1) throw Object.assign(new Error("DOM target is ambiguous."), { code: "AMBIGUOUS_TARGET", detail: { count: candidates.length } });
    if (isSensitive(candidates[0])) throw Object.assign(new Error("Sensitive credential field blocked."), { code: "SENSITIVE_FIELD" });
    return candidates[0];
  };
  const summary = (element) => ({
    tag: String(element?.tagName || "").toLowerCase(),
    id: element?.id || null,
    name: element?.getAttribute?.("name") || null,
    role: candidateRole(element) || null,
  });
  const disabled = (element) => element?.disabled === true || element?.hasAttribute?.("disabled") || normalize(element?.getAttribute?.("aria-disabled")).toLowerCase() === "true";
  const readonly = (element) => element?.readOnly === true || element?.hasAttribute?.("readonly") || normalize(element?.getAttribute?.("aria-readonly")).toLowerCase() === "true";
  const nativeSet = (element, property, value) => {
    const prototypes = [];
    if (element instanceof HTMLInputElement) prototypes.push(HTMLInputElement.prototype);
    if (element instanceof HTMLTextAreaElement) prototypes.push(HTMLTextAreaElement.prototype);
    if (element instanceof HTMLSelectElement) prototypes.push(HTMLSelectElement.prototype);
    for (const prototype of prototypes) {
      const descriptor = Object.getOwnPropertyDescriptor(prototype, property);
      if (descriptor?.set) { descriptor.set.call(element, value); return; }
    }
    element[property] = value;
  };
  const events = (element) => {
    element.dispatchEvent(new Event("input", { bubbles: true }));
    element.dispatchEvent(new Event("change", { bubbles: true }));
  };
  const preflightText = (element, text, clearFirst = true) => {
    if (disabled(element) || readonly(element)) throw Object.assign(new Error("Target is not actionable."), { code: "NOT_ACTIONABLE" });
    const tag = String(element.tagName || "").toLowerCase();
    const type = normalize(element.getAttribute?.("type") || "text").toLowerCase();
    const editableInput = tag === "input" && !["button", "submit", "reset", "checkbox", "radio", "file", "hidden", "image"].includes(type);
    const contentEditable = element.isContentEditable || normalize(element.getAttribute?.("contenteditable")).toLowerCase() === "true";
    if (!editableInput && tag !== "textarea" && !contentEditable) throw Object.assign(new Error("Native text editing is unavailable."), { code: "SEMANTIC_FALLBACK_REQUIRED" });
    if (contentEditable && element.children.length > 0) throw Object.assign(new Error("Rich editor needs semantic fallback."), { code: "SEMANTIC_FALLBACK_REQUIRED" });
    const current = contentEditable ? String(element.textContent || "") : String(element.value ?? "");
    const incoming = String(text ?? "");
    return { element, kind: "text", desired: clearFirst ? incoming : current + incoming };
  };
  const preflightSelect = (element, value, label) => {
    if (disabled(element)) throw Object.assign(new Error("Target is disabled."), { code: "NOT_ACTIONABLE" });
    if (String(element.tagName || "").toLowerCase() !== "select") throw Object.assign(new Error("Custom combobox needs semantic fallback."), { code: "SEMANTIC_FALLBACK_REQUIRED" });
    const byValue = typeof value === "string";
    const wanted = byValue ? value : label;
    const option = byValue
      ? Array.from(element.options).find((candidate) => candidate.value === wanted)
      : Array.from(element.options).find((candidate) => normalize(candidate.textContent) === normalize(wanted));
    if (!option) throw Object.assign(new Error("Option not found."), { code: "OPTION_NOT_FOUND" });
    if (option.disabled) throw Object.assign(new Error("Option is disabled."), { code: "NOT_ACTIONABLE" });
    return { element, kind: byValue ? "selectValue" : "selectLabel", desired: option.value };
  };
  const preflightChecked = (element, checked) => {
    if (disabled(element)) throw Object.assign(new Error("Target is disabled."), { code: "NOT_ACTIONABLE" });
    const tag = String(element.tagName || "").toLowerCase();
    const type = normalize(element.getAttribute?.("type")).toLowerCase();
    if (tag !== "input" || !["checkbox", "radio"].includes(type)) throw Object.assign(new Error("Custom checked control needs semantic fallback."), { code: "SEMANTIC_FALLBACK_REQUIRED" });
    return { element, kind: "checked", desired: checked === true };
  };
  const apply = (prepared) => {
    const { element, kind, desired } = prepared;
    if (kind === "text") {
      const contentEditable = element.isContentEditable || normalize(element.getAttribute?.("contenteditable")).toLowerCase() === "true";
      if (contentEditable) element.textContent = desired;
      else nativeSet(element, "value", desired);
      element.focus?.();
      events(element);
      const actual = contentEditable ? String(element.textContent || "") : String(element.value ?? "");
      if (actual !== desired) throw Object.assign(new Error("Text postcondition failed."), { code: "POSTCONDITION_FAILED" });
      return { verified: true, operation: "text", characterCount: desired.length };
    }
    if (kind === "selectValue" || kind === "selectLabel") {
      nativeSet(element, "value", desired);
      events(element);
      if (String(element.value) !== desired) throw Object.assign(new Error("Select postcondition failed."), { code: "POSTCONDITION_FAILED" });
      return { verified: true, operation: kind };
    }
    if (Boolean(element.checked) !== desired) {
      nativeSet(element, "checked", desired);
      element.dispatchEvent(new Event("click", { bubbles: true }));
      events(element);
    }
    if (Boolean(element.checked) !== desired) throw Object.assign(new Error("Checked postcondition failed."), { code: "POSTCONDITION_FAILED" });
    return { verified: true, operation: "checked", checked: desired };
  };
  const safeMissing = (targetOrLocator) => {
    const locator = targetOrLocator?.kind === "locator" ? targetOrLocator.value : targetOrLocator;
    try { return { found: true, element: resolve(locator) }; }
    catch (error) {
      if (error?.code === "FALLBACK_REQUIRED" && locator && (locator.css || locator.id || locator.name || locator.label || locator.ariaLabel || locator.placeholder || locator.role || locator.query)) return { found: false, element: null };
      throw error;
    }
  };

  try {
    if (operation === "resolve") return { ok: true, ...summary(resolve(payload.locator)) };
    if (operation === "fill") return {
      ok: true,
      ...apply(preflightText(resolve(payload.locator), payload.text, payload.clearFirst !== false)),
    };
    if (operation === "selectOption") return { ok: true, ...apply(preflightSelect(resolve(payload.locator), payload.value, payload.label)) };
    if (operation === "setChecked") return { ok: true, ...apply(preflightChecked(resolve(payload.locator), payload.checked)) };
    if (operation === "click") {
      const element = resolve(payload.locator);
      if (disabled(element)) throw Object.assign(new Error("Target is disabled."), { code: "NOT_ACTIONABLE" });
      const tag = String(element.tagName || "").toLowerCase();
      const type = normalize(element.getAttribute?.("type")).toLowerCase();
      const role = candidateRole(element);
      const nativeClickable = tag === "button" || (tag === "a" && element.hasAttribute?.("href")) || (tag === "input" && ["button", "submit", "reset", "checkbox", "radio"].includes(type));
      if (!nativeClickable && ["button", "link", "checkbox", "radio", "switch", "tab", "menuitem", "option"].includes(role)) throw Object.assign(new Error("Custom click needs semantic fallback."), { code: "SEMANTIC_FALLBACK_REQUIRED" });
      if (typeof element.click !== "function") throw Object.assign(new Error("Click needs semantic fallback."), { code: "SEMANTIC_FALLBACK_REQUIRED" });
      element.click();
      return { ok: true, verified: true, operation: "click", ...summary(element) };
    }
    if (operation === "markSemanticClick") {
      const element = resolve(payload.locator);
      if (disabled(element)) throw Object.assign(new Error("Target is disabled."), { code: "NOT_ACTIONABLE" });
      const marker = String(payload.marker || "");
      if (!/^[A-Za-z0-9_-]{1,128}$/.test(marker)) throw Object.assign(new Error("Semantic click marker is invalid."), { code: "INVALID_ARGUMENT" });
      element.setAttribute("data-repotunnel-semantic-click-marker", marker);
      return { ok: true, verified: true, operation: "markSemanticClick", ...summary(element) };
    }
    if (operation === "markUpload") {
      const element = resolve(payload.locator);
      const tag = String(element.tagName || "").toLowerCase();
      const type = normalize(element.getAttribute?.("type")).toLowerCase();
      if (tag !== "input" || type !== "file") throw Object.assign(new Error("Upload target must be input[type=file]."), { code: "UNSUPPORTED_CONTROL" });
      const marker = String(payload.marker || "");
      if (!/^[A-Za-z0-9_-]{1,128}$/.test(marker)) throw Object.assign(new Error("Upload marker is invalid."), { code: "INVALID_ARGUMENT" });
      element.setAttribute("data-repotunnel-upload-marker", marker);
      return { ok: true, verified: true, operation: "markUpload" };
    }
    if (operation === "fillForm") {
      if (!Array.isArray(payload.fields) || payload.fields.length < 1 || payload.fields.length > 64) throw Object.assign(new Error("fillForm requires 1..64 fields."), { code: "INVALID_ARGUMENT" });
      const prepareField = (field) => {
        if (!field?.target || field.target.kind !== "locator") throw Object.assign(new Error("fillForm requires locator targets."), { code: "INVALID_ARGUMENT" });
        const element = resolve(field.target.value);
        const kind = String(field.value?.kind || "");
        let prepared;
        if (kind === "text") prepared = preflightText(element, field.value.value);
        else if (kind === "selectValue") prepared = preflightSelect(element, String(field.value.value ?? ""), null);
        else if (kind === "selectLabel") prepared = preflightSelect(element, null, String(field.value.value ?? ""));
        else if (kind === "checked") prepared = preflightChecked(element, field.value.value === true);
        else throw Object.assign(new Error("Unsupported fillForm value."), { code: "INVALID_ARGUMENT" });
        return { name: field.name || null, field, prepared };
      };
      const verifyLive = (entry) => {
        const element = resolve(entry.field.target.value);
        const { kind, desired } = entry.prepared;
        const contentEditable = element.isContentEditable || normalize(element.getAttribute?.("contenteditable")).toLowerCase() === "true";
        const matches = kind === "text"
          ? (contentEditable ? String(element.textContent || "") : String(element.value ?? "")) === desired
          : (kind === "selectValue" || kind === "selectLabel")
            ? String(element.value ?? "") === desired
            : Boolean(element.checked) === desired;
        if (!matches) throw Object.assign(new Error("Live form postcondition failed."), { code: "POSTCONDITION_FAILED" });
      };
      payload.fields.map(prepareField); // Static all-target preflight before the first mutation.
      const applied = [];
      const fields = payload.fields.map((field) => {
        const entry = prepareField(field); // Re-resolve after earlier field events may have replaced the DOM node.
        const result = apply(entry.prepared);
        verifyLive(entry);
        applied.push(entry);
        return { name: entry.name, ...result };
      });
      for (const entry of applied) verifyLive(entry); // Later field events may replace earlier controls.
      return { ok: true, verified: true, fields };
    }
    if (operation === "condition") {
      const condition = payload.condition || {};
      const kind = String(condition.condition || "");
      const value = condition.value;
      if (kind === "urlEquals") return { ok: true, verified: location.href === String(value || "") };
      if (kind === "urlContains") return { ok: true, verified: location.href.includes(String(value || "")) };
      if (["elementExists", "elementMissing", "elementEnabled"].includes(kind)) {
        const resolved = safeMissing(value);
        const verified = kind === "elementExists" ? resolved.found : kind === "elementMissing" ? !resolved.found : resolved.found && !disabled(resolved.element);
        return { ok: true, verified };
      }
      const resolved = safeMissing(value?.target);
      if (!resolved.found) return { ok: true, verified: false };
      const element = resolved.element;
      if (kind === "valueEquals") return { ok: true, verified: String(element.value ?? "") === String(value.value ?? "") };
      if (kind === "valueContains") return { ok: true, verified: String(element.value ?? "").includes(String(value.value ?? "")) };
      if (kind === "checkedEquals") return { ok: true, verified: Boolean(element.checked) === Boolean(value.checked) };
      if (kind === "selectedValueEquals") return { ok: true, verified: String(element.value ?? "") === String(value.value ?? "") };
      if (kind === "selectedLabelEquals") return { ok: true, verified: normalize(element.selectedOptions?.[0]?.textContent || "") === normalize(value.label) };
      if (kind === "textContains") return { ok: true, verified: normalize(element.textContent).includes(normalize(value.text)) };
      throw Object.assign(new Error("Unsupported condition."), { code: "INVALID_ARGUMENT" });
    }
    throw Object.assign(new Error("Unsupported page command."), { code: "INVALID_ARGUMENT" });
  } catch (error) {
    return failResult(error?.code || "PAGE_COMMAND_FAILED", error instanceof Error ? error.message : String(error), error?.detail);
  }
}

function browserTransactionPageExpression(operation, payload = {}) {
  return `(${browserTransactionPageCommand.toString()})(${JSON.stringify(operation)}, ${JSON.stringify(payload)})`;
}

function createRemoteBrowserTransactionRuntime(env = {}) {
  if (typeof env.pageCommand !== "function" || typeof env.getState !== "function") {
    throw transactionError("INVALID_ENVIRONMENT", "Remote browser transaction runtime requires pageCommand and getState adapters.");
  }
  const sleep = typeof env.sleep === "function" ? env.sleep : (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const now = typeof env.now === "function" ? env.now : Date.now;
  const uploadFile = typeof env.uploadFile === "function" ? env.uploadFile : null;
  const semanticClick = typeof env.semanticClick === "function" ? env.semanticClick : null;
  const activeTransactions = new Map();

  const page = async (tabId, operation, payload) => {
    const result = await env.pageCommand(tabId, operation, payload);
    if (!result?.ok) throw transactionError(result?.code || "PAGE_COMMAND_FAILED", result?.message || "Browser page command failed.", result?.detail);
    return result;
  };
  const state = async (tabId) => {
    const result = await env.getState(tabId);
    return {
      url: String(result?.url || ""),
      documentIdentity: String(result?.documentIdentity || result?.documentGeneration || ""),
    };
  };
  const resolveLocator = async (tabId, target, aliases) => {
    if (target.kind === "locator") return target.value;
    const alias = aliases.get(String(target.value || ""));
    if (!alias) throw transactionError("INVALID_ARGUMENT", `Browser transaction alias '${String(target.value || "")}' is unavailable.`);
    const current = await state(tabId);
    if (current.documentIdentity !== alias.documentIdentity || current.url !== alias.url) {
      throw transactionError("STALE_DOCUMENT", "Browser document or URL changed after this alias was found.");
    }
    return alias.locator;
  };
  const transformCondition = async (tabId, condition, aliases) => {
    const kind = String(condition.condition || "");
    const value = condition.value;
    if (["urlEquals", "urlContains", "documentChanged"].includes(kind)) return condition;
    if (["elementExists", "elementMissing", "elementEnabled"].includes(kind)) {
      return { condition: kind, value: { kind: "locator", value: await resolveLocator(tabId, value, aliases) } };
    }
    const locator = await resolveLocator(tabId, value.target, aliases);
    return { condition: kind, value: { ...value, target: { kind: "locator", value: locator } } };
  };
  const evaluateCondition = async (tabId, condition, aliases, initialState) => {
    if (condition.condition === "documentChanged") {
      const current = await state(tabId);
      return current.documentIdentity !== initialState.documentIdentity || current.url !== initialState.url;
    }
    if (condition.condition === "urlEquals" || condition.condition === "urlContains") {
      const current = await state(tabId);
      return condition.condition === "urlEquals"
        ? current.url === String(condition.value || "")
        : current.url.includes(String(condition.value || ""));
    }
    const transformed = await transformCondition(tabId, condition, aliases);
    const result = await page(tabId, "condition", { condition: transformed });
    return result.verified === true;
  };

  async function execute(plan) {
    validateTransactionPlan(plan);
    const transactionId = String(plan.transactionId);
    const tabId = String(plan.tabId || "");
    if (!tabId) throw transactionError("INVALID_ARGUMENT", "Remote browser transaction requires a tab ID.");
    if (activeTransactions.has(transactionId)) throw transactionError("TRANSACTION_ACTIVE", "A browser transaction with this ID is already active.");
    const token = { cancelled: false };
    activeTransactions.set(transactionId, token);
    const startedAt = Number(now());
    const initialState = await state(tabId);
    const aliases = new Map();
    const receipts = [];
    let completedSteps = 0;
    const guard = (failedStep) => {
      if (token.cancelled) throw attachExecutionContext(transactionError("TRANSACTION_CANCELLED", "Browser transaction was cancelled."), transactionId, completedSteps, failedStep, receipts);
      if (Number(now()) - startedAt > TRANSACTION_MAX_RUNTIME_MS) throw attachExecutionContext(transactionError("TRANSACTION_TIMEOUT", "Browser transaction exceeded 25000 ms."), transactionId, completedSteps, failedStep, receipts);
    };

    try {
      for (let index = 0; index < plan.steps.length; index += 1) {
        const step = plan.steps[index];
        const stepNumber = index + 1;
        const operation = String(step.operation || "");
        const stepStarted = Number(now());
        guard(stepNumber);
        try {
          let result;
          if (operation === "find") {
            result = await page(tabId, "resolve", { locator: step.locator });
            const current = await state(tabId);
            aliases.set(step.asName, {
              locator: { ...step.locator },
              documentIdentity: current.documentIdentity,
              url: current.url,
            });
            result = { operation: "find", verified: true, alias: step.asName, tag: result.tag, id: result.id, name: result.name, role: result.role };
          } else if (operation === "fill") {
            const locator = await resolveLocator(tabId, step.target, aliases);
            result = await page(tabId, "fill", { locator, text: step.text, clearFirst: step.clearFirst !== false });
          } else if (operation === "selectOption") {
            const locator = await resolveLocator(tabId, step.target, aliases);
            result = await page(tabId, "selectOption", { locator, value: step.value ?? null, label: step.label ?? null });
          } else if (operation === "setChecked") {
            const locator = await resolveLocator(tabId, step.target, aliases);
            result = await page(tabId, "setChecked", { locator, checked: step.checked === true });
          } else if (operation === "click") {
            const locator = await resolveLocator(tabId, step.target, aliases);
            try {
              result = await page(tabId, "click", { locator });
            } catch (error) {
              if (error?.code !== "SEMANTIC_FALLBACK_REQUIRED" || !semanticClick) throw error;
              result = await semanticClick({ tabId, locator });
            }
          } else if (operation === "uploadFile") {
            if (!uploadFile) throw transactionError("UNSUPPORTED_OPERATION", "Remote browser upload adapter is unavailable.");
            const locator = await resolveLocator(tabId, step.target, aliases);
            result = await uploadFile({ tabId, locator, workspaceRelativePath: step.workspaceRelativePath });
          } else if (operation === "fillForm") {
            const fields = [];
            for (const field of step.fields) {
              fields.push({ ...field, target: { kind: "locator", value: await resolveLocator(tabId, field.target, aliases) } });
            }
            result = await page(tabId, "fillForm", { fields });
          } else if (operation === "verify") {
            const verified = await evaluateCondition(tabId, step.condition, aliases, initialState);
            if (!verified) throw transactionError("POSTCONDITION_FAILED", `Browser transaction condition '${step.condition.condition}' is false.`);
            result = { operation: "verify", condition: step.condition.condition, verified: true };
          } else if (operation === "waitUntil") {
            const timeoutMs = Number(step.timeoutMs);
            const waitStarted = Number(now());
            while (true) {
              guard(stepNumber);
              if (await evaluateCondition(tabId, step.condition, aliases, initialState)) break;
              const elapsed = Number(now()) - waitStarted;
              if (elapsed >= timeoutMs) throw transactionError("CONDITION_TIMEOUT", `Browser transaction condition '${step.condition.condition}' did not become true within ${timeoutMs} ms.`);
              await sleep(Math.min(50, Math.max(1, timeoutMs - elapsed)));
            }
            guard(stepNumber);
            result = { operation: "waitUntil", condition: step.condition.condition, verified: true, timeoutMs };
          } else {
            throw transactionError("INVALID_ARGUMENT", `Unsupported browser transaction operation: ${operation || "<empty>"}.`);
          }
          guard(stepNumber);
          receipts.push({ step: stepNumber, operation, elapsedMs: Math.max(0, Number(now()) - stepStarted), result });
          completedSteps += 1;
        } catch (error) {
          if (error?.transactionId === transactionId) throw error;
          throw attachExecutionContext(error, transactionId, completedSteps, stepNumber, receipts);
        }
      }
      const finalState = await state(tabId);
      return {
        ok: true,
        transactionId,
        completedSteps,
        totalSteps: plan.steps.length,
        steps: receipts,
        finalUrl: finalState.url,
        finalDocumentIdentity: finalState.documentIdentity,
        documentChanged: finalState.documentIdentity !== initialState.documentIdentity || finalState.url !== initialState.url,
        timings: { repoTunnelInternalMs: Math.max(0, Number(now()) - startedAt), externalMcpRoundTripIncluded: false },
        fallback: null,
      };
    } finally {
      if (activeTransactions.get(transactionId) === token) activeTransactions.delete(transactionId);
    }
  }

  function cancel(transactionId) {
    const id = String(transactionId || "");
    if (!TRANSACTION_ID_RE.test(id)) throw transactionError("INVALID_ARGUMENT", "Browser transaction ID is invalid.");
    const token = activeTransactions.get(id);
    if (!token) return { transactionId: id, cancelled: false, active: false };
    token.cancelled = true;
    return { transactionId: id, cancelled: true, active: true };
  }

  return { execute, cancel };
}

module.exports = {
  createBrowserTransactionRuntime,
  createRemoteBrowserTransactionRuntime,
  browserTransactionPageExpression,
  privateFieldHint,
  formatTransactionError,
};
