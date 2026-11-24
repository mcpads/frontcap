#import <XCTest/XCTest.h>
#import <objc/runtime.h>
#import <objc/message.h>
#include <stdatomic.h>
#include <string.h>
#import "ui_control.h"

#import "xctest_wait.h"

@interface NativeProbeTests : XCTestCase
@end
@implementation NativeProbeTests
- (void)tearDown {
  NSString *directory = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/native-proof"];
  [[NSFileManager defaultManager] createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:NULL];
  [XCUIScreen.mainScreen.screenshot.PNGRepresentation writeToFile:[directory stringByAppendingPathComponent:@"last-screen.png"] atomically:YES];
  XCUIApplication *app = [[XCUIApplication alloc] initWithBundleIdentifier:@"dev.frontcap.s18.probe"];
  if (app.state == XCUIApplicationStateRunningForeground) {
    [app.debugDescription writeToFile:[directory stringByAppendingPathComponent:@"last-tree.txt"] atomically:YES encoding:NSUTF8StringEncoding error:NULL];
    XCUIElement *clock = app.staticTexts[@"native-clock"];
    if (clock.exists) [(NSString *)clock.value writeToFile:[directory stringByAppendingPathComponent:@"last-clock.json"] atomically:YES encoding:NSUTF8StringEncoding error:NULL];
    [app terminate];
  }
  manual_input = NO;
  [super tearDown];
}

- (void)testVirtualTimeInput {
  self.continueAfterFailure = NO;
  XCTAssertTrue(install_animation_wait_control(), @"Unsupported XCTest animation-wait selector");
  manual_input = YES;
  NSString *directory = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/native-proof"];
  XCTAssertTrue([[NSFileManager defaultManager] createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:NULL]);
  XCUIApplication *app = [[XCUIApplication alloc] initWithBundleIdentifier:@"dev.frontcap.s18.probe"];
  serve_ui(directory, ^NSDictionary *(NSString *operation, NSDictionary *args) {
    XCUIElement *label = app.staticTexts[@"native-count"];
    XCUIElement *clock = app.staticTexts[@"native-clock"];
    if ([operation isEqual:@"launch"]) {
      NSMutableDictionary *environment = [@{@"FC_MANUAL":@"1", @"FC_TRACE_ACTIVITY":@"1", @"FC_TRACE_TIMERS":@"1", @"FC_TRACE_UI":@"1", @"FC_UC_HOST_TIME":@"1", @"DYLD_INSERT_LIBRARIES":@"@executable_path/vclock.dylib:@executable_path/ui_schedule.dylib"} mutableCopy];
      if (args[@"clock_stop_at"]) environment[@"FC_CLOCK_STOP_AT"] = args[@"clock_stop_at"];
      app.launchEnvironment = environment;
      [app launch];
      XCTAssertTrue([label waitForExistenceWithTimeout:10]);
      XCTAssertEqualObjects(label.label, @"Count: 0");
      XCTAssertEqualObjects(clock.label, @"Time: 0");
      XCTAssertFalse(app.buttons[@"native-step-before"].exists);
      NSDictionary *reads = [NSJSONSerialization JSONObjectWithData:[(NSString *)clock.value dataUsingEncoding:NSUTF8StringEncoding] options:0 error:NULL];
      return @{@"state":@"launched", @"text":label.label, @"reads":reads};
    }
    if ([@[@"terminate", @"finish"] containsObject:operation]) {
      [app terminate];
      XCTAssertEqual(app.state, XCUIApplicationStateNotRunning);
      return @{@"state":@"terminated"};
    }
    XCTAssertEqual(app.state, XCUIApplicationStateRunningForeground);
    if ([operation isEqual:@"tap"]) {
      XCUIElement *button = app.buttons[args[@"target"]];
      XCTAssertTrue(button.hittable);
      [button tap];
      if (args[@"label"]) {
        XCTNSPredicateExpectation *entered = [[XCTNSPredicateExpectation alloc] initWithPredicate:
          [NSPredicate predicateWithFormat:@"label == %@", args[@"label"]] object:button];
        XCTAssertEqual([XCTWaiter waitForExpectations:@[entered] timeout:10], XCTWaiterResultCompleted);
      }
      return @{@"input":@"xctest-tap", @"target":args[@"target"], @"label":button.label};
    }
    XCTAssertEqualObjects(operation, @"capture");
    XCTNSPredicateExpectation *ready = [[XCTNSPredicateExpectation alloc] initWithPredicate:
      [NSPredicate predicateWithFormat:@"label == %@", args[@"clock"]] object:clock];
    XCTAssertEqual([XCTWaiter waitForExpectations:@[ready] timeout:10], XCTWaiterResultCompleted);
    XCTAssertEqualObjects(label.label, args[@"text"]);
    NSString *name = args[@"name"];
    XCTAssertTrue([XCUIScreen.mainScreen.screenshot.PNGRepresentation writeToFile:
      [directory stringByAppendingPathComponent:[name stringByAppendingString:@".png"]] atomically:YES]);
    XCTAssertTrue([app.debugDescription writeToFile:[directory stringByAppendingPathComponent:[name stringByAppendingString:@".txt"]]
      atomically:YES encoding:NSUTF8StringEncoding error:NULL]);
    CGRect color = app.otherElements[@"native-color"].frame;
    XCTAssertGreaterThan(color.size.width, 0);
    NSDictionary *reads = [NSJSONSerialization JSONObjectWithData:[(NSString *)clock.value dataUsingEncoding:NSUTF8StringEncoding] options:0 error:NULL];
    return @{@"phase":name, @"text":label.label, @"clock":clock.label, @"reads":reads,
      @"sample":@[@(CGRectGetMidX(color)), @(CGRectGetMidY(color))],
      @"screen_points":@[@(app.frame.size.width), @(app.frame.size.height)]};
  });
}

- (void)testTapChangesState {
  self.continueAfterFailure = NO;
  XCUIApplication *app = [[XCUIApplication alloc] initWithBundleIdentifier:@"dev.frontcap.s18.probe"];
  app.launchEnvironment = @{@"FC_TRACE_UI":@"1", @"DYLD_INSERT_LIBRARIES":@"@executable_path/ui_schedule.dylib"};
  [app launch];
  XCUIElement *label = app.staticTexts[@"native-count"];
  XCTAssertTrue([label waitForExistenceWithTimeout:10]);
  XCTAssertEqualObjects(label.label, @"Count: 0");
  NSString *directory = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/native-proof"];
  NSError *error;
  XCTAssertTrue([[NSFileManager defaultManager] createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:&error]);
  NSMutableArray *rows = [NSMutableArray new];
  for (NSInteger count = 0; count < 2; count++) {
    if (count) {
      XCUIElement *button = app.buttons[@"native-increment"];
      XCTAssertTrue(button.hittable);
      [button tap];
      NSPredicate *updated = [NSPredicate predicateWithFormat:@"label == %@", @"Count: 1"];
      XCTNSPredicateExpectation *expectation = [[XCTNSPredicateExpectation alloc] initWithPredicate:updated object:label];
      XCTAssertEqual([XCTWaiter waitForExpectations:@[expectation] timeout:10], XCTWaiterResultCompleted);
    }
    NSString *name = count ? @"app-after" : @"app-before";
    XCUIScreenshot *screen = XCUIScreen.mainScreen.screenshot;
    XCTAssertTrue([screen.PNGRepresentation writeToFile:[directory stringByAppendingPathComponent:[name stringByAppendingString:@".png"]] atomically:YES]);
    XCTAssertTrue([app.debugDescription writeToFile:[directory stringByAppendingPathComponent:[name stringByAppendingString:@".txt"]] atomically:YES encoding:NSUTF8StringEncoding error:&error]);
    CGRect color = app.otherElements[@"native-color"].frame;
    CGRect window = app.frame;
    XCTAssertGreaterThan(color.size.width, 0);
    [rows addObject:@{@"phase":name, @"text":label.label, @"count":@(count),
                     @"sample":@[@(CGRectGetMidX(color)), @(CGRectGetMidY(color))],
                     @"screen_points":@[@(window.size.width), @(window.size.height)]}];
  }
  NSData *data = [NSJSONSerialization dataWithJSONObject:@{@"input":@"xctest-tap", @"observations":rows} options:NSJSONWritingPrettyPrinted error:&error];
  XCTAssertNotNil(data);
  XCTAssertTrue([data writeToFile:[directory stringByAppendingPathComponent:@"app-state.json"] atomically:YES]);
  [app terminate];
}
@end
