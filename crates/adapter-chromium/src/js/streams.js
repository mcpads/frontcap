// Main-world stream wrappers are local to each document. Intercepted anchors select rules;
// the adapter binds logical ordinals and delivers only to the owning execution context.
// Local DOM announcements are relayed by that document's isolated observer.
(() => {
  const KEY = "__FRONTCAP_STREAMS__";
  const EVENT = "__FRONTCAP_DIALOG_EVENT__";
  const dispatch = EventTarget.prototype.dispatchEvent;
  const Custom = CustomEvent;
  const stringify = JSON.stringify;
  const nativeFetch = window.fetch.bind(window);
  const Url = URL;
  const Encoder = TextEncoder;
  const later = (f) => Promise.resolve().then(f);
  const announce = (detail) => {
    try {
      dispatch.call(document, new Custom(EVENT, { detail: stringify({ stream: detail }) }));
    } catch (e) {
      // A document being torn down sends nothing more.
    }
  };
  const streams = new Map();
  let next = 0;

  const handlerProperty = (proto, type) => {
    const slot = Symbol(type);
    Object.defineProperty(proto, "on" + type, {
      get() { return this[slot] ?? null; },
      set(f) {
        if (this[slot]) this.removeEventListener(type, this[slot]);
        this[slot] = typeof f === "function" ? f : null;
        if (this[slot]) this.addEventListener(type, this[slot]);
      },
      configurable: true,
    });
  };

  // The anchor: the stream's own URL, as the page wrote it, with the marker added to its query; an
  // EventSource reconnecting carries its last event id (hex of the UTF-8) for the server.
  const open = (target, url, kind, last) => {
    const id = ++next;
    streams.set(id, target);
    target._seq = -1;
    delete target._ordinal;
    let [base, fragment] = url.split("#");
    if (kind === "ws") base = base.replace(/^ws(s?):/i, "http$1:");
    const hex = last ? "." + Array.from(new Encoder().encode(last), (b) => b.toString(16).padStart(2, "0")).join("") : "";
    const anchor = base + (base.includes("?") ? "&" : "?") + "__frontcap_stream=" + id + "." + kind + hex + (fragment === undefined ? "" : "#" + fragment);
    nativeFetch(anchor, { mode: "no-cors", cache: "no-store", credentials: "omit" }).catch(() => {});
    return id;
  };

  // The connection could not be made: as a browser reports it.
  const fail = (s) => {
    const ws = s instanceof FrontcapWebSocket;
    s.readyState = ws ? 3 : 2;
    s.dispatchEvent(new Event("error"));
    if (ws) s.dispatchEvent(new CloseEvent("close", { code: 1006, reason: "", wasClean: false }));
  };

  class FrontcapEventSource extends EventTarget {
    constructor(url, init) {
      super();
      this.url = new Url(url, location.href).href;
      this.withCredentials = !!(init && init.withCredentials);
      this.readyState = 0;
      this._last = "";
      this._retry = 3000;
      this._id = open(this, this.url, "sse");
    }
    close() {
      if (this.readyState === 2) return;
      this.readyState = 2;
      clearTimeout(this._reconnect);
      streams.delete(this._id);
      announce({ id: this._id, ordinal: this._ordinal, op: "close" });
    }
  }
  for (const [k, v] of [["CONNECTING", 0], ["OPEN", 1], ["CLOSED", 2]]) {
    FrontcapEventSource[k] = v;
    FrontcapEventSource.prototype[k] = v;
  }
  for (const t of ["open", "message", "error"]) handlerProperty(FrontcapEventSource.prototype, t);

  class FrontcapWebSocket extends EventTarget {
    constructor(url, protocols) {
      super();
      const u = new Url(url, location.href);
      if (u.protocol === "http:") u.protocol = "ws:";
      if (u.protocol === "https:") u.protocol = "wss:";
      if (u.protocol !== "ws:" && u.protocol !== "wss:") throw new DOMException("The URL's scheme must be either 'ws' or 'wss'.", "SyntaxError");
      if (u.hash) throw new DOMException("The URL contains a fragment identifier.", "SyntaxError");
      this.url = u.href;
      this.protocol = "";
      this.extensions = "";
      this.binaryType = "blob";
      this.bufferedAmount = 0;
      this.readyState = 0;
      this._id = open(this, this.url, "ws");
    }
    send(data) {
      if (this.readyState === 0) throw new DOMException("Still in CONNECTING state.", "InvalidStateError");
      if (this.readyState !== 1) return;
      announce({ id: this._id, ordinal: this._ordinal, op: "send", data: typeof data === "string" ? data : String(data) });
    }
    close(code, reason) {
      if (this.readyState >= 2) return;
      const connecting = this.readyState === 0;
      this.readyState = 2;
      streams.delete(this._id);
      announce({ id: this._id, ordinal: this._ordinal, op: "close", code: code ?? 1000, reason: reason ?? "" });
      // The closing handshake completes at once: there is no server to wait for. Closed while still
      // connecting, the connection failed.
      later(() => {
        this.readyState = 3;
        if (connecting) this.dispatchEvent(new Event("error"));
        this.dispatchEvent(new CloseEvent("close", { code: connecting ? 1006 : code ?? 1000, reason: connecting ? "" : reason ?? "", wasClean: !connecting }));
      });
    }
  }
  for (const [k, v] of [["CONNECTING", 0], ["OPEN", 1], ["CLOSING", 2], ["CLOSED", 3]]) {
    FrontcapWebSocket[k] = v;
    FrontcapWebSocket.prototype[k] = v;
  }
  for (const t of ["open", "message", "error", "close"]) handlerProperty(FrontcapWebSocket.prototype, t);

  // Event number `seq` of stream `id`, dispatched now; false when the page has closed the stream, the
  // event was delivered already, or the stream is not this document's.
  const deliver = (id, seq, e) => {
    const s = streams.get(id);
    if (!s || seq <= s._seq) return false;
    s._seq = seq;
    const ws = s instanceof FrontcapWebSocket;
    if (e.type === "open") {
      s.readyState = 1;
      // Retain the opening in the browser before its callback can destroy this document.
      announce({ id, ordinal: s._ordinal, op: "received_open", sequence: seq });
      s.dispatchEvent(new Event("open"));
    } else if (e.type === "message") {
      if (!ws && e.id != null) s._last = String(e.id);
      s.dispatchEvent(new MessageEvent(e.event || "message", { data: e.data, lastEventId: ws ? "" : s._last, origin: new Url(s.url).origin }));
    } else if (e.type === "error") {
      // The handshake failed: an EventSource does not reconnect; a WebSocket closes next.
      streams.delete(id);
      fail(s);
    } else if (e.type === "close") {
      streams.delete(id);
      if (ws) {
        s.readyState = 3;
        s.dispatchEvent(new CloseEvent("close", { code: e.code ?? 1000, reason: e.reason ?? "", wasClean: (e.code ?? 1000) === 1000 }));
      } else {
        // The server ended the stream: an EventSource reconnects after its retry delay.
        s.readyState = 0;
        s.dispatchEvent(new Event("error"));
        s._reconnect = setTimeout(() => {
          if (s.readyState === 0) s._id = open(s, s.url, "sse", s._last);
        }, s._retry);
      }
    }
    return true;
  };
  const delivered = (id) => (streams.get(id) ? streams.get(id)._seq : null);
  // The logical ordinal remains with the living wrapper through recorder replacement.
  const bind = (id, ordinal) => {
    const s = streams.get(id);
    if (!s) return null; // Closed before its anchor was admitted.
    if (s._ordinal !== undefined && s._ordinal !== ordinal) return false;
    s._ordinal = ordinal;
    return true;
  };
  const owned = (ordinal) => {
    for (const [id, s] of streams) if (s._ordinal === ordinal) return { page_stream: id, last: s._seq };
    return null;
  };
  Object.defineProperty(window, KEY, { value: Object.freeze({ deliver, delivered, bind, owned }), enumerable: false });
  window.EventSource = FrontcapEventSource;
  window.WebSocket = FrontcapWebSocket;
})();
