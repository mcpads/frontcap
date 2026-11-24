"""Prepare the clock-measurement fixture without a document script import.

The injected probe retains a host timer only for measuring wall holds. This is
test apparatus, not part of the timeline API or a required consumer dependency.
"""
from pathlib import Path

root = Path(__file__).resolve().parent
fixture = (root / 'fixture.html').read_text()
start = fixture.index('<script>')
end = fixture.index('<style>')
prelude = """globalThis.waitOnHost = (() => {
  const nativeTimeout = globalThis.setTimeout.bind(globalThis);
  return ms => new Promise(resolve => nativeTimeout(resolve, ms));
})();
"""
(root / 'dist/injected-probe.js').write_text(prelude + (root / 'dist/timeline.js').read_text())
(root / 'dist/injected-fixture.html').write_text(fixture[:start] + fixture[end:])
