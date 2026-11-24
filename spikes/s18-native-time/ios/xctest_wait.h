#import <XCTest/XCTest.h>
#import <objc/runtime.h>
#import <objc/message.h>
#include <stdatomic.h>
#include <string.h>
#ifndef FC_XCTEST_BUNDLE
#define FC_XCTEST_BUNDLE @"dev.frontcap.s18.probe"
#endif

// XCTest controller only. Keep event-loop quiescence checks, but do not wait
// for animations whose application clock is intentionally frozen.
static _Atomic int manual_input;
static void (*wait_animations)(id, SEL, BOOL);
static void (*wait_animations_event)(id, SEL, BOOL, BOOL);
static BOOL is_manual_fixture(id process) {
  SEL selector = NSSelectorFromString(@"bundleID");
  return manual_input && [process respondsToSelector:selector] &&
    [((id (*)(id, SEL))objc_msgSend)(process, selector) isEqualToString:FC_XCTEST_BUNDLE];
}
static void wait_without_frozen_animations(id process, SEL selector, BOOL animations) {
  wait_animations(process, selector, is_manual_fixture(process) ? NO : animations);
}
static void wait_without_frozen_animations_event(id process, SEL selector, BOOL animations, BOOL before) {
  // A delivered scroll may remain kinetically active while app time is held. Its caller observes
  // that state explicitly; XCTest must not turn post-event global idleness into completion.
  if (!before && is_manual_fixture(process) &&
      [objc_getAssociatedObject(NSClassFromString(@"XCUIApplicationProcess"),NSSelectorFromString(@"frontcapScrollWaitBundle")) isEqual:FC_XCTEST_BUNDLE]) return;
  wait_animations_event(process, selector, is_manual_fixture(process) ? NO : animations, before);
}
static BOOL valid_wait_signature(Method method, unsigned arguments) {
  if (!method || method_getNumberOfArguments(method) != arguments) return NO;
  char type[16];
  method_getReturnType(method, type, sizeof(type));
  if (strcmp(type, "v")) return NO;
  for (unsigned i = 2; i < arguments; i++) {
    method_getArgumentType(method, i, type, sizeof(type));
    if (strcmp(type, "B") && strcmp(type, "c")) return NO;
  }
  return YES;
}
static BOOL install_animation_wait_control(void) {
  static BOOL installed;
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    Class process = NSClassFromString(@"XCUIApplicationProcess");
    Method simple = class_getInstanceMethod(process, NSSelectorFromString(@"waitForQuiescenceIncludingAnimationsIdle:"));
    Method event = class_getInstanceMethod(process, NSSelectorFromString(@"waitForQuiescenceIncludingAnimationsIdle:isPreEvent:"));
    if (valid_wait_signature(simple, 3)) {
      wait_animations = (void (*)(id, SEL, BOOL))method_setImplementation(simple, (IMP)wait_without_frozen_animations);
      installed = YES;
    }
    if (valid_wait_signature(event, 4)) {
      wait_animations_event = (void (*)(id, SEL, BOOL, BOOL))method_setImplementation(event, (IMP)wait_without_frozen_animations_event);
      objc_setAssociatedObject(process,NSSelectorFromString(@"frontcapScrollWaitReady"),@YES,OBJC_ASSOCIATION_RETAIN_NONATOMIC);
      installed = YES;
    }
  });
  return installed;
}
