#pragma once
// Platform-owned XCTest interaction with observed SpringBoard alerts. Never launches SpringBoard.
#import <XCTest/XCTest.h>
#include <math.h>
#include <limits.h>

static void system_alert_failure(NSString *reason) {
  @throw [NSException exceptionWithName:@"SystemAlertObservation" reason:reason userInfo:nil];
}
static NSString *system_alert_text(id value, NSUInteger *budget) {
  if (![value isKindOfClass:NSString.class] || [value length] > 1024 || [value length] > *budget)
    system_alert_failure(@"System alert text exceeds budget");
  *budget -= [value length];
  return value;
}
static NSArray *system_alert_frame(CGRect frame) {
  double values[] = {frame.origin.x,frame.origin.y,frame.size.width,frame.size.height};
  NSMutableArray *result = [NSMutableArray new];
  for (NSUInteger i=0; i<4; i++) {
    double n = round(values[i]*1000);
    if (!isfinite(n) || n < -INT_MAX || n > INT_MAX || (i>=2 && n<0))
      system_alert_failure(@"Invalid system alert frame");
    [result addObject:@((int32_t)n)];
  }
  return result;
}
static BOOL system_button_same(NSDictionary *a, NSDictionary *b) {
  return [a[@"identifier"] isEqual:b[@"identifier"]] && [a[@"label"] isEqual:b[@"label"]]
    && [a[@"frame_mpt"] isEqual:b[@"frame_mpt"]];
}

@interface FCSystemAlerts : NSObject
@property(nonatomic,strong) XCUIApplication *application;
@property(nonatomic,strong) NSDictionary *latest;
@property(nonatomic,strong) NSArray *basis;
- (NSDictionary *)observe:(NSArray *(^)(NSString *, NSString *))capture;
- (NSDictionary *)tap:(NSDictionary *)arguments;
- (void)invalidate;
@end
@implementation FCSystemAlerts
- (instancetype)init {
  if ((self=[super init])) _application=[[XCUIApplication alloc] initWithBundleIdentifier:@"com.apple.springboard"];
  return self;
}
- (void)invalidate { self.latest=nil; self.basis=nil; }
- (NSArray *)snapshot {
  XCUIElementQuery *alerts=self.application.alerts;
  if (alerts.count>8) system_alert_failure(@"Too many system alerts");
  NSMutableArray *result=[NSMutableArray new]; NSUInteger budget=65536;
  for (XCUIElement *alert in alerts.allElementsBoundByIndex) {
    if (alert.staticTexts.count>16 || alert.buttons.count>16) system_alert_failure(@"System alert inventory exceeds budget");
    NSMutableArray *texts=[NSMutableArray new], *buttons=[NSMutableArray new];
    NSString *label=system_alert_text(alert.label,&budget);
    NSArray *frame=system_alert_frame(alert.frame);
    for (XCUIElement *text in alert.staticTexts.allElementsBoundByIndex)
      [texts addObject:system_alert_text(text.label,&budget)];
    for (XCUIElement *button in alert.buttons.allElementsBoundByIndex) {
      NSString *identifier=system_alert_text(button.identifier,&budget);
      NSString *title=system_alert_text(button.label,&budget);
      NSArray *bounds=system_alert_frame(button.frame);
      BOOL enabled=button.enabled, hittable=button.hittable;
      [buttons addObject:[@{@"identifier":identifier,@"label":title,@"frame_mpt":bounds,
        @"enabled":@(enabled),@"hittable":hittable && enabled && [bounds[2] intValue]>0 && [bounds[3] intValue]>0 ? @YES : @NO} mutableCopy]];
    }
    [result addObject:@{@"label":label,@"texts":texts,@"frame_mpt":frame,@"buttons":buttons}];
  }
  for (NSDictionary *alert in result) for (NSMutableDictionary *button in alert[@"buttons"]) {
    NSUInteger matches=0;
    for (NSDictionary *otherAlert in result) for (NSDictionary *other in otherAlert[@"buttons"])
      if (system_button_same(button,other)) matches++;
    button[@"unique"]=matches==1 ? @YES : @NO;
    if (matches!=1) button[@"hittable"]=@NO;
  }
  return result;
}
- (NSDictionary *)observe:(NSArray *(^)(NSString *, NSString *))capture {
  [self invalidate];
  NSArray *before=[self snapshot];
  NSString *observation=NSUUID.UUID.UUIDString.lowercaseString;
  NSArray *artifacts=capture(observation,self.application.debugDescription);
  BOOL stable=[before isEqual:[self snapshot]];
  NSMutableArray *alerts=[NSMutableArray new]; NSUInteger index=0;
  for (NSDictionary *source in before) {
    NSMutableDictionary *alert=[source mutableCopy]; NSMutableArray *buttons=[NSMutableArray new];
    for (NSDictionary *sourceButton in source[@"buttons"]) {
      NSMutableDictionary *button=[sourceButton mutableCopy];
      button[@"reference"]=[NSString stringWithFormat:@"%@-%lu",observation,(unsigned long)index++];
      [buttons addObject:button];
    }
    alert[@"buttons"]=buttons; [alerts addObject:alert];
  }
  NSDictionary *catalog=@{@"observation":observation,@"bundle":@"com.apple.springboard",@"stable":@(stable),@"alerts":alerts};
  if (stable) { self.latest=catalog; self.basis=before; }
  return @{@"state":@"system_observed",@"catalog":catalog,@"artifacts":artifacts};
}
- (NSDictionary *)tap:(NSDictionary *)arguments {
  NSDictionary *(^rejected)(NSString *)=^NSDictionary *(NSString *reason) {
    [self invalidate];
    return @{@"input":@"not_applied",@"reference":arguments[@"reference"],@"observation":arguments[@"observation"],@"reason":reason};
  };
  if (![self.latest[@"observation"] isEqual:arguments[@"observation"]]) return rejected(@"stale_observation");
  NSUInteger alertIndex=0, buttonIndex=0; NSDictionary *selected=nil;
  for (NSDictionary *alert in self.latest[@"alerts"]) {
    buttonIndex=0;
    for (NSDictionary *button in alert[@"buttons"]) {
      if ([button[@"reference"] isEqual:arguments[@"reference"]]) { selected=button; break; }
      buttonIndex++;
    }
    if (selected) break;
    alertIndex++;
  }
  if (!selected) return rejected(@"unknown_target");
  if (![selected[@"unique"] boolValue]) return rejected(@"ambiguous_target");
  if (![selected[@"enabled"] boolValue]) return rejected(@"disabled_target");
  if (![selected[@"hittable"] boolValue]) return rejected(@"unhittable_target");
  if (![self.basis isEqual:[self snapshot]]) return rejected(@"observation_changed");
  XCUIElementQuery *alerts=self.application.alerts;
  if (alerts.count<=alertIndex) return rejected(@"observation_changed");
  XCUIElementQuery *buttons=[alerts elementBoundByIndex:alertIndex].buttons;
  if (buttons.count<=buttonIndex) return rejected(@"observation_changed");
  XCUIElement *button=[buttons elementBoundByIndex:buttonIndex];
  if (!button.exists || !button.enabled || !button.hittable ||
      ![button.identifier isEqual:selected[@"identifier"]] || ![button.label isEqual:selected[@"label"]] ||
      ![system_alert_frame(button.frame) isEqual:selected[@"frame_mpt"]]) return rejected(@"observation_changed");
  [self invalidate]; // Delivery is irreversible; the same observation never authorizes another tap.
  [button tap];
  return @{@"input":@"xctest_system_tap",@"reference":arguments[@"reference"],@"observation":arguments[@"observation"]};
}
@end
