// Posts at 100 and 437 ms of its own timers.
setTimeout(() => postMessage("a"), 100);
setTimeout(() => postMessage("b"), 437);
