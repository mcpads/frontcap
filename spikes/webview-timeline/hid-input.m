// Simulator-only native input experiment. API layout follows WebKit's
// HIDEventGenerator.mm and PAL IOKitSPIIOS.h; see README for sources and limits.
#import <UIKit/UIKit.h>
#import <objc/message.h>
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <math.h>
#define FRONTCAP_TOUCH_PLANNING_ONLY
#include "touch_paths.h"

typedef CFTypeRef HIDEvent;
static HIDEvent (*makeHand)(CFAllocatorRef,uint64_t,uint32_t,uint32_t,uint32_t,uint32_t,uint32_t,
    double,double,double,double,double,boolean_t,boolean_t,uint32_t);
static HIDEvent (*makeFinger)(CFAllocatorRef,uint64_t,uint32_t,uint32_t,uint32_t,
    double,double,double,double,double,boolean_t,boolean_t,uint32_t);
static void (*setInteger)(HIDEvent,uint32_t,CFIndex);
static void (*setFloat)(HIDEvent,uint32_t,double);
static void (*appendEvent)(HIDEvent,HIDEvent,uint32_t);
static void (*setContext)(HIDEvent,uint32_t,uint8_t,uint8_t,CFStringRef,CFTimeInterval,float);
static CFTypeRef (*makeClient)(CFAllocatorRef);
static CFTypeRef client;
static dispatch_source_t poller;
static NSString *directory, *generation;
static const NSUInteger requestCapacity = 4096;
static NSUInteger nextRequest = 1;
static BOOL busy, fenced, available;
static NSDictionary *heldContacts;
static UIWindow *heldWindow;
static CGRect heldBounds;
static NSUInteger contactRevision;
static NSDictionary *lastGeometry;

static BOOL contactsValid(id contacts) {
    if (![contacts isKindOfClass:NSArray.class] || [contacts count] > 5) return NO;
    NSMutableSet *ids = [NSMutableSet set];
    for (id contact in contacts) {
        if (![contact isKindOfClass:NSDictionary.class] || [contact count] != 3 ||
            !touch_number(contact[@"id"],5) || [contact[@"id"] doubleValue] < 1 ||
            floor([contact[@"id"] doubleValue]) != [contact[@"id"] doubleValue] ||
            !touch_number(contact[@"x"],1) || !touch_number(contact[@"y"],1) ||
            [ids containsObject:contact[@"id"]]) return NO;
        [ids addObject:contact[@"id"]];
    }
    return YES;
}

static void saveHID(NSString *name, NSDictionary *value) {
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingPrettyPrinted error:NULL];
    if (![data writeToFile:[directory stringByAppendingPathComponent:name] options:NSDataWritingAtomic error:NULL])
        NSLog(@"FRONTCAP_HID evidence write failed: %@", name);
}
static UIWindow *inputWindow(void) {
    for (UIScene *scene in UIApplication.sharedApplication.connectedScenes) {
        if (![scene isKindOfClass:UIWindowScene.class] || scene.activationState != UISceneActivationStateForegroundActive)
            continue;
        for (UIWindow *window in ((UIWindowScene *)scene).windows)
            if (window.isKeyWindow) return window;
    }
    return nil;
}
static NSDictionary *windowRecord(UIWindow *window) {
    if (!window) return nil;
    SEL selector = NSSelectorFromString(@"_contextId");
    if (![window respondsToSelector:selector]) return nil;
    uint32_t context = ((uint32_t (*)(id,SEL))objc_msgSend)(window,selector);
    if (!context) return nil;
    CGRect screenRect = [window convertRect:window.bounds toCoordinateSpace:window.screen.coordinateSpace];
    return @{@"generation":generation, @"pid":@(getpid()),
        @"token":[NSString stringWithFormat:@"%p-%u",window,context],
        @"width":@(window.bounds.size.width), @"height":@(window.bounds.size.height),
        @"scale":@(window.screen.scale),
        @"fullscreen":(CGRectEqualToRect(screenRect,window.screen.bounds) &&
            window.bounds.origin.x == 0 && window.bounds.origin.y == 0 &&
            window.bounds.size.width == window.screen.bounds.size.width &&
            window.bounds.size.height == window.screen.bounds.size.height) ? @YES : @NO};
}
static BOOL enqueuePointsStamped(NSArray *points, UIWindow *window, uint64_t *eventTimestamp) {
    if (UIApplication.sharedApplication.applicationState != UIApplicationStateActive || inputWindow() != window)
        return NO;
    SEL contextSelector = NSSelectorFromString(@"_contextId");
    SEL enqueueSelector = NSSelectorFromString(@"_enqueueHIDEvent:");
    if (![window respondsToSelector:contextSelector] ||
        ![UIApplication.sharedApplication respondsToSelector:enqueueSelector]) return NO;
    uint32_t context = ((uint32_t (*)(id,SEL))objc_msgSend)(window,contextSelector);
    if (!context) return NO;
    BOOL touching = NO;
    uint32_t parentMask = 0;
    for (NSDictionary *point in points) {
        touching |= [point[@"down"] boolValue];
        parentMask |= [point[@"mask"] unsignedIntValue];
    }
    uint64_t timestamp = mach_absolute_time();
    HIDEvent hand = makeHand(kCFAllocatorDefault,timestamp,3,0,0,parentMask,0,0,0,0,0,0,false,touching,0);
    if (!hand) return NO;
    setInteger(hand,(11<<16)+25,1); // DigitizerIsDisplayIntegrated.
    for (NSDictionary *point in points) {
        uint32_t identifier = [point[@"id"] unsignedIntValue];
        BOOL down = [point[@"down"] boolValue];
        HIDEvent finger = makeFinger(kCFAllocatorDefault,timestamp,identifier,identifier,
            [point[@"mask"] unsignedIntValue],[point[@"x"] doubleValue],[point[@"y"] doubleValue],
            0,0,0,down,down,0);
        if (!finger) { CFRelease(hand); return NO; }
        setFloat(finger,(11<<16)+20,5); // Major/minor radius.
        setFloat(finger,(11<<16)+21,5);
        appendEvent(hand,finger,0);
        CFRelease(finger);
    }
    setContext(hand,context,false,false,NULL,0,0);
    ((void (*)(id,SEL,HIDEvent))objc_msgSend)(UIApplication.sharedApplication,enqueueSelector,hand);
    CFRelease(hand);
    if (eventTimestamp) *eventTimestamp = timestamp;
    return YES; // Enqueued, not a delivery or consumer-effect acknowledgement.
}
static BOOL enqueuePoints(NSArray *points, UIWindow *window) {
    return enqueuePointsStamped(points, window, NULL);
}
static BOOL enqueueFrame(NSArray *paths, double ms, UIWindow *window) {
    NSMutableArray *points = [NSMutableArray array];
    for (NSUInteger i=0; i<paths.count; i++) {
        NSArray *path = paths[i];
        double first = [path[0][@"ms"] doubleValue], last = [path.lastObject[@"ms"] doubleValue];
        if (ms < first || ms > last) continue;
        NSDictionary *point = path[0];
        for (NSDictionary *candidate in path) {
            if ([candidate[@"ms"] doubleValue] > ms) break;
            point = candidate;
        }
        BOOL down = ms != last;
        BOOL changed = ms == first || ms == last;
        BOOL moved = !changed && [point[@"ms"] doubleValue] == ms;
        uint32_t mask = changed ? 3 : (moved ? 4 : 0); // Range/Touch or Position.
        [points addObject:@{@"id":@(i+1), @"mask":@(mask), @"down":@(down),
            @"x":@([point[@"x"] doubleValue]*window.bounds.size.width),
            @"y":@([point[@"y"] doubleValue]*window.bounds.size.height)}];
    }
    return enqueuePoints(points,window);
}
static void updateContacts(NSDictionary *request, NSString *replyName, NSUInteger number, UIWindow *window) {
    if ([request[@"expected_revision"] unsignedIntegerValue] != contactRevision) {
        saveHID(replyName,@{@"status":@"rejected", @"generation":generation, @"request_id":@(number),
            @"error":@"stale_contact_revision", @"contact_revision":@(contactRevision)});
        return;
    }
    if (heldContacts.count && (window != heldWindow || !CGRectEqualToRect(window.bounds,heldBounds))) {
        fenced = YES;
        saveHID(replyName,@{@"status":@"unknown", @"generation":generation, @"request_id":@(number),
            @"error":@"held_contact_window_changed"});
        return;
    }
    BOOL cancelling = [request[@"cancel"] isEqual:@YES];
    NSMutableDictionary *next = [NSMutableDictionary dictionary];
    for (NSDictionary *contact in request[@"contacts"]) next[contact[@"id"]] = contact;
    NSMutableSet *ids = [NSMutableSet setWithArray:heldContacts.allKeys ?: @[]];
    [ids addObjectsFromArray:next.allKeys];
    NSMutableArray *points = [NSMutableArray array];
    BOOL changed = NO;
    for (NSNumber *identifier in [[ids allObjects] sortedArrayUsingSelector:@selector(compare:)]) {
        NSDictionary *old = heldContacts[identifier], *current = next[identifier];
        NSDictionary *point = current ?: old;
        uint32_t mask = (!old || !current) ? 3 : ([old isEqual:current] ? 0 : 4);
        if (cancelling) mask |= 1 << 7; // DigitizerEventCancel, distinct from lift.
        changed |= mask != 0;
        [points addObject:@{@"id":identifier, @"mask":@(mask), @"down":@(current != nil),
            @"x":@([point[@"x"] doubleValue]*window.bounds.size.width),
            @"y":@([point[@"y"] doubleValue]*window.bounds.size.height)}];
    }
    saveHID(replyName,@{@"status":@"executing", @"generation":generation, @"request_id":@(number)});
    @try {
        if (changed && !enqueuePoints(points,window)) fenced = YES;
    } @catch (NSException *error) { fenced = YES; }
    if (!fenced) {
        heldContacts = [next copy];
        heldWindow = next.count ? window : nil;
        heldBounds = window.bounds;
        contactRevision++;
    }
    saveHID(replyName,@{@"status":fenced ? @"unknown" : @"enqueued", @"generation":generation,
        @"request_id":@(number), @"contact_revision":@(contactRevision),
        @"contacts":heldContacts.allValues ?: @[], @"changed":@(changed),
        @"mach_timestamp":@(mach_absolute_time()), @"request":request});
}
static void finishShortTap(NSDictionary *request, NSString *replyName, NSUInteger number,
    NSArray *frames, mach_timebase_info_data_t timebase, NSString *error) {
    if (error) fenced = YES;
    else { heldContacts = @{}; heldWindow = nil; contactRevision++; }
    saveHID(replyName,@{@"status":error ? @"unknown" : @"enqueued", @"generation":generation,
        @"request_id":@(number), @"contact_revision":@(contactRevision),
        @"contacts":heldContacts.allValues ?: @[], @"changed":frames.count > 0 ? @YES : @NO,
        @"mach_timestamp":@(mach_absolute_time()), @"request":request,
        @"tap":@{@"timebase_numer":@(timebase.numer), @"timebase_denom":@(timebase.denom), @"frames":frames},
        @"error":error ?: NSNull.null});
    busy = NO;
}
static NSDictionary *enqueueTapPoint(NSDictionary *point, BOOL down, UIWindow *window) {
    uint64_t timestamp = 0;
    NSDictionary *native = @{@"id":@1, @"mask":@3, @"down":@(down),
        @"x":point[@"x"], @"y":point[@"y"]};
    BOOL enqueued = enqueuePointsStamped(@[native],window,&timestamp);
    return @{@"id":@1, @"down":@(down), @"x":point[@"x"], @"y":point[@"y"],
        @"enqueued":@(enqueued), @"mach_timestamp":@(timestamp)};
}
static void shortTap(NSDictionary *request, NSString *replyName, NSUInteger number, UIWindow *window) {
    mach_timebase_info_data_t timebase = {0};
    if ([request[@"expected_revision"] unsignedIntegerValue] != contactRevision || heldContacts.count ||
        mach_timebase_info(&timebase) != KERN_SUCCESS || !timebase.numer || !timebase.denom) {
        saveHID(replyName,@{@"status":@"rejected", @"generation":generation, @"request_id":@(number),
            @"error":@"short_tap_requires_released_contacts_current_revision_and_clock"});
        return;
    }
    busy = YES;
    NSDictionary *point = @{@"x":@([request[@"tap"][@"x"] doubleValue] * window.bounds.size.width),
        @"y":@([request[@"tap"][@"y"] doubleValue] * window.bounds.size.height)};
    CGRect bounds = window.bounds;
    NSMutableArray *frames = [NSMutableArray array];
    saveHID(replyName,@{@"status":@"executing", @"generation":generation, @"request_id":@(number)});
    @try {
        NSDictionary *down = enqueueTapPoint(point,YES,window);
        [frames addObject:down];
        if (![down[@"enqueued"] boolValue]) {
            finishShortTap(request,replyName,number,frames,timebase,@"short_tap_down_uncertain"); return;
        }
    } @catch (NSException *exception) {
        finishShortTap(request,replyName,number,frames,timebase,@"short_tap_down_exception"); return;
    }
    heldContacts = @{@1:@{@"id":@1, @"x":request[@"tap"][@"x"], @"y":request[@"tap"][@"y"]}};
    heldWindow = window; heldBounds = bounds;
    // Give UIKit a run-loop turn for DOWN, without a host capture/transport between events.
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW,20*NSEC_PER_MSEC),dispatch_get_main_queue(),^{
        NSString *error = nil;
        uint64_t start = [frames[0][@"mach_timestamp"] unsignedLongLongValue];
        long double age = (long double)(mach_absolute_time()-start)*timebase.numer/timebase.denom;
        if (age > 100*NSEC_PER_MSEC) error = @"short_tap_age_exceeded";
        else if (!CGRectEqualToRect(window.bounds,bounds) ||
            ![windowRecord(window) isEqual:request[@"expected_window"]]) error = @"short_tap_window_changed";
        else @try {
            NSDictionary *up = enqueueTapPoint(point,NO,window);
            [frames addObject:up];
            if (![up[@"enqueued"] boolValue]) error = @"short_tap_up_uncertain";
            else if ((long double)([up[@"mach_timestamp"] unsignedLongLongValue]-start)*timebase.numer/timebase.denom
                > 100*NSEC_PER_MSEC) error = @"short_tap_age_exceeded";
        } @catch (NSException *exception) { error = @"short_tap_up_exception"; }
        // Failure never creates another UP/cancel or authorizes replay.
        finishShortTap(request,replyName,number,frames,timebase,error);
    });
}
static void pollHID(void) {
    // Publish observation-only geometry even when input is fenced. The command rechecks it
    // on this same main queue immediately before native enqueue; polling is not admission.
    NSDictionary *geometry = windowRecord(inputWindow());
    geometry = geometry ?: @{};
    if (![geometry isEqual:lastGeometry]) { saveHID(@"window.json",geometry); lastGeometry = geometry; }
    if (busy || fenced || nextRequest > requestCapacity) return;
    NSString *name = [NSString stringWithFormat:@"request-%lu.json",(unsigned long)nextRequest];
    NSString *path = [directory stringByAppendingPathComponent:name];
    NSDictionary *attributes = [NSFileManager.defaultManager attributesOfItemAtPath:path error:NULL];
    if (!attributes) return;
    NSData *data = [attributes fileSize] <= 8192 ? [NSData dataWithContentsOfFile:path] : nil;
    id request = data ? [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL] : nil;
    NSUInteger number = nextRequest++;
    NSString *replyName = [NSString stringWithFormat:@"reply-%lu.json",(unsigned long)number];
    BOOL tapRequest = [request isKindOfClass:NSDictionary.class] && request[@"tap"] != nil;
    BOOL cancelRequest = [request isKindOfClass:NSDictionary.class] && request[@"cancel"] != nil;
    BOOL contactRequest = [request isKindOfClass:NSDictionary.class] &&
        (request[@"contacts"] != nil || cancelRequest || tapRequest);
    BOOL observed = [request isKindOfClass:NSDictionary.class] && request[@"expected_window"] != nil;
    BOOL valid = [request isKindOfClass:NSDictionary.class] &&
        (!tapRequest || observed) &&
        (!observed || (contactRequest && [request[@"expected_window"] isKindOfClass:NSDictionary.class])) &&
        [request count] == (contactRequest ? 4 + (observed ? 1 : 0) : 3) &&
        [request[@"generation"] isKindOfClass:NSString.class] && [request[@"generation"] isEqual:generation] &&
        touch_number(request[@"request_id"],requestCapacity) && [request[@"request_id"] doubleValue] == number &&
        (contactRequest ? ((cancelRequest ? (request[@"cancel"] == (__bridge id)kCFBooleanTrue &&
            request[@"contacts"] == nil) : (tapRequest ?
            ([request[@"tap"] isKindOfClass:NSDictionary.class] && [request[@"tap"] count] == 2 &&
                touch_number(request[@"tap"][@"x"],1) && touch_number(request[@"tap"][@"y"],1)) :
            contactsValid(request[@"contacts"]))) &&
            touch_number(request[@"expected_revision"],requestCapacity) &&
            floor([request[@"expected_revision"] doubleValue]) == [request[@"expected_revision"] doubleValue]) :
            touch_paths_valid(request[@"paths"]));
    UIWindow *window = inputWindow();
    if (!valid || !available || !window) {
        saveHID(replyName,@{@"status":@"rejected", @"generation":generation, @"request_id":@(number),
            @"error":!valid ? @"invalid_request" : (!available ? @"hid_api_unavailable" : @"no_foreground_window")});
        return;
    }
    if (observed && ![request[@"expected_window"] isEqual:windowRecord(window)]) {
        saveHID(replyName,@{@"status":@"rejected", @"generation":generation, @"request_id":@(number),
            @"error":@"observation_window_changed"});
        return;
    }
    if (tapRequest) { shortTap(request,replyName,number,window); return; }
    if (contactRequest) { updateContacts(request,replyName,number,window); return; }
    if (heldContacts.count) {
        saveHID(replyName,@{@"status":@"rejected", @"generation":generation, @"request_id":@(number),
            @"error":@"contacts_still_held"});
        return;
    }
    busy = YES;
    NSArray *paths = request[@"paths"];
    NSMutableSet *times = [NSMutableSet set];
    for (NSArray *points in paths)
        for (NSDictionary *point in points) [times addObject:point[@"ms"]];
    NSArray *ordered = [[times allObjects] sortedArrayUsingSelector:@selector(compare:)];
    NSMutableArray *receipts = [NSMutableArray array];
    saveHID(replyName,@{@"status":@"executing", @"generation":generation, @"request_id":@(number)});
    uint64_t start = mach_absolute_time();
    dispatch_time_t origin = dispatch_time(DISPATCH_TIME_NOW,0);
    for (NSUInteger i=0; i<ordered.count; i++) {
        double ms = [ordered[i] doubleValue];
        BOOL final = i == ordered.count-1;
        dispatch_after(dispatch_time(origin,(int64_t)(ms*NSEC_PER_MSEC)),dispatch_get_main_queue(),^{
            if (fenced) return;
            @try {
                BOOL enqueued = enqueueFrame(paths,ms,window);
                [receipts addObject:@{@"offset_ms":@(ms), @"mach_timestamp":@(mach_absolute_time()),
                    @"enqueued":@(enqueued)}];
                if (!enqueued) fenced = YES;
            } @catch (NSException *error) {
                fenced = YES;
                [receipts addObject:@{@"offset_ms":@(ms), @"error":error.reason ?: @"native_exception"}];
            }
            if (final || fenced) {
                saveHID(replyName,@{@"status":fenced ? @"unknown" : @"enqueued",
                    @"generation":generation, @"request_id":@(number), @"start_mach_timestamp":@(start),
                    @"frames":receipts, @"request":request});
                busy = NO;
            }
        });
    }
}
__attribute__((constructor)) static void startHIDProbe(void) {
    dispatch_async(dispatch_get_main_queue(),^{
        void *io = dlopen("/System/Library/Frameworks/IOKit.framework/IOKit",RTLD_NOW);
        void *backboard = dlopen("/System/Library/PrivateFrameworks/BackBoardServices.framework/BackBoardServices",RTLD_NOW);
        makeHand = dlsym(io ?: RTLD_DEFAULT,"IOHIDEventCreateDigitizerEvent");
        makeFinger = dlsym(io ?: RTLD_DEFAULT,"IOHIDEventCreateDigitizerFingerEvent");
        setInteger = dlsym(io ?: RTLD_DEFAULT,"IOHIDEventSetIntegerValue");
        setFloat = dlsym(io ?: RTLD_DEFAULT,"IOHIDEventSetFloatValue");
        appendEvent = dlsym(io ?: RTLD_DEFAULT,"IOHIDEventAppendEvent");
        makeClient = dlsym(io ?: RTLD_DEFAULT,"IOHIDEventSystemClientCreate");
        setContext = dlsym(backboard ?: RTLD_DEFAULT,"BKSHIDEventSetDigitizerInfo");
        available = makeHand && makeFinger && setInteger && setFloat && appendEvent && makeClient && setContext;
        if (available) { client = makeClient(kCFAllocatorDefault); available = client != NULL; }
        directory = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/frontcap-hid"];
        [NSFileManager.defaultManager createDirectoryAtPath:directory withIntermediateDirectories:YES attributes:nil error:NULL];
        generation = NSUUID.UUID.UUIDString.lowercaseString;
        saveHID(@"endpoint.json",@{@"generation":generation, @"pid":@(getpid()), @"available":@(available), @"observed_contacts":@YES, @"short_tap":@YES, @"request_capacity":@(requestCapacity)});
        poller = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER,0,0,dispatch_get_main_queue());
        dispatch_source_set_timer(poller,dispatch_time(DISPATCH_TIME_NOW,0),50*NSEC_PER_MSEC,5*NSEC_PER_MSEC);
        dispatch_source_set_event_handler(poller,^{ pollHID(); });
        dispatch_resume(poller);
    });
}
