"""Compose a platform-owned main-realm random starting condition before app scripts.

This prepares an injection artifact, not storage, a heap checkpoint or a product recipe.
The existing timeline runs first so its document identity remains fresh in every child.
"""
import argparse
import hashlib
import json
from pathlib import Path


def compose(timeline, seed_source, seed):
    if type(seed) is not int or not 0 <= seed <= 0xffffffff:
        raise ValueError('Seed must be an unsigned 32-bit integer')
    if seed_source.count('__FRONTCAP_SEED__') != 1 or '__FRONTCAP_DIALOG_EVENT__' not in seed_source:
        raise ValueError('Unsupported random source template')
    random = seed_source.replace('__FRONTCAP_SEED__', str(seed)).replace(
        '__FRONTCAP_DIALOG_EVENT__', 'frontcap:starting-randomness')
    observation = '''
;(() => {
  if (Object.hasOwn(globalThis, '__frontcapStartingState')) throw Error('Starting condition already installed');
  const documentIdentity = __frontcapDocument;
  const taints = [];
  let received = 0;
  document.addEventListener('frontcap:starting-randomness', event => {
    const value = JSON.parse(event.detail);
    if (typeof value.taint !== 'string') return;
    received++;
    taints.push(value.taint.slice(0, 256));
    if (taints.length > 16) taints.shift();
  });
  Object.defineProperty(globalThis, '__frontcapStartingState', {value:Object.freeze({
    randomness:() => {
      const calls = globalThis.__frontcapRandomCalls?.snapshot();
      if (!calls) return null; // Missing instrumentation is not zero random activity.
      return {document:documentIdentity, seed:SEED,
        math_calls:calls.counts.math, crypto_byte_calls:calls.counts.bytes,
        crypto_uuid_calls:calls.counts.uuid, observation_errors:calls.errors,
        uncontrolled_calls:received};
    },
    snapshot:() => ({seed:SEED, scope:'main_realm', secureRandomness:false,
      // Same publication boundary as android_page_read.js; never invoke a page getter.
      canvasKit:{document:documentIdentity, published:(() => {
        const value = Object.getOwnPropertyDescriptor(globalThis, 'flutterCanvasKit')?.value;
        return value !== null && typeof value === 'object';
      })()},
      received, dropped:received-taints.length, taints:[...taints],
      randomCalls:globalThis.__frontcapRandomCalls?.snapshot() ?? null,
      resources:globalThis.__frontcapResources?.snapshot() ?? null}),
  }), configurable:false, writable:false});
})();
'''.replace('seed:SEED', 'seed:' + str(seed))
    return timeline + '\n' + observation + '\n' + random + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--timeline', type=Path, required=True)
    parser.add_argument('--seed', type=int, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--trace-random', action='store_true', help='Retain bounded random-call diagnostics')
    parser.add_argument('--trace-resources', action='store_true', help='Observe bounded resource completions, not readiness')
    parser.add_argument('--random-call-window', type=int, nargs=2, metavar=('START_MS','END_MS'), help='Aggregate at most 16 random caller stacks in this half-open page-time window')
    args = parser.parse_args()
    if args.random_call_window and (not args.trace_random or not 0 <= args.random_call_window[0] < args.random_call_window[1]):
        parser.error('--random-call-window requires --trace-random and 0 <= start < end')
    source = Path(__file__).resolve().parents[2] / 'crates/adapter-chromium/src/js/seed.js'
    timeline = args.timeline.read_bytes()
    random = source.read_bytes()
    script = compose(timeline.decode(), random.decode(), args.seed).encode()
    observer = Path(__file__).with_name('random_observer.js').read_bytes() if args.trace_random else None
    if observer is not None and args.random_call_window:
        observer = observer.replace(b'const callWindow = null;', ('const callWindow = ' + json.dumps(args.random_call_window) + ';').encode())
    if observer is not None:
        script += b'\n' + observer
    resources = Path(__file__).with_name('resource_observer.js').read_bytes() if args.trace_resources else None
    if resources is not None:
        # Subscribe on native Performance before the timeline replaces that object.
        script = resources + b'\n' + script
    # Validate all inputs before reserving a new output; never overwrite frozen artifacts.
    args.output.mkdir(parents=True, exist_ok=False)
    target = args.output / 'start.js'
    target.write_bytes(script)
    manifest = dict(kind='main_realm_randomness', seed=args.seed, random_call_window=args.random_call_window,
        timeline_sha256=hashlib.sha256(timeline).hexdigest(),
        random_source_sha256=hashlib.sha256(random).hexdigest(),
        builder_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        script_sha256=hashlib.sha256(script).hexdigest(),
        random_observer_sha256=hashlib.sha256(observer).hexdigest() if observer is not None else None,
        resource_observer_sha256=hashlib.sha256(resources).hexdigest() if resources is not None else None,
        fresh=['document_identity', 'native_generation'],
        excluded=['storage', 'native_randomness', 'workers', 'in_flight_work', 'host_service_inputs'])
    # The manifest is the final artifact; a partial output without it is not usable.
    (args.output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    main()
