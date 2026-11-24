// Posts at frame ticks of its own timers: 51 and 101 ms.
setTimeout(() => postMessage("a"), 51);
setTimeout(() => postMessage("b"), 101);
