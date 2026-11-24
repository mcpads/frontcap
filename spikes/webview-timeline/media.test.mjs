import test from 'node:test';
import assert from 'node:assert/strict';
import {observeMedia} from './media.js';

test('detached media preserves native results and observes host progress with page held', () => {
  let host = 0;
  const result = Promise.resolve();
  const failure = new Error('native failure');
  class Media extends EventTarget {
    currentTime = 0; duration = NaN; paused = true;
    play() { if (this.fail) throw failure; return result; }
    pause() { this.paused = true; }
  }
  const scope = {HTMLMediaElement:Media, performance:{now:() => host}};
  const observer = observeMedia(scope);
  scope.performance.now = () => 100;
  const player = new Media();
  assert.equal(player.play(), result);
  player.paused = false;
  player.dispatchEvent(new Event('playing'));
  host = 700;
  player.currentTime = .7;
  const snapshot = observer.snapshot();
  assert.equal(snapshot.hostMs, 700);
  assert.equal(snapshot.players[0].currentTime, .7);
  assert.equal(snapshot.players[0].duration, null);
  assert.equal(snapshot.events[1].pageMs, 100);
  assert.equal(snapshot.events[1].kind, 'playing');
  // Event evidence describes the dispatch boundary, not the later read.
  assert.equal(snapshot.events[1].state.currentTime, 0);
  assert.equal(snapshot.events[1].state.paused, false);
  assert.equal(player.pause(), undefined);
  player.fail = true;
  assert.throws(() => player.play(), error => error === failure);
  // Returned snapshots do not mutate the retained evidence.
  snapshot.events[0].kind = 'changed';
  snapshot.events[1].state.currentTime = 99;
  assert.equal(observer.snapshot().events[0].kind, 'play-call');
  assert.equal(observer.snapshot().events[1].state.currentTime, 0);
});

test('bounded media evidence explicitly reports omitted players and dropped events', () => {
  class Media extends EventTarget { play() {} pause() {} }
  const observer = observeMedia({HTMLMediaElement:Media, performance:{now:() => 0}});
  for (let i = 0; i < 12; i++) {
    const player = new Media();
    player.play();
    player.pause();
  }
  const snapshot = observer.snapshot();
  assert.equal(snapshot.players.length, 8);
  assert.equal(snapshot.omitted, 4);
  assert.equal(snapshot.received, 16);
  assert.equal(snapshot.dropped, 8);
});

test('muted output keeps native playback and completion while refusing an unmute', () => {
  const result = Promise.resolve();
  class Media extends EventTarget {
    #muted = false;
    currentTime = 0; duration = 1; paused = true; ended = false;
    get muted() { return this.#muted; }
    set muted(value) { this.#muted = value; }
    play() { assert.equal(this.muted, true); this.paused = false; return result; }
    pause() { this.paused = true; }
  }
  const observer = observeMedia({HTMLMediaElement:Media, performance:{now:() => 0}},
    {mutedOutput:true});
  const player = new Media();
  assert.equal(player.play(), result);
  player.muted = false;
  assert.equal(player.muted, true);
  player.currentTime = 1;
  player.ended = true;
  player.dispatchEvent(new Event('ended'));
  const snapshot = observer.snapshot();
  assert.equal(snapshot.mutedOutput, true);
  assert.equal(snapshot.controlled, false);
  assert.equal(snapshot.players[0].currentTime, 1);
  assert.equal(snapshot.players[0].ended, true);
  assert.equal(snapshot.events.at(-1).kind, 'ended');
});


test('flow evidence separates player identity, requested source and actual short playback', () => {
  class Media extends EventTarget {
    src = ''; currentSrc = ''; paused = true; ended = false;
    play() {} pause() {}
  }
  const scope = {HTMLMediaElement:Media, performance:{now:() => 0},
    __frontcapDocument:'document-one'};
  const observer = observeMedia(scope);
  const player = new Media();
  player.src = 'https://local.invalid/correct.wav?private=token';
  player.play();
  const requested = observer.snapshot().events.at(-1);
  assert.equal(requested.kind, 'play-call');
  assert.equal(requested.state.sourceId, null);
  const sound = requested.state.requestedSourceId;
  assert.ok(sound > 0);
  player.currentSrc = player.src;
  player.paused = false;
  player.dispatchEvent(new Event('playing'));
  player.ended = true;
  player.dispatchEvent(new Event('ended'));
  const finished = observer.snapshot();
  assert.equal(finished.document, 'document-one');
  assert.deepEqual(finished.events.slice(-2).map(e => [e.kind,e.state.sourceId]),
    [['playing',sound],['ended',sound]]);
  // Both boundaries remain visible even when the first read happens after completion.
  assert.equal(finished.players[0].ended, true);
  player.src = 'https://local.invalid/wrong.wav';
  player.play();
  let state = observer.snapshot().events.at(-1).state;
  assert.equal(state.sourceId, sound); // Old selection until the browser selects the new resource.
  assert.notEqual(state.requestedSourceId, sound);
  const second = new Media();
  second.src = second.currentSrc = 'https://local.invalid/correct.wav?private=token';
  second.play();
  assert.equal(observer.snapshot().players[1].sourceId, sound);
  assert.equal(JSON.stringify(observer.snapshot()).includes('private=token'), false);
  for (let i = 0; i < 70; i++) { player.currentSrc = `https://local.invalid/${i}`; observer.snapshot(); }
  const limited = observer.snapshot();
  assert.equal(limited.sourceLimitReached, true);
  assert.equal(limited.players[0].sourceId, null);
  assert.equal(limited.players[1].sourceId, sound);
});

test('sound occurrence survives a busy event tail without polling or retaining payloads', () => {
  class Media extends EventTarget {
    src = 'local:first'; currentSrc = 'local:first';
    play() {} pause() {}
  }
  const observer = observeMedia({HTMLMediaElement:Media, performance:{now:() => 0}});
  const player = new Media();
  player.play();
  player.dispatchEvent(new Event('playing'));
  player.dispatchEvent(new Event('ended'));
  for (let i = 0; i < 30; i++) player.dispatchEvent(new Event('seeked'));
  const first = observer.snapshot();
  assert.ok(first.events.every(e => e.kind === 'seeked'));
  assert.deepEqual(first.flow.sources, [{sourceId:1, events:{
    'play-call':{count:1,lastSequence:1}, playing:{count:1,lastSequence:2},
    ended:{count:1,lastSequence:3}}}]);
  player.src = 'local:second';
  player.play(); // New request, old selected source; do not invent a native start.
  player.currentSrc = player.src;
  player.dispatchEvent(new Event('playing'));
  player.dispatchEvent(new Event('playing')); // A resume is also a playing event.
  player.dispatchEvent(new Event('error'));
  player.pause();
  player.dispatchEvent(new Event('pause'));
  const second = observer.snapshot();
  assert.equal(second.flow.sources[1].events['play-call'].count, 1);
  assert.equal(second.flow.sources[1].events.playing.count, 2);
  assert.equal(second.flow.sources[1].events.error.count, 1);
  assert.equal(second.flow.sources[1].events.pause.count, 1);
  assert.equal(second.flow.sources[1].events.ended, undefined);
  second.flow.sources[0].events.ended.count = 999;
  assert.equal(observer.snapshot().flow.sources[0].events.ended.count, 1);
  player.currentSrc = '';
  player.dispatchEvent(new Event('error'));
  assert.equal(observer.snapshot().flow.unattributed, 1);
  assert.equal(observer.snapshot().flow.sources.length, 2);
});
