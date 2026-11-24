// Opt-in mechanism experiment, not part of the default attachment build.
#import <Foundation/Foundation.h>
#import <WebKit/WebKit.h>
#import <objc/runtime.h>
static char probeKey;

@interface FrontcapMediaSuspensionProbe : NSObject <WKScriptMessageHandler>
@property(nonatomic, weak) WKWebView *view;
@property(nonatomic) BOOL started;
@property(nonatomic, strong) NSMutableArray *records;
@end

@implementation FrontcapMediaSuspensionProbe
- (void)save:(NSString *)status {
    NSDictionary *report = @{@"status":status, @"records":self.records,
        @"mechanism":@"WKWebView.setAllMediaPlaybackSuspended"};
    NSData *data = [NSJSONSerialization dataWithJSONObject:report options:NSJSONWritingPrettyPrinted error:NULL];
    NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/media-suspension.json"];
    if (![data writeToFile:path options:NSDataWritingAtomic error:NULL])
        NSLog(@"FRONTCAP_MEDIA_PROBE recording failed");
}
- (void)sample:(NSString *)phase next:(void (^)(void))next {
    [self.view evaluateJavaScript:
        @"({pageMs:performance.now(),media:__frontcapMedia.snapshot()})"
        completionHandler:^(id value, NSError *error) {
            if (error || ![value isKindOfClass:NSDictionary.class]) {
                [self.records addObject:@{@"phase":phase, @"error":error.localizedDescription ?: @"invalid_result"}];
                [self save:@"failed"];
                return;
            }
            [self.records addObject:@{@"phase":phase, @"sample":value}];
            [self save:@"running"];
            next();
        }];
}
- (void)userContentController:(WKUserContentController *)controller didReceiveScriptMessage:(WKScriptMessage *)message {
    if (self.started) return;
    self.started = YES;
    self.records = [NSMutableArray array];
    [self sample:@"playing" next:^{
        [self.view setAllMediaPlaybackSuspended:YES completionHandler:^{
            [self sample:@"suspended" next:^{
                dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 700 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
                    [self sample:@"held" next:^{
                        [self.view setAllMediaPlaybackSuspended:NO completionHandler:^{
                            [self sample:@"resumed" next:^{
                                dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 700 * NSEC_PER_MSEC),
                                    dispatch_get_main_queue(), ^{
                                    [self sample:@"after_resume" next:^{ [self save:@"finished"]; }];
                                });
                            }];
                        }];
                    }];
                });
            }];
        }];
    }];
}
@end

void prepareMediaSuspensionProbe(WKWebViewConfiguration *configuration) {
    FrontcapMediaSuspensionProbe *probe = [FrontcapMediaSuspensionProbe new];
    [configuration.userContentController addScriptMessageHandler:probe name:@"FrontcapMediaSuspensionProbe"];
    NSString *source =
        @"(()=>{const original=HTMLMediaElement.prototype.play;let sent=false;"
         "HTMLMediaElement.prototype.play=function(...args){"
         "this.addEventListener('playing',()=>{if(sent)return;sent=true;"
         "webkit.messageHandlers.FrontcapMediaSuspensionProbe.postMessage('playing');},{once:true});"
         "return Reflect.apply(original,this,args);};})();";
    [configuration.userContentController addUserScript:[[WKUserScript alloc] initWithSource:source
        injectionTime:WKUserScriptInjectionTimeAtDocumentStart forMainFrameOnly:YES]];
    // Keep ownership with the controller; bind the view after its initialization.
    objc_setAssociatedObject(configuration.userContentController, &probeKey,
        probe, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
}

void bindMediaSuspensionProbe(WKWebView *view) {
    FrontcapMediaSuspensionProbe *probe = objc_getAssociatedObject(
        view.configuration.userContentController, &probeKey);
    probe.view = view;
}
