// Simulator-only prerequisite: inject into an existing host without rebuilding it.
#import <Foundation/Foundation.h>
#import <WebKit/WebKit.h>
#import <objc/runtime.h>

static NSString *script;
static IMP originalInit;
static IMP originalAddHandler;
void registerControlledWebView(WKWebView *webView);
void startControlledWebViewRegistry(void);
static BOOL controlled(void) {
    NSString *mode = NSProcessInfo.processInfo.environment[@"FRONTCAP_WEBVIEW_CONTROL"];
    return [mode isEqual:@"1"] || [mode isEqual:@"pages"];
}
#ifdef FRONTCAP_MEDIA_SUSPENSION_PROBE
void prepareMediaSuspensionProbe(WKWebViewConfiguration *configuration);
void bindMediaSuspensionProbe(WKWebView *view);
#endif

@interface FrontcapMessageForwarder : NSObject <WKScriptMessageHandler>
@property(nonatomic, strong) id<WKScriptMessageHandler> recipient;
@end
@implementation FrontcapMessageForwarder
- (void)userContentController:(WKUserContentController *)controller didReceiveScriptMessage:(WKScriptMessage *)message {
    @try {
    NSDictionary *record = @{@"name":message.name, @"body":message.body ?: NSNull.null,
        @"url":message.frameInfo.request.URL.absoluteString ?: @"",
        @"main_frame":@(message.frameInfo.mainFrame), @"host_epoch_s":@(NSDate.date.timeIntervalSince1970)};
    NSData *data = [NSJSONSerialization dataWithJSONObject:record options:0 error:NULL];
    NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/webview-messages.jsonl"];
    @synchronized(FrontcapMessageForwarder.class) {
        if (data && data.length <= 65536) {
            if (![NSFileManager.defaultManager fileExistsAtPath:path])
                [NSFileManager.defaultManager createFileAtPath:path contents:nil attributes:nil];
            NSFileHandle *file = [NSFileHandle fileHandleForWritingAtPath:path];
            if (file) {
                [file seekToEndOfFile];
                [file writeData:data];
                [file writeData:[@"\n" dataUsingEncoding:NSUTF8StringEncoding]];
                [file closeFile];
            } else NSLog(@"FRONTCAP_ATTACH message recording unavailable");
        } else NSLog(@"FRONTCAP_ATTACH message recording omitted: invalid or oversized body");
    }
    } @catch (NSException *error) {
        NSLog(@"FRONTCAP_ATTACH message recording failed: %@", error.reason);
    }
    [self.recipient userContentController:controller didReceiveScriptMessage:message];
}
@end

static void addHandler(id self, SEL selector, id<WKScriptMessageHandler> handler, NSString *name) {
    FrontcapMessageForwarder *forwarder = [FrontcapMessageForwarder new];
    forwarder.recipient = handler;
    ((void (*)(id, SEL, id, NSString *))originalAddHandler)(self, selector, forwarder, name);
}

void addPlatformScriptMessageHandler(WKUserContentController *controller,
    id<WKScriptMessageHandler> handler, NSString *name) {
    if (originalAddHandler)
        ((void (*)(id, SEL, id, NSString *))originalAddHandler)(controller,
            @selector(addScriptMessageHandler:name:), handler, name);
    else [controller addScriptMessageHandler:handler name:name];
}

static id initializeWebView(id self, SEL selector, CGRect frame, WKWebViewConfiguration *configuration) {
    WKWebViewConfiguration *copy = [configuration copy];
    WKUserScript *injection = [[WKUserScript alloc] initWithSource:script
        injectionTime:WKUserScriptInjectionTimeAtDocumentStart forMainFrameOnly:YES];
    [copy.userContentController addUserScript:injection];
#ifdef FRONTCAP_MEDIA_SUSPENSION_PROBE
    prepareMediaSuspensionProbe(copy);
#endif
    NSLog(@"FRONTCAP_ATTACH document-start script registered");
    id result = ((id (*)(id, SEL, CGRect, WKWebViewConfiguration *))originalInit)(self, selector, frame, copy);
#ifdef FRONTCAP_MEDIA_SUSPENSION_PROBE
    bindMediaSuspensionProbe(result);
#endif
    if (controlled())
        registerControlledWebView(result);
    return result;
}

__attribute__((constructor)) static void attach(void) {
    @autoreleasepool {
        NSString *path = NSProcessInfo.processInfo.environment[@"FRONTCAP_WEBVIEW_SCRIPT"];
        NSError *error = nil;
        script = [NSString stringWithContentsOfFile:path encoding:NSUTF8StringEncoding error:&error];
        if (script.length && [NSProcessInfo.processInfo.environment[@"FRONTCAP_MUTE_AUDIO"] isEqual:@"1"])
            script = [@"Object.defineProperty(globalThis,'__frontcapMuteOutput',{value:true});\n"
                stringByAppendingString:script];
        Method method = class_getInstanceMethod(WKWebView.class, @selector(initWithFrame:configuration:));
        if (!script.length || !method) {
            NSLog(@"FRONTCAP_ATTACH failed: %@", error ?: @"missing script or initializer");
            return;
        }
        originalInit = method_setImplementation(method, (IMP)initializeWebView);
        if (controlled()) {
            Method add = class_getInstanceMethod(WKUserContentController.class, @selector(addScriptMessageHandler:name:));
            if (add) originalAddHandler = method_setImplementation(add, (IMP)addHandler);
        }
        if ([NSProcessInfo.processInfo.environment[@"FRONTCAP_WEBVIEW_CONTROL"] isEqual:@"pages"])
            dispatch_async(dispatch_get_main_queue(), ^{ startControlledWebViewRegistry(); });
        NSLog(@"FRONTCAP_ATTACH installed");
    }
}
