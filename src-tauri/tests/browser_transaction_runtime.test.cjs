"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { JSDOM } = require("jsdom");
const {
  createBrowserTransactionRuntime,
  createRemoteBrowserTransactionRuntime,
  browserTransactionPageExpression,
  privateFieldHint,
  formatTransactionError,
} = require("../resources/browser_transaction_runtime.cjs");

function dom(html, url = "https://example.test/form") {
  const instance = new JSDOM(html, { url, pretendToBeVisual: true, runScripts: "outside-only" });
  return { instance, window: instance.window, document: instance.window.document };
}

function runtimeFor(html, url) {
  const page = dom(html, url);
  const runtime = createBrowserTransactionRuntime({
    window: page.window,
    document: page.document,
    getUrl: () => page.window.location.href,
    getDocumentIdentity: () => "doc-1",
    sleep: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  });
  return { ...page, runtime };
}

function locator(value) {
  return { ...value };
}

function target(locatorValue) {
  return { kind: "locator", value: locator(locatorValue) };
}

function textField(name, locatorValue, value) {
  return {
    name,
    target: target(locatorValue),
    value: { kind: "text", value },
  };
}

function selectValueField(name, locatorValue, value) {
  return {
    name,
    target: target(locatorValue),
    value: { kind: "selectValue", value },
  };
}

function selectLabelField(name, locatorValue, value) {
  return {
    name,
    target: target(locatorValue),
    value: { kind: "selectLabel", value },
  };
}

function checkedField(name, locatorValue, value) {
  return {
    name,
    target: target(locatorValue),
    value: { kind: "checked", value },
  };
}

async function expectCode(promise, code) {
  await assert.rejects(promise, (error) => {
    assert.equal(error.code, code);
    return true;
  });
}

test("resolves id name css label aria-label and placeholder deterministically", async () => {
  const { runtime } = runtimeFor(`
    <form>
      <label for="by-label">Display name</label>
      <input id="by-id" />
      <input name="by-name" />
      <input class="by-css" />
      <input id="by-label" />
      <input aria-label="Accessible field" />
      <input placeholder="Placeholder field" />
    </form>
  `);

  const cases = [
    [{ id: "by-id" }, "by-id"],
    [{ name: "by-name" }, "by-name"],
    [{ css: ".by-css" }, null],
    [{ label: "Display name" }, "by-label"],
    [{ ariaLabel: "Accessible field" }, null],
    [{ placeholder: "Placeholder field" }, null],
  ];

  for (const [query, expectedIdOrName] of cases) {
    const resolved = await runtime.resolve(query);
    assert.equal(resolved.ok, true);
    assert.equal(resolved.tag, "input");
    if (expectedIdOrName) {
      assert.equal(resolved.id || resolved.name, expectedIdOrName);
    }
  }
});

test("rejects ambiguous matching labels", async () => {
  const { runtime } = runtimeFor(`
    <label>Shared <input /></label>
    <label>Shared <input /></label>
  `);
  await expectCode(runtime.resolve({ label: "Shared" }), "AMBIGUOUS_TARGET");
});

test("resolves role and free-text query through accessible control semantics", async () => {
  const { runtime, document } = runtimeFor(`
    <label for="project-name">Project Name</label>
    <input id="project-name" />
    <button id="save">Save Project</button>
    <button id="cancel">Cancel</button>
  `);

  await runtime.execute(transactionPlan("semantic-query-fill", [
    { operation: "fill", target: target({ role: "textbox", query: "Project Name" }), text: "RepoTunnel", clearFirst: true },
    { operation: "click", target: target({ role: "button", query: "Save Project" }) },
  ]));

  assert.equal(document.getElementById("project-name").value, "RepoTunnel");
});

test("semantic query matching fails closed when more than one candidate matches", async () => {
  const { runtime } = runtimeFor(`
    <button>Save Draft</button>
    <button>Save Copy</button>
  `);
  await assert.rejects(
    runtime.execute(transactionPlan("semantic-query-ambiguous", [
      { operation: "click", target: target({ role: "button", query: "Save" }) },
    ])),
    (error) => error.code === "AMBIGUOUS_TARGET",
  );
});

test("returns fallback instead of guessing when DOM locator cannot resolve", async () => {
  const { runtime } = runtimeFor(`<div role="combobox" aria-label="Category"></div>`);
  await expectCode(
    runtime.resolve({ role: "combobox", query: "Missing category" }),
    "FALLBACK_REQUIRED",
  );
});

test("blocks password otp and api-key-like fields", async () => {
  assert.equal(privateFieldHint("API Key"), true);
  assert.equal(privateFieldHint("Normal description"), false);

  const { runtime } = runtimeFor(`
    <input id="password" type="password" />
    <input id="otp" autocomplete="one-time-code" />
    <input id="api" aria-label="API Key" />
  `);
  for (const id of ["password", "otp", "api"]) {
    await expectCode(runtime.resolve({ id }), "SENSITIVE_FIELD");
  }
});

test("fillForm fills multiple text controls and dispatches framework-observable events", async () => {
  const { runtime, document, window } = runtimeFor(`
    <form>
      <input name="toolName" value="Old" />
      <textarea name="description">Old description</textarea>
    </form>
  `);
  const counts = { input: 0, change: 0 };
  for (const element of [document.querySelector('[name="toolName"]'), document.querySelector('[name="description"]')]) {
    element.addEventListener("input", () => counts.input++);
    element.addEventListener("change", () => counts.change++);
  }

  const result = await runtime.fillForm([
    textField("name", { name: "toolName" }, "RepoTunnel"),
    textField("description", { name: "description" }, "Fast browser bridge"),
  ]);

  assert.equal(document.querySelector('[name="toolName"]').value, "RepoTunnel");
  assert.equal(document.querySelector('[name="description"]').value, "Fast browser bridge");
  assert.deepEqual(counts, { input: 2, change: 2 });
  assert.equal(result.ok, true);
  assert.equal(result.fields.length, 2);
  assert.equal(result.fields.every((field) => field.verified), true);
  assert.equal(result.fields[0].characterCount, 10);
  assert.equal(window.document.activeElement !== null, true);
});

test("fillForm selects native options by value and normalized visible label", async () => {
  const { runtime, document } = runtimeFor(`
    <select name="category">
      <option value="general">General</option>
      <option value="dev">Developer Tools</option>
    </select>
    <select name="pricing">
      <option value="paid">Paid</option>
      <option value="free">  Free   Forever  </option>
    </select>
  `);

  const result = await runtime.fillForm([
    selectValueField("category", { name: "category" }, "dev"),
    selectLabelField("pricing", { name: "pricing" }, "Free Forever"),
  ]);

  assert.equal(document.querySelector('[name="category"]').value, "dev");
  assert.equal(document.querySelector('[name="pricing"]').value, "free");
  assert.equal(result.fields.every((field) => field.verified), true);
});

test("fillForm sets checkbox and radio desired state", async () => {
  const { runtime, document } = runtimeFor(`
    <input type="checkbox" name="featured" />
    <input type="radio" name="plan" value="free" />
    <input type="radio" name="plan" value="paid" checked />
  `);

  await runtime.fillForm([
    checkedField("featured", { name: "featured" }, true),
    checkedField("free", { css: 'input[name="plan"][value="free"]' }, true),
  ]);

  assert.equal(document.querySelector('[name="featured"]').checked, true);
  assert.equal(document.querySelector('input[value="free"]').checked, true);
  assert.equal(document.querySelector('input[value="paid"]').checked, false);
});

test("fillForm rejects disabled and readonly controls before mutation", async () => {
  const { runtime, document } = runtimeFor(`
    <input name="first" value="original" />
    <input name="disabled" value="disabled-original" disabled />
    <input name="readonly" value="readonly-original" readonly />
  `);

  await expectCode(runtime.fillForm([
    textField("first", { name: "first" }, "changed"),
    textField("disabled", { name: "disabled" }, "changed-disabled"),
  ]), "NOT_ACTIONABLE");
  assert.equal(document.querySelector('[name="first"]').value, "original");

  await expectCode(runtime.fillForm([
    textField("first", { name: "first" }, "changed"),
    textField("readonly", { name: "readonly" }, "changed-readonly"),
  ]), "NOT_ACTIONABLE");
  assert.equal(document.querySelector('[name="first"]').value, "original");
});

test("fillForm preflights every field so one invalid target changes nothing", async () => {
  const { runtime, document } = runtimeFor(`
    <input name="one" value="one-original" />
    <input name="two" value="two-original" />
  `);

  await expectCode(runtime.fillForm([
    textField("one", { name: "one" }, "one-new"),
    textField("missing", { name: "missing" }, "missing-new"),
    textField("two", { name: "two" }, "two-new"),
  ]), "FALLBACK_REQUIRED");

  assert.equal(document.querySelector('[name="one"]').value, "one-original");
  assert.equal(document.querySelector('[name="two"]').value, "two-original");
});

test("fillForm never submits a form implicitly", async () => {
  const { runtime, document } = runtimeFor(`
    <form id="form">
      <input name="tool" value="old" />
      <button type="submit">Publish</button>
    </form>
  `);
  let submits = 0;
  document.querySelector("#form").addEventListener("submit", (event) => {
    submits++;
    event.preventDefault();
  });

  await runtime.fillForm([textField("tool", { name: "tool" }, "RepoTunnel")]);
  assert.equal(submits, 0);
});

test("fillForm result never echoes supplied text values", async () => {
  const secret = "private-description-value";
  const { runtime } = runtimeFor(`<input name="description" />`);
  const result = await runtime.fillForm([
    textField("description", { name: "description" }, secret),
  ]);
  const encoded = JSON.stringify(result);
  assert.equal(encoded.includes(secret), false);
  assert.equal(result.fields[0].characterCount, secret.length);
});

function aliasTarget(alias) {
  return { kind: "alias", value: alias };
}

function transactionPlan(id, steps) {
  return { transactionId: id, tabId: "tab-1", steps };
}

function condition(condition, value) {
  return value === undefined ? { condition } : { condition, value };
}

function runtimeWithMutableState(html, options = {}) {
  const page = dom(html, options.url || "https://example.test/form");
  let url = options.url || "https://example.test/form";
  let identity = options.identity || "doc-1";
  let clock = options.clock || 0;
  const runtime = createBrowserTransactionRuntime({
    window: page.window,
    document: page.document,
    getUrl: () => url,
    getDocumentIdentity: () => identity,
    now: () => clock,
    sleep: options.sleep || (async (ms) => {
      clock += ms;
      await new Promise((resolve) => setTimeout(resolve, 1));
    }),
  });
  return {
    ...page,
    runtime,
    setUrl(value) { url = value; },
    setIdentity(value) { identity = value; },
    setClock(value) { clock = value; },
    advanceClock(value) { clock += value; },
  };
}

test("transaction plan accepts empty values for clearing and empty native select options", async () => {
  const { runtime, document } = runtimeWithMutableState(`
    <input name="tool" value="old" />
    <select name="category"><option value="">Choose</option><option value="dev">Dev</option></select>
  `);

  const result = await runtime.execute(transactionPlan("empty-form-state", [
    { operation: "fill", target: target({ name: "tool" }), text: "", clearFirst: true },
    { operation: "selectOption", target: target({ name: "category" }), value: "", label: null },
    { operation: "verify", condition: condition("valueEquals", { target: target({ name: "tool" }), value: "" }) },
    { operation: "verify", condition: condition("selectedValueEquals", { target: target({ name: "category" }), value: "" }) },
  ]));

  assert.equal(result.completedSteps, 4);
  assert.equal(document.querySelector('[name="tool"]').value, "");
  assert.equal(document.querySelector('[name="category"]').value, "");
});

test("execute find alias then fill revalidates and verifies the target", async () => {
  const { runtime, document } = runtimeWithMutableState(`<input name="tool" value="old" />`);
  const result = await runtime.execute(transactionPlan("alias-fill", [
    { operation: "find", locator: { name: "tool" }, asName: "tool" },
    { operation: "fill", target: aliasTarget("tool"), text: "RepoTunnel", clearFirst: true },
    { operation: "verify", condition: condition("valueEquals", { target: aliasTarget("tool"), value: "RepoTunnel" }) },
  ]));
  assert.equal(result.ok, true);
  assert.equal(result.completedSteps, 3);
  assert.equal(document.querySelector('[name="tool"]').value, "RepoTunnel");
  assert.equal(JSON.stringify(result).includes("RepoTunnel"), false);
});

test("alias re-resolves after same-document framework replacement", async () => {
  const state = runtimeWithMutableState(`
    <input name="tool" value="old" />
    <button id="replace" type="button">Replace</button>
  `);
  state.document.querySelector("#replace").addEventListener("click", () => {
    const replacement = state.document.createElement("input");
    replacement.name = "tool";
    replacement.value = "replacement";
    state.document.querySelector('[name="tool"]').replaceWith(replacement);
  });

  const result = await state.runtime.execute(transactionPlan("replace-one", [
    { operation: "find", locator: { name: "tool" }, asName: "tool" },
    { operation: "click", target: target({ id: "replace" }) },
    { operation: "fill", target: aliasTarget("tool"), text: "after-replace", clearFirst: true },
  ]));

  assert.equal(result.completedSteps, 3);
  assert.equal(state.document.querySelector('[name="tool"]').value, "after-replace");
});

test("alias re-resolution fails closed when replacement becomes ambiguous", async () => {
  const state = runtimeWithMutableState(`
    <input name="tool" value="old" />
    <button id="replace" type="button">Replace</button>
  `);
  state.document.querySelector("#replace").addEventListener("click", () => {
    state.document.querySelector('[name="tool"]').outerHTML = '<input name="tool"><input name="tool">';
  });

  await assert.rejects(
    state.runtime.execute(transactionPlan("replace-many", [
      { operation: "find", locator: { name: "tool" }, asName: "tool" },
      { operation: "click", target: target({ id: "replace" }) },
      { operation: "fill", target: aliasTarget("tool"), text: "must-not-guess", clearFirst: true },
    ])),
    (error) => {
      assert.equal(error.code, "AMBIGUOUS_TARGET");
      assert.equal(error.completedSteps, 2);
      assert.equal(error.failedStep, 3);
      return true;
    },
  );
  assert.equal(Array.from(state.document.querySelectorAll('[name="tool"]')).every((el) => el.value === ""), true);
});

test("document identity change invalidates aliases", async () => {
  const state = runtimeWithMutableState(`
    <input name="tool" value="old" />
    <button id="navigate" type="button">Navigate</button>
  `);
  state.document.querySelector("#navigate").addEventListener("click", () => state.setIdentity("doc-2"));

  await assert.rejects(
    state.runtime.execute(transactionPlan("stale-doc", [
      { operation: "find", locator: { name: "tool" }, asName: "tool" },
      { operation: "click", target: target({ id: "navigate" }) },
      { operation: "fill", target: aliasTarget("tool"), text: "nope", clearFirst: true },
    ])),
    (error) => {
      assert.equal(error.code, "STALE_DOCUMENT");
      assert.equal(error.completedSteps, 2);
      return true;
    },
  );
  assert.equal(state.document.querySelector('[name="tool"]').value, "old");
});

test("execute supports native select checked and explicit click operations", async () => {
  const state = runtimeWithMutableState(`
    <select name="category"><option value="a">A</option><option value="dev">Developer Tools</option></select>
    <input type="checkbox" name="featured" />
    <button id="apply" type="button">Apply</button>
  `);
  let clicks = 0;
  state.document.querySelector("#apply").addEventListener("click", () => clicks++);

  const result = await state.runtime.execute(transactionPlan("native-actions", [
    { operation: "selectOption", target: target({ name: "category" }), value: null, label: "Developer Tools" },
    { operation: "setChecked", target: target({ name: "featured" }), checked: true },
    { operation: "click", target: target({ id: "apply" }) },
  ]));

  assert.equal(result.completedSteps, 3);
  assert.equal(state.document.querySelector('[name="category"]').value, "dev");
  assert.equal(state.document.querySelector('[name="featured"]').checked, true);
  assert.equal(clicks, 1);
});

test("custom combobox and switch mutations require semantic fallback", async () => {
  const state = runtimeWithMutableState(`
    <div role="combobox" aria-label="Category"></div>
    <div role="switch" aria-label="Featured"></div>
  `);
  await assert.rejects(
    state.runtime.execute(transactionPlan("custom-select", [
      { operation: "selectOption", target: target({ ariaLabel: "Category", role: "combobox" }), value: "dev", label: null },
    ])),
    (error) => error.code === "SEMANTIC_FALLBACK_REQUIRED",
  );
  await assert.rejects(
    state.runtime.execute(transactionPlan("custom-switch", [
      { operation: "setChecked", target: target({ ariaLabel: "Featured", role: "switch" }), checked: true },
    ])),
    (error) => error.code === "SEMANTIC_FALLBACK_REQUIRED",
  );
});

test("verify covers every supported transaction condition", async () => {
  const state = runtimeWithMutableState(`
    <input name="tool" value="RepoTunnel browser bridge" />
    <input type="checkbox" name="featured" checked />
    <select name="category"><option value="dev" selected>Developer Tools</option></select>
    <button id="enabled">Ready button</button>
    <button id="change-document" type="button">Change document</button>
  `, { url: "https://example.test/done?source=form", identity: "doc-before" });
  state.document.querySelector("#change-document").addEventListener("click", () => state.setIdentity("doc-after"));

  const result = await state.runtime.execute(transactionPlan("conditions", [
    { operation: "click", target: target({ id: "change-document" }) },
    { operation: "verify", condition: condition("urlEquals", "https://example.test/done?source=form") },
    { operation: "verify", condition: condition("urlContains", "/done?") },
    { operation: "verify", condition: condition("documentChanged") },
    { operation: "verify", condition: condition("elementExists", target({ name: "tool" })) },
    { operation: "verify", condition: condition("elementMissing", target({ id: "missing" })) },
    { operation: "verify", condition: condition("elementEnabled", target({ id: "enabled" })) },
    { operation: "verify", condition: condition("valueEquals", { target: target({ name: "tool" }), value: "RepoTunnel browser bridge" }) },
    { operation: "verify", condition: condition("valueContains", { target: target({ name: "tool" }), value: "browser" }) },
    { operation: "verify", condition: condition("checkedEquals", { target: target({ name: "featured" }), checked: true }) },
    { operation: "verify", condition: condition("selectedValueEquals", { target: target({ name: "category" }), value: "dev" }) },
    { operation: "verify", condition: condition("selectedLabelEquals", { target: target({ name: "category" }), label: "Developer Tools" }) },
    { operation: "verify", condition: condition("textContains", { target: target({ id: "enabled" }), text: "Ready" }) },
  ]));

  assert.equal(result.completedSteps, 13);
  assert.equal(result.steps.every((step) => step.result.verified === true), true);
});

test("semantic role/query existence conditions treat no match as missing without swallowing ambiguity", async () => {
  const { runtime } = runtimeFor(`<button>Save Project</button>`);
  const result = await runtime.execute(transactionPlan("semantic-missing-condition", [
    { operation: "verify", condition: condition("elementMissing", target({ role: "button", query: "Delete Project" })) },
  ]));
  assert.equal(result.completedSteps, 1);

  const page = dom(`<button>Save Project</button>`);
  const pageResult = page.window.eval(browserTransactionPageExpression("condition", {
    condition: { condition: "elementMissing", value: target({ role: "button", query: "Delete Project" }) },
  }));
  assert.equal(pageResult.ok, true);
  assert.equal(pageResult.verified, true);
});

test("wait_until enforces per-step and total wait budgets before mutation", async () => {
  const state = runtimeWithMutableState(`<input name="tool" value="old" />`);
  for (const timeoutMs of [49, 10001]) {
    await assert.rejects(
      state.runtime.execute(transactionPlan(`wait-${timeoutMs}`, [
        { operation: "fill", target: target({ name: "tool" }), text: "new", clearFirst: true },
        { operation: "waitUntil", condition: condition("elementExists", target({ id: "missing" })), timeoutMs },
      ])),
      (error) => error.code === "INVALID_ARGUMENT",
    );
    assert.equal(state.document.querySelector('[name="tool"]').value, "old");
  }

  await assert.rejects(
    state.runtime.execute(transactionPlan("wait-total", [
      { operation: "fill", target: target({ name: "tool" }), text: "new", clearFirst: true },
      { operation: "waitUntil", condition: condition("elementExists", target({ id: "a" })), timeoutMs: 10000 },
      { operation: "waitUntil", condition: condition("elementExists", target({ id: "b" })), timeoutMs: 10000 },
      { operation: "waitUntil", condition: condition("elementExists", target({ id: "c" })), timeoutMs: 50 },
    ])),
    (error) => error.code === "INVALID_ARGUMENT",
  );
  assert.equal(state.document.querySelector('[name="tool"]').value, "old");
});

test("wait_until polls locally until a condition becomes true", async () => {
  const state = runtimeWithMutableState(`<div id="root"></div>`, {
    sleep: async () => {
      if (!state.document.querySelector("#ready")) {
        const ready = state.document.createElement("button");
        ready.id = "ready";
        state.document.querySelector("#root").appendChild(ready);
      }
      await new Promise((resolve) => setTimeout(resolve, 1));
    },
  });
  const result = await state.runtime.execute(transactionPlan("wait-local", [
    { operation: "waitUntil", condition: condition("elementExists", target({ id: "ready" })), timeoutMs: 500 },
  ]));
  assert.equal(result.completedSteps, 1);
});

test("verify false fails with postcondition accounting", async () => {
  const state = runtimeWithMutableState(`<input name="tool" value="old" />`);
  await assert.rejects(
    state.runtime.execute(transactionPlan("verify-false", [
      { operation: "verify", condition: condition("valueEquals", { target: target({ name: "tool" }), value: "new" }) },
    ])),
    (error) => {
      assert.equal(error.code, "POSTCONDITION_FAILED");
      assert.equal(error.completedSteps, 0);
      assert.equal(error.failedStep, 1);
      return true;
    },
  );
});

test("formats transaction failures with safe step accounting and no partial-step payloads", () => {
  const error = new Error("POSTCONDITION_FAILED: Browser transaction condition is false.");
  error.code = "POSTCONDITION_FAILED";
  error.transactionId = "transaction-failure-1";
  error.completedSteps = 2;
  error.failedStep = 3;
  error.partialSteps = [{ result: { unexpectedValue: "must-not-leak" } }];

  const message = formatTransactionError(error);
  assert.match(message, /POSTCONDITION_FAILED/);
  assert.match(message, /transactionId=transaction-failure-1/);
  assert.match(message, /completedSteps=2/);
  assert.match(message, /failedStep=3/);
  assert.equal(message.includes("must-not-leak"), false);
});

test("whole transaction runtime stays below the 30000 ms helper transport timeout", async () => {
  let calls = 0;
  const page = dom(`<div id="ready"></div>`);
  const runtime = createBrowserTransactionRuntime({
    window: page.window,
    document: page.document,
    getUrl: () => "https://example.test/form",
    getDocumentIdentity: () => "doc-1",
    now: () => (calls++ === 0 ? 0 : 25001),
    sleep: async () => {},
  });
  await assert.rejects(
    runtime.execute(transactionPlan("runtime-bound", [
      { operation: "verify", condition: condition("elementExists", target({ id: "ready" })) },
      { operation: "verify", condition: condition("elementExists", target({ id: "ready" })) },
    ])),
    (error) => error.code === "TRANSACTION_TIMEOUT",
  );
});

test("cancellation during wait stops later steps without rolling back completed work", async () => {
  const page = dom(`<input name="tool" value="old"><button id="later" type="button">Later</button>`);
  let clicks = 0;
  page.document.querySelector("#later").addEventListener("click", () => clicks++);
  const runtime = createBrowserTransactionRuntime({
    window: page.window,
    document: page.document,
    getUrl: () => "https://example.test/form",
    getDocumentIdentity: () => "doc-1",
    sleep: (ms) => new Promise((resolve) => setTimeout(resolve, Math.min(ms, 5))),
  });

  const execution = runtime.execute(transactionPlan("cancel-wait", [
    { operation: "fill", target: target({ name: "tool" }), text: "kept", clearFirst: true },
    { operation: "waitUntil", condition: condition("elementExists", target({ id: "never" })), timeoutMs: 5000 },
    { operation: "click", target: target({ id: "later" }) },
  ]));
  await new Promise((resolve) => setTimeout(resolve, 20));
  const cancelled = runtime.cancel("cancel-wait");
  assert.equal(cancelled.cancelled, true);

  await assert.rejects(execution, (error) => {
    assert.equal(error.code, "TRANSACTION_CANCELLED");
    assert.equal(error.completedSteps, 1);
    assert.equal(error.failedStep, 2);
    return true;
  });
  assert.equal(page.document.querySelector('[name="tool"]').value, "kept");
  assert.equal(clicks, 0);
});


test("page command resolves accessible role and query locators", () => {
  const page = dom(`
    <label for="page-project">Project Name</label>
    <input id="page-project" />
    <button>Save Project</button>
  `);
  const fill = page.window.eval(browserTransactionPageExpression("fill", {
    locator: { role: "textbox", query: "Project Name" },
    text: "RepoTunnel",
    clearFirst: true,
  }));
  assert.equal(fill.ok, true);
  assert.equal(page.document.getElementById("page-project").value, "RepoTunnel");

  const click = page.window.eval(browserTransactionPageExpression("click", {
    locator: { role: "button", query: "Save Project" },
  }));
  assert.equal(click.ok, true);
});

test("page command expression uses the same native DOM semantics without echoing values", async () => {
  const page = dom(`<input name="tool" value="old">`);
  const expression = browserTransactionPageExpression("fill", {
    locator: { name: "tool" },
    text: "remote-secret-value",
  });
  const result = page.window.eval(expression);
  assert.equal(result.ok, true);
  assert.equal(page.document.querySelector('[name="tool"]').value, "remote-secret-value");
  assert.equal(JSON.stringify(result).includes("remote-secret-value"), false);
});

test("page command fill honors clearFirst false and supports clearing to empty", async () => {
  const page = dom(`<input name="tool" value="old">`);
  const appendResult = page.window.eval(browserTransactionPageExpression("fill", {
    locator: { name: "tool" },
    text: "-more",
    clearFirst: false,
  }));
  assert.equal(appendResult.ok, true);
  assert.equal(page.document.querySelector('[name="tool"]').value, "old-more");

  const clearResult = page.window.eval(browserTransactionPageExpression("fill", {
    locator: { name: "tool" },
    text: "",
    clearFirst: true,
  }));
  assert.equal(clearResult.ok, true);
  assert.equal(page.document.querySelector('[name="tool"]').value, "");
});

test("page command condition accepts the remote locator target wrapper", async () => {
  const page = dom(`<input name="tool" value="old">`);
  const expression = browserTransactionPageExpression("condition", {
    condition: condition("valueEquals", { target: target({ name: "tool" }), value: "old" }),
  });
  const result = page.window.eval(expression);
  assert.equal(result.ok, true);
  assert.equal(result.verified, true);
});

test("page command blocks sensitive controls named only by associated or accessible labels", async () => {
  for (const [html, locatorValue] of [
    [`<label for="field">Verification Code</label><input id="field">`, { label: "Verification Code" }],
    [`<label for="field">API Key</label><input id="field">`, { label: "API Key" }],
    [`<span id="secret-label">Private Key</span><input id="field" aria-labelledby="secret-label">`, { id: "field" }],
  ]) {
    const page = dom(html);
    const result = page.window.eval(browserTransactionPageExpression("fill", {
      locator: locatorValue,
      text: "must-not-write",
    }));
    assert.equal(result.ok, false);
    assert.equal(result.code, "SENSITIVE_FIELD");
    assert.equal(page.document.querySelector("input").value, "");
  }
});

test("page command updates React-controlled checkbox state", async () => {
  const page = dom(`<div id="root"></div>`);
  page.window.HTMLElement.prototype.attachEvent = function attachEvent() {};
  page.window.HTMLElement.prototype.detachEvent = function detachEvent() {};
  const previousGlobals = new Map();
  for (const [name, value] of Object.entries({
    window: page.window,
    document: page.document,
    navigator: page.window.navigator,
    HTMLElement: page.window.HTMLElement,
    HTMLInputElement: page.window.HTMLInputElement,
    HTMLTextAreaElement: page.window.HTMLTextAreaElement,
    HTMLSelectElement: page.window.HTMLSelectElement,
    Event: page.window.Event,
    IS_REACT_ACT_ENVIRONMENT: true,
  })) {
    // Modern Node exposes navigator as a getter-only global. Preserve the
    // descriptor so the React/JSDOM fixture can safely shadow it and restore
    // the exact original global afterwards.
    previousGlobals.set(name, Object.getOwnPropertyDescriptor(global, name));
    Object.defineProperty(global, name, {
      configurable: true,
      enumerable: true,
      writable: true,
      value,
    });
  }

  const React = require("react");
  const { createRoot } = require("react-dom/client");
  const root = createRoot(page.document.getElementById("root"));
  let changes = 0;
  function App() {
    const [checked, setChecked] = React.useState(false);
    return React.createElement("div", null,
      React.createElement("input", {
        id: "controlled-checkbox",
        type: "checkbox",
        checked,
        onChange: (event) => {
          changes += 1;
          setChecked(event.target.checked);
        },
      }),
      React.createElement("output", { id: "controlled-state" }, String(checked)),
    );
  }

  try {
    await React.act(async () => root.render(React.createElement(App)));
    let result;
    await React.act(async () => {
      result = page.window.eval(browserTransactionPageExpression("setChecked", {
        locator: { id: "controlled-checkbox" },
        checked: true,
      }));
    });
    assert.equal(result.ok, true);
    assert.equal(result.verified, true);
    assert.equal(page.document.getElementById("controlled-checkbox").checked, true);
    assert.equal(page.document.getElementById("controlled-state").textContent, "true");
    assert.equal(changes, 1);
  } finally {
    await React.act(async () => root.unmount());
    for (const [name, descriptor] of previousGlobals) {
      if (descriptor === undefined) delete global[name];
      else Object.defineProperty(global, name, descriptor);
    }
  }
});

test("page command marks a custom clickable for semantic pointer fallback without activating it", () => {
  const page = dom(`<div id="custom-action" role="button">Custom Action</div><output id="count">0</output>`);
  page.document.getElementById("custom-action").addEventListener("click", () => {
    page.document.getElementById("count").textContent = "1";
  });

  const result = page.window.eval(browserTransactionPageExpression("markSemanticClick", {
    locator: { role: "button", query: "Custom Action" },
    marker: "semantic-marker-1",
  }));

  assert.equal(result.ok, true);
  assert.equal(result.operation, "markSemanticClick");
  assert.equal(page.document.getElementById("custom-action").getAttribute("data-repotunnel-semantic-click-marker"), "semantic-marker-1");
  assert.equal(page.document.getElementById("count").textContent, "0");
});

test("page command expression preserves bulk all-target preflight", async () => {
  const page = dom(`<input name="one" value="one"><input name="two" value="two">`);
  const expression = browserTransactionPageExpression("fillForm", {
    fields: [
      textField("one", { name: "one" }, "changed-one"),
      textField("missing", { name: "missing" }, "changed-missing"),
      textField("two", { name: "two" }, "changed-two"),
    ],
  });
  const result = page.window.eval(expression);
  assert.equal(result.ok, false);
  assert.equal(result.code, "FALLBACK_REQUIRED");
  assert.equal(page.document.querySelector('[name="one"]').value, "one");
  assert.equal(page.document.querySelector('[name="two"]').value, "two");
});

test("page command fillForm never verifies a detached field after synchronous DOM replacement", async () => {
  const page = dom(`
    <input id="first" value="">
    <div id="slot"><input id="second" value=""></div>
  `);
  page.document.getElementById("first").addEventListener("input", () => {
    page.document.getElementById("slot").innerHTML = `<input id="second" value="replacement">`;
  });

  const result = page.window.eval(browserTransactionPageExpression("fillForm", {
    fields: [
      textField("first", { id: "first" }, "first-value"),
      textField("second", { id: "second" }, "second-value"),
    ],
  }));

  assert.equal(result.ok, true);
  assert.equal(result.verified, true);
  assert.equal(page.document.getElementById("first").value, "first-value");
  assert.equal(page.document.getElementById("second").value, "second-value");
});

test("remote click falls back to existing semantic pointer adapter for custom clickable widgets", async () => {
  const page = dom(`<div id="custom-action" role="button" tabindex="0">Custom Action</div><output id="clicked">0</output>`);
  let semanticCalls = 0;
  page.document.getElementById("custom-action").addEventListener("click", () => {
    page.document.getElementById("clicked").textContent = "1";
  });
  const remote = createRemoteBrowserTransactionRuntime({
    pageCommand: async (_tabId, operation, payload) => page.window.eval(browserTransactionPageExpression(operation, payload)),
    getState: async () => ({ url: page.window.location.href, documentIdentity: "loader-1" }),
    semanticClick: async ({ tabId, locator }) => {
      semanticCalls += 1;
      assert.equal(tabId, "tab-1");
      assert.equal(locator.role, "button");
      assert.equal(locator.query, "Custom Action");
      page.document.getElementById("custom-action").click();
      return { ok: true, verified: true, operation: "semanticClick", fallback: "semanticPointer" };
    },
    sleep: async () => {},
  });

  const result = await remote.execute(transactionPlan("remote-semantic-click", [
    { operation: "click", target: target({ role: "button", query: "Custom Action" }) },
  ]));

  assert.equal(result.completedSteps, 1);
  assert.equal(semanticCalls, 1);
  assert.equal(page.document.getElementById("clicked").textContent, "1");
  assert.equal(result.steps[0].result.operation, "semanticClick");
});

test("remote runtime survives document-generation changes for inline locators and invalidates old aliases", async () => {
  const page = dom(`<input name="tool" value="old"><button id="nav" type="button">Nav</button>`);
  let identity = "doc-1";
  page.document.querySelector("#nav").addEventListener("click", () => { identity = "doc-2"; });
  const remote = createRemoteBrowserTransactionRuntime({
    pageCommand: async (_tabId, operation, payload) => page.window.eval(browserTransactionPageExpression(operation, payload)),
    getState: async () => ({ url: page.window.location.href, documentIdentity: identity }),
    sleep: (ms) => new Promise((resolve) => setTimeout(resolve, Math.min(ms, 2))),
  });

  await assert.rejects(
    remote.execute(transactionPlan("remote-stale", [
      { operation: "find", locator: { name: "tool" }, asName: "tool" },
      { operation: "click", target: target({ id: "nav" }) },
      { operation: "fill", target: aliasTarget("tool"), text: "must-not-write", clearFirst: true },
    ])),
    (error) => error.code === "STALE_DOCUMENT" && error.completedSteps === 2,
  );
  assert.equal(page.document.querySelector('[name="tool"]').value, "old");
});

test("remote runtime invalidates aliases after same-document SPA URL changes", async () => {
  const page = dom(`<input name="tool" value="old">`, "https://example.test/one");
  let stateReads = 0;
  const remote = createRemoteBrowserTransactionRuntime({
    pageCommand: async (_tabId, operation, payload) => page.window.eval(browserTransactionPageExpression(operation, payload)),
    getState: async () => {
      stateReads += 1;
      const snapshot = { url: page.window.location.href, documentIdentity: "same-loader" };
      if (stateReads === 2) page.window.history.pushState({}, "", "/two");
      return snapshot;
    },
    sleep: async () => {},
  });

  await assert.rejects(
    remote.execute(transactionPlan("remote-spa-stale", [
      { operation: "find", locator: { name: "tool" }, asName: "tool" },
      { operation: "fill", target: aliasTarget("tool"), text: "must-not-write", clearFirst: true },
    ])),
    (error) => error.code === "STALE_DOCUMENT" && error.completedSteps === 1,
  );
  assert.equal(page.document.querySelector('[name="tool"]').value, "old");
});
