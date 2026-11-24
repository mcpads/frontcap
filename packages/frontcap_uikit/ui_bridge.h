#pragma once
// XCTest-thread bridge. The enclosing test installs its scoped animation-wait control.
#import "observed_ui.h"
#import "timed_press.h"
#import "capture.h"
#import "system_alerts.h"
#import "../../spikes/s18-native-time/ios/ui_control.h"
#import <QuartzCore/QuartzCore.h>
#include <signal.h>

static NSDictionary *uikit_metadata(XCUIApplication *app) {
  id value = app.windows[@"frontcap-clock"].value;
  if (![value isKindOfClass:NSString.class]) return nil;
  id result = [NSJSONSerialization JSONObjectWithData:[value dataUsingEncoding:NSUTF8StringEncoding] options:0 error:NULL];
  return [result isKindOfClass:NSDictionary.class] ? result : nil;
}
// Clock identity is independent of focus changes caused by input.
static NSDictionary *uikit_clock(XCUIApplication *app) {
  NSDictionary *metadata = uikit_metadata(app);
  if (!metadata[@"endpoint"] || !metadata[@"now_ns"]) return nil;
  return @{@"endpoint":metadata[@"endpoint"], @"now_ns":metadata[@"now_ns"]};
}
static NSDictionary *uikit_retirement(NSDictionary *endpoint) {
  CFTimeInterval deadline = CACurrentMediaTime() + 5;
  int processError = 0, portError = 0;
  do {
    errno = 0; int process = kill([endpoint[@"pid"] intValue], 0); processError = process ? errno : 0;
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) break;
    struct sockaddr_in address = {.sin_len=sizeof(address), .sin_family=AF_INET,
      .sin_addr.s_addr=htonl(INADDR_LOOPBACK), .sin_port=htons([endpoint[@"port"] intValue])};
    errno = 0; int connection = connect(fd, (struct sockaddr *)&address, sizeof(address));
    portError = connection ? errno : 0; close(fd);
    if (processError == ESRCH && portError == ECONNREFUSED) break;
    [NSThread sleepForTimeInterval:.02];
  } while (CACurrentMediaTime() < deadline);
  return @{@"endpoint":endpoint, @"process_errno":@(processError), @"port_errno":@(portError),
    @"process_absent":processError == ESRCH ? @YES : @NO, @"port_closed":portError == ECONNREFUSED ? @YES : @NO};
}
static BOOL uikit_app_environment(id value) {
  if (![value isKindOfClass:NSDictionary.class] || [value count] > 16) return NO;
  NSCharacterSet *keys = [NSCharacterSet characterSetWithCharactersInString:@"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_"];
  for (id key in value) {
    if (![key isKindOfClass:NSString.class] || ![key length] || [key length] > 64 ||
        [key characterAtIndex:0] < 'A' || [key characterAtIndex:0] > 'Z' ||
        [key rangeOfCharacterFromSet:keys.invertedSet].location != NSNotFound ||
        [key hasPrefix:@"FC_"] || [key hasPrefix:@"DYLD_"] || [key hasPrefix:@"SIMCTL_"]) return NO;
    id text = value[key];
    if (![text isKindOfClass:NSString.class]) return NO;
    NSData *utf8 = [text dataUsingEncoding:NSUTF8StringEncoding];
    if (!utf8 || utf8.length > 2048) return NO;
    for (NSUInteger i=0; i<[text length]; i++) {
      unichar c = [text characterAtIndex:i];
      if (c < 32 || (c >= 127 && c <= 159)) return NO;
    }
  }
  return YES;
}
static BOOL uikit_request(NSDictionary *request) {
  NSString *operation = request[@"operation"];
  if ([operation isEqual:@"launch"]) {
    id args = request[@"arguments"];
    if (![args isKindOfClass:NSDictionary.class] ||
        ([args count] && ([args count] != 1 || !uikit_app_environment(args[@"environment"])))) return NO;
    NSMutableDictionary *envelope = [request mutableCopy];
    envelope[@"arguments"] = @{};
    return ui_request_valid_for_targets(envelope, @[]);
  }
  if ([@[@"terminate", @"finish"] containsObject:operation])
    return [request[@"arguments"] count] == 0 && ui_request_valid_for_targets(request, @[]);
  NSMutableDictionary *envelope = [request mutableCopy];
  envelope[@"operation"] = @"finish"; envelope[@"arguments"] = @{};
  if (!ui_request_valid_for_targets(envelope, @[])) return NO;
  NSDictionary *args = request[@"arguments"];
  if (![args isKindOfClass:NSDictionary.class]) return NO;
  if ([operation isEqual:@"set_app_state"]) {
    if (args.count != 2 || ![@[@"foreground",@"background"] containsObject:args[@"state"]]) return NO;
    NSMutableDictionary *boundRequest=[request mutableCopy];
    boundRequest[@"operation"]=@"restart";boundRequest[@"arguments"]=@{@"app":args[@"app"] ?: NSNull.null};
    return uikit_request(boundRequest);
  }
  if ([operation isEqual:@"restart"]) {
    NSDictionary *bound=args[@"app"];
    if ((args.count != 1 && !(args.count == 2 && args[@"url"])) || ![bound isKindOfClass:NSDictionary.class] || bound.count != 3 ||
        !uikit_press_integer(bound[@"pid"],1) || [bound[@"pid"] longLongValue] > INT_MAX) return NO;
    for (NSString *key in @[@"generation",@"simulator"]) {
      if (!ui_string(bound[key],36) || ![[[[NSUUID alloc] initWithUUIDString:bound[key]] UUIDString].lowercaseString
          isEqual:[bound[key] lowercaseString]]) return NO;
    }
    if (args[@"url"]) {
      id url = args[@"url"];
      if (!ui_string(url,8192) || [url lengthOfBytesUsingEncoding:NSUTF8StringEncoding] > 8192 ||
          [url rangeOfCharacterFromSet:[NSCharacterSet whitespaceAndNewlineCharacterSet]].location != NSNotFound ||
          [url rangeOfCharacterFromSet:[NSCharacterSet controlCharacterSet]].location != NSNotFound) return NO;
      NSRegularExpression *pattern=[NSRegularExpression regularExpressionWithPattern:@"^[A-Za-z][A-Za-z0-9+.-]{0,63}:.+$" options:0 error:NULL];
      NSTextCheckingResult *match=[pattern firstMatchInString:url options:0 range:NSMakeRange(0,[url length])];
      if (!match || match.range.length != [url length] || ![NSURL URLWithString:url]) return NO;
    }
    return [bound[@"simulator"] caseInsensitiveCompare:NSProcessInfo.processInfo.environment[@"SIMULATOR_UDID"]] == NSOrderedSame;
  }
  if ([operation isEqual:@"observe_targets"] || [operation isEqual:@"observe_system_alerts"]) return args.count == 0;
  BOOL press = [operation isEqual:@"press_target"];
  BOOL scroll = [operation isEqual:@"scroll_target"];
  if (scroll && ![@[@"up",@"down",@"left",@"right"] containsObject:args[@"direction"]]) return NO;
  BOOL edit = [operation isEqual:@"replace_text"];
  if (edit && (!uikit_press_integer(args[@"start"],0) || !uikit_press_integer(args[@"end"],0) ||
      [args[@"start"] longLongValue] > [args[@"end"] longLongValue] || [args[@"end"] longLongValue] > 16384 ||
      ![args[@"text"] isKindOfClass:NSString.class] || [args[@"text"] length] > 16384 ||
      ![args[@"text"] dataUsingEncoding:NSUTF8StringEncoding])) return NO;
  if (press) {
    NSDictionary *clock = args[@"clock_request"];
    if (![clock isKindOfClass:NSDictionary.class] || clock.count != 4 ||
        !ui_string(clock[@"generation"],36) ||
        ![[[[NSUUID alloc] initWithUUIDString:clock[@"generation"]] UUIDString].lowercaseString isEqual:clock[@"generation"]] ||
        ![clock[@"operation"] isEqual:@"advance"] || !uikit_press_integer(clock[@"request_id"],1) ||
        !uikit_press_integer(clock[@"delta_ns"],0)) return NO;
  }
  if ((!press && !edit && !scroll && ![operation isEqual:@"tap_target"] && ![operation isEqual:@"tap_system_button"]) || args.count != (press || scroll ? 3 : edit ? 5 : 2) ||
      !ui_string(args[@"observation"], 36) || !ui_string(args[@"reference"], 42)) return NO;
  NSString *observation = args[@"observation"], *reference = args[@"reference"];
  NSUUID *uuid = [[NSUUID alloc] initWithUUIDString:observation];
  NSString *prefix = [observation stringByAppendingString:@"-"];
  if (![uuid.UUIDString.lowercaseString isEqual:observation] || ![reference hasPrefix:prefix]) return NO;
  NSString *number = [reference substringFromIndex:prefix.length];
  NSInteger index = number.integerValue;
  return index >= 0 && index < 2048 && [[NSString stringWithFormat:@"%ld", (long)index] isEqual:number];
}
static NSArray *uikit_capture(XCUIApplication *app, NSString *directory, NSString *observation) {
  NSData *source = [NSJSONSerialization dataWithJSONObject:uikit_metadata(app) ?: @{}
    options:NSJSONWritingPrettyPrinted error:NULL];
  NSString *description = [NSString stringWithFormat:@"%@\nFrontcap source diagnostics:\n%@\n",
    app.debugDescription, [[NSString alloc] initWithData:source encoding:NSUTF8StringEncoding]];
  return uikit_capture_description(description,directory,observation);
}
static void serve_uikit_ui(NSString *bundle, NSArray *arguments, NSDictionary *environment, NSString *directory) {
  if (![NSFileManager.defaultManager createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:NULL]) abort();
  XCUIApplication *app = [[XCUIApplication alloc] initWithBundleIdentifier:bundle];
  __block NSDictionary *endpoint;
  __block NSDictionary *textEndpoint;
  __block int64_t nextTextRequest = 1;
  __block FCObservedUI *ui;
  FCSystemAlerts *systemAlerts=[FCSystemAlerts new];
  __block NSDictionary *diagnosticRequest;
  void (^phase)(NSString *) = ^(NSString *name) {
    // Bounded phase markers survive a killed host bridge in the owned XCTest log.
    // Starting a phase is not proof of its completion or permission to retry input.
    NSDictionary *event = @{@"generation":diagnosticRequest[@"generation"] ?: NSNull.null,
      @"request_id":diagnosticRequest[@"request_id"] ?: NSNull.null,
      @"operation":diagnosticRequest[@"operation"] ?: NSNull.null, @"phase":name};
    NSData *data = [NSJSONSerialization dataWithJSONObject:event options:0 error:NULL];
    if (data) NSLog(@"FRONTCAP_UI_PHASE %@", [[NSString alloc] initWithData:data encoding:NSUTF8StringEncoding]);
  };
  BOOL canOpenURL = NO;
  if (@available(iOS 16.4, *)) canOpenURL = YES;
  @try {
    serve_ui_with_capabilities(directory, -1, 256*1024, 1024*1024, @{@"system_alerts":@YES,@"app_restart":@YES,@"app_entry":@(canOpenURL),@"app_lifecycle":@YES}, ^BOOL(NSDictionary *request) {
      diagnosticRequest = [request copy];
      if ([request[@"operation"] isEqual:@"restart"] && request[@"arguments"][@"url"] && !canOpenURL) return NO;
      return uikit_request(request);
    }, ^NSDictionary *(NSString *operation, NSDictionary *args) {
      phase(@"operation-start");
      if ([operation isEqual:@"observe_system_alerts"]) {
        ui=nil;
        return [systemAlerts observe:^NSArray *(NSString *observation,NSString *description) {
          return uikit_capture_description(description,directory,observation);
        }];
      }
      if ([operation isEqual:@"tap_system_button"]) { ui=nil; return [systemAlerts tap:args]; }
      [systemAlerts invalidate];
      if ([operation isEqual:@"launch"] || [operation isEqual:@"restart"]) {
        BOOL restart=[operation isEqual:@"restart"];
        NSDictionary *retirement;
        if (restart) {
          NSDictionary *bound=args[@"app"];
          if (!endpoint || ![endpoint[@"generation"] isEqual:bound[@"generation"]] ||
              ![endpoint[@"pid"] isEqual:bound[@"pid"]] || ![uikit_clock(app)[@"endpoint"] isEqual:endpoint])
            return @{@"state":@"unresolved",@"error":@"restart_app_identity_changed"};
          phase(@"restart-terminate");
          [app terminate]; ui=nil;
          retirement=uikit_retirement(endpoint);
          if (![retirement[@"process_absent"] boolValue] || ![retirement[@"port_closed"] boolValue])
            return @{@"state":@"unresolved",@"error":@"restart_retirement_unconfirmed",@"retirement":retirement};
        } else {
          app.launchArguments = arguments;
          NSMutableDictionary *env = [environment mutableCopy];
          [env addEntriesFromDictionary:args[@"environment"] ?: @{}];
          [env addEntriesFromDictionary:@{@"FC_MANUAL":@"1", @"FC_UC_HOST_TIME":@"1", @"FC_UI_INSPECT":@"1", @"FC_UI_EDIT":@"1",
            @"DYLD_INSERT_LIBRARIES":@"@executable_path/vclock.dylib:@executable_path/ui_schedule.dylib"}];
          app.launchEnvironment = env;
        }
        phase(restart ? @"restart-launch" : @"initial-launch");
        if (restart && args[@"url"]) {
          phase(@"restart-open-url");
          if (@available(iOS 16.4, *)) [app openURL:[NSURL URLWithString:args[@"url"]]];
          else return @{@"state":@"unresolved",@"error":@"url_launch_unavailable"};
        }
        else [app launch];
        if (![app.windows[@"frontcap-clock"] waitForExistenceWithTimeout:10])
          return @{@"state":@"unresolved", @"error":@"clock_window_unavailable"};
        NSDictionary *reads = uikit_clock(app); endpoint = reads[@"endpoint"];
        textEndpoint = uikit_metadata(app)[@"text_endpoint"];
        nextTextRequest = 1;
        if (!endpoint || !reads[@"now_ns"]) return @{@"state":@"unresolved", @"error":@"clock_identity_unavailable"};
        ui = [[FCObservedUI alloc] initWithApplication:app];
        if (restart) {
          if ([endpoint[@"generation"] isEqual:retirement[@"endpoint"][@"generation"]] ||
              [endpoint[@"pid"] isEqual:retirement[@"endpoint"][@"pid"]] || [reads[@"now_ns"] longLongValue] != 0)
            return @{@"state":@"unresolved",@"error":@"restart_identity_or_clock_differs"};
          NSMutableDictionary *result=[@{@"state":@"restarted",@"retirement":retirement,@"reads":reads} mutableCopy];
          if (args[@"url"]) result[@"url"]=args[@"url"];
          return result;
        }
        return @{@"state":@"launched", @"reads":reads};
      }
      if ([@[@"terminate", @"finish"] containsObject:operation]) {
        [app terminate]; ui = nil;
        return @{@"state":@"terminated", @"retirement":uikit_retirement(endpoint)};
      }
      if ([operation isEqual:@"set_app_state"]) {
        NSDictionary *bound=args[@"app"];
        BOOL foreground=[args[@"state"] isEqual:@"foreground"];
        XCUIApplicationState beforeState=app.state;
        if (!endpoint || ![endpoint[@"generation"] isEqual:bound[@"generation"]] ||
            ![endpoint[@"pid"] isEqual:bound[@"pid"]] || kill([endpoint[@"pid"] intValue],0) != 0 ||
            (foreground ? (beforeState != XCUIApplicationStateRunningBackground && beforeState != XCUIApplicationStateRunningBackgroundSuspended)
                : beforeState != XCUIApplicationStateRunningForeground))
          return @{@"state":@"unresolved",@"error":@"host_transition_owner_or_state_changed"};
        if (!foreground && ![uikit_clock(app)[@"endpoint"] isEqual:endpoint])
          return @{@"state":@"unresolved",@"error":@"host_transition_clock_changed"};
        ui=nil;
        phase(foreground ? @"host-foreground" : @"host-background");
        if (foreground) [app activate];
        else [XCUIDevice.sharedDevice pressButton:XCUIDeviceButtonHome];
        // XCTest delivers visibility updates through its run loop. Use its bounded waiter,
        // as the WebView runner does, rather than blocking that loop with thread sleeps.
        NSMutableArray *states=[NSMutableArray new];
        XCTNSPredicateExpectation *settled=[[XCTNSPredicateExpectation alloc]
          initWithPredicate:[NSPredicate predicateWithBlock:^BOOL(id value, NSDictionary *bindings) {
            XCUIApplicationState state=app.state;
            if (states.count < 64) [states addObject:@{@"state":@(state),@"host_epoch_s":@(NSDate.date.timeIntervalSince1970)}];
            return foreground ? state == XCUIApplicationStateRunningForeground :
              (state == XCUIApplicationStateRunningBackground || state == XCUIApplicationStateRunningBackgroundSuspended);
          }] object:app];
        XCTWaiterResult waited=[XCTWaiter waitForExpectations:@[settled] timeout:5];
        XCUIApplicationState observed=app.state;
        errno=0;int process=kill([endpoint[@"pid"] intValue],0);int processError=process ? errno : 0;
        NSDictionary *diagnostics=@{@"request_id":diagnosticRequest[@"request_id"],@"requested":args[@"state"],
          @"before_state":@(beforeState),@"app_state":@(observed),@"waiter_result":@(waited),
          @"process_errno":@(processError),@"states":states};
        NSData *diagnosticData=[NSJSONSerialization dataWithJSONObject:diagnostics options:0 error:NULL];
        if (diagnosticData) NSLog(@"FRONTCAP_UI_HOST_STATE %@",[[NSString alloc] initWithData:diagnosticData encoding:NSUTF8StringEncoding]);
        if (waited != XCTWaiterResultCompleted || process != 0 || (foreground ? observed != XCUIApplicationStateRunningForeground :
            (observed != XCUIApplicationStateRunningBackground && observed != XCUIApplicationStateRunningBackgroundSuspended)))
          return @{@"state":@"unresolved",@"error":@"host_transition_unconfirmed",@"diagnostics":diagnostics};
        id now=NSNull.null;
        if (foreground) {
          NSDictionary *reads=uikit_clock(app);
          if (![reads[@"endpoint"] isEqual:endpoint] || !reads[@"now_ns"])
            return @{@"state":@"unresolved",@"error":@"foreground_clock_changed"};
          now=reads[@"now_ns"];
          ui=[[FCObservedUI alloc] initWithApplication:app];
        }
        return @{@"state":@"app_state_changed",@"app":endpoint,@"change":@{
          @"requested":args[@"state"],@"before_state":@(beforeState),@"app_state":@(observed),@"now_ns":now}};
      }
      phase(@"metadata-before");
      NSDictionary *metadata = uikit_metadata(app);
      phase(@"clock-before");
      NSDictionary *before = uikit_clock(app);
      phase(@"foreground-check");
      XCUIApplicationState observedState = app.state;
      BOOL sameEndpoint = [before[@"endpoint"] isEqual:endpoint];
      if (observedState != XCUIApplicationStateRunningForeground || !sameEndpoint)
        return @{@"state":@"unresolved", @"error":sameEndpoint ? @"app_not_foreground" : @"app_identity_changed",
          @"diagnostics":@{@"application_state":@(observedState),
            @"expected_endpoint":endpoint ?: NSNull.null,
            @"observed_endpoint":before[@"endpoint"] ?: NSNull.null,
            @"metadata_available":metadata ? @YES : @NO,
            @"clock_available":before ? @YES : @NO}};
      NSDictionary *focus = metadata[@"text_input"];
      if (!focus || !metadata[@"transitioning"] || !metadata[@"scrolls"]) return @{@"state":@"unresolved", @"error":@"text_focus_unavailable"};
      NSDictionary *context = @{@"app":endpoint, @"now_ns":before[@"now_ns"], @"text_input":focus, @"transitioning":metadata[@"transitioning"], @"scrolls":metadata[@"scrolls"]};
      if ([operation isEqual:@"observe_targets"]) {
        if (!ui) ui=[[FCObservedUI alloc] initWithApplication:app];
        for (NSDictionary *scroll in metadata[@"scrolls"]) {
          if ([scroll[@"dragging"] boolValue] || [scroll[@"decelerating"] boolValue]) {
            ui = [[FCObservedUI alloc] initWithApplication:app];
            return @{@"state":@"unavailable",@"app":endpoint,@"now_ns":before[@"now_ns"],
              @"reason":@"scroll_in_motion",@"artifacts":@[]};
          }
        }
        phase(@"observe-catalog");
        NSDictionary *snapshot = [ui observeWithContext:context];
        if (![snapshot[@"state"] isEqual:@"observed"]) return snapshot;
        phase(@"capture-artifacts");
        NSArray *artifacts = uikit_capture(app, directory, snapshot[@"observation"]);
        phase(@"metadata-after");
        NSDictionary *metadataAfter=uikit_metadata(app);
        NSDictionary *after=metadataAfter[@"endpoint"] && metadataAfter[@"now_ns"] ?
          @{@"endpoint":metadataAfter[@"endpoint"],@"now_ns":metadataAfter[@"now_ns"]} : nil;
        BOOL sameClock=[before isEqual:after];
        BOOL sameScrolls=[metadata[@"scrolls"] isEqual:metadataAfter[@"scrolls"]];
        BOOL sameFocus=[focus isEqual:metadataAfter[@"text_input"]];
        BOOL sameTransition=[metadata[@"transitioning"] isEqual:metadataAfter[@"transitioning"]];
        phase(@"revalidate-catalog");
        BOOL current=[ui isCurrentObservation:snapshot[@"observation"] context:context];
        phase(@"observation-result");
        if (!sameClock || !sameScrolls || !sameFocus || !sameTransition || !current) {
          NSDictionary *check=@{@"clock_matches":@(sameClock),@"scrolls_match":@(sameScrolls),
            @"focus_matches":@(sameFocus),@"transition_matches":@(sameTransition),
            @"before_metadata":metadata ?: @{},@"after_metadata":metadataAfter ?: @{},
            @"catalog_check":[ui lastObservationCheck] ?: @{}};
          NSString *path=[directory stringByAppendingPathComponent:[NSString stringWithFormat:@"capture-check-%@.json",snapshot[@"observation"]]];
          [[NSJSONSerialization dataWithJSONObject:check options:NSJSONWritingPrettyPrinted error:NULL] writeToFile:path options:NSDataWritingAtomic error:NULL];
          ui = [[FCObservedUI alloc] initWithApplication:app];
          if (![after[@"endpoint"] isEqual:endpoint]) return @{@"state":@"unresolved",@"error":@"capture_owner_changed",@"artifacts":artifacts};
          return @{@"state":@"unavailable",@"app":endpoint,@"now_ns":after[@"now_ns"],@"reason":@"capture_changed",@"artifacts":artifacts};
        }
        NSDictionary *catalog = @{@"observation":snapshot[@"observation"], @"keyboard":snapshot[@"keyboard"], @"targets":snapshot[@"targets"], @"text_input":focus, @"transitioning":metadata[@"transitioning"], @"scrolls":metadata[@"scrolls"]};
        NSDictionary *result = @{@"state":@"observed", @"app":endpoint, @"now_ns":after[@"now_ns"], @"catalog":catalog, @"artifacts":artifacts};
        // Leave room for the transport receipt envelope inside the one-MiB response budget.
        if ([NSJSONSerialization dataWithJSONObject:result options:0 error:NULL].length > 900*1024) {
          ui = [[FCObservedUI alloc] initWithApplication:app];
          return @{@"state":@"unresolved", @"error":@"catalog_response_budget_exceeded", @"artifacts":artifacts};
        }
        return result;
      }
      BOOL press = [operation isEqual:@"press_target"];
      BOOL scroll = [operation isEqual:@"scroll_target"];
      BOOL edit = [operation isEqual:@"replace_text"];
      NSDictionary *input;
      if ([metadata[@"transitioning"] boolValue]) {
        ui = [[FCObservedUI alloc] initWithApplication:app];
        input = @{@"input":@"not_applied", @"reason":@"transition_in_progress"};
      } else if (press) {
        input = [ui pressReference:args[@"reference"] observation:args[@"observation"] context:context
          endpoint:endpoint clockRequest:args[@"clock_request"] boundary:metadata[@"input_boundary"]];
      } else if (scroll) {
        input = [ui scrollReference:args[@"reference"] observation:args[@"observation"] context:context direction:args[@"direction"] bundle:bundle];
      } else if (edit) {
        if (![metadata[@"text_endpoint"] isEqual:textEndpoint] || ![textEndpoint[@"pid"] isEqual:endpoint[@"pid"]])
          return @{@"input":@"unresolved", @"error":@"text_owner_changed"};
        input = [ui replaceReference:args[@"reference"] observation:args[@"observation"] context:context
          arguments:args endpoint:textEndpoint receipt:metadata[@"text_receipt"] nextRequest:&nextTextRequest];
      } else {
        input = [ui tapReference:args[@"reference"] observation:args[@"observation"] context:context];
      }
      NSDictionary *after = uikit_clock(app);
      BOOL timed = [input[@"input"] isEqual:@"xctest_timed_press"];
      if (![after[@"endpoint"] isEqual:endpoint] ||
          (timed ? (![input[@"clock_reply"][@"before_ns"] isEqual:before[@"now_ns"]] ||
                    ![input[@"clock_reply"][@"now_ns"] isEqual:after[@"now_ns"]]) : ![before isEqual:after]))
        return @{@"input":@"unresolved", @"error":@"app_or_clock_changed_during_input",
          @"delivery":input, @"before":before, @"after":after ?: (id)NSNull.null};
      NSMutableDictionary *result = [input mutableCopy];
      if (press) result[@"clock_request"] = args[@"clock_request"];
      if (edit) result[@"now_ns"] = after[@"now_ns"];
      if (scroll && [input[@"input"] isEqual:@"xctest_scroll"]) {
        NSDictionary *scrolled;
        for (NSDictionary *row in uikit_metadata(app)[@"scrolls"])
          if ([row[@"token"] isEqual:input[@"before"][@"token"]]) scrolled=row;
        if (!scrolled) return @{@"input":@"unresolved",@"error":@"scroll_owner_changed",@"delivery":input};
        result[@"after"]=scrolled;result[@"now_ns"]=after[@"now_ns"];
      }
      [result addEntriesFromDictionary:@{@"app":endpoint, @"reference":args[@"reference"], @"observation":args[@"observation"]}];
      return result;
    });
  } @finally { [app terminate]; }
}
