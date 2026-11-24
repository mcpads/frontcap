// Installed consumer checks. Selectors belong to consumer tests, never the shared implementation.
#import "observed_ui.h"
static NSDictionary *observed_target(NSDictionary *snapshot, NSString *name) {
  for (NSDictionary *target in snapshot[@"targets"])
    if (![target[@"role"] isEqual:@"static_text"] &&
        ([target[@"identifier"] isEqual:name] || [target[@"label"] isEqual:name])) return target;
  return nil;
}
static NSDictionary *check_observed_ui(XCUIApplication *app, NSString *action, NSString *disabled, BOOL requireListBoundaries) {
  FCObservedUI *ui = [[FCObservedUI alloc] initWithApplication:app];
  NSDictionary *context = @{@"generation":NSUUID.UUID.UUIDString.lowercaseString};
  NSDictionary *first = [ui observeWithContext:context];
  NSString *diagnostic = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/observed-ui.json"];
  [[NSJSONSerialization dataWithJSONObject:first options:NSJSONWritingPrettyPrinted error:nil] writeToFile:diagnostic atomically:YES];
  NSCAssert([first[@"state"] isEqual:@"observed"], @"App must be observable");
  NSCAssert(CFGetTypeID((__bridge CFTypeRef)first[@"keyboard"]) == CFBooleanGetTypeID(), @"Keyboard state must be Boolean");
  for (NSDictionary *row in first[@"targets"])
    for (NSString *key in @[@"unique", @"enabled", @"selected", @"hittable", @"intersects_screen"])
      NSCAssert(CFGetTypeID((__bridge CFTypeRef)row[key]) == CFBooleanGetTypeID(), @"Target flags must be Boolean");
  NSDictionary *target = observed_target(first, action);
  NSCAssert(target && [target[@"unique"] boolValue] && [target[@"hittable"] boolValue], @"Action must be unique and ready");
  NSDictionary *readOnly;
  for (NSDictionary *row in first[@"targets"])
    if ([row[@"role"] isEqual:@"static_text"] && [row[@"label"] length]) { readOnly = row; break; }
  NSCAssert(readOnly && ![readOnly[@"hittable"] boolValue], @"Consumer must expose read-only text");
  NSDictionary *readOnlyResult = [ui tapReference:readOnly[@"reference"] observation:first[@"observation"] context:context];
  NSCAssert([readOnlyResult[@"reason"] isEqual:@"unsupported_target"], @"Static text must not dispatch a tap");
  NSDictionary *second = [ui observeWithContext:context];
  NSDictionary *stale = [ui tapReference:target[@"reference"] observation:first[@"observation"] context:context];
  NSCAssert([stale[@"reason"] isEqual:@"stale_observation"], @"Re-observation invalidates old references");
  id disabledResult = NSNull.null;
  if (disabled) {
    NSDictionary *snapshot = [ui observeWithContext:context];
    NSDictionary *button = observed_target(snapshot, disabled);
    NSCAssert(button && ![button[@"enabled"] boolValue], @"Expected disabled control");
    disabledResult = [ui tapReference:button[@"reference"] observation:snapshot[@"observation"] context:context];
    NSCAssert([disabledResult[@"reason"] isEqual:@"disabled_target"], @"Disabled control must reject before action");
  }
  NSMutableArray *boundaryResults = [NSMutableArray new];
  if (requireListBoundaries) {
    for (NSString *reason in @[@"ambiguous_target", @"unhittable_target"]) {
      NSDictionary *snapshot = [ui observeWithContext:context], *selected;
      for (NSDictionary *row in snapshot[@"targets"]) {
        if ([row[@"role"] isEqual:@"static_text"]) continue;
        BOOL ambiguous = ![row[@"unique"] boolValue] && [row[@"selector_value"] length] > 0;
        BOOL offscreen = [row[@"unique"] boolValue] && [row[@"enabled"] boolValue] && ![row[@"hittable"] boolValue];
        if (([reason isEqual:@"ambiguous_target"] && ambiguous) ||
            ([reason isEqual:@"unhittable_target"] && offscreen)) { selected = row; break; }
      }
      NSCAssert(selected, @"Consumer must expose boundary %@; catalog retained in observed-ui.json", reason);
      NSDictionary *outcome = [ui tapReference:selected[@"reference"] observation:snapshot[@"observation"] context:context];
      NSCAssert([outcome[@"reason"] isEqual:reason], @"Unsafe target must reject before dispatch");
      [boundaryResults addObject:@{@"target":selected, @"outcome":outcome}];
    }
  }
  NSDictionary *fresh = [ui observeWithContext:context];
  target = observed_target(fresh, action);
  NSDictionary *wrongGeneration = [ui tapReference:target[@"reference"] observation:fresh[@"observation"]
    context:@{@"generation":NSUUID.UUID.UUIDString.lowercaseString}];
  NSCAssert([wrongGeneration[@"reason"] isEqual:@"stale_observation"], @"Generation change must invalidate input");
  fresh = [ui observeWithContext:context]; target = observed_target(fresh, action);
  NSDictionary *applied = [ui tapReference:target[@"reference"] observation:fresh[@"observation"] context:context];
  NSCAssert([applied[@"input"] isEqual:@"xctest_tap"], @"Fresh observed action must dispatch");
  NSDictionary *reused = [ui tapReference:target[@"reference"] observation:fresh[@"observation"] context:context];
  NSCAssert([reused[@"reason"] isEqual:@"stale_observation"], @"Consumed observation cannot dispatch twice");
  return @{@"initial":first, @"second":second, @"stale":stale, @"disabled":disabledResult,
    @"read_only":readOnly, @"read_only_result":readOnlyResult,
    @"boundaries":boundaryResults, @"wrong_generation":wrongGeneration, @"applied":applied, @"reused":reused};
}
