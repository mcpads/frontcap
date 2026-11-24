// Main world, in every frame, before any page script: dialogs are answered by the platform instead of
// blocking the page, by the agent's dialog rules (first match wins; `*` is the only wildcard), or else
// with the dismissed answer, so nothing is approved on the agent's behalf. Each call is announced to the
// top frame's observer, which records it at this instant. Event names derive from the launch, and helpers
// are taken before any page script runs, so a page can neither forge records, break them, nor change the
// rules. A frame that cannot reach the top document (another origin) keeps the native dialogs, which the
// platform answers by the same rules and records through the engine.
(() => {
  const nativeAlert = window.alert.bind(window);
  const nativeConfirm = window.confirm.bind(window);
  const nativePrompt = window.prompt.bind(window);
  const EVENT = "__FRONTCAP_DIALOG_EVENT__";
  const RULES_EVENT = "__FRONTCAP_DIALOG_RULES_EVENT__";
  let rules = __FRONTCAP_DIALOG_RULES__;
  const dispatch = EventTarget.prototype.dispatchEvent;
  const listen = EventTarget.prototype.addEventListener;
  const Custom = CustomEvent;
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const text = String;
  const dialogText = (value) => value === undefined ? "" : `${value}`;
  // Each document keeps its own native composition across observer/recorder replacement.
  // Install this before the cross-origin dialog guard: IME input also targets child documents.
  const compositionDocument = document;
  const COMPOSITION_EVENT = "__FRONTCAP_COMPOSITION_EVENT__";
  const data = Object.getOwnPropertyDescriptor(CompositionEvent.prototype, "data").get;
  let composition = null;
  listen.call(window, "compositionstart", () => { composition = ""; }, true);
  listen.call(window, "compositionupdate", (e) => { composition = data.call(e); }, true);
  listen.call(window, "compositionend", () => { composition = null; }, true);
  listen.call(compositionDocument, COMPOSITION_EVENT, (e) => {
    if (e.detail === "query") dispatch.call(compositionDocument, new Custom(COMPOSITION_EVENT, { detail: stringify(composition) }));
  });
  let target;
  try {
    target = window.top.document;
  } catch (e) {
    return;
  }
  // New rules for this document, from the platform (`set_dialog_rules`).
  listen.call(target, RULES_EVENT, (e) => {
    try { rules = parse(e.detail); } catch (err) { /* keep the rules in force */ }
  });
  const glob = (p, s) => {
    let pi = 0, si = 0, star = -1, mark = 0;
    while (si < s.length) {
      if (pi < p.length && p[pi] === "*") { star = pi; mark = si; pi += 1; }
      else if (pi < p.length && p[pi] === s[si]) { pi += 1; si += 1; }
      else if (star >= 0) { pi = star + 1; mark += 1; si = mark; }
      else return false;
    }
    while (pi < p.length && p[pi] === "*") pi += 1;
    return pi === p.length;
  };
  const ruleFor = (kind, message) => {
    for (let i = 0; i < rules.length; i += 1) {
      const r = rules[i];
      if ((r.kind === undefined || r.kind === kind) && (r.message === undefined || glob(r.message, message))) return r;
    }
    return undefined;
  };
  const announce = (kind, message, answer, native = false) => {
    try {
      dispatch.call(target, new Custom(EVENT, { detail: stringify({ kind, message, answer, native }) }));
    } catch (e) {
      // A document being torn down still gets its answer.
    }
  };
  window.alert = function alert(message) {
    const m = dialogText(message);
    if (ruleFor("alert", m)?.answer === "hold") { announce("alert", m, "", true); return nativeAlert(m); }
    announce("alert", m, "returned");
  };
  window.confirm = function confirm(message) {
    const m = dialogText(message);
    const r = ruleFor("confirm", m);
    if (r?.answer === "hold") { announce("confirm", m, "", true); return nativeConfirm(m); }
    const accepted = r !== undefined && r.answer === "accept";
    announce("confirm", m, accepted ? "true" : "false");
    return accepted;
  };
  // Unless the generation enrolls the windows its pages open, a window would run outside the platform's
  // clock, rules and evidence: refused as a popup blocker refuses it, and announced.
  if (__FRONTCAP_POPUPS_BLOCKED__) {
    window.open = function open(url) {
      try {
        dispatch.call(target, new Custom(EVENT, { detail: stringify({ window: text(url ?? "") }) }));
      } catch (e) {
        // Refused all the same.
      }
      return null;
    };
  }
  window.prompt = function prompt(message, value) {
    const m = dialogText(message);
    const r = ruleFor("prompt", m);
    if (r?.answer === "hold") {
      const initial = dialogText(value);
      announce("prompt", m, "", true);
      return nativePrompt(m, initial);
    }
    if (r === undefined || r.answer !== "accept") { announce("prompt", m, "null"); return null; }
    const answer = r.text !== undefined ? r.text : dialogText(value);
    announce("prompt", m, stringify(answer));
    return answer;
  };

})();
