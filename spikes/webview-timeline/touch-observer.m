// Opt-in native boundary diagnostic. Link only into the experiment's injected dylib.
#import <UIKit/UIKit.h>
#import <objc/runtime.h>

static IMP originalSendEvent;
static NSUInteger count;
static void observeEvent(id app, SEL selector, UIEvent *event) {
    @try {
        if (event.type == UIEventTypeTouches && count < 2000) {
            NSMutableArray *touches = [NSMutableArray array];
            for (UITouch *touch in event.allTouches) {
                CGPoint point = [touch locationInView:touch.window];
                [touches addObject:@{@"id":[NSString stringWithFormat:@"%p", touch],
                    @"phase":@(touch.phase), @"timestamp":@(touch.timestamp),
                    @"x":@(point.x), @"y":@(point.y)}];
            }
            count++;
            NSDictionary *record = @{@"sequence":@(count), @"timestamp":@(event.timestamp),
                @"touches":touches, @"limit_reached":@(count == 2000)};
            NSData *data = [NSJSONSerialization dataWithJSONObject:record options:0 error:NULL];
            NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/native-touches.jsonl"];
            if (![NSFileManager.defaultManager fileExistsAtPath:path])
                [NSFileManager.defaultManager createFileAtPath:path contents:nil attributes:nil];
            NSFileHandle *file = [NSFileHandle fileHandleForWritingAtPath:path];
            if (data && file) {
                [file seekToEndOfFile]; [file writeData:data];
                [file writeData:[@"\n" dataUsingEncoding:NSUTF8StringEncoding]];
                [file closeFile];
            } else NSLog(@"FRONTCAP_TOUCH observation write failed");
        }
    } @catch (NSException *error) {
        NSLog(@"FRONTCAP_TOUCH observation failed: %@", error.reason);
    }
    ((void (*)(id, SEL, UIEvent *))originalSendEvent)(app, selector, event);
}
__attribute__((constructor)) static void installTouchObserver(void) {
    Method method = class_getInstanceMethod(UIApplication.class, @selector(sendEvent:));
    if (method) originalSendEvent = method_setImplementation(method, (IMP)observeEvent);
    else NSLog(@"FRONTCAP_TOUCH sendEvent unavailable");
}
