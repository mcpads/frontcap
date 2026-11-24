// Host-side regression for the actual clock channel, without an app or simulator.
// clang -fobjc-arc -fblocks -framework Foundation clock_control.m clock_response_test.m -o clock-response-test
#import <Foundation/Foundation.h>
#import "clock_control.h"
#import "control_transport.h"
#include <assert.h>
#include <poll.h>

int main(void) {
  @autoreleasepool {
    NSString *directory = [@"frontcap-clock-response-" stringByAppendingString:NSUUID.UUID.UUIDString];
    NSString *receipt = [directory stringByAppendingPathComponent:@"receipt.json"];
    FCClockChannel *channel = start_clock_channel(^NSDictionary *(NSDictionary *request) {
      NSUInteger number = [request[@"request_id"] unsignedIntegerValue];
      NSUInteger size = number == 1 ? 9000 : (number == 2 ? 1024 * 1024 - 1024 : 1024 * 1024 + 1);
      return @{@"status":@"ok", @"payload":[@"" stringByPaddingToLength:size withString:@"x" startingAtIndex:0]};
    }, ^uint64_t { return 0; }, receipt);
    NSDictionary *endpoint = channel.endpoint;
    NSString *path = [[NSHomeDirectory() stringByAppendingPathComponent:@"Documents"] stringByAppendingPathComponent:receipt];
    for (NSUInteger number = 1; number <= 3; number++) {
      int fd = socket(AF_INET, SOCK_STREAM, 0);
      assert(fd >= 0 && control_socket_options(fd));
      struct sockaddr_in address = {.sin_len=sizeof(address), .sin_family=AF_INET,
        .sin_addr.s_addr=htonl(INADDR_LOOPBACK), .sin_port=htons([endpoint[@"port"] unsignedShortValue])};
      assert(connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
      NSDictionary *request = @{@"generation":endpoint[@"generation"], @"request_id":@(number),
        @"operation":@"read", @"delta_ns":@0};
      assert(control_send(fd, request));
      NSDictionary *reply = control_receive_bounded(fd, 1024 * 1024);
      close(fd);
      NSDictionary *saved = [NSJSONSerialization JSONObjectWithData:[NSData dataWithContentsOfFile:path] options:0 error:NULL];
      assert([saved[@"last_id"] unsignedIntegerValue] == number);
      assert([saved[@"request"] isEqual:request] && [saved[@"status"] isEqual:@"ok"]);
      if (number < 3) {
        assert([reply isEqual:saved]);
        NSUInteger bytes = [NSJSONSerialization dataWithJSONObject:reply options:0 error:NULL].length;
        assert(bytes > 8192 && bytes <= 1024 * 1024);
      } else {
        // An oversized reply stays bounded on the wire and retains its original receipt.
        assert(reply == nil && [saved[@"payload"] length] > 1024 * 1024);
      }
    }
    [channel stop];
    for (NSUInteger i = 0; i < 100 && !channel.stopped; i++) poll(NULL, 0, 10);
    assert(channel.stopped);
    assert([[NSFileManager defaultManager] removeItemAtPath:path.stringByDeletingLastPathComponent error:NULL]);
    puts("clock response sizes and retained oversized receipt: passed");
  }
  return 0;
}
