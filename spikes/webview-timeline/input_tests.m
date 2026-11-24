#import <XCTest/XCTest.h>
#import "consumer.h"
#import "xctest_wait.h"
#import "ui_control.h"
#include <math.h>
#import "touch_paths.h"

@interface WebViewInputTests : XCTestCase
@end
@implementation WebViewInputTests
- (void)testExternalInput {
    self.continueAfterFailure = NO;
    XCTAssertTrue(install_animation_wait_control());
    manual_input = YES;
    XCUIApplication *app = [[XCUIApplication alloc] initWithBundleIdentifier:FC_XCTEST_BUNDLE];
    NSString *directory = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/webview-input"];
    XCTAssertTrue([[NSFileManager defaultManager] createDirectoryAtPath:directory
        withIntermediateDirectories:YES attributes:nil error:NULL]);
    serve_ui_with_idle_timeout(directory, 120000, ^BOOL(NSDictionary *request) {
        // Reuse the shared envelope validator for the common lifecycle operations.
        NSString *op = request[@"operation"];
        if (![op isKindOfClass:NSString.class] ||
            ![request[@"arguments"] isKindOfClass:NSDictionary.class]) return NO;
        if ([@[@"launch", @"finish"] containsObject:op])
            return [request[@"arguments"] count] == 0 && ui_request_valid_for_targets(request, @[]);
        NSMutableDictionary *envelope = [request mutableCopy];
        envelope[@"operation"] = @"launch"; envelope[@"arguments"] = @{};
        if (!ui_request_valid_for_targets(envelope, @[]) || request.count != 4 ||
            ![request[@"arguments"] isKindOfClass:NSDictionary.class]) return NO;
        NSDictionary *args = request[@"arguments"];
        if ([op isEqual:@"touches"]) return args.count == 1 && touch_paths_valid(args[@"paths"]);
        if ([@[@"capture", @"background", @"foreground"] containsObject:op]) return args.count == 0;
        if (![op isEqual:@"tap"] || args.count != 2) return NO;
        for (NSString *key in @[@"x", @"y"]) {
            id value = args[key];
            if (![value isKindOfClass:NSNumber.class] ||
                CFGetTypeID((__bridge CFTypeRef)value) == CFBooleanGetTypeID() ||
                !isfinite([value doubleValue]) || [value doubleValue] < 0 || [value doubleValue] > 1)
                return NO;
        }
        return YES;
    }, ^NSDictionary *(NSString *op, NSDictionary *args) {
        NSDictionary *touches = nil;
        if ([op isEqual:@"launch"]) {
            XCTAssertNotEqual(app.state, XCUIApplicationStateNotRunning);
            [app activate]; // Attach to the already injected process, never relaunch.
        } else if ([op isEqual:@"tap"]) {
            XCTAssertEqual(app.state, XCUIApplicationStateRunningForeground);
            XCUICoordinate *point = [app coordinateWithNormalizedOffset:
                CGVectorMake([args[@"x"] doubleValue], [args[@"y"] doubleValue])];
            [point tap];
        } else if ([op isEqual:@"touches"]) {
            XCTAssertEqual(app.state, XCUIApplicationStateRunningForeground);
            touches = deliver_touch_paths(app, args[@"paths"]);
            XCTAssertTrue([touches[@"delivered"] boolValue], @"Native touch delivery failed: %@", touches);
        } else if ([op isEqual:@"background"]) {
            XCTAssertEqual(app.state, XCUIApplicationStateRunningForeground);
            [XCUIDevice.sharedDevice pressButton:XCUIDeviceButtonHome];
            NSMutableArray *states = [NSMutableArray new];
            XCTNSPredicateExpectation *background = [[XCTNSPredicateExpectation alloc]
                initWithPredicate:[NSPredicate predicateWithBlock:^BOOL(id value, NSDictionary *bindings) {
                    XCUIApplicationState state = app.state;
                    [states addObject:@{@"state":@(state), @"host_epoch_s":@(NSDate.date.timeIntervalSince1970)}];
                    return state == XCUIApplicationStateRunningBackground ||
                           state == XCUIApplicationStateRunningBackgroundSuspended;
                }] object:app];
            XCTWaiterResult outcome = [XCTWaiter waitForExpectations:@[background] timeout:5];
            [[NSJSONSerialization dataWithJSONObject:states options:0 error:NULL]
                writeToFile:[directory stringByAppendingPathComponent:@"background-states.json"] atomically:YES];
            XCTAssertEqual(outcome, XCTWaiterResultCompleted, @"Background transition failed: %@", states);
        } else if ([op isEqual:@"foreground"]) {
            XCTAssertNotEqual(app.state, XCUIApplicationStateNotRunning);
            [app activate];
            XCTAssertEqual(app.state, XCUIApplicationStateRunningForeground);
        }
        BOOL visible = app.state == XCUIApplicationStateRunningForeground;
        CGRect frame = visible ? app.frame : CGRectZero;
        NSData *image = XCUIScreen.mainScreen.screenshot.PNGRepresentation;
        XCTAssertTrue([image writeToFile:[directory stringByAppendingPathComponent:@"last.png"] atomically:YES]);
        [(visible ? app.debugDescription : @"App is backgrounded; no current app tree captured")
            writeToFile:[directory stringByAppendingPathComponent:@"last-tree.txt"]
            atomically:YES encoding:NSUTF8StringEncoding error:NULL];
        return @{@"operation":op, @"app_state":@(app.state),
            @"touches":touches ?: NSNull.null,
            @"frame_points":@[@(frame.origin.x), @(frame.origin.y), @(frame.size.width), @(frame.size.height)],
            @"input": [op isEqual:@"tap"] ? @"xctest-coordinate-tap" :
                ([op isEqual:@"touches"] ? @"xctest-touch-paths" : @"none")};
    });
    manual_input = NO;
}
@end
