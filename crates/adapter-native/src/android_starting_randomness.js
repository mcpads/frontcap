// Install after the platform frame producer, before consumer scripts. Not secure randomness.
(() => {
  if (Object.hasOwn(globalThis, '__frontcapStartingRandomness')) throw Error('Randomness already prepared');
  const identity = __frontcapFrames.receipt().document;
  let uncontrolled = 0;
  document.addEventListener('frontcap:starting-randomness', () => { uncontrolled++; });
  __FRONTCAP_RANDOM_SOURCE__
  __FRONTCAP_RANDOM_OBSERVER__
  Object.defineProperty(globalThis, '__frontcapStartingRandomness', {value:Object.freeze({
    snapshot:() => {
      const calls = __frontcapRandomCalls.snapshot();
      return {document:identity, seed:__FRONTCAP_SEED__,
        math_calls:calls.counts.math, crypto_byte_calls:calls.counts.bytes,
        crypto_uuid_calls:calls.counts.uuid, observation_errors:calls.errors,
        uncontrolled_calls:uncontrolled};
    },
  }), configurable:false, writable:false});
})();
