// Reuse the simulator clock request journal/transport; WebKit work stays on main.
#import <WebKit/WebKit.h>
#import "../s18-native-time/ios/clock_control.h"
#include <stdatomic.h>
#include <unistd.h>
// Optional producer: a linked observer supplies the strong definition.
__attribute__((weak)) NSDictionary *speechObservationSnapshot(void) { return nil; }
#ifdef FRONTCAP_SPEECH_CONTROL
NSDictionary *speechControlPreflight(uint64_t delta, uint64_t *identity);
void speechControlAdvance(uint64_t delta, uint64_t identity);
NSDictionary *speechControlSnapshot(void);
#endif

// Each state keeps only a weak reference to its original view. Neither discovery nor
// a subsequent view can rebind an accepted request sequence.
@interface FCWebViewClock : NSObject {
@public
    __weak WKWebView *target;
    NSString *document;
    _Atomic(bool) fenced;
    _Atomic(uint64_t) frontier;
}
@property(nonatomic, strong) FCClockChannel *channel;
@property(nonatomic, copy) NSString *identifier;
@property(nonatomic, copy) NSString *viewIdentifier;
@property(nonatomic, copy) NSString *observedDocument;
@property(nonatomic, copy) NSString *documentURL;
@property(nonatomic) BOOL superseded;
- (NSDictionary *)perform:(NSDictionary *)request;
@end

#ifdef FRONTCAP_MEDIA_PROCESS_PROBE
static NSDictionary *mediaProcesses(WKWebView *view) {
    NSMutableDictionary *processes = [NSMutableDictionary dictionary];
    for (NSString *name in @[@"_webProcessIdentifier", @"_gpuProcessIdentifier"]) {
        SEL selector = NSSelectorFromString(name);
        if (![view respondsToSelector:selector]) {
            processes[name] = NSNull.null;
            continue;
        }
        // Diagnostic-only private SPI. No process mutation or alternate WebView selection.
        @try {
            pid_t (*getter)(id, SEL) = (pid_t (*)(id, SEL))[view methodForSelector:selector];
            processes[name] = @(getter(view, selector));
        } @catch (NSException *exception) {
            processes[name] = @{@"error":exception.name};
        }
    }
    return processes;
}
#endif

@implementation FCWebViewClock
- (NSDictionary *)perform:(NSDictionary *)request {
        if (atomic_load(&fenced))
            return @{@"status":@"rejected", @"error":@"replacement_required"};
        dispatch_semaphore_t finished = dispatch_semaphore_create(0);
        __block NSDictionary *reply;
        dispatch_async(dispatch_get_main_queue(), ^{
            WKWebView *view = target;
            if (!view || atomic_load(&fenced) || self.superseded) {
                reply = @{@"status":@"rejected", @"error":@"webview_unavailable"};
                dispatch_semaphore_signal(finished);
                return;
            }
            if (self.viewIdentifier && !self.observedDocument) {
                reply = @{@"status":@"not_ready"};
                dispatch_semaphore_signal(finished);
                return;
            }
#ifdef FRONTCAP_SPEECH_CONTROL
            uint64_t speechIdentity = 0;
            BOOL advancing = [request[@"operation"] isEqual:@"advance"];
            uint64_t speechDelta = [request[@"delta_ns"] unsignedLongLongValue];
            if (advancing) {
                NSDictionary *rejection = speechControlPreflight(speechDelta, &speechIdentity);
                if (rejection) {
                    reply = rejection;
                    dispatch_semaphore_signal(finished);
                    return;
                }
            }
#endif
            NSString *body =
                @"if (!globalThis.__frontcapTime || !globalThis.__frontcapDocument) "
                 "return {status:'not_ready'};"
                 "if (expected && expected !== __frontcapDocument) "
                 "return {status:'rejected',error:'document_changed'};"
                 "const progress = advance ? await __frontcapTime.advance(delta) : null;"
                 "return {status:'ok', document:__frontcapDocument, url:location.href,"
                 "visibility:document.visibilityState, events:__frontcapEvents.snapshot(),"
                 "navigation:globalThis.__frontcapNavigation?.snapshot() ?? null,"
                 "console:globalThis.__frontcapConsole?.snapshot() ?? null,"
                 "media:__frontcapMedia.snapshot(),"
                 "startingState:globalThis.__frontcapStartingState?.snapshot() ?? null,"
                 "starting_randomness:globalThis.__frontcapStartingState?.randomness?.() ?? null,"
                 "canvas_kit:{document:__frontcapDocument,published:(()=>{const value="
                 "Object.getOwnPropertyDescriptor(globalThis,'flutterCanvasKit')?.value;"
                 "return value!==null && typeof value==='object';})()},"
                 "pointers:__frontcapPointers.snapshot(),"
                 "state:__frontcapTime.snapshot(), progress};";
            NSDictionary *arguments = @{
                @"expected":document ?: @"",
                @"advance":@([request[@"operation"] isEqual:@"advance"]),
                @"delta":@([request[@"delta_ns"] doubleValue] / 1000000.0)};
            [view callAsyncJavaScript:body arguments:arguments inFrame:nil
                inContentWorld:WKContentWorld.pageWorld completionHandler:^(id value, NSError *error) {
                if (error || ![value isKindOfClass:NSDictionary.class]) {
                    atomic_store(&fenced, true);
                    reply = @{@"status":@"unknown", @"applied":NSNull.null,
                        @"error":error.localizedDescription ?: @"invalid_result"};
                } else {
                    reply = value;
                    if ([value[@"status"] isEqual:@"ok"]) {
                        document = value[@"document"];
                        atomic_store(&frontier, (uint64_t)([value[@"state"][@"elapsedMs"] doubleValue] * 1000000));
                        NSMutableDictionary *result = [value mutableCopy];
                        result[@"applied"] = arguments[@"advance"];
#ifndef FRONTCAP_SPEECH_CONTROL
                        NSDictionary *nativeSpeech = speechObservationSnapshot();
                        if (nativeSpeech) result[@"native_speech"] = nativeSpeech;
#endif
#ifdef FRONTCAP_MEDIA_PROCESS_PROBE
                        result[@"media_processes"] = mediaProcesses(view);
#endif
#ifdef FRONTCAP_SPEECH_CONTROL
                        if (advancing) speechControlAdvance(speechDelta, speechIdentity);
                        NSDictionary *speech = speechControlSnapshot();
                        result[@"speech"] = speech;
                        if (speech[@"failure"] != NSNull.null) {
                            atomic_store(&fenced, true);
                            result[@"status"] = @"unknown";
                            result[@"applied"] = NSNull.null;
                            result[@"error"] = speech[@"failure"];
                        }
#endif
                        reply = result;
                    } else if ([value[@"error"] isEqual:@"document_changed"]) {
                        atomic_store(&fenced, true);
                    }
                }
                dispatch_semaphore_signal(finished);
            }];
        });
        if (dispatch_semaphore_wait(finished, dispatch_time(DISPATCH_TIME_NOW, 5 * NSEC_PER_SEC))) {
            // Timeout does not cancel WebKit work. Fence new requests; the shared
            // journal caches this original unknown result instead of replaying it.
            atomic_store(&fenced, true);
            return @{@"status":@"unknown", @"applied":NSNull.null, @"error":@"webkit_completion_timeout"};
        }
        return reply;
}
@end

static FCWebViewClock *legacyClock;
static NSMutableArray<FCWebViewClock *> *pages;
static NSDictionary *publishedPages;
static NSUInteger revision;
static BOOL capacityExceeded;
static NSMutableArray<NSDictionary *> *navigation;
static uint64_t navigationReceived;
static NSString *navigationFailure;
static NSHashTable<WKUserContentController *> *navigationControllers;
static NSString *navigationHandlerName;

static NSString *boundedURL(NSString *url, BOOL *truncated) {
    NSData *data = [url dataUsingEncoding:NSUTF8StringEncoding];
    *truncated = data.length > 1024;
    if (!*truncated) return url;
    for (NSUInteger length = 1024; length > 1020; length--) {
        NSString *prefix = [[NSString alloc] initWithData:[data subdataWithRange:NSMakeRange(0,length)]
            encoding:NSUTF8StringEncoding];
        if (prefix) return prefix;
    }
    return @"";
}

static FCWebViewClock *newPageClock(WKWebView *view, NSString *viewIdentifier, NSString *identity);
static void publishPages(void);

@interface FCNavigationObserver : NSObject <WKScriptMessageHandler>
@end
@implementation FCNavigationObserver
- (void)userContentController:(WKUserContentController *)controller didReceiveScriptMessage:(WKScriptMessage *)message {
    NSCAssert(NSThread.isMainThread, @"document notifications run on main");
    if (!message.frameInfo.mainFrame || !message.webView) return;
    FCWebViewClock *page;
    for (FCWebViewClock *candidate in pages.reverseObjectEnumerator)
        if (candidate->target == message.webView) { page = candidate; break; }
    if (!page) return;
    NSString *identity = [message.body isKindOfClass:NSString.class] ? message.body : nil;
    NSUUID *uuid = identity.length == 36 ? [[NSUUID alloc] initWithUUIDString:identity] : nil;
    if (!uuid || ![uuid.UUIDString.lowercaseString isEqual:identity]) {
        if (!navigationFailure) navigationFailure = @"invalid_document_start_identity";
        return;
    }
    if (page.observedDocument && ![page.observedDocument isEqual:identity]) {
        page.superseded = YES;
        if (pages.count >= 64) {
            capacityExceeded = YES;
            if (!navigationFailure) navigationFailure = @"document_channel_capacity_exceeded";
            publishPages(); return;
        }
        page = newPageClock(message.webView,page.viewIdentifier,identity);
        [pages addObject:page];
    } else if (!page.observedDocument) {
        // Bind before publishing discovery; later channels never reuse this request sequence.
        page->document = identity;
        page.observedDocument = identity;
    }
    BOOL truncated;
    NSString *url = boundedURL(message.frameInfo.request.URL.absoluteString ?: @"", &truncated);
    page.documentURL = message.frameInfo.request.URL.absoluteString ?: @"";
    [navigation addObject:@{@"sequence":@(++navigationReceived), @"page":page.identifier,
        @"document":identity, @"url":url, @"url_truncated":@(truncated)}];
    if (navigation.count > 32) [navigation removeObjectAtIndex:0];
    publishPages();
}
@end
static FCNavigationObserver *navigationObserver;
void addPlatformScriptMessageHandler(WKUserContentController *controller,
    id<WKScriptMessageHandler> handler, NSString *name);

static void observeDocuments(WKWebView *view) {
    WKUserContentController *controller = view.configuration.userContentController;
    if ([navigationControllers containsObject:controller]) return;
    [navigationControllers addObject:controller];
    addPlatformScriptMessageHandler(controller, navigationObserver, navigationHandlerName);
    // Runs after the platform timeline's document identity is installed, before app scripts.
    // Only a notification: no app delegate replacement, navigation, time advance or new authority.
    NSString *source = [NSString stringWithFormat:
        @"webkit.messageHandlers['%@'].postMessage(globalThis.__frontcapDocument ?? null);",
        navigationHandlerName];
    [controller addUserScript:[[WKUserScript alloc] initWithSource:source
        injectionTime:WKUserScriptInjectionTimeAtDocumentStart forMainFrameOnly:YES]];
}


static void publishPages(void) {
    NSCAssert(NSThread.isMainThread, @"page discovery runs on main");
    NSMutableArray *entries = [NSMutableArray array];
    for (FCWebViewClock *page in pages) {
        WKWebView *view = page->target;
        if (!view) [page.channel stop];
        BOOL truncated;
        NSString *url = boundedURL(page.superseded ? (page.documentURL ?: @"") : (view.URL.absoluteString ?: @""), &truncated);
        [entries addObject:@{@"id":page.identifier, @"view":page.viewIdentifier,
            @"document":page.observedDocument ?: NSNull.null, @"superseded":page.superseded ? @YES : @NO,
            @"endpoint":page.channel.endpoint,
            @"alive":view ? @YES : @NO, @"control_stopped":@(page.channel.stopped), @"url":url, @"url_truncated":@(truncated)}];
    }
    NSDictionary *state = @{@"pid":@(getpid()), @"capacity_exceeded":@(capacityExceeded), @"pages":entries,
        @"navigation":@{@"received":@(navigationReceived),
            @"dropped":@(navigationReceived - navigation.count),
            @"failure":navigationFailure ?: NSNull.null, @"entries":[navigation copy]}};
    if ([state isEqual:publishedPages]) return;
    NSMutableDictionary *record = [state mutableCopy];
    record[@"revision"] = @(++revision);
    NSData *data = [NSJSONSerialization dataWithJSONObject:record options:0 error:NULL];
    NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/webview-pages.json"];
    if (!data || ![data writeToFile:path options:NSDataWritingAtomic error:NULL]) abort();
    publishedPages = state;
}

void startControlledWebViewRegistry(void) {
#ifdef FRONTCAP_SPEECH_CONTROL
    // The experimental speech queue is process-wide. Independent page clocks
    // cannot each own it, so reject this combination before exposing endpoints.
    NSLog(@"FRONTCAP_ATTACH page registry is incompatible with controlled native speech");
    abort();
#endif
    NSCAssert(NSThread.isMainThread, @"page discovery starts on main");
    if (pages) return;
    pages = [NSMutableArray array];
    navigation = [NSMutableArray array];
    navigationControllers = [NSHashTable weakObjectsHashTable];
    navigationObserver = [FCNavigationObserver new];
    navigationHandlerName = [@"frontcapDocuments" stringByAppendingString:
        [NSUUID.UUID.UUIDString stringByReplacingOccurrencesOfString:@"-" withString:@""]];
    publishPages();
    // One bounded inventory, rewritten only on changes. A missing weak reference is
    // view deallocation evidence, not proof that WebKit killed every producer.
    [NSTimer scheduledTimerWithTimeInterval:0.25 repeats:YES block:^(NSTimer *timer) { publishPages(); }];
}

static FCWebViewClock *newPageClock(WKWebView *view, NSString *viewIdentifier, NSString *identity) {
    FCWebViewClock *state = [FCWebViewClock new];
    state->target = view;
    state.viewIdentifier = viewIdentifier ?: NSUUID.UUID.UUIDString.lowercaseString;
    state.observedDocument = identity; state->document = identity;
    atomic_init(&state->fenced, false); atomic_init(&state->frontier, 0);
    state.identifier = NSUUID.UUID.UUIDString.lowercaseString;
    __weak FCWebViewClock *weakState = state;
    NSDictionary *(^perform)(NSDictionary *) = ^NSDictionary *(NSDictionary *request) {
        FCWebViewClock *page = weakState;
        return page ? [page perform:request] : @{@"status":@"rejected", @"error":@"webview_unavailable"};
    };
    uint64_t (^now)(void) = ^uint64_t {
        FCWebViewClock *page = weakState;
        return page ? atomic_load(&page->frontier) : 0;
    };
    state.channel = start_clock_channel(perform, now,
        [NSString stringWithFormat:@"webview-pages/%@/clock-control.json", state.identifier]);
    return state;
}

void registerControlledWebView(WKWebView *webView) {
    NSCAssert(NSThread.isMainThread, @"WebViews register on main");
    BOOL multiple = [NSProcessInfo.processInfo.environment[@"FRONTCAP_WEBVIEW_CONTROL"] isEqual:@"pages"];
    if (!multiple && legacyClock) {
        atomic_store(&legacyClock->fenced, true);
        return;
    }
    if (multiple) {
        startControlledWebViewRegistry();
        // Retain tombstones for this process lifetime; refuse overflow instead of
        // recycling a page identity or dropping original-result provenance.
        if (pages.count >= 64) { capacityExceeded = YES; publishPages(); return; }
    }
    if (multiple) {
        FCWebViewClock *state = newPageClock(webView,nil,nil);
        [pages addObject:state];
        observeDocuments(webView); publishPages(); return;
    }
    FCWebViewClock *state = [FCWebViewClock new];
    state->target = webView;
    atomic_init(&state->fenced, false); atomic_init(&state->frontier, 0);
    state.identifier = NSUUID.UUID.UUIDString.lowercaseString;
    __weak FCWebViewClock *weakState = state;
    NSDictionary *(^perform)(NSDictionary *) = ^NSDictionary *(NSDictionary *request) {
        FCWebViewClock *page = weakState;
        return page ? [page perform:request] : @{@"status":@"rejected", @"error":@"webview_unavailable"};
    };
    uint64_t (^now)(void) = ^uint64_t {
        FCWebViewClock *page = weakState;
        return page ? atomic_load(&page->frontier) : 0;
    };
    legacyClock = state;
    start_clock_control(perform, now);
}
