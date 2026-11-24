// Minimal dynamic bindings for XCTest event paths; see README for upstream API references.
// These private APIs are test-host capabilities, never consumer dependencies.
#import <objc/message.h>

static BOOL touch_number(id value, double maximum) {
    return [value isKindOfClass:NSNumber.class] &&
        CFGetTypeID((__bridge CFTypeRef)value) != CFBooleanGetTypeID() &&
        isfinite([value doubleValue]) && [value doubleValue] >= 0 && [value doubleValue] <= maximum;
}
static BOOL touch_paths_valid(id paths) {
    if (![paths isKindOfClass:NSArray.class] || [paths count] < 1 || [paths count] > 5) return NO;
    NSUInteger total = 0;
    for (id path in paths) {
        if (![path isKindOfClass:NSArray.class] || [path count] < 2 || [path count] > 32) return NO;
        total += [path count];
        if (total > 32) return NO; // Bound the request plus resolved-coordinate receipt.
        double previous = -1;
        for (id point in path) {
            if (![point isKindOfClass:NSDictionary.class] || [point count] != 3 ||
                !touch_number(point[@"x"], 1) || !touch_number(point[@"y"], 1) ||
                !touch_number(point[@"ms"], 5000) || [point[@"ms"] doubleValue] <= previous) return NO;
            previous = [point[@"ms"] doubleValue];
        }
    }
    return YES;
}
static NSArray<NSNumber *> *touch_path_order(NSArray *paths) {
    NSMutableArray *order = [NSMutableArray array];
    for (NSUInteger i = 0; i < paths.count; i++) [order addObject:@(i)];
    return [order sortedArrayUsingComparator:^NSComparisonResult(NSNumber *a, NSNumber *b) {
        NSArray *left = paths[a.unsignedIntegerValue], *right = paths[b.unsignedIntegerValue];
        NSComparisonResult start = [left[0][@"ms"] compare:right[0][@"ms"]];
        if (start != NSOrderedSame) return start;
        // Installed XCTest retargeted a remaining contact when an earlier path
        // ended first. Keep later releases first within each simultaneous cohort.
        NSComparisonResult end = [right.lastObject[@"ms"] compare:left.lastObject[@"ms"]];
        return end == NSOrderedSame ? [a compare:b] : end;
    }];
}
#ifndef FRONTCAP_TOUCH_PLANNING_ONLY
static NSDictionary *deliver_touch_paths(XCUIApplication *app, NSArray *paths) {
    Class pathClass = NSClassFromString(@"XCPointerEventPath");
    Class recordClass = NSClassFromString(@"XCSynthesizedEventRecord");
    Class sessionClass = NSClassFromString(@"XCTRunnerDaemonSession");
    SEL shared = NSSelectorFromString(@"sharedSession");
    SEL legacy = NSSelectorFromString(@"useLegacyEventCoordinateTransformationPath");
    SEL initialize = NSSelectorFromString(@"initForTouchAtPoint:offset:");
    SEL move = NSSelectorFromString(@"moveToPoint:atOffset:");
    SEL lift = NSSelectorFromString(@"liftUpAtOffset:");
    SEL recordInit = NSSelectorFromString(@"initWithName:interfaceOrientation:");
    SEL add = NSSelectorFromString(@"addPointerEventPath:");
    SEL synthesizerSelector = NSSelectorFromString(@"eventSynthesizer");
    SEL synthesize = NSSelectorFromString(@"synthesizeEvent:completion:");
    if (![sessionClass respondsToSelector:shared] ||
        ![pathClass instancesRespondToSelector:initialize] ||
        ![pathClass instancesRespondToSelector:move] ||
        ![pathClass instancesRespondToSelector:lift] ||
        ![recordClass instancesRespondToSelector:recordInit] ||
        ![recordClass instancesRespondToSelector:add] ||
        ![XCUIDevice.sharedDevice respondsToSelector:synthesizerSelector])
        return @{@"delivered":@NO, @"error":@"touch_path_api_unavailable"};
    id synthesizer = ((id (*)(id, SEL))objc_msgSend)(XCUIDevice.sharedDevice, synthesizerSelector);
    if (![synthesizer respondsToSelector:synthesize])
        return @{@"delivered":@NO, @"error":@"touch_synthesizer_unavailable"};
    id session = ((id (*)(id, SEL))objc_msgSend)(sessionClass, shared);
    if (![session respondsToSelector:legacy] || ((BOOL (*)(id, SEL))objc_msgSend)(session, legacy))
        return @{@"delivered":@NO, @"error":@"legacy_coordinate_transform_unqualified"};
    id record = ((id (*)(id, SEL, id, NSInteger))objc_msgSend)([recordClass alloc], recordInit,
        @"frontcap native touch paths", UIInterfaceOrientationPortrait);
#ifdef FRONTCAP_TOUCH_PERSISTENCE_PROBE
    // Mechanism experiment only: qualify how the native synthesizer brackets state.
    [record setValue:@YES forKey:@"beginsPersistentState"];
    [record setValue:@YES forKey:@"endsPersistentState"];
#endif
    NSMutableArray *resolved = [NSMutableArray array];
    NSArray *order = touch_path_order(paths);
    for (NSNumber *sourceIndex in order) {
        NSArray *path = paths[sourceIndex.unsignedIntegerValue];
        id nativePath;
        NSMutableArray *screenPath = [NSMutableArray array];
        for (NSUInteger i = 0; i < path.count; i++) {
            NSDictionary *point = path[i];
            CGPoint screen = [app coordinateWithNormalizedOffset:
                CGVectorMake([point[@"x"] doubleValue], [point[@"y"] doubleValue])].screenPoint;
            double offset = [point[@"ms"] doubleValue] / 1000.0;
            [screenPath addObject:@{@"x":@(screen.x), @"y":@(screen.y), @"ms":point[@"ms"]}];
            if (i == 0)
                nativePath = ((id (*)(id, SEL, CGPoint, double))objc_msgSend)([pathClass alloc], initialize, screen, offset);
            else {
                NSDictionary *previous = path[i-1];
                if (i < path.count-1 || ![previous[@"x"] isEqual:point[@"x"]] || ![previous[@"y"] isEqual:point[@"y"]])
                    ((void (*)(id, SEL, CGPoint, double))objc_msgSend)(nativePath, move, screen, offset);
                if (i == path.count-1)
                    ((void (*)(id, SEL, double))objc_msgSend)(nativePath, lift, offset);
            }
        }
        ((void (*)(id, SEL, id))objc_msgSend)(record, add, nativePath);
        [resolved addObject:screenPath];
    }
    __block NSError *error = nil;
    __block BOOL delivered = NO;
    NSLog(@"FRONTCAP_TOUCH_RECORD %@", record);
    XCTestExpectation *completion = [[XCTestExpectation alloc] initWithDescription:@"Native touch synthesis"];
    void (^callback)(BOOL, NSError *) = ^(BOOL success, NSError *failure) {
        delivered = success;
        error = failure;
        [completion fulfill];
    };
    ((void (*)(id, SEL, id, id))objc_msgSend)(synthesizer, synthesize, record, callback);
    XCTWaiterResult outcome = [XCTWaiter waitForExpectations:@[completion] timeout:10];
    if (outcome != XCTWaiterResultCompleted)
        return @{@"delivered":@NO, @"error":@"touch_completion_unknown",
            @"applied":NSNull.null, @"screen_paths":resolved};
    return @{@"delivered":@(delivered), @"error":error.localizedDescription ?: NSNull.null,
        @"screen_paths":resolved, @"path_order":order, @"timing":@"host-relative-ms"};
}
#endif
