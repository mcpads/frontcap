#pragma once
// XCTest-owned UI operations. This loop runs on the test's existing execution thread,
// never in the injected application and never on the native clock control thread.
#import "control_transport.h"
#include <poll.h>

static void save_ui_receipt(NSString *directory, NSDictionary *value) {
  NSData *data = [NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingPrettyPrinted error:NULL];
  if (![data writeToFile:[directory stringByAppendingPathComponent:@"ui-control.json"] options:NSDataWritingAtomic error:NULL])
    @throw [NSException exceptionWithName:@"UIControl" reason:@"Cannot persist UI receipt" userInfo:nil];
}
static BOOL ui_string(id value, NSUInteger limit) {
  return [value isKindOfClass:NSString.class] && [value length] && [value length] <= limit;
}
static void ui_checkpoint(NSString *directory, NSString *phase, NSDictionary *request) {
  if (![@"tap" isEqual:request[@"operation"]] || ![phase isEqual:request[@"arguments"][@"stop_at"]]) return;
  NSString *path = [directory stringByAppendingPathComponent:@"ui-checkpoint.json"];
  NSMutableDictionary *marker = [@{@"pid":@(getpid()), @"phase":phase, @"request":request,
    @"status":@"waiting", @"wait_s":@10} mutableCopy];
  NSData *data = [NSJSONSerialization dataWithJSONObject:marker options:0 error:NULL];
  if (![data writeToFile:path options:NSDataWritingAtomic error:NULL]) abort();
  poll(NULL, 0, 10000);
  marker[@"status"] = @"expired";
  data = [NSJSONSerialization dataWithJSONObject:marker options:0 error:NULL];
  [data writeToFile:path options:NSDataWritingAtomic error:NULL];
  _Exit(78); // A missed kill is a failed fault injection, never a successful operation.
}
static BOOL ui_request_valid_for_targets(NSDictionary *request, NSArray *targets) {
  id number = request[@"request_id"];
  NSDictionary *args = request[@"arguments"];
  if (request.count != 4 || !ui_string(request[@"generation"], 36) ||
      ![number isKindOfClass:NSNumber.class] || CFGetTypeID((__bridge CFTypeRef)number) == CFBooleanGetTypeID() ||
      CFNumberIsFloatType((__bridge CFNumberRef)number) || [number longLongValue] < 1 ||
      [number compare:@([number longLongValue])] != NSOrderedSame || ![args isKindOfClass:NSDictionary.class]) return NO;
  NSString *operation = request[@"operation"];
  if (!ui_string(operation, 20)) return NO;
  if ([@"launch" isEqual:operation]) return args.count == 0 || (args.count == 1 &&
    [@[@"before-advance", @"after-advance", @"after-receipt"] containsObject:args[@"clock_stop_at"]]);
  if ([@[@"terminate", @"finish"] containsObject:operation]) return args.count == 0;
  if ([@"tap" isEqual:operation]) {
    for (id key in args) if (![@[@"target", @"label", @"stop_at"] containsObject:key]) return NO;
    return [targets containsObject:args[@"target"]] &&
      (!args[@"label"] || ui_string(args[@"label"], 80)) &&
      (!args[@"stop_at"] || [@[@"before-tap", @"after-tap", @"after-receipt"] containsObject:args[@"stop_at"]]);
  }
  if ([@"capture" isEqual:operation]) {
    NSCharacterSet *invalid = [[NSCharacterSet characterSetWithCharactersInString:@"abcdefghijklmnopqrstuvwxyz0123456789-"] invertedSet];
    return args.count == 3 && ui_string(args[@"name"], 80) &&
      [args[@"name"] rangeOfCharacterFromSet:invalid].location == NSNotFound &&
      ui_string(args[@"clock"], 80) && ui_string(args[@"text"], 80);
  }
  return NO;
}
static void serve_ui_session(NSString *directory, int idle_timeout_ms, NSUInteger request_limit, NSUInteger response_limit, NSDictionary *capabilities, BOOL session_ready, BOOL (^validate)(NSDictionary *),
                                    NSDictionary *(^perform)(NSString *, NSDictionary *)) {
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in address = {.sin_len=sizeof(address), .sin_family=AF_INET,
    .sin_addr.s_addr=htonl(INADDR_LOOPBACK), .sin_port=0};
  socklen_t size = sizeof(address);
  if (listener < 0 || bind(listener, (struct sockaddr *)&address, size) || listen(listener, 4) ||
      getsockname(listener, (struct sockaddr *)&address, &size)) abort();
  NSDictionary *endpoint = @{@"generation":NSUUID.UUID.UUIDString.lowercaseString,
    @"port":@(ntohs(address.sin_port)), @"pid":@(getpid())};
  NSMutableDictionary *receipt = [endpoint mutableCopy];
  [receipt addEntriesFromDictionary:@{@"status":@"ready", @"last_id":@0}];
  [receipt addEntriesFromDictionary:capabilities];
  save_ui_receipt(directory, receipt);
  int64_t last_id = 0;
  NSDictionary *last_request, *last_reply;
  BOOL launched = session_ready;
  @try {
    while (1) {
      struct pollfd pending = {.fd=listener, .events=POLLIN};
      if (poll(&pending, 1, idle_timeout_ms) != 1)
        @throw [NSException exceptionWithName:@"UIControl" reason:@"Host controller stopped responding" userInfo:nil];
      int fd = accept(listener, NULL, NULL);
      if (fd < 0) abort();
      @try {
        if (!control_socket_options(fd)) abort();
        NSDictionary *request = control_receive_bounded(fd,request_limit);
        if (!request) continue;
        NSString *error;
        int64_t number = 0;
        if (!validate(request)) error = @"invalid_request";
        else if (![request[@"generation"] isEqual:endpoint[@"generation"]]) error = @"generation_mismatch";
        else {
          number = [request[@"request_id"] longLongValue];
          if (number == last_id) {
            if ([request isEqual:last_request]) {
              NSMutableDictionary *replay = [last_reply mutableCopy]; replay[@"replayed"] = @YES;
              save_ui_receipt(directory, replay); control_send_bounded(fd, replay, response_limit); continue;
            }
            error = @"request_conflict";
          } else if (number < last_id) error = @"request_retired";
          else if (number != last_id + 1) error = @"sequence_gap";
          else if (launched == [request[@"operation"] isEqual:@"launch"]) error = @"invalid_lifecycle";
        }
        NSMutableDictionary *reply = [endpoint mutableCopy];
        [reply addEntriesFromDictionary:@{@"request":request, @"last_id":@(last_id), @"replayed":@NO,
          @"status":error ? @"rejected" : @"executing"}];
        if (error) reply[@"error"] = error;
        save_ui_receipt(directory, reply);
        if (error) { control_send_bounded(fd, reply, response_limit); continue; }
        ui_checkpoint(directory, @"before-tap", request);
        NSDictionary *result = perform(request[@"operation"], request[@"arguments"]);
        ui_checkpoint(directory, @"after-tap", request);
        // XCTest assertion failures stop the test; no input is automatically replayed.
        last_id = number;
        launched = ![@[@"terminate", @"finish"] containsObject:request[@"operation"]];
        [reply addEntriesFromDictionary:@{@"last_id":@(last_id), @"status":@"ok", @"result":result}];
        last_request = [request copy]; last_reply = [reply copy];
        save_ui_receipt(directory, reply);
        ui_checkpoint(directory, @"after-receipt", request);
        control_send_bounded(fd, reply, response_limit);
        if ([request[@"operation"] isEqual:@"finish"]) break;
      } @finally { close(fd); }
    }
  } @finally { close(listener); }
}
static void serve_ui_with_capabilities(NSString *directory, int idle_timeout_ms, NSUInteger request_limit, NSUInteger response_limit, NSDictionary *capabilities, BOOL (^validate)(NSDictionary *),
                                    NSDictionary *(^perform)(NSString *, NSDictionary *)) {
  serve_ui_session(directory,idle_timeout_ms,request_limit,response_limit,capabilities,NO,validate,perform);
}
static void serve_ui_with_limits(NSString *directory, int idle_timeout_ms, NSUInteger request_limit, NSUInteger response_limit, BOOL (^validate)(NSDictionary *),
                                    NSDictionary *(^perform)(NSString *, NSDictionary *)) {
  serve_ui_with_capabilities(directory,idle_timeout_ms,request_limit,response_limit,@{},validate,perform);
}
static void serve_ui_with_response_limit(NSString *directory, int idle_timeout_ms, NSUInteger response_limit, BOOL (^validate)(NSDictionary *),
                                         NSDictionary *(^perform)(NSString *, NSDictionary *)) {
  serve_ui_with_limits(directory,idle_timeout_ms,8192,response_limit,validate,perform);
}
static void serve_ui_with_idle_timeout(NSString *directory, int idle_timeout_ms, BOOL (^validate)(NSDictionary *),
                                     NSDictionary *(^perform)(NSString *, NSDictionary *)) {
  serve_ui_with_response_limit(directory, idle_timeout_ms, 8192, validate, perform);
}
// Legacy probes retain their host-response timeout. Owned workflows are retired by their worker.
static void serve_ui_with_validation(NSString *directory, BOOL (^validate)(NSDictionary *),
                                     NSDictionary *(^perform)(NSString *, NSDictionary *)) {
  serve_ui_with_idle_timeout(directory, 30000, validate, perform);
}
static void serve_ui(NSString *directory, NSDictionary *(^perform)(NSString *, NSDictionary *)) {
  serve_ui_with_validation(directory, ^BOOL(NSDictionary *request) {
    return ui_request_valid_for_targets(request, @[@"native-increment", @"native-hold-work", @"native-hold-due"]);
  }, perform);
}
