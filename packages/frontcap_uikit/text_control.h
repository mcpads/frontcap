#pragma once
// App-owned, test-only text endpoint. No clock thread or XCTest execution thread is reused.
#import "text_input.h"
#import "../../spikes/s18-native-time/ios/control_transport.h"
#include <pthread.h>
#include <poll.h>
#include <limits.h>

static BOOL uikit_text_integer(id value, int64_t maximum) {
  return [value isKindOfClass:NSNumber.class] && CFGetTypeID((__bridge CFTypeRef)value) != CFBooleanGetTypeID() &&
    !CFNumberIsFloatType((__bridge CFNumberRef)value) && [value longLongValue] >= 0 &&
    [value longLongValue] <= maximum && [value compare:@([value longLongValue])] == NSOrderedSame;
}
static BOOL uikit_text_string(id value, NSUInteger maximum) {
  return [value isKindOfClass:NSString.class] && [value length] <= maximum &&
    [value dataUsingEncoding:NSUTF8StringEncoding] != nil;
}
static BOOL uikit_text_uuid(id value) {
  if (!uikit_text_string(value, 36)) return NO;
  return [[[[NSUUID alloc] initWithUUIDString:value] UUIDString].lowercaseString isEqual:value];
}
static BOOL uikit_text_request(NSDictionary *request) {
  NSDictionary *args = request[@"arguments"];
  if (![args isKindOfClass:NSDictionary.class]) return NO;
  NSDictionary *basis = args[@"basis"];
  if (request.count != 4 || !uikit_text_uuid(request[@"generation"]) ||
      !uikit_text_integer(request[@"request_id"], INT64_MAX) || [request[@"request_id"] longLongValue] == 0 ||
      ![request[@"operation"] isEqual:@"replace_text"] || ![args isKindOfClass:NSDictionary.class] || args.count != 4 ||
      !uikit_text_integer(args[@"start"], 16384) || !uikit_text_integer(args[@"end"], 16384) ||
      !uikit_text_string(args[@"text"], 16384) || ![basis isKindOfClass:NSDictionary.class] || basis.count != 9) return NO;
  return [basis[@"state"] isEqual:@"focused"] && uikit_text_uuid(basis[@"token"]) &&
    [@[@"text_field", @"text_view"] containsObject:basis[@"role"]] &&
    uikit_text_string(basis[@"identifier"],4096) && uikit_text_string(basis[@"label"],4096) &&
    uikit_text_string(basis[@"text"],16384) && uikit_text_integer(basis[@"start"],16384) &&
    uikit_text_integer(basis[@"end"],16384) && basis[@"marked"] &&
    CFGetTypeID((__bridge CFTypeRef)basis[@"marked"]) == CFBooleanGetTypeID();
}
static void save_uikit_text_receipt(NSString *path, NSDictionary *receipt) {
  NSData *data = [NSJSONSerialization dataWithJSONObject:receipt options:NSJSONWritingPrettyPrinted error:NULL];
  if (!data || ![data writeToFile:path options:NSDataWritingAtomic error:NULL]) abort();
}
static void *serve_uikit_text(void *opaque) {
  @autoreleasepool {
    NSDictionary *owner = CFBridgingRelease(opaque);
    UIWindow *window = owner[@"window"];
    NSDictionary *endpoint = owner[@"endpoint"];
    NSString *path = owner[@"path"];
    int listener = [owner[@"listener"] intValue];
    int64_t last = 0;
    NSDictionary *lastRequest, *lastReply;
    @try {
      while (1) {
        @autoreleasepool {
        int fd = accept(listener, NULL, NULL);
        if (fd < 0) { if (errno == EINTR) continue; abort(); }
        @try {
          if (!control_socket_options(fd)) abort();
          // Two 16K UTF-16 strings may use six JSON bytes per code unit.
          NSDictionary *request = control_receive_bounded(fd, 256*1024);
          if (!request) continue;
          NSString *error;
          int64_t number = [request[@"request_id"] isKindOfClass:NSNumber.class] ? [request[@"request_id"] longLongValue] : 0;
          if (!uikit_text_request(request)) error = @"invalid_request";
          else if (![request[@"generation"] isEqual:endpoint[@"generation"]]) error = @"generation_mismatch";
          else if (number == last && [request isEqual:lastRequest]) {
            NSMutableDictionary *reply = [lastReply mutableCopy]; reply[@"replayed"] = @YES;
            save_uikit_text_receipt(path,reply); control_send_bounded(fd,reply,1024*1024); continue;
          } else if (number <= last) error = number == last ? @"request_conflict" : @"request_retired";
          else if (last == INT64_MAX || number != last+1) error = @"sequence_gap";
          NSMutableDictionary *reply = [endpoint mutableCopy];
          [reply addEntriesFromDictionary:@{@"request":request, @"last_id":@(last), @"replayed":@NO,
            @"status":error ? @"rejected" : @"executing"}];
          if (error) {
            reply[@"error"] = error;
            // Rejections do not replace the durable receipt of a previously admitted operation.
            control_send_bounded(fd,reply,1024*1024); continue;
          }
          save_uikit_text_receipt(path,reply);
          __block NSDictionary *result;
          dispatch_sync(dispatch_get_main_queue(), ^{
            NSDictionary *args = request[@"arguments"];
            result = replace_uikit_text(window, args[@"basis"], [args[@"start"] unsignedIntegerValue],
                                        [args[@"end"] unsignedIntegerValue], args[@"text"]);
          });
          last = number; lastRequest = request;
          [reply addEntriesFromDictionary:@{@"status":@"ok", @"last_id":@(last), @"result":result}];
          lastReply = [reply copy]; save_uikit_text_receipt(path,reply);
          control_send_bounded(fd,reply,1024*1024);
        } @finally { close(fd); }
        }
      }
    } @finally { close(listener); }
  }
  return NULL;
}
static NSDictionary *start_uikit_text_control(UIWindow *window) {
  NSCAssert(NSThread.isMainThread, @"Text control must start on the main thread");
  int listener = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in address = {.sin_len=sizeof(address), .sin_family=AF_INET,
    .sin_addr.s_addr=htonl(INADDR_LOOPBACK), .sin_port=0};
  socklen_t size = sizeof(address);
  if (listener < 0 || bind(listener,(struct sockaddr *)&address,size) || listen(listener,4) ||
      getsockname(listener,(struct sockaddr *)&address,&size)) abort();
  NSDictionary *endpoint = @{@"generation":NSUUID.UUID.UUIDString.lowercaseString,
    @"pid":@(getpid()), @"port":@(ntohs(address.sin_port))};
  NSString *directory = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/native-text"];
  if (![NSFileManager.defaultManager createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:NULL]) abort();
  NSString *path = [directory stringByAppendingPathComponent:@"text-control.json"];
  NSMutableDictionary *ready = [endpoint mutableCopy];
  [ready addEntriesFromDictionary:@{@"status":@"ready", @"last_id":@0}]; save_uikit_text_receipt(path,ready);
  NSDictionary *owner = @{@"window":window, @"endpoint":endpoint, @"listener":@(listener), @"path":path};
  pthread_t thread;
  if (pthread_create(&thread,NULL,serve_uikit_text,(__bridge_retained void *)owner) || pthread_detach(thread)) abort();
  return endpoint;
}
