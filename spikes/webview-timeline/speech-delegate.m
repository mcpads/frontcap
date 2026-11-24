// Optional native callback observation, independent of the consumer delegate's implementation.
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#import <objc/runtime.h>

void recordSpeech(id synthesizer, NSString *operation, NSString *phase, NSDictionary *extra);
static IMP originalSetDelegate;
BOOL speechDelegateObservationAvailable(void) { return originalSetDelegate != NULL; }
NSDictionary *speechUtteranceFields(AVSpeechUtterance *utterance);

static void callback(AVSpeechSynthesizer *synthesizer, AVSpeechUtterance *utterance,
                     NSString *kind, NSDictionary *extra) {
    @try {
        @synchronized(AVSpeechSynthesizer.class) {
            NSMutableDictionary *fields = [extra mutableCopy];
            [fields addEntriesFromDictionary:speechUtteranceFields(utterance)];
            recordSpeech(synthesizer, @"delegate", kind, fields);
        }
    } @catch (NSException *error) {
        NSLog(@"FRONTCAP_SPEECH callback observation failed: %@", error.reason);
    }
}

static NSMutableSet *observedClasses;
static NSMutableSet *observedImplementations;

static void observeDelegateClass(Class cls) {
    @synchronized(AVSpeechSynthesizer.class) {
        NSValue *key = [NSValue valueWithPointer:(__bridge const void *)cls];
        if ([observedClasses containsObject:key]) return;
        [observedClasses addObject:key];
        NSArray *names = @[@"speechSynthesizer:didStartSpeechUtterance:",
            @"speechSynthesizer:didFinishSpeechUtterance:",
            @"speechSynthesizer:didPauseSpeechUtterance:",
            @"speechSynthesizer:didContinueSpeechUtterance:",
            @"speechSynthesizer:didCancelSpeechUtterance:"];
        NSArray *labels = @[@"playing", @"done", @"paused", @"resumed", @"stopped"];
        for (NSUInteger index = 0; index < names.count; index++) {
            SEL selector = NSSelectorFromString(names[index]);
            Method method = class_getInstanceMethod(cls, selector);
            if (!method) continue; // Do not introduce optional methods the consumer did not implement.
            IMP original = method_getImplementation(method);
            // An inherited observer already records this callback. Wrapping it again would
            // count one native callback twice when a subclass becomes a delegate.
            if ([observedImplementations containsObject:[NSValue valueWithPointer:original]]) continue;
            NSString *label = labels[index];
            IMP observed = imp_implementationWithBlock(^(id recipient, AVSpeechSynthesizer *synthesizer,
                                                         AVSpeechUtterance *utterance) {
                // An override may call an already observed superclass implementation.
                // Suppress only nested observation of this same dispatch; always invoke it.
                NSMutableDictionary *thread = NSThread.currentThread.threadDictionary;
                NSString *key = [NSString stringWithFormat:@"frontcap.speech.%p.%p.%p.%@",
                    (__bridge void *)recipient, (__bridge void *)synthesizer,
                    (__bridge void *)utterance, names[index]];
                BOOL nested = thread[key] != nil;
                if (!nested) thread[key] = @YES;
                @try {
                    if (!nested) callback(synthesizer, utterance, label, @{});
                    ((void (*)(id, SEL, id, id))original)(recipient, selector, synthesizer, utterance);
                } @finally {
                    if (!nested) [thread removeObjectForKey:key];
                }
            });
            [observedImplementations addObject:[NSValue valueWithPointer:observed]];
            // Copy inherited implementations onto this class rather than modifying its superclass.
            if (!class_addMethod(cls, selector, observed, method_getTypeEncoding(method)))
                method_setImplementation(class_getInstanceMethod(cls, selector), observed);
        }
    }
}

static void setDelegate(id self, SEL selector, id<AVSpeechSynthesizerDelegate> recipient) {
    if (recipient) observeDelegateClass(object_getClass(recipient));
    ((void (*)(id, SEL, id))originalSetDelegate)(self, selector, recipient);
    recordSpeech(self, @"delegate", @"assigned", @{
        @"present":@(recipient != nil),
        @"getter_matches":@([(AVSpeechSynthesizer *)self delegate] == recipient)});
}

__attribute__((constructor)) static void observeSpeechDelegate(void) {
    @autoreleasepool {
        NSString *mode = NSProcessInfo.processInfo.environment[@"FRONTCAP_WEBVIEW_CONTROL"];
        if (![mode isEqual:@"1"] && ![mode isEqual:@"pages"]) return;
        Method setter = class_getInstanceMethod(AVSpeechSynthesizer.class, @selector(setDelegate:));
        if (!setter) {
            NSLog(@"FRONTCAP_SPEECH delegate API unavailable");
            return;
        }
        observedClasses = [NSMutableSet new];
        observedImplementations = [NSMutableSet new];
        originalSetDelegate = method_setImplementation(setter, (IMP)setDelegate);
    }
}
