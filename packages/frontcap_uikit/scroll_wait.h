#pragma once
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
// Shared across the test's translation units. One serialized UI owner scopes this flag to its app.
static BOOL begin_uikit_scroll_wait_scope(NSString *bundle) {
  Class process=NSClassFromString(@"XCUIApplicationProcess");
  if (!bundle.length || ![objc_getAssociatedObject(process,NSSelectorFromString(@"frontcapScrollWaitReady")) boolValue] ||
      objc_getAssociatedObject(process,NSSelectorFromString(@"frontcapScrollWaitBundle"))) return NO;
  objc_setAssociatedObject(process,NSSelectorFromString(@"frontcapScrollWaitBundle"),bundle,OBJC_ASSOCIATION_COPY_NONATOMIC);
  return YES;
}
static void end_uikit_scroll_wait_scope(void) {
  objc_setAssociatedObject(NSClassFromString(@"XCUIApplicationProcess"),NSSelectorFromString(@"frontcapScrollWaitBundle"),nil,OBJC_ASSOCIATION_COPY_NONATOMIC);
}
