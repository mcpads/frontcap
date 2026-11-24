#pragma once
// The caller reserves the exact clock request. This helper never allocates IDs or retries input.
#import <XCTest/XCTest.h>
#import <QuartzCore/QuartzCore.h>
#import "../../spikes/s18-native-time/ios/control_transport.h"

static BOOL uikit_press_integer(id value, int64_t minimum) {
  return [value isKindOfClass:NSNumber.class] && CFGetTypeID((__bridge CFTypeRef)value) != CFBooleanGetTypeID() &&
    !CFNumberIsFloatType((__bridge CFNumberRef)value) && [value longLongValue] >= minimum &&
    [value compare:@([value longLongValue])] == NSOrderedSame;
}

static NSDictionary *uikit_input_boundary(NSDictionary *descriptor) {
  NSData *data = [NSData dataWithContentsOfFile:descriptor[@"path"]];
  if (!data || data.length > 16384) return nil;
  id value = [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
  return [value isKindOfClass:NSDictionary.class] &&
    [value[@"generation"] isEqual:descriptor[@"generation"]] ? value : nil;
}
static NSDictionary *uikit_control_exchange_bounded(NSDictionary *endpoint, NSDictionary *request, uint32_t requestLimit, uint32_t replyLimit) {
  int fd = socket(AF_INET, SOCK_STREAM, 0);
  if (fd < 0) return nil;
  @try {
    struct sockaddr_in address = {.sin_len=sizeof(address), .sin_family=AF_INET,
      .sin_addr.s_addr=htonl(INADDR_LOOPBACK), .sin_port=htons([endpoint[@"port"] intValue])};
    if (!control_socket_options(fd) || connect(fd,(struct sockaddr *)&address,sizeof(address)) ||
        !control_send_bounded(fd,request,requestLimit)) return nil;
    return control_receive_bounded(fd,replyLimit);
  } @finally { close(fd); }
}
static NSDictionary *uikit_control_exchange(NSDictionary *endpoint, NSDictionary *request) {
  return uikit_control_exchange_bounded(endpoint,request,8192,8192);
}
static BOOL uikit_touch_matches(NSDictionary *record, NSString *key, NSString *selector, NSString *role, NSInteger phase) {
  NSArray *touches = record[@"touches"];
  return touches.count == 1 && [touches[0][@"phase"] integerValue] == phase &&
    [touches[0][key] isEqual:selector] && [touches[0][@"input_role"] isEqual:role];
}
static NSDictionary *press_uikit_target(XCUIElement *target, NSDictionary *endpoint,
                                        NSDictionary *request, NSDictionary *boundary) {
  NSDictionary *before = uikit_input_boundary(boundary);
  NSString *key = target.identifier.length ? @"input_identifier" : @"input_label";
  NSString *role = target.elementType == XCUIElementTypeTextField ? @"text_field" : @"text_view";
  NSString *selector = target.identifier.length ? target.identifier : target.label;
  if (!before || !selector.length || request.count != 4 || ![boundary[@"generation"] isEqual:endpoint[@"generation"]] ||
      ![request[@"generation"] isEqual:endpoint[@"generation"]] ||
      ![request[@"operation"] isEqual:@"advance"] ||
      !uikit_press_integer(request[@"request_id"],1) || !uikit_press_integer(request[@"delta_ns"],0))
    return @{@"input":@"not_applied", @"reason":@"invalid_press_context"};
  __block NSDictionary *began, *reply;
  dispatch_group_t control = dispatch_group_create();
  dispatch_group_async(control, dispatch_get_global_queue(QOS_CLASS_USER_INITIATED,0), ^{
    CFTimeInterval deadline = CACurrentMediaTime()+3;
    while (CACurrentMediaTime() < deadline) {
      NSDictionary *record = uikit_input_boundary(boundary);
      if ([record[@"serial"] unsignedLongLongValue] > [before[@"serial"] unsignedLongLongValue]) {
        if ([record[@"serial"] unsignedLongLongValue] != [before[@"serial"] unsignedLongLongValue]+1 ||
            !uikit_touch_matches(record,key,selector,role,UITouchPhaseBegan)) return;
        began = record;
        reply = uikit_control_exchange(endpoint,request);
        return;
      }
      [NSThread sleepForTimeInterval:.001];
    }
  });
  @try { [target pressForDuration:1]; }
  @finally { dispatch_group_wait(control, DISPATCH_TIME_FOREVER); }
  NSDictionary *ended = uikit_input_boundary(boundary);
  BOOL ordered = began && reply && ended && [reply[@"request"] isEqual:request] &&
    [reply[@"replayed"] isEqual:@NO] &&
    [reply[@"generation"] isEqual:endpoint[@"generation"]] && [reply[@"pid"] isEqual:endpoint[@"pid"]] &&
    [reply[@"last_id"] isEqual:request[@"request_id"]] && [reply[@"status"] isEqual:@"ok"] &&
    [reply[@"applied"] boolValue] && [reply[@"advance_result"] isEqual:@0] && [reply[@"drain_result"] isEqual:@0] &&
    [reply[@"before_ns"] isEqual:began[@"now_ns"]] &&
    [reply[@"now_ns"] unsignedLongLongValue] == [began[@"now_ns"] unsignedLongLongValue]+[request[@"delta_ns"] unsignedLongLongValue] &&
    [ended[@"now_ns"] isEqual:reply[@"now_ns"]] &&
    [ended[@"serial"] unsignedLongLongValue] > [began[@"serial"] unsignedLongLongValue] &&
    uikit_touch_matches(ended,key,selector,role,UITouchPhaseEnded);
  return @{@"input":ordered ? @"xctest_timed_press" : @"unresolved", @"before":before,
    @"began":began ?: NSNull.null, @"ended":ended ?: NSNull.null,
    @"clock_request":request, @"clock_reply":reply ?: NSNull.null};
}
