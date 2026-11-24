#pragma once
// Host visibility of a live external app. No consumer clock, launch, installation or termination.
#include <signal.h>
#include <errno.h>
static BOOL host_state_arguments(NSDictionary *args) {
  if (args.count != 3 || !ui_string(args[@"bundle"],255) ||
      ![@[@"background",@"foreground"] containsObject:args[@"state"]]) return NO;
  NSString *bundle=args[@"bundle"];
  NSRegularExpression *pattern=[NSRegularExpression regularExpressionWithPattern:@"^[A-Za-z0-9_-]+(\\.[A-Za-z0-9_-]+)+$" options:0 error:NULL];
  if ([pattern numberOfMatchesInString:bundle options:0 range:NSMakeRange(0,bundle.length)] != 1) return NO;
  id pid=args[@"pid"];
  return [pid isKindOfClass:NSNumber.class] && CFGetTypeID((__bridge CFTypeRef)pid) != CFBooleanGetTypeID() &&
    !CFNumberIsFloatType((__bridge CFNumberRef)pid) && [pid longLongValue] > 0 && [pid longLongValue] <= INT32_MAX &&
    [pid intValue] != getpid();
}
static NSDictionary *set_host_app_state(NSDictionary *args) {
  XCUIApplication *app=[[XCUIApplication alloc] initWithBundleIdentifier:args[@"bundle"]];
  BOOL foreground=[args[@"state"] isEqual:@"foreground"];
  XCUIApplicationState before=app.state;
  pid_t pid=[args[@"pid"] intValue];
  if (kill(pid,0) != 0 || (foreground ? (before != XCUIApplicationStateRunningBackground &&
      before != XCUIApplicationStateRunningBackgroundSuspended) : before != XCUIApplicationStateRunningForeground))
    return @{@"state":@"unresolved",@"error":@"host_transition_owner_or_state_changed"};
  if (foreground) [app activate];
  else [XCUIDevice.sharedDevice pressButton:XCUIDeviceButtonHome];
  XCTNSPredicateExpectation *expected=[[XCTNSPredicateExpectation alloc]
    initWithPredicate:[NSPredicate predicateWithBlock:^BOOL(id value,NSDictionary *bindings) {
      return foreground ? app.state == XCUIApplicationStateRunningForeground :
        (app.state == XCUIApplicationStateRunningBackground || app.state == XCUIApplicationStateRunningBackgroundSuspended);
    }] object:app];
  XCTWaiterResult waited=[XCTWaiter waitForExpectations:@[expected] timeout:5];
  XCUIApplicationState after=app.state;
  if (waited != XCTWaiterResultCompleted || kill(pid,0) != 0 ||
      (foreground ? after != XCUIApplicationStateRunningForeground :
       (after != XCUIApplicationStateRunningBackground && after != XCUIApplicationStateRunningBackgroundSuspended)))
    return @{@"state":@"unresolved",@"error":@"host_transition_unconfirmed"};
  return @{@"state":@"host_app_state_changed",@"change":@{@"bundle":args[@"bundle"],@"pid":args[@"pid"],
    @"requested":args[@"state"],@"before_state":@(before),@"app_state":@(after)}};
}
