// Host-only feasibility probe. Never load this library into the consumer application.
// The SDK interception point is documented by facebook/idb; see README.md.
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#include <stdlib.h>

static NSString *deviceSetPath;
static IMP originalDeviceSet;

static id selectDeviceSet(id context, SEL selector, NSString *requested, NSError **error) {
    (void)requested;
    id (*original)(id, SEL, NSString *, NSError **) = (void *)originalDeviceSet;
    return original(context, selector, deviceSetPath, error);
}

__attribute__((constructor)) static void configureDeviceSet(void) {
    @autoreleasepool {
        // This injection belongs to xcodebuild alone, including its process lifetime.
        const char *configured = getenv("FRONTCAP_XCODE_DEVICE_SET");
        unsetenv("DYLD_INSERT_LIBRARIES");
        if (!configured) return;
        deviceSetPath = [[NSString alloc] initWithUTF8String:configured];
        unsetenv("FRONTCAP_XCODE_DEVICE_SET");
        BOOL directory = NO;
        if (![deviceSetPath isAbsolutePath] ||
            ![[NSFileManager defaultManager] fileExistsAtPath:deviceSetPath isDirectory:&directory] ||
            !directory) {
            fprintf(stderr, "frontcap device-set probe: invalid directory\n");
            _Exit(78);
        }
        NSError *error = nil;
        NSBundle *framework = [NSBundle bundleWithPath:@"/Library/Developer/PrivateFrameworks/CoreSimulator.framework"];
        if (![framework loadAndReturnError:&error]) {
            fprintf(stderr, "frontcap device-set probe: CoreSimulator unavailable\n");
            _Exit(78);
        }
        Method method = class_getInstanceMethod(NSClassFromString(@"SimServiceContext"),
                                               NSSelectorFromString(@"deviceSetWithPath:error:"));
        if (!method || method_getNumberOfArguments(method) != 4) {
            fprintf(stderr, "frontcap device-set probe: SDK selector unavailable\n");
            _Exit(78);
        }
        originalDeviceSet = method_setImplementation(method, (IMP)selectDeviceSet);
        fprintf(stderr, "frontcap device-set probe: host routing installed\n");
    }
}
