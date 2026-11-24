#import "clock_control.h"
#import "control_transport.h"
#include <pthread.h>
#include <poll.h>
#include <stdatomic.h>

@interface FCClockChannel ()
- (instancetype)initWithOperation:(NSDictionary *(^)(NSDictionary *))perform now:(uint64_t (^)(void))now receipt:(NSString *)name;
- (void)serve;
- (void)start;
- (void)checkpoint:(NSString *)phase request:(NSDictionary *)request;
@end
@implementation FCClockChannel {
  int listener;
  NSDictionary *endpoint;
  NSDictionary *(^operation)(NSDictionary *);
  uint64_t (^instant)(void);
  NSDictionary *lastRequest, *lastReply;
  int64_t lastID;
  NSString *receiptPath;
  _Atomic(bool) stopping;
  _Atomic(bool) stopped;
}
- (NSDictionary *)endpoint { return endpoint; }
- (BOOL)stopped { return atomic_load(&stopped); }
- (void)stop { atomic_store(&stopping, true); }
- (void)checkpoint:(NSString *)phase request:(NSDictionary *)request {
  const char *stop = getenv("FC_CLOCK_STOP_AT");
  if (!stop || ![phase isEqual:@(stop)] || ![request[@"operation"] isEqual:@"advance"]) return;
  NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/clock-checkpoint.json"];
  NSMutableDictionary *marker = [endpoint mutableCopy];
  [marker addEntriesFromDictionary:@{@"phase":phase, @"request":request, @"now_ns":@(instant()),
    @"status":@"waiting", @"wait_ms":@10000}];
  NSData *data = [NSJSONSerialization dataWithJSONObject:marker options:NSJSONWritingPrettyPrinted error:NULL];
  if (![data writeToFile:path options:NSDataWritingAtomic error:NULL]) abort();
  poll(NULL, 0, 10000);
  marker[@"status"] = @"expired";
  data = [NSJSONSerialization dataWithJSONObject:marker options:NSJSONWritingPrettyPrinted error:NULL];
  [data writeToFile:path options:NSDataWritingAtomic error:NULL];
  _Exit(78);
}
- (void)saveReceipt:(NSDictionary *)reply {
  NSData *data = [NSJSONSerialization dataWithJSONObject:reply options:NSJSONWritingPrettyPrinted error:NULL];
  if (!data || ![data writeToFile:receiptPath options:NSDataWritingAtomic error:NULL]) abort();
}
- (NSMutableDictionary *)receipt:(NSDictionary *)request status:(NSString *)status {
  NSMutableDictionary *reply = [endpoint mutableCopy];
  [reply addEntriesFromDictionary:@{@"request":request, @"status":status, @"last_id":@(lastID),
    @"applied":@NO, @"replayed":@NO, @"now_ns":@(instant())}];
  return reply;
}
static BOOL integer(id value, int64_t minimum) {
  return [value isKindOfClass:NSNumber.class] && CFGetTypeID((__bridge CFTypeRef)value) != CFBooleanGetTypeID() &&
    !CFNumberIsFloatType((__bridge CFNumberRef)value) && [value longLongValue] >= minimum &&
    [value compare:@([value longLongValue])] == NSOrderedSame;
}
- (NSDictionary *)handle:(NSDictionary *)request {
  NSString *error;
  NSUUID *uuid = [request[@"generation"] isKindOfClass:NSString.class]
    ? [[NSUUID alloc] initWithUUIDString:request[@"generation"]] : nil;
  BOOL valid = request.count == 4 && [uuid.UUIDString.lowercaseString isEqual:request[@"generation"]] &&
    integer(request[@"request_id"], 1) && integer(request[@"delta_ns"], 0) &&
    ([@"advance" isEqual:request[@"operation"]] ||
     ([@"read" isEqual:request[@"operation"]] && ![request[@"delta_ns"] longLongValue]));
  int64_t number = valid ? [request[@"request_id"] longLongValue] : 0;
  if (!valid) error = @"invalid_request";
  else if (![endpoint[@"generation"] isEqual:request[@"generation"]]) error = @"generation_mismatch";
  else if (number == lastID) {
    if ([lastRequest isEqual:request]) {
      NSMutableDictionary *reply = [lastReply mutableCopy];
      reply[@"replayed"] = @YES;
      [self saveReceipt:reply];
      return reply;
    }
    error = @"request_conflict";
  } else if (number < lastID) error = @"request_retired";
  else if (number != lastID + 1) error = @"sequence_gap";
  if (error) {
    NSMutableDictionary *reply = [self receipt:request status:@"rejected"];
    reply[@"error"] = error;
    [self saveReceipt:reply];
    return reply;
  }
  if (atomic_load(&stopping)) {
    NSMutableDictionary *reply = [self receipt:request status:@"rejected"];
    reply[@"error"] = @"channel_stopped";
    return reply; // Retirement does not overwrite the original effect receipt.
  }
  [self saveReceipt:[self receipt:request status:@"executing"]];
  [self checkpoint:@"before-advance" request:request];
  NSDictionary *result = operation(request);
  lastID = number;
  NSMutableDictionary *reply = [self receipt:request status:@"ok"];
  [reply addEntriesFromDictionary:result];
  lastRequest = [request copy]; lastReply = [reply copy];
  [self saveReceipt:reply];
  [self checkpoint:@"after-receipt" request:request];
  return reply;
}
- (void)serve {
  while (!atomic_load(&stopping)) {
    struct pollfd waiting = {.fd=listener, .events=POLLIN};
    int ready = poll(&waiting, 1, 100);
    if (ready < 0) { if (errno == EINTR) continue; abort(); }
    if (!ready || atomic_load(&stopping)) continue;
    int fd = accept(listener, NULL, NULL);
    if (fd < 0) { if (errno == EINTR) continue; abort(); }
    @autoreleasepool {
      if (!atomic_load(&stopping) && control_socket_options(fd)) {
        NSDictionary *request = control_receive(fd);
        // WebView observations include bounded event/media histories. Match the
        // native client's 1 MiB response bound; the 8 KiB request bound is separate.
        if (request) control_send_bounded(fd, [self handle:request], 1024 * 1024);
      }
    }
    close(fd);
  }
  // Only this thread closes its descriptor, so a later channel cannot reuse it during accept.
  close(listener);
  atomic_store(&stopped, true);
}
static void *serveChannel(void *retained) {
  @autoreleasepool {
    FCClockChannel *channel = (__bridge_transfer FCClockChannel *)retained;
    [channel serve];
  }
  return NULL;
}
- (instancetype)initWithOperation:(NSDictionary *(^)(NSDictionary *))perform now:(uint64_t (^)(void))now receipt:(NSString *)name {
  self = [super init]; if (!self) return nil;
  operation = [perform copy]; instant = [now copy];
  atomic_init(&stopping, false); atomic_init(&stopped, false);
  if (name.isAbsolutePath || [name.pathComponents containsObject:@".."] || !name.length) abort();
  receiptPath = [[NSHomeDirectory() stringByAppendingPathComponent:@"Documents"] stringByAppendingPathComponent:name];
  if (![NSFileManager.defaultManager createDirectoryAtPath:receiptPath.stringByDeletingLastPathComponent
      withIntermediateDirectories:YES attributes:nil error:NULL]) abort();
  listener = socket(AF_INET, SOCK_STREAM, 0);
  struct sockaddr_in address = {.sin_len = sizeof(address), .sin_family = AF_INET,
    .sin_addr.s_addr = htonl(INADDR_LOOPBACK), .sin_port = 0};
  socklen_t size = sizeof(address);
  if (listener < 0 || bind(listener, (struct sockaddr *)&address, size) || listen(listener, 4) ||
      getsockname(listener, (struct sockaddr *)&address, &size)) abort();
  endpoint = @{@"generation":NSUUID.UUID.UUIDString.lowercaseString, @"port":@(ntohs(address.sin_port)), @"pid":@(getpid())};
  [self saveReceipt:[self receipt:@{} status:@"ready"]];
  return self;
}
- (void)start {
  pthread_t thread;
  if (pthread_create(&thread, NULL, serveChannel, (__bridge_retained void *)self) || pthread_detach(thread)) abort();
}
@end
static FCClockChannel *appChannel;
FCClockChannel *start_clock_channel(NSDictionary *(^perform)(NSDictionary *), uint64_t (^now)(void), NSString *receiptName) {
  FCClockChannel *channel = [[FCClockChannel alloc] initWithOperation:perform now:now receipt:receiptName];
  [channel start];
  return channel;
}
NSDictionary *start_clock_control(NSDictionary *(^perform)(NSDictionary *), uint64_t (^now)(void)) {
  if (appChannel) abort();
  [NSFileManager.defaultManager removeItemAtPath:[NSHomeDirectory() stringByAppendingPathComponent:@"Documents/clock-checkpoint.json"] error:NULL];
  appChannel = [[FCClockChannel alloc] initWithOperation:perform now:now receipt:@"clock-control.json"];
  // Publish the compatibility owner before its thread can invoke a checkpoint.
  [appChannel start];
  return appChannel.endpoint;
}
void clock_control_checkpoint(NSString *phase, NSDictionary *request) { [appChannel checkpoint:phase request:request]; }
