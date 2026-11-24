import test from 'node:test';
import assert from 'node:assert/strict';
import {observeNavigation} from './navigation.js';

const documentId = 'abcd0000-0000-0000-0000-000000000001';
function host(id = documentId) {
  const listeners = new Map();
  const calls = [];
  const scope = {location:{href:'http://localhost/start'}, document:{hasFocus:() => true},
    performance:{now:() => 50}, addEventListener(kind, callback) {listeners.set(kind, callback);}};
  scope.history = {
    pushState(...args) {
      assert.equal(this, scope.history);
      if (args[2] === 'invalid') throw Error('Original history failure');
      calls.push(args);
      scope.location.href = args[2];
      return 'original result';
    },
    replaceState(...args) {return this.pushState(...args);},
  };
  // Real History methods do not call each other's public wrapper.
  scope.history.replaceState = scope.history.pushState;
  const navigation = observeNavigation(scope, id);
  return {scope, calls, navigation, fire(kind, data = {}) {listeners.get(kind)({isTrusted:true, ...data});}};
}

test('history results, arguments and exceptions survive observation; reads do no app work', () => {
  const h = host();
  const state = {ordinary:'app state'};
  assert.equal(h.scope.history.pushState(state, '', 'http://localhost/next'), 'original result');
  assert.equal(h.calls[0][0], state);
  assert.throws(() => h.scope.history.pushState(null, '', 'invalid'), /Original history failure/);
  h.scope.history.replaceState(null, '', 'http://localhost/edited');
  h.fire('popstate');h.fire('hashchange', {isTrusted:false});
  h.scope.document.hasFocus = () => false;h.fire('blur');
  h.fire('pagehide', {persisted:true});
  const before = h.navigation.snapshot();
  assert.deepEqual(before.events.map(e => e.kind), ['push_state','replace_state','popstate','hashchange','blur','pagehide']);
  assert.equal(before.current.focused, false);
  assert.equal(before.events[0].trusted, null);
  assert.equal(before.events[3].trusted, false);
  assert.equal(before.events.at(-1).persisted, true);
  assert.ok(before.events.every(e => e.elapsedMs === 50 && !Object.hasOwn(e, 'state')));
  assert.deepEqual(h.navigation.snapshot(), before);
  assert.equal(h.calls.length, 2);
  before.events[0].href = 'mutated copy';
  assert.equal(h.navigation.snapshot().events[0].href, 'http://localhost/next');
  h.scope.history.pushState = () => {};
  assert.equal(h.navigation.snapshot().historyObserved, false);
});

test('tails, UTF-16 URL bounds and isolated documents preserve explicit missing evidence', () => {
  const h = host();
  h.scope.location.href = 'x'.repeat(2047)+'🚆';
  for (let i = 0; i < 12; i++) h.fire('focus');
  const value = h.navigation.snapshot();
  assert.equal(value.received, 12);assert.equal(value.dropped, 4);
  assert.deepEqual(value.events.map(e => e.sequence), [5,6,7,8,9,10,11,12]);
  assert.equal(value.current.href, 'x'.repeat(2047));assert.equal(value.current.hrefTruncated, true);
  assert.ok(value.events.every(e => e.href === value.current.href && e.hrefTruncated));
  const other = host('abcd0000-0000-0000-0000-000000000002').navigation.snapshot();
  assert.notEqual(other.document, value.document);assert.equal(other.received, 0);
  assert.equal(other.current.href, 'http://localhost/start');
});

test('an observation failure cannot change a successful consumer history effect', () => {
  const h = host();
  h.scope.document.hasFocus = () => {throw Error('focus sample unavailable');};
  assert.equal(h.scope.history.pushState(null, '', 'http://localhost/applied'), 'original result');
  const value = h.navigation.snapshot();
  assert.equal(h.calls.length, 1);assert.equal(h.scope.location.href, 'http://localhost/applied');
  assert.match(value.failure, /focus sample unavailable/);assert.equal(value.current, null);
  h.scope.document.hasFocus = () => true;
  h.fire('focus');
  assert.equal(h.navigation.snapshot().received, 0);
  assert.match(h.navigation.snapshot().failure, /focus sample unavailable/);
  const replaced = host();
  Object.defineProperty(replaced.scope.history, 'pushState', {get() {throw Error('history unavailable');}});
  const unavailable = replaced.navigation.snapshot();
  assert.equal(unavailable.historyObserved, false);
  assert.match(unavailable.failure, /history unavailable/);
  assert.equal(unavailable.current.href, 'http://localhost/start');
});
