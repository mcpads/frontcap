// Test apparatus only: ordinary WebView creation/navigation/removal, commanded by
// the external probe. The fixture neither imports nor calls the injected library.
#import <UIKit/UIKit.h>
#import <WebKit/WebKit.h>

@interface PageFixture : UIResponder <UIWindowSceneDelegate>
@property(nonatomic, strong) UIWindow *window;
@property(nonatomic, strong) NSMutableDictionary<NSString *, WKWebView *> *views;
@property(nonatomic) NSUInteger lastCommand;
@end
@implementation PageFixture
- (void)poll {
    NSString *root = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents"];
    NSData *data = [NSData dataWithContentsOfFile:[root stringByAppendingPathComponent:@"page-command.json"]];
    NSDictionary *command = data ? [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL] : nil;
    NSUInteger sequence = [command[@"sequence"] unsignedIntegerValue];
    if (!sequence || sequence <= self.lastCommand) return;
    NSString *name = command[@"name"], *action = command[@"action"];
    if ([action isEqual:@"create"]) {
        NSCAssert(!self.views[name], @"duplicate fixture view");
        WKWebView *view = [[WKWebView alloc] initWithFrame:self.window.bounds configuration:[WKWebViewConfiguration new]];
        self.views[name] = view;
        [self.window.rootViewController.view addSubview:view];
    }
    if ([action isEqual:@"create"] || [action isEqual:@"navigate"]) {
        [self.views[name] loadHTMLString:@"<html><body>Ordinary page<script>crypto.getRandomValues(new Uint8Array(4));crypto.randomUUID();setInterval(()=>{document.body.dataset.tick=Date.now();document.body.dataset.choice=Math.random()},10)</script></body></html>"
            baseURL:[NSURL URLWithString:[NSString stringWithFormat:@"https://fixture.invalid/%@/%lu", name, (unsigned long)sequence]]];
    } else if ([action isEqual:@"remove"]) {
        [self.views[name] removeFromSuperview];
        [self.views removeObjectForKey:name];
    } else if (![action isEqual:@"create"] && ![action isEqual:@"navigate"]) abort();
    self.lastCommand = sequence;
    NSData *reply = [NSJSONSerialization dataWithJSONObject:@{@"sequence":@(sequence)} options:0 error:NULL];
    if (![reply writeToFile:[root stringByAppendingPathComponent:@"page-command-result.json"] options:NSDataWritingAtomic error:NULL]) abort();
}
- (void)scene:(UIScene *)scene willConnectToSession:(UISceneSession *)session options:(UISceneConnectionOptions *)options {
    if (![scene isKindOfClass:UIWindowScene.class]) return;
    self.views = [NSMutableDictionary dictionary];
    self.window = [[UIWindow alloc] initWithWindowScene:(UIWindowScene *)scene];
    self.window.rootViewController = [UIViewController new];
    [self.window makeKeyAndVisible];
    [NSTimer scheduledTimerWithTimeInterval:0.05 target:self selector:@selector(poll) userInfo:nil repeats:YES];
}
@end
@interface PageFixtureApp : UIResponder <UIApplicationDelegate>
@end
@implementation PageFixtureApp
@end
int main(int argc, char **argv) {
    @autoreleasepool { return UIApplicationMain(argc, argv, nil, NSStringFromClass(PageFixtureApp.class)); }
}
