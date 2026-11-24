#import <Foundation/Foundation.h>
// One owned request sequence. Stop closes its listener on the control thread; it never resets or reuses the identity.
@interface FCClockChannel : NSObject
@property(nonatomic, readonly) NSDictionary *endpoint;
@property(nonatomic, readonly) BOOL stopped;
- (void)stop;
@end
// The blocks run on an untracked POSIX thread. The receipt path is private platform evidence below Documents.
FCClockChannel *start_clock_channel(NSDictionary *(^perform)(NSDictionary *), uint64_t (^now)(void), NSString *receiptName);
// Compatibility entry point for the one app-clock endpoint.
NSDictionary *start_clock_control(NSDictionary *(^perform)(NSDictionary *), uint64_t (^now)(void));
// Test-only rendezvous for host-directed app termination. Never an outcome receipt.
void clock_control_checkpoint(NSString *phase, NSDictionary *request);
