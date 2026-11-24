#import <XCTest/XCTest.h>
#import "system_bridge.h"

@interface SystemUITests : XCTestCase
@end
@implementation SystemUITests
- (void)testSystemBridge {
  self.continueAfterFailure=NO;
  serve_system_ui([NSHomeDirectory() stringByAppendingPathComponent:@"Documents/system-ui"]);
}
@end
