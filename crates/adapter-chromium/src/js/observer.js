// Installed with Page.addScriptToEvaluateOnNewDocument in the isolated world "frontcap", before any page
// script, in every frame. The top frame's instance observes the whole page: each child frame's instance
// hands its window to it, and it watches mutations, receives input and resolves locators in every frame it
// can reach — those of its origin, and of any origin under engine patch 11, which lets this world read every
// window. Reports use __frontcapReport, exposed only to this world. Stream reports originate in each owning frame.
(() => {
  // Publish before the task or document can disappear. The binding preserves normal live ordering;
  // the browser copy survives a controller disconnect until its trace record is synced.
  const streamReport = (s, t) => {
    const msg = { type: "stream", id: Number(s.id), ordinal: s.ordinal ?? null, op: String(s.op), sequence: s.sequence ?? null,
      data: s.data === undefined ? null : String(s.data), code: s.code ?? null, reason: s.reason ?? null, t_us: t };
    if (typeof globalThis.__frontcapRetainStreamReport === "function") {
      msg.native_receipt = globalThis.__frontcapRetainStreamReport(JSON.stringify(msg));
    }
    globalThis.__frontcapReport(JSON.stringify(msg));
  };
  if (window !== window.top) {
    // Child stream reports use this document's binding, never cross-origin main-world access.
    window.addEventListener("__FRONTCAP_STREAM_EVENT__", (e) => {
      try {
        const s = JSON.parse(e.detail).stream;
        const origin = BigInt("__FRONTCAP_INITIAL_DATE_MS__") * 1000n;
        const t = __FRONTCAP_NATIVE_CLOCK__ ? (globalThis.__frontcapNativeTime() - origin).toString()
          : String((Date.now() - __FRONTCAP_INITIAL_DATE_MS__) * 1000);
        streamReport(s, t);
      } catch (err) {
        // Not a stream announcement.
      }
    }, true);
    try {
      const hub = window.top.__frontcap;
      if (hub) hub.adopt(window);
    } catch (e) {
      // A frame of another origin, which this world cannot reach without engine patch 11.
    }
    return;
  }
  const ORIGIN = __FRONTCAP_INITIAL_DATE_MS__;
  const FRAME_MS = __FRONTCAP_FRAME_MS__;
  const STOP_URL = "__FRONTCAP_STOP_URL__";
  const now = () => Date.now() - ORIGIN;
  const ORIGIN_US = BigInt("__FRONTCAP_INITIAL_DATE_MS__") * 1000n;
  const nowUs = __FRONTCAP_NATIVE_CLOCK__
    ? () => (globalThis.__frontcapNativeTime() - ORIGIN_US).toString()
    : () => String((Date.now() - ORIGIN) * 1000);

  // Durable reports have a shared receipt cursor and remain until acknowledged. Transient mutation
  // batches must not consume that cursor: the same DOM changes may arrive in different batch sizes.
  // Mutation totals still guard state revisions, and every retained sample/event remains ordered.
  let rseq = 0;
  const buffer = [];
  const report = (msg) => {
    if (msg.type === "sample" || msg.type === "navigate" || msg.type === "navigation_committed" || msg.type === "dialog" || msg.type === "focus" || msg.type === "message") {
      msg.rseq = ++rseq;
      buffer.push(msg);
      if (buffer.length > 100000) buffer.shift();
    }
    globalThis.__frontcapReport(JSON.stringify(msg));
  };
  const watches = new Map();
  const references = new Map();
  let stopOn = null;
  let stopOnNavigation = false;
  let stopOnException = false;
  let stopOnFocus = false;

  // Nodes of child frames have their own window and styles. Their objects here may come from either
  // realm (one the platform resolved is made in this frame's), so elements are told apart by name, never by
  // `instanceof`.
  const viewOf = (n) => n.ownerDocument.defaultView;
  const HTML = "http://www.w3.org/1999/xhtml";
  const is = (n, tag) => !!n && n.nodeType === 1 && n.namespaceURI === HTML && n.localName === tag;
  // The frame element `n`'s document is shown in, or null in the top frame.
  const frameOf = (n) => {
    const w = viewOf(n);
    return w ? w.frameElement : null;
  };
  const FRAMES = "iframe, frame, object, embed";
  const isFrame = (n) => !!n && n.nodeType === 1 && n.matches(FRAMES);
  function parentOf(n) {
    if (n.parentElement) return n.parentElement;
    const root = n.getRootNode && n.getRootNode();
    return root && root.host ? root.host : null;
  }
  // A text node (a snapshot's `text` reference) is shown in, and reached by input through, the element
  // that contains it (a shadow root's host for a text node directly in the root).
  const elementOf = (n) => (n && n.nodeType === 3 ? n.parentElement || (n.parentNode && n.parentNode.host) || null : n);
  function textBox(t) {
    const range = t.ownerDocument.createRange();
    range.selectNodeContents(t);
    return range.getBoundingClientRect();
  }
  function visibleHere(el) {
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) return false;
    return viewOf(el).getComputedStyle(el).visibility === "visible";
  }
  // Visible in its frame, and every frame element it is shown in is visible.
  function visible(el) {
    if (!el || !el.isConnected) return false;
    if (el.nodeType === 3) {
      const r = textBox(el);
      return (r.width > 0 || r.height > 0) && visible(elementOf(el));
    }
    for (let e = el; e; e = frameOf(e)) if (!visibleHere(e)) return false;
    return true;
  }
  function enabled(el) {
    for (let n = elementOf(el); n && n.nodeType === 1; n = parentOf(n)) {
      if (n.inert) return false;
      if (typeof n.matches === "function" && n.matches(":disabled")) return false;
    }
    // Inside a frame element that is inert, the frame's content is too.
    for (let f = frameOf(el); f; f = frameOf(f)) {
      for (let n = f; n && n.nodeType === 1; n = parentOf(n)) if (n.inert) return false;
    }
    return true;
  }
  // The product over the element, its ancestors, and those of every frame element it is shown in.
  function opacity(el) {
    let o = 1;
    for (let e = el; e; e = frameOf(e)) {
      for (let n = e; n && n.nodeType === 1; n = parentOf(n)) o *= parseFloat(viewOf(n).getComputedStyle(n).opacity);
    }
    return o;
  }

  // Whether `win` is the top window or shown in it through frame elements this world can read. A frame
  // inside one of another origin is not (without engine patch 11): its element reads as null from here,
  // so where it is on the page is unknown.
  function reachable(win) {
    for (let w = win; w; ) {
      if (w === window) return true;
      let f = null;
      try {
        f = w.frameElement;
      } catch (e) {
        // Another origin without engine patch 11.
      }
      if (!f) return false;
      w = viewOf(f);
    }
    return false;
  }
  const observed = (el) => !!el && !!el.ownerDocument && reachable(el.ownerDocument.defaultView);

  // Child frames handed over (or found) are observed from here: their mutations and input count as the
  // page's. A frame's document is adopted once; a new document in the frame hands itself over again.
  const adopted = new WeakSet();
  // The documents frames announced (`adopt`), by the element that shows each.
  const shownBy = new WeakMap();
  const observedRoots = new WeakSet();
  // Closed shadow roots by host. The patched engine hands each one to this world as it is attached
  // (page script cannot reach it); open roots are read from their hosts.
  const closedRoots = new WeakMap();
  const shadowOf = (el) => el.shadowRoot || closedRoots.get(el) || null;
  function observeRoot(root) {
    if (observedRoots.has(root)) return;
    observedRoots.add(root);
    mutations.observe(root, { subtree: true, childList: true, attributes: true, characterData: true });
    // Focus moving within a shadow tree reaches nothing outside it (its target and related target are
    // the same host there): the root's own listeners see it.
    if (root.nodeType === 11) {
      for (const type of ["focus", "blur"]) root.addEventListener(type, (event) => focusChanged(root, type, event), true);
    }
  }
  // Search each open root with ordinary CSS scope. Re-discover roots at observation/sample boundaries
  // so inserted components join existing watches; mutations within discovered roots are observed too.
  function rootsOf(doc) {
    const roots = [];
    const walk = (root) => {
      roots.push(root);
      observeRoot(root);
      for (const el of root.querySelectorAll('*')) { const shadow = shadowOf(el); if (shadow) walk(shadow); }
    };
    walk(doc);
    return roots;
  }
  const queryAll = (doc, selector) => rootsOf(doc).flatMap((root) => Array.from(root.querySelectorAll(selector)));
  function adopt(win) {
    let doc;
    try {
      doc = win.document;
    } catch (e) {
      return;
    }
    if (!doc || adopted.has(doc) || !reachable(win)) return;
    adopted.add(doc);
    if (win.frameElement) shownBy.set(win.frameElement, doc);
    observeRoot(doc);
    listen(win);
  }
  // The document frame element `f` shows, or null when this world cannot reach it. An `embed` has no
  // `contentDocument`: its document is the one that announced itself shown by it, or else its owner
  // window's child window whose frame element it is (`window.frames` leaves out shadow trees' frames).
  function frameDocument(f) {
    try {
      if (f.contentDocument || !is(f, "embed")) return f.contentDocument || null;
    } catch (e) {
      return null; // Another origin without engine patch 11.
    }
    const announced = shownBy.get(f);
    if (announced && announced.defaultView && announced.defaultView.frameElement === f) return announced;
    const w = viewOf(f);
    for (let i = 0; w && i < w.length; i++) {
      try {
        if (w[i].frameElement === f) return w[i].document;
      } catch (e) {
        // Another origin without engine patch 11.
      }
    }
    return null;
  }
  // The document a frame element shows, adopted, or null when this world cannot reach it.
  function contentOf(f) {
    const d = frameDocument(f);
    if (!d || !d.defaultView) return null;
    adopt(d.defaultView);
    return d;
  }
  // The documents under `roots` this world can reach: each root, then its frames', depth first.
  function documentsFrom(roots) {
    const out = [];
    const walk = (doc) => {
      out.push(doc);
      for (const f of queryAll(doc, FRAMES)) {
        const d = contentOf(f);
        if (d) walk(d);
      }
    };
    roots.forEach(walk);
    return out;
  }
  const documents = () => documentsFrom([document]);
  // The elements `loc` names in `docs`. A frame scope narrows to the documents of the frames it names.
  function resolve(loc, docs) {
    docs = docs || documents();
    const all = (selector) => docs.flatMap((d) => queryAll(d, selector));
    switch (loc.by) {
      case "css": return all(loc.selector);
      case "role": {
        if (typeof globalThis.__frontcapQueryRole !== "function") throw new Error("native role queries unavailable");
        const role = loc.role === "iframe" ? "Iframe" : loc.role;
        return docs.flatMap((d) => globalThis.__frontcapQueryRole(d, role, loc.name ?? null));
      }
      case "test_id": return all(`[data-testid="${CSS.escape(loc.id)}"]`);
      // The scope's own document: the top one, or a frame scope's.
      case "document": return docs.length === 0 ? [] : loc.selector !== undefined
        ? queryAll(docs[0], loc.selector) : docs[0].documentElement ? [docs[0].documentElement] : [];
      case "text": return all("*").filter((e) =>
        (e.getRootNode().nodeType === 11 || (e !== e.ownerDocument.body && e.ownerDocument.body?.contains(e))) &&
        e.childElementCount === 0 && (loc.contains ? e.textContent.includes(loc.text) : e.textContent.trim() === loc.text));
      case "ref": {
        if (!references.has(loc.id)) throw new Error('reference is not bound in this document');
        const node = references.get(loc.id);
        return node.isConnected && docs.includes(node.ownerDocument) ? [node] : [];
      }
      case "frame": {
        // As for an action: exactly one frame element, whose document this world reaches; otherwise the
        // watch cannot say (`unknown`), or there is no such frame (nothing found).
        const named = resolve(loc.frame, docs);
        if (named.length > 1) throw new Error("the frame scope names several elements");
        if (named.length === 0 || !isFrame(named[0])) return [];
        const d = contentOf(named[0]);
        if (!d) throw new Error("the frame scope's document is not reachable");
        return resolve(loc.target, documentsFrom([d]));
      }
      default: throw new Error(`locator ${loc.by} is resolved by the recorder`);
    }
  }
  function evaluate(p, docs) {
    try {
      const nodes = resolve(p.target, docs);
      const one = nodes.length === 1 ? nodes[0] : undefined;
      const single = (f) => (nodes.length > 1 ? "unknown" : one ? String(f(one)) : "false");
      switch (p.is) {
        case "attached": return nodes.length > 1 ? "unknown" : String(nodes.length === 1);
        case "visible": return single(visible);
        case "opacity_at_least": return single((e) => opacity(e) * 1000 >= p.milli - 1e-6);
        case "text_equals": return single((e) => e.textContent === p.text);
        case "value_equals": return single((e) => e.value === p.value);
        case "count_at_least": return String(nodes.length >= p.n);
        default: return "unknown";
      }
    } catch (e) {
      return "unknown";
    }
  }
  function sample(name, w, point, docs) {
    const value = evaluate(w.predicate, docs);
    report({ type: "sample", name, value, point, t_us: nowUs() });
    if (value === "true" && stopOn === name) {
      stopOn = null;
      // A request held at the Fetch request stage stops virtual time at this instant under
      // pauseIfNetworkFetchesPending; the recorder reads the stop from the request URL.
      fetch(STOP_URL + encodeURIComponent(name)).catch(() => {});
    }
    return value;
  }
  function sampleAll(point) {
    const docs = documents();
    for (const [name, w] of watches) sample(name, w, point, docs);
  }

  let mutationTotal = 0;
  const mutations = new MutationObserver((records) => {
    for (const r of records) {
      if (r.type === "shadowRoot" && r.target.nodeType === 11 && r.target.host) {
        closedRoots.set(r.target.host, r.target);
        observeRoot(r.target);
      }
    }
    mutationTotal += records.length;
    report({ type: "mutations", n: records.length, total: mutationTotal, t_us: nowUs() });
    if (watches.size) sampleAll("mutation");
  });
  adopted.add(document);
  observeRoot(document);

  // Frame samples on virtual-time frame ticks, one per tick, while there is a watch to sample. On the
  // patched engine the controller takes them at each tick it renders (`sampleFrame`): the ticks it
  // skips on an idle page, where nothing can change, take none, and no timer of the platform's keeps
  // the page from being idle. Otherwise a timer per tick takes them.
  let lastTick = -1;
  let ticking = false;
  const tick = (k) => {
    if (k !== undefined) {
      lastTick = k;
      if (watches.size) sampleAll("frame");
    }
    if (!watches.size) {
      ticking = false;
      return;
    }
    ticking = true;
    const t = now();
    const next = Math.max(Math.floor(t / FRAME_MS) + 1, lastTick + 1);
    setTimeout(tick, Math.max(0, Math.ceil(next * FRAME_MS - t)), next);
  };
  const startTicking = () => { if (!__FRONTCAP_NATIVE_CLOCK__ && !ticking) tick(); };

  // Reported before a navigation's request leaves the page; the recorder stamps the request with it.
  if (globalThis.navigation) {
    navigation.addEventListener("currententrychange", () => {
      report({ type: "navigation_committed", url: location.href, t_us: nowUs() });
    });
    navigation.addEventListener("navigate", (e) => {
      report({ type: "navigate", url: e.destination.url, t_us: nowUs() });
      if (stopOnNavigation && e.destination.sameDocument) {
        // Same-document navigation has no document request to hold the clock after this task.
        fetch(STOP_URL + "mcp-next-navigation").catch(() => {});
      }
    });
  }

  // Receipts of platform input, per request and substep, kept in this world so they survive a recorder
  // fault (S11), with the node each marked event reached. The listeners run first (this world registers
  // them before any page script, a child frame's when its document starts) and see every event, cancelled
  // or not: a page cannot suppress them, and `document.open()` does not erase them (engine patch 11).
  let mark = null;
  const receipts = new Map();
  // Steps the platform recorded as sent before sending them: receipts for recovery, not deliveries.
  const sent = new Map();
  const recipients = new Map();
  const deliveredEvents = new Map();
  const seenDeliveredEvents = new WeakSet();
  const receive = (id, step) => {
    const list = receipts.get(id) || [];
    if (!list.includes(step)) list.push(step);
    receipts.set(id, list);
  };
  // A release that ends an HTML drag and drop is received as `dragend` (no `pointerup` follows a drag
  // start); files set on an input as `change`, or `cancel` for the same files again.
  // During an HTML drag a held move is received as `drag` (no `pointermove` follows a drag start).
  // `change` and `cancel` count only for the step that sets files: a focus moved by another step fires
  // `change` on the element left.
  // `drop` and `dragover` (which reach the window when the drag's source has been removed) count only
  // for a drag's steps.
  // Composition and touch events count only for the steps that compose or touch: a click that moves the
  // focus out of a composition ends it.
  const ONLY_WHEN_NAMED = ["change", "cancel", "drop", "dragover", "dragleave", "pointercancel", "compositionupdate", "compositionend", "touchstart", "touchmove", "touchend", "touchcancel"];
  // Requests whose drag started an HTML drag and drop: the engine runs it, and it ends with the
  // session that started it.
  const htmlDrags = new Set();
  // Each document's adoption registers listeners of its own: a window the frame's first document reuses
  // (its initial empty document's) keeps the earlier ones, but they do not receive the new document's
  // events. A step is received once however many listeners see it.
  // The closed roots that hold the focus (their hosts stand for it outside them).
  const focusedIn = new WeakSet();
  // One focus or blur, reported once: by the listener of the document or shadow root that holds its
  // target. A window's capture listener runs first and observes every root on the way, so a root's
  // listeners see the event even the first time focus enters it. A closed root's host stands, outside
  // it, for the element focused inside it, which that root's listener reports.
  function focusChanged(scope, type, event) {
    const path = event.composedPath();
    const target = path[0];
    if (!event.isTrusted || !target || target.nodeType !== 1) return;
    for (const n of path) if (n.nodeType === 11) observeRoot(n);
    const inner = closedRoots.get(target);
    if (inner) observeRoot(inner);
    const home = target.getRootNode();
    if (scope.nodeType === 11 ? home !== scope : home.nodeType !== 9) return;
    if (inner && (type === "focus" ? inner.activeElement : focusedIn.has(inner))) return;
    if (scope.nodeType === 11 && scope.host.shadowRoot !== scope) {
      if (type === "focus") focusedIn.add(scope); else focusedIn.delete(scope);
    }
    report({ type: "focus", target: describe(target), focused: type === "focus", t_us: nowUs() });
    if (stopOnFocus) fetch(STOP_URL + "mcp-next-focus").catch(() => {});
  }
  function listen(win) {
    for (const type of ["focus", "blur"]) win.addEventListener(type, (event) => focusChanged(win, type, event), true);
    win.addEventListener("error", (event) => {
      if (stopOnException && typeof event.message === "string") {
        fetch(STOP_URL + "mcp-next-exception").catch(() => {});
      }
    });
    win.addEventListener("unhandledrejection", () => {
      if (stopOnException) fetch(STOP_URL + "mcp-next-exception").catch(() => {});
    });
    // Messages from other windows (a popup to its opener, a frame to its parent). A window posting to
    // itself is a scheduler's business, not an exchange between windows.
    win.addEventListener("message", (event) => {
      if (!event.isTrusted || event.source === win) return;
      let data;
      try { data = JSON.stringify(event.data); } catch (_) { data = undefined; }
      if (data === undefined) data = Object.prototype.toString.call(event.data);
      report({ type: "message", origin: event.origin, data: data.slice(0, 120), t_us: nowUs() });
    }, true);
    for (const type of ["pointerdown", "pointerup", "pointercancel", "pointermove", "wheel", "keydown", "keyup", "beforeinput", "drag", "dragover", "dragleave", "drop", "dragend", "change", "cancel", "compositionupdate", "compositionend", "touchstart", "touchmove", "touchend", "touchcancel"]) {
      win.addEventListener(type, (e) => {
        if (!mark) return;
        if (mark.only ? !mark.only.includes(type) : ONLY_WHEN_NAMED.includes(type)) return;
        if (!seenDeliveredEvents.has(e)) {
          seenDeliveredEvents.add(e);
          const events = deliveredEvents.get(mark.id) || [];
          events.push({ step: mark.step, event: type, t_us: nowUs(), trusted: e.isTrusted });
          deliveredEvents.set(mark.id, events);
        }
        receive(mark.id, mark.step);
        const list = recipients.get(mark.id) || [];
        list.push({ step: mark.step, node: e.composedPath()[0] || e.target });
        recipients.set(mark.id, list);
      }, true);
    }
    win.addEventListener("dragstart", () => { if (mark) htmlDrags.add(mark.id); }, true);
  }
  listen(window);
  // Whether `node` is `target` or inside it, across shadow roots and into the frames `target` shows.
  function accepted(target, node) {
    target = elementOf(target);
    for (let n = node; n; n = n.parentNode || n.host || (n.nodeType === 9 && n.defaultView ? n.defaultView.frameElement : null)) {
      if (n === target) return true;
    }
    // A listener outside a closed or user-agent shadow tree (a date field's segments) sees an event inside
    // it retargeted to the tree's host. The native hit test before dispatch chose the target; a receipt at
    // the host of such a tree holding the target is that delivery. A tree is open when its host shows it;
    // a user-agent root's `mode` is not read: the engine has no value for it and the renderer fails.
    for (let root = target.getRootNode(); root && root.host; root = root.host.getRootNode()) {
      if (root.host === node && root.host.shadowRoot !== root) return true;
    }
    return false;
  }
  function describe(n) { return n && n.outerHTML ? n.outerHTML.slice(0, 80) : String(n); }
  const TEXT_INPUTS = ["text", "search", "email", "url", "tel", "password", "number"];
  function editable(el) {
    if (el.isContentEditable) return true;
    if (is(el, "textarea")) return !el.readOnly;
    if (is(el, "input")) return TEXT_INPUTS.includes(el.type) && !el.readOnly;
    return false;
  }
  // The focused element, through shadow roots and into the frames this world can reach: a frame element
  // when focus is inside a frame it cannot reach.
  function deepActive() {
    let a = document.activeElement;
    for (;;) {
      while (a && shadowOf(a) && shadowOf(a).activeElement) a = shadowOf(a).activeElement;
      if (!isFrame(a)) return a;
      const d = frameDocument(a);
      if (!d || !d.activeElement) return a;
      a = d.activeElement;
    }
  }
  // Element-child indices from `documentElement` down to `n`, in `n`'s own document.
  function pathIn(n) {
    const root = n.ownerDocument.documentElement;
    const path = [];
    while (n !== root) {
      const p = n.parentNode;
      if (!p) return null;
      path.push(Array.prototype.indexOf.call(p.children, n));
      if (p.nodeType === 11) {
        if (!p.host || shadowOf(p.host) !== p) return null;
        path.push('shadow_root');
        n = p.host;
      } else n = p;
    }
    return path.reverse();
  }

  // Dialogs the main world answered (dialogs.js) and randomness it could not seed (seed.js), recorded
  // at the instant of the call.
  // Registered on the window's capture phase before any page script, so this listener runs first.
  // What the page sends on a platform stream (streams.js), recorded at this instant.
  window.addEventListener("__FRONTCAP_STREAM_EVENT__", (e) => {
    try {
      const s = JSON.parse(e.detail).stream;
      streamReport(s, nowUs());
    } catch (err) {
      // Not a platform announcement.
    }
  }, true);
  window.addEventListener("__FRONTCAP_DIALOG_EVENT__", (e) => {
    try {
      const d = JSON.parse(e.detail);
      if (d.taint !== undefined) {
        report({ type: "taint", detail: String(d.taint), t_us: nowUs() });
        return;
      }
      if (d.window !== undefined) {
        report({ type: "new_window", url: String(d.window), t_us: nowUs() });
        return;
      }
      report({ type: "dialog", kind: String(d.kind), message: String(d.message), answer: String(d.answer), native: d.native === true, t_us: nowUs() });
    } catch (err) {
      // A detail this world cannot read is not a platform announcement.
    }
  }, true);


  globalThis.__frontcap = {
    addWatch(name, predicate) {
      const w = { predicate };
      watches.set(name, w);
      startTicking();
      return sample(name, w, "initial");
    },
    evaluatePredicate(predicate) { return evaluate(predicate, documents()); },
    removeWatch(name) {
      const w = watches.get(name);
      if (!w) return null;
      const value = sample(name, w, "final");
      watches.delete(name);
      return value;
    },
    sampleFrozen() { sampleAll("frozen"); },
    sampleFrame(k) {
      if (!watches.size) return;
      lastTick = k;
      sampleAll("frame");
    },
    reportsAfter(n) { return JSON.stringify(buffer.filter((m) => m.rseq > n)); },
    ack(n) { while (buffer.length && buffer[0].rseq <= n) buffer.shift(); },
    hasWatch(name) { return watches.has(name); },
    hasReference(id) { return references.has(id); },
    bindReference(id, node) {
      references.set(id, node);
      observeRoot(node.getRootNode());
    },
    mutationTotal() { return mutationTotal; },
    stopWhenTrue(name) { stopOn = name; },
    stopAtNavigation() { stopOnNavigation = true; },
    stopAtException() { stopOnException = true; },
    stopAtFocus() { stopOnFocus = true; },
    clearStop() { stopOn = null; stopOnNavigation = false; stopOnException = false; stopOnFocus = false; },
    animationsRunning() {
      return documents().reduce((n, d) => n + d.getAnimations().filter((a) => a.playState === "running").length, 0);
    },
    adopt,
    picked: [],
    pick(loc) { this.picked = resolve(loc); return this.picked.length; },
    // Picks what `loc` names in the frames `owner` (a frame element) shows; -1 when its document is not
    // reachable from this world.
    pickIn(owner, loc) {
      const d = isFrame(owner) ? contentOf(owner) : null;
      this.picked = d ? resolve(loc, documentsFrom([d])) : [];
      return d ? this.picked.length : -1;
    },
    observed,
    accepted,
    is,
    // A text node (a snapshot's `text` reference) has its own text, box and visibility; attributes are
    // none, and style, interaction, scroll and selection are those of the element it is in.
    query(node, props) {
      const out = {};
      const text = node.nodeType === 3;
      const el = elementOf(node);
      for (const p of props) {
        if (p === "text") out.text = (node.textContent || "").trim();
        else if (p === "value") out.value = !text && "value" in el ? el.value : null;
        else if (p === "visible") out.visible = visible(node);
        else if (!el) out[p] = null;
        else if (p === "interaction") out.interaction = {
          enabled: enabled(el), editable: editable(el), focused: deepActive() === el,
          tab_index: el.tabIndex, content_editable: el.isContentEditable
        };
        else if (p === "scroll") out.scroll = {
          x: el.scrollLeft, y: el.scrollTop, width: el.scrollWidth, height: el.scrollHeight,
          viewport_width: el.clientWidth, viewport_height: el.clientHeight,
          max_x: Math.max(0, el.scrollWidth - el.clientWidth), max_y: Math.max(0, el.scrollHeight - el.clientHeight)
        };
        else if (p === "selection") {
          // Native form-control accessors in the observer world do not invoke main-world getters.
          if (is(el, "input") || is(el, "textarea")) {
            out.selection = { start: el.selectionStart, end: el.selectionEnd, direction: el.selectionDirection };
          } else {
            const sel = viewOf(el).getSelection();
            const inside = sel && accepted(el, sel.anchorNode) && accepted(el, sel.focusNode);
            out.selection = inside ? { text: sel.toString(), collapsed: sel.isCollapsed, anchor_offset: sel.anchorOffset, focus_offset: sel.focusOffset } : null;
          }
        }
        else if (p === "bbox") {
          // A node with no box; one with a box is placed by the recorder from the engine's layout.
          const r = text ? textBox(node) : el.getBoundingClientRect();
          out.bbox = { x: r.x, y: r.y, width: r.width, height: r.height };
        }
        else if (p.startsWith("attr:")) out[p] = text ? null : el.getAttribute(p.slice(5));
        else if (p.startsWith("style:")) out[p] = viewOf(el).getComputedStyle(el).getPropertyValue(p.slice(6));
      }
      return out;
    },
    // The adapter first checks the inspector's assignedSlot (including closed roots). Frame-document
    // joins use native frame identities; this reader only crosses DOM and shadow-root parents.
    hitParent(node) { return node.parentNode || node.host || null; },
    // Where `el` is, for re-execution without the snapshot that showed it: the paths of the frame elements
    // it is shown in, from the top document inward, and its path in its own document; null when it is not
    // an element reached that way.
    elementPath(node) {
      // A snapshot's text reference is a text node: its element's path, then its place among that
      // element's child nodes.
      const el = node && node.nodeType === 3 ? node.parentNode : node;
      if (!el || el.nodeType !== 1 || !observed(el)) return null;
      const path = pathIn(el);
      if (!path) return null;
      if (node !== el) path.push({ text: Array.prototype.indexOf.call(el.childNodes, node) });
      const frames = [];
      for (let f = frameOf(el); f; f = frameOf(f)) {
        const p = pathIn(f);
        if (!p) return null;
        frames.unshift(p);
      }
      return { frames, path };
    },
    // Picks the element `elementPath` gave; whether there is one.
    pickPath(frames, path) {
      const walk = (doc, p) => {
        let n = doc.documentElement;
        for (const i of p) {
          if (!n) break;
          if (i === 'shadow_root') n = shadowOf(n);
          else if (typeof i === 'object') n = n.childNodes[i.text]?.nodeType === 3 ? n.childNodes[i.text] : null;
          else n = n.children[i];
        }
        return n;
      };
      let doc = document;
      for (const p of frames) {
        const f = walk(doc, p);
        doc = isFrame(f) ? frameDocument(f) : null;
        if (!doc) {
          this.picked = [];
          return false;
        }
      }
      const n = walk(doc, path);
      this.picked = n ? [n] : [];
      return !!n;
    },
    mark(id, step, only) { mark = { id, step, only: only || null }; },
    // Written by the platform before it sends a step the page receives only later.
    sent(id, step) { const l = sent.get(id) || []; if (!l.includes(step)) l.push(step); sent.set(id, l); },
    htmlDrag(id) { return htmlDrags.has(id); },
    received(id, step) { return (receipts.get(id) || []).includes(step); },
    receivedEvents(id) { return deliveredEvents.get(id) || []; },
    unreceived(id) { const got = receipts.get(id) || []; return JSON.stringify((sent.get(id) || []).filter((s) => !got.includes(s))); },
    unmark() { mark = null; },
    receipts(id) { return JSON.stringify([...new Set([...(receipts.get(id) || []), ...(sent.get(id) || [])])]); },
    // The recipient of `id`'s first input event when `target` does not accept it, or the first of the
    // `expected` substeps that left no receipt; null otherwise. Only the first event follows the final
    // check: where later events go is the page's doing (a handler may move focus or the target).
    // Without a target only the receipts are checked.
    undelivered(target, id, expected) {
      const first = (recipients.get(id) || [])[0];
      if (target && first && !accepted(target, first.node)) return describe(first.node);
      const got = receipts.get(id) || [];
      for (const step of expected) if (!got.includes(step)) return `no input was delivered for substep ${step}`;
      return null;
    },
    editable,
    // The element that takes focus for keys aimed at `el`: itself if focusable, the editing host of a
    // node inside one, otherwise none.
    focusHost(el) {
      if (el.isContentEditable) {
        let n = el;
        while (n.parentElement && n.parentElement.isContentEditable) n = n.parentElement;
        return n;
      }
      const focusable = el.matches("input, select, textarea, button, a[href], area[href], iframe, [tabindex], summary");
      return focusable ? el : null;
    },
    // Whether focus is inside a frame this world cannot reach, whose input it does not observe.
    unobservedFrameFocused() { return isFrame(deepActive()); },
    // The focused document's composition while one is in progress, else null, from its main world's
    // record of its composition events (dialogs.js), which outlives this world's session.
    composition() {
      const active = deepActive();
      if (active && isFrame(active)) return null;
      const owner = active?.ownerDocument || document;
      let answer;
      const listener = (e) => { if (e.detail !== "query") answer = e.detail; };
      owner.addEventListener("__FRONTCAP_COMPOSITION_EVENT__", listener);
      owner.dispatchEvent(new CustomEvent("__FRONTCAP_COMPOSITION_EVENT__", { detail: "query" }));
      owner.removeEventListener("__FRONTCAP_COMPOSITION_EVENT__", listener);
      return typeof answer === "string" ? JSON.parse(answer) : null;
    },
    focusEditable() { const a = deepActive(); return !!a && !isFrame(a) && editable(a); },
    // A request to the state providers the app registered in `doc` (state.js, its main world), answered
    // synchronously: `{providers}` for `list`, `{value}` (JSON text) or `{pending}` for `read`, `{written,
    // pending?}` for `write`, `{state, value?, error?}` for the `outcome` of the request named `token`.
    // A provider's failure, or a document without the channel, throws.
    appState(doc, op, id, value, token) {
      let answer;
      const listener = (e) => { answer = e.detail; };
      doc.addEventListener("__FRONTCAP_STATE_REPLY__", listener);
      try {
        doc.dispatchEvent(new CustomEvent("__FRONTCAP_STATE_REQUEST__", { detail: JSON.stringify({ op, id, value, token }) }));
      } finally {
        doc.removeEventListener("__FRONTCAP_STATE_REPLY__", listener);
      }
      if (typeof answer !== "string") throw new Error("the document has no state provider channel");
      const reply = JSON.parse(answer);
      if (typeof reply.error === "string") throw new Error(reply.error);
      return reply;
    },
    // Script substeps act on the target directly and write their receipt in the same call. They return
    // null, or {not_found} / {not_actionable} without a receipt when nothing was done.
    focusTarget(el, atEnd, id, step) {
      el.focus();
      if (atEnd) {
        if (is(el, "input") || is(el, "textarea")) {
          try {
            el.setSelectionRange(el.value.length, el.value.length);
          } catch (e) {
            // email and number fields have no text selection API; the caret still moves.
            viewOf(el).getSelection().modify("move", "forward", "documentboundary");
          }
        } else if (el.isContentEditable) {
          const range = el.ownerDocument.createRange();
          range.selectNodeContents(el);
          range.collapse(false);
          const sel = viewOf(el).getSelection();
          sel.removeAllRanges();
          sel.addRange(range);
        }
      }
      const a = deepActive();
      if (a !== el) return { not_actionable: `focus did not land on the target (it is on ${describe(a)})` };
      receive(id, step);
      return null;
    },
    selectContents(el, id, step) {
      if (is(el, "input") || is(el, "textarea")) el.select();
      else {
        const range = el.ownerDocument.createRange();
        range.selectNodeContents(el);
        const sel = viewOf(el).getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
      }
      receive(id, step);
    },
    // The options `wanted` names, by value and then by label, or why they cannot be chosen.
    matchOptions(el, wanted) {
      const options = Array.from(el.options);
      const chosen = [];
      for (const w of wanted) {
        const o = options.find((o) => o.value === w) || options.find((o) => o.label === w || o.textContent.trim() === w);
        if (!o) return { not_found: `option ${JSON.stringify(w)}` };
        if (o.disabled || (is(o.parentElement, "optgroup") && o.parentElement.disabled)) {
          return { not_actionable: `option ${JSON.stringify(w)} is disabled` };
        }
        chosen.push(o);
      }
      return { chosen };
    },
    // Selects the options and reports the change as a user's choice would; nothing when it changes nothing.
    selectOptions(el, wanted, id, step) {
      const m = this.matchOptions(el, wanted);
      if (!m.chosen) return m;
      const options = Array.from(el.options);
      const changed = options.some((o) => o.selected !== m.chosen.includes(o));
      for (const o of options) o.selected = m.chosen.includes(o);
      if (changed) {
        const Ev = viewOf(el).Event;
        el.dispatchEvent(new Ev("input", { bubbles: true, composed: true }));
        el.dispatchEvent(new Ev("change", { bubbles: true }));
      }
      receive(id, step);
      return null;
    },
    // The scroller a wheel moving by (dx, dy) over `el` reaches first: its nearest ancestor (itself
    // included) with overflow to scroll on an axis the wheel moves, or its document's scrolling element.
    scrollerOf(el, dx, dy) {
      el = elementOf(el);
      const doc = el.ownerDocument;
      const scrolls = (n) => {
        const s = viewOf(n).getComputedStyle(n);
        return (dy !== 0 && /(auto|scroll)/.test(s.overflowY) && n.scrollHeight > n.clientHeight)
          || (dx !== 0 && /(auto|scroll)/.test(s.overflowX) && n.scrollWidth > n.clientWidth);
      };
      let n = el;
      while (n && n.nodeType === 1 && n !== doc.scrollingElement && !scrolls(n)) n = parentOf(n);
      return n && n.nodeType === 1 && n !== doc.documentElement ? n : doc.scrollingElement || doc.documentElement;
    },
    // Whether a wheel over `hit` reaches `scroller` first.
    scrollsFirst(scroller, hit, dx, dy) {
      return !!hit && this.scrollerOf(hit, dx, dy) === scroller;
    },
    describe,
    visible,
    enabled,
    now,
  };
})();
