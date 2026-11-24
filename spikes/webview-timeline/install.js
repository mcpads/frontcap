import '../android-webview-clock/frame-gate.js';
import {installTimeline} from './timeline.js';
import {observeMedia} from './media.js';
import {observePointers} from './pointers.js';
import {observeNavigation} from './navigation.js';
import {observePageEvents} from './page-events.js';
import {observeConsole} from './console.js';
if (Object.hasOwn(globalThis, '__frontcapTime')) throw Error('Timeline already installed');
const media = observeMedia(globalThis, {mutedOutput:globalThis.__frontcapMuteOutput === true});
Object.defineProperty(globalThis, '__frontcapMedia', {
  value:media, configurable:false, writable:false,
});
Object.defineProperty(globalThis, '__frontcapPointers', {
  value:observePointers(globalThis), configurable:false, writable:false,
});
const timeline = installTimeline(globalThis, globalThis.__frontcapFrames,
  globalThis.__frontcapClockConfig ?? {});
Object.defineProperty(globalThis, '__frontcapDocument', {
  value:crypto.randomUUID(), configurable:false, writable:false,
});
Object.defineProperty(globalThis, '__frontcapNavigation', {
  value:observeNavigation(globalThis, globalThis.__frontcapDocument), configurable:false, writable:false,
});
Object.defineProperty(globalThis, '__frontcapTime', {
  value:timeline, configurable:false, writable:false,
});
Object.defineProperty(globalThis, '__frontcapEvents', {
  value:observePageEvents(globalThis), configurable:false, writable:false,
});
Object.defineProperty(globalThis, '__frontcapConsole', {
  value:observeConsole(globalThis, globalThis.__frontcapDocument), configurable:false, writable:false,
});
