// Installed API diagnostic. The ordinary producer is independent of the injected observer.
#import <UIKit/UIKit.h>
#import <AVFoundation/AVFoundation.h>
#include <dlfcn.h>

static void save(NSString *name, id value) {
    NSURL *documents = [NSFileManager.defaultManager URLsForDirectory:NSDocumentDirectory inDomains:NSUserDomainMask].firstObject;
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:value options:NSJSONWritingSortedKeys error:&error];
    if (!data || ![data writeToURL:[documents URLByAppendingPathComponent:name] options:NSDataWritingAtomic error:&error])
        @throw [NSException exceptionWithName:@"FixtureEvidenceFailure" reason:error.description userInfo:nil];
}

@interface SpeechFailureApp : UIResponder <UIApplicationDelegate, AVSpeechSynthesizerDelegate>
@property(nonatomic) UIWindow *window;
@property(nonatomic) AVSpeechSynthesizer *speech;
@end
@implementation SpeechFailureApp
- (BOOL)application:(UIApplication *)application didFinishLaunchingWithOptions:(NSDictionary *)options {
    self.window = [[UIWindow alloc] initWithFrame:UIScreen.mainScreen.bounds];
    UIViewController *controller = [UIViewController new];
    self.window.rootViewController = controller;
    [self.window makeKeyAndVisible];
    self.speech = [AVSpeechSynthesizer new];
    self.speech.delegate = self;
    dispatch_async(dispatch_get_main_queue(), ^{
        AVSpeechUtterance *utterance = [AVSpeechUtterance speechUtteranceWithString:@"Submission diagnostic"];
        utterance.volume = 0;
        utterance.preUtteranceDelay = 10;
        NSMutableArray *oracle = [NSMutableArray new];
        for (NSUInteger attempt = 0; attempt < 2; attempt++) {
            @try {
                [self.speech speakUtterance:utterance];
                [oracle addObject:@{@"attempt":@(attempt), @"outcome":@"returned"}];
            } @catch (NSException *error) {
                [oracle addObject:@{@"attempt":@(attempt), @"outcome":@"threw", @"exception":error.name}];
            }
        }
        save(@"speech-failure-oracle.json", oracle);
        NSDictionary *(*snapshot)(void) = dlsym(RTLD_DEFAULT, "speechObservationSnapshot");
        if (snapshot) save(@"speech-failure-observation.json", snapshot());
        BOOL accepted = [self.speech stopSpeakingAtBoundary:AVSpeechBoundaryImmediate];
        save(@"speech-failure-stop.json", @{@"accepted":@(accepted)});
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC), dispatch_get_main_queue(), ^{
            if (snapshot) save(@"speech-failure-after-stop.json", snapshot());
            save(@"speech-failure-done.json", @{@"observerAvailable":@(snapshot != NULL)});
        });
    });
    return YES;
}
- (void)speechSynthesizer:(AVSpeechSynthesizer *)s didStartSpeechUtterance:(AVSpeechUtterance *)u {}
- (void)speechSynthesizer:(AVSpeechSynthesizer *)s didFinishSpeechUtterance:(AVSpeechUtterance *)u {}
- (void)speechSynthesizer:(AVSpeechSynthesizer *)s didCancelSpeechUtterance:(AVSpeechUtterance *)u {}
@end

int main(int argc, char **argv) {
    @autoreleasepool { return UIApplicationMain(argc, argv, nil, NSStringFromClass(SpeechFailureApp.class)); }
}
