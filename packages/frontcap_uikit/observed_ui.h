#import <XCTest/XCTest.h>

// XCTest-thread-only observation and input. The owner supplies verified live app generation/time
// on every call, persists request outcomes, and owns screenshots and app lifecycle separately.
@interface FCObservedUI : NSObject
- (instancetype)initWithApplication:(XCUIApplication *)app;
- (NSDictionary *)observeWithContext:(NSDictionary *)context;
- (NSDictionary *)lastObservationCheck;
- (BOOL)isCurrentObservation:(NSString *)observation context:(NSDictionary *)context;
- (NSDictionary *)tapReference:(NSString *)reference observation:(NSString *)observation
                      context:(NSDictionary *)context;
// The owner reserves the clock ID and retains the returned raw clock/delivery evidence.
// This consumes the observation; an unresolved press cannot be retried as new input.
- (NSDictionary *)pressReference:(NSString *)reference observation:(NSString *)observation
                        context:(NSDictionary *)context endpoint:(NSDictionary *)endpoint
                   clockRequest:(NSDictionary *)request boundary:(NSDictionary *)boundary;
- (NSDictionary *)replaceReference:(NSString *)reference observation:(NSString *)observation
                          context:(NSDictionary *)context arguments:(NSDictionary *)arguments
                         endpoint:(NSDictionary *)endpoint receipt:(NSDictionary *)receipt
                      nextRequest:(int64_t *)nextRequest;
- (NSDictionary *)scrollReference:(NSString *)reference observation:(NSString *)observation
                          context:(NSDictionary *)context direction:(NSString *)direction bundle:(NSString *)bundle;
@end
