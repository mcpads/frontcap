#import "observed_ui.h"
#import "timed_press.h"
#import "text_delivery.h"
#import "scroll_wait.h"
#include <math.h>

@interface FCObservedUI ()
@property(nonatomic, strong) XCUIApplication *app;
@property(nonatomic, strong) NSDictionary *basis;
@property(nonatomic, strong) NSDictionary *observationCheck;
@end

@implementation FCObservedUI
- (instancetype)initWithApplication:(XCUIApplication *)app {
  if ((self = [super init])) _app = app;
  return self;
}
- (NSString *)role:(XCUIElementType)type {
  switch (type) {
    case XCUIElementTypeTable: return @"table";
    case XCUIElementTypeCollectionView: return @"collection_view";
    case XCUIElementTypeScrollView: return @"scroll_view";
    case XCUIElementTypeButton: return @"button";
    case XCUIElementTypeStaticText: return @"static_text";
    case XCUIElementTypeTextField: return @"text_field";
    case XCUIElementTypeSecureTextField: return @"secure_text_field";
    case XCUIElementTypeTextView: return @"text_view";
    case XCUIElementTypeCell: return @"cell";
    case XCUIElementTypeLink: return @"link";
    case XCUIElementTypeSwitch: return @"switch";
    default: return nil;
  }
}
- (XCUIElementQuery *)query:(NSDictionary *)target {
  NSPredicate *predicate = [target[@"selector_key"] isEqual:@"identifier"]
    ? [NSPredicate predicateWithFormat:@"identifier == %@", target[@"selector_value"]]
    : [NSPredicate predicateWithFormat:@"label == %@", target[@"selector_value"]];
  return [[self.app descendantsMatchingType:[target[@"type"] unsignedIntegerValue]] matchingPredicate:predicate];
}
- (NSArray *)snapshotRows {
  NSError *error;
  id<XCUIElementSnapshot> root = [self.app snapshotWithError:&error];
  if (!root) return nil;
  NSMutableArray *pending = [NSMutableArray arrayWithObject:root], *rows = [NSMutableArray new];
  NSUInteger visited = 0;
  while (pending.count) {
    id<XCUIElementSnapshot> element = pending.lastObject;
    [pending removeLastObject];
    if (++visited > 2048) return nil; // Never truncate the projected catalog.
    CGRect frame = element.frame;
    BOOL valid = isfinite(frame.origin.x) && isfinite(frame.origin.y) &&
      isfinite(frame.size.width) && isfinite(frame.size.height) && frame.size.width > 0 && frame.size.height > 0;
    BOOL onScreen = valid && CGRectIntersectsRect(root.frame, frame);
    // UIKit virtualizes offscreen cells. Their child snapshots may alias recycled views,
    // including another row's text and frame. Keep the cell for discovery, but do not
    // publish those unrealized descendants. Live queries still check global ambiguity.
    if (element.elementType != XCUIElementTypeCell || onScreen)
      [pending addObjectsFromArray:element.children.reverseObjectEnumerator.allObjects];
    NSString *role = [self role:element.elementType];
    if (!role) continue;
    NSString *identifier = element.identifier ?: @"", *label = element.label ?: @"";
    NSString *key = identifier.length ? @"identifier" : @"label";
    NSString *value = identifier.length ? identifier : label;
    id text = element.value;
    if (![text isKindOfClass:NSString.class] && ![text isKindOfClass:NSNumber.class]) text = NSNull.null;
    [rows addObject:@{@"role":role, @"type":@(element.elementType),
      @"identifier":identifier, @"label":label, @"value":text,
      @"selector_key":key, @"selector_value":value, @"unique":@NO,
      @"enabled":@(element.enabled), @"selected":@(element.selected),
      @"hittable":@NO, @"intersects_screen":(onScreen ? @YES : @NO),
      @"frame_mpt":valid ? @[@(llround(frame.origin.x*1000)), @(llround(frame.origin.y*1000)),
        @(llround(frame.size.width*1000)), @(llround(frame.size.height*1000))] : @[@0,@0,@0,@0]}];
  }
  return rows;
}
- (NSArray *)rows {
  NSArray *rows = [self snapshotRows];
  if (!rows) return nil;
  NSMutableArray *catalog = [NSMutableArray new];
  for (NSDictionary *row in rows) {
    NSUInteger matches = 0;
    for (NSDictionary *candidate in rows)
      if ([candidate[@"type"] isEqual:row[@"type"]] &&
          [candidate[row[@"selector_key"]] isEqual:row[@"selector_value"]]) matches++;
    NSMutableDictionary *target = [row mutableCopy];
    BOOL unique = [row[@"selector_value"] length] > 0 && matches == 1;
    target[@"unique"] = @(unique);
    // Snapshot attributes are read once. Only usable on-screen selectors need a live hit test.
    if (![row[@"role"] isEqual:@"static_text"] && unique && [row[@"enabled"] boolValue] && [row[@"intersects_screen"] boolValue]) {
      XCUIElementQuery *query = [self query:row];
      target[@"hittable"] = (query.count == 1 && query.element.hittable ? @YES : @NO);
    }
    [catalog addObject:target];
  }
  return catalog;
}
- (NSDictionary *)observeWithContext:(NSDictionary *)context {
  self.basis = nil;
  if (!context.count || self.app.state != XCUIApplicationStateRunningForeground)
    return @{@"state":@"unavailable", @"reason":@"app_context_unavailable"};
  NSArray *rows = [self rows];
  if (!rows) return @{@"state":@"unavailable", @"reason":@"hierarchy_unavailable_or_over_budget"};
  NSString *capture = NSUUID.UUID.UUIDString.lowercaseString;
  NSMutableArray *targets = [NSMutableArray new];
  for (NSDictionary *row in rows) {
    NSMutableDictionary *target = [row mutableCopy];
    target[@"reference"] = [NSString stringWithFormat:@"%@-%lu", capture, (unsigned long)targets.count];
    [targets addObject:target];
  }
  self.basis = @{@"state":@"observed", @"observation":capture, @"context":[context copy],
    @"keyboard":(self.app.keyboards.count > 0 ? @YES : @NO), @"targets":targets, @"rows":rows};
  NSMutableDictionary *result = [self.basis mutableCopy];
  [result removeObjectForKey:@"rows"];
  return result;
}
- (NSDictionary *)lastObservationCheck { return self.observationCheck; }
- (BOOL)isCurrentObservation:(NSString *)observation context:(NSDictionary *)context {
  NSDictionary *basis = self.basis;
  BOOL foreground=self.app.state == XCUIApplicationStateRunningForeground;
  NSArray *current=foreground && basis ? [self rows] : nil;
  BOOL captured=basis && [basis[@"observation"] isEqual:observation];
  BOOL sameContext=[basis[@"context"] isEqual:context];
  BOOL sameRows=[basis[@"rows"] isEqual:current];
  BOOL sameKeyboard=foreground && basis && [basis[@"keyboard"] boolValue] == (self.app.keyboards.count > 0);
  NSMutableDictionary *check=[@{@"capture_matches":@(captured),@"context_matches":@(sameContext),
    @"foreground":@(foreground),@"rows_match":@(sameRows),@"keyboard_matches":@(sameKeyboard),
    @"before_rows":basis[@"rows"] ?: @[],@"after_rows":current ?: @[]} mutableCopy];
  if (foreground && basis && !sameRows) {
    // Diagnostic only: no live selector, hit test or input between these two snapshots.
    // They never replace the rejected basis or authorize a subsequent action.
    NSArray *first=[self snapshotRows], *second=[self snapshotRows];
    check[@"snapshot_only_before"]=first ?: @[];
    check[@"snapshot_only_after"]=second ?: @[];
    check[@"snapshot_only_matches"]=@(first && second && [first isEqual:second]);
  }
  self.observationCheck=check;
  return captured && sameContext && foreground && sameRows && sameKeyboard;
}
- (NSDictionary *)deliverReference:(NSString *)reference observation:(NSString *)observation
                          context:(NSDictionary *)context delivery:(NSDictionary *(^)(XCUIElement *))delivery {
  NSDictionary *basis = self.basis;
  self.basis = nil; // Every attempt consumes its basis; durable retry belongs to the request owner.
  NSString *reason;
  NSDictionary *target;
  if (!basis || ![basis[@"observation"] isEqual:observation] || ![basis[@"context"] isEqual:context])
    reason = @"stale_observation";
  if (!reason) {
    for (NSDictionary *row in basis[@"targets"]) if ([row[@"reference"] isEqual:reference]) target = row;
    if (!target) reason = @"unknown_target";
    else if ([target[@"role"] isEqual:@"static_text"]) reason = @"unsupported_target";
    else if (![target[@"unique"] boolValue]) reason = @"ambiguous_target";
    else if (![target[@"enabled"] boolValue]) reason = @"disabled_target";
    else if (![target[@"hittable"] boolValue]) reason = @"unhittable_target";
  }
  if (!reason) {
    if (self.app.state != XCUIApplicationStateRunningForeground ||
        ![basis[@"rows"] isEqual:[self rows]] ||
        [basis[@"keyboard"] boolValue] != (self.app.keyboards.count > 0)) reason = @"observation_changed";
  }
  XCUIElement *resolved;
  if (!reason) {
    XCUIElementQuery *query = [self query:target];
    if (query.count != 1) reason = @"ambiguous_target";
    else {
      resolved = query.element;
      if (!resolved.enabled || !resolved.hittable) reason = @"unhittable_target";
    }
  }
  if (reason) return @{@"input":@"not_applied", @"reason":reason};
  return delivery(resolved); // Delivery errors leave dispatch uncertain.
}
- (NSDictionary *)tapReference:(NSString *)reference observation:(NSString *)observation
                      context:(NSDictionary *)context {
  return [self deliverReference:reference observation:observation context:context delivery:^NSDictionary *(XCUIElement *target) {
    [target tap];
    return @{@"input":@"xctest_tap", @"reference":reference, @"observation":observation};
  }];
}
- (NSDictionary *)pressReference:(NSString *)reference observation:(NSString *)observation
                        context:(NSDictionary *)context endpoint:(NSDictionary *)endpoint
                   clockRequest:(NSDictionary *)request boundary:(NSDictionary *)boundary {
  return [self deliverReference:reference observation:observation context:context delivery:^NSDictionary *(XCUIElement *target) {
    if (target.elementType != XCUIElementTypeTextField && target.elementType != XCUIElementTypeTextView)
      return @{@"input":@"not_applied", @"reason":@"unsupported_target"};
    NSMutableDictionary *result = [press_uikit_target(target,endpoint,request,boundary) mutableCopy];
    [result addEntriesFromDictionary:@{@"reference":reference, @"observation":observation}];
    return result;
  }];
}
- (NSDictionary *)replaceReference:(NSString *)reference observation:(NSString *)observation
                          context:(NSDictionary *)context arguments:(NSDictionary *)arguments
                         endpoint:(NSDictionary *)endpoint receipt:(NSDictionary *)receipt
                      nextRequest:(int64_t *)nextRequest {
  return [self deliverReference:reference observation:observation context:context delivery:^NSDictionary *(XCUIElement *target) {
    NSDictionary *focus = context[@"text_input"];
    NSString *role = target.elementType == XCUIElementTypeTextField ? @"text_field" :
      target.elementType == XCUIElementTypeTextView ? @"text_view" : nil;
    NSString *key = target.identifier.length ? @"identifier" : @"label";
    NSString *selector = target.identifier.length ? target.identifier : target.label;
    if (!role || ![focus[@"state"] isEqual:@"focused"] || ![focus[@"role"] isEqual:role] ||
        !selector.length || ![focus[key] isEqual:selector] || [focus[@"marked"] boolValue])
      return @{@"input":@"not_applied", @"reason":@"input_unavailable"};
    if (!endpoint || !receipt || *nextRequest <= 0 || *nextRequest == INT64_MAX)
      return @{@"input":@"unresolved", @"error":@"text_owner_unavailable"};
    NSDictionary *request = @{@"generation":endpoint[@"generation"], @"request_id":@((*nextRequest)++),
      @"operation":@"replace_text", @"arguments":@{@"basis":focus,
        @"start":arguments[@"start"], @"end":arguments[@"end"], @"text":arguments[@"text"]}};
    return deliver_uikit_text(endpoint,receipt,request);
  }];
}
- (NSDictionary *)scrollReference:(NSString *)reference observation:(NSString *)observation
                          context:(NSDictionary *)context direction:(NSString *)direction bundle:(NSString *)bundle {
  return [self deliverReference:reference observation:observation context:context delivery:^NSDictionary *(XCUIElement *target) {
    NSString *role=[self role:target.elementType];
    NSString *key=target.identifier.length ? @"identifier" : @"label";
    NSString *selector=target.identifier.length ? target.identifier : target.label;
    NSMutableArray *matches=[NSMutableArray new];
    for (NSDictionary *row in context[@"scrolls"])
      if ([row[@"role"] isEqual:role] && [row[key] isEqual:selector]) [matches addObject:row];
    if (matches.count!=1) return @{@"input":@"not_applied",@"reason":@"scroll_unavailable"};
    NSDictionary *before=matches[0];
    BOOL forward=[@[@"down",@"right"] containsObject:direction];
    NSUInteger axis=[@[@"up",@"down"] containsObject:direction] ? 1 : 0;
    BOOL ready=[before[@"enabled"] boolValue] && ![before[@"dragging"] boolValue] && ![before[@"decelerating"] boolValue];
    for (NSUInteger i=0;i<2;i++) ready=ready && [before[@"viewport_mpt"][i] longLongValue]>0 &&
      [before[@"offset_mpt"][i] longLongValue]>=[before[@"minimum_mpt"][i] longLongValue] &&
      [before[@"offset_mpt"][i] longLongValue]<=[before[@"maximum_mpt"][i] longLongValue];
    int64_t offset=[before[@"offset_mpt"][axis] longLongValue];
    ready=ready && (forward ? offset<[before[@"maximum_mpt"][axis] longLongValue] : offset>[before[@"minimum_mpt"][axis] longLongValue]);
    if (!ready) return @{@"input":@"not_applied",@"reason":@"scroll_unavailable"};
    if (!begin_uikit_scroll_wait_scope(bundle)) return @{@"input":@"not_applied",@"reason":@"scroll_unavailable"};
    @try {
    if ([direction isEqual:@"down"]) [target swipeUpWithVelocity:XCUIGestureVelocitySlow];
    else if ([direction isEqual:@"up"]) [target swipeDownWithVelocity:XCUIGestureVelocitySlow];
    else if ([direction isEqual:@"right"]) [target swipeLeftWithVelocity:XCUIGestureVelocitySlow];
    else [target swipeRightWithVelocity:XCUIGestureVelocitySlow];
    } @finally { end_uikit_scroll_wait_scope(); }
    return @{@"input":@"xctest_scroll",@"direction":direction,@"before":before};
  }];
}
@end
