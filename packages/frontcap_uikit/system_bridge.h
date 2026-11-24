#pragma once
// A platform-owned runner may inspect system alerts without launching or binding a consumer app.
#import "system_alerts.h"
#import "capture.h"
#import "../../spikes/s18-native-time/ios/ui_control.h"
#import "host_state.h"

static BOOL system_ui_request(NSDictionary *request) {
  NSMutableDictionary *envelope=[request mutableCopy];
  envelope[@"operation"]=@"finish"; envelope[@"arguments"]=@{};
  if (!ui_request_valid_for_targets(envelope,@[])) return NO;
  NSString *operation=request[@"operation"];
  NSDictionary *args=request[@"arguments"];
  if (![args isKindOfClass:NSDictionary.class]) return NO;
  if ([operation isEqual:@"set_host_app_state"]) return host_state_arguments(args);
  if ([@[@"observe_system_alerts",@"finish"] containsObject:operation]) return args.count==0;
  if (![operation isEqual:@"tap_system_button"] || args.count!=2 ||
      !ui_string(args[@"observation"],36) || !ui_string(args[@"reference"],42)) return NO;
  NSString *observation=args[@"observation"], *reference=args[@"reference"];
  if (![[[NSUUID alloc] initWithUUIDString:observation].UUIDString.lowercaseString isEqual:observation]) return NO;
  NSString *prefix=[observation stringByAppendingString:@"-"];
  if (![reference hasPrefix:prefix]) return NO;
  NSString *number=[reference substringFromIndex:prefix.length]; NSInteger index=number.integerValue;
  return index>=0 && index<2048 && [[NSString stringWithFormat:@"%ld",(long)index] isEqual:number];
}
static void serve_system_ui(NSString *directory) {
  if (![NSFileManager.defaultManager createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:NULL]) abort();
  FCSystemAlerts *alerts=[FCSystemAlerts new];
  serve_ui_session(directory,-1,256*1024,1024*1024,@{@"system_alerts":@YES,@"system_only":@YES,@"host_app_state":@YES},YES,
    ^BOOL(NSDictionary *request) { return system_ui_request(request); },
    ^NSDictionary *(NSString *operation,NSDictionary *arguments) {
      if ([operation isEqual:@"set_host_app_state"]) { [alerts invalidate]; return set_host_app_state(arguments); }
      if ([operation isEqual:@"observe_system_alerts"])
        return [alerts observe:^NSArray *(NSString *observation,NSString *description) {
          return uikit_capture_description(description,directory,observation);
        }];
      if ([operation isEqual:@"tap_system_button"]) return [alerts tap:arguments];
      [alerts invalidate];
      return @{@"state":@"terminated"}; // Finish only this runner; never terminate an app.
    });
}
