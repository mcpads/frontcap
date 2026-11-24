// Native observation fixture: invokes real wrappers with a silent fake producer.
// No TTS engine is started and no consumer callback is replaced by a synthetic result.
#import <Foundation/Foundation.h>
#import <AVFoundation/AVFoundation.h>
#include <assert.h>
#include <stdio.h>
static NSString *fixtureHome;
static NSString *speechFixtureHome(void) { return fixtureHome; }
#define NSHomeDirectory speechFixtureHome
#define FRONTCAP_SPEECH_DELEGATE_LINKED
#import "speech-observer.m"
#import "speech-delegate.m"
#undef NSHomeDirectory

@interface SpeechObserverOwner : NSObject
@property(nonatomic, weak) id delegate;
@property(nonatomic) BOOL speaking;
@property(nonatomic) BOOL paused;
@property(nonatomic) NSUInteger requests;
@property(nonatomic) BOOL fail;
- (BOOL)isSpeaking;
- (BOOL)isPaused;
@end
@implementation SpeechObserverOwner
- (BOOL)isSpeaking { return self.speaking; }
- (BOOL)isPaused { return self.paused; }
@end
@interface SpeechObserverCallbacks : NSObject <AVSpeechSynthesizerDelegate>
@property(nonatomic) NSUInteger starts;
@property(nonatomic) NSUInteger finishes;
@property(nonatomic) NSUInteger cancellations;
@end
@implementation SpeechObserverCallbacks
- (void)speechSynthesizer:(AVSpeechSynthesizer *)s didStartSpeechUtterance:(AVSpeechUtterance *)u { self.starts++; }
- (void)speechSynthesizer:(AVSpeechSynthesizer *)s didFinishSpeechUtterance:(AVSpeechUtterance *)u { self.finishes++; }
- (void)speechSynthesizer:(AVSpeechSynthesizer *)s didCancelSpeechUtterance:(AVSpeechUtterance *)u { self.cancellations++; }
@end
@interface SpeechObserverInheritedCallbacks : SpeechObserverCallbacks
@end
@implementation SpeechObserverInheritedCallbacks
@end
@interface SpeechObserverOverrideCallbacks : SpeechObserverCallbacks
@end
@implementation SpeechObserverOverrideCallbacks
- (void)speechSynthesizer:(AVSpeechSynthesizer *)s didStartSpeechUtterance:(AVSpeechUtterance *)u {
    [super speechSynthesizer:s didStartSpeechUtterance:u];
}
@end
static void fixtureSpeak(SpeechObserverOwner *owner, SEL selector, AVSpeechUtterance *utterance) {
    owner.requests++;
    if (owner.fail) @throw [NSException exceptionWithName:@"native_failure" reason:nil userInfo:nil];
}
static void fixtureSetDelegate(SpeechObserverOwner *owner, SEL selector, id recipient) {
    owner.delegate = recipient;
}
static NSArray *records(void) {
    NSString *text = [NSString stringWithContentsOfFile:[fixtureHome stringByAppendingPathComponent:@"Documents/native-speech-api.jsonl"] encoding:NSUTF8StringEncoding error:NULL];
    NSMutableArray *result = [NSMutableArray new];
    for (NSString *line in [text componentsSeparatedByString:@"\n"]) {
        if (line.length) [result addObject:[NSJSONSerialization JSONObjectWithData:[line dataUsingEncoding:NSUTF8StringEncoding] options:0 error:NULL]];
    }
    return result;
}
int main(int argc, const char *argv[]) {
    @autoreleasepool {
        assert(NSProcessInfo.processInfo.environment[@"FRONTCAP_WEBVIEW_CONTROL"] == nil);
        assert(NSProcessInfo.processInfo.environment[@"FRONTCAP_MUTE_AUDIO"] == nil);
        fixtureHome = [NSTemporaryDirectory() stringByAppendingPathComponent:NSUUID.UUID.UUIDString];
        assert([NSFileManager.defaultManager createDirectoryAtPath:[fixtureHome stringByAppendingPathComponent:@"Documents"] withIntermediateDirectories:YES attributes:nil error:NULL]);
        synthesizers = [NSMapTable weakToStrongObjectsMapTable];
        observedClasses = [NSMutableSet new];
        observedImplementations = [NSMutableSet new];
        originalSpeak = (IMP)fixtureSpeak;
        originalSetDelegate = (IMP)fixtureSetDelegate;
        SpeechObserverOwner *owner = [SpeechObserverOwner new];
        SpeechObserverCallbacks *first = [SpeechObserverCallbacks new];
        setDelegate(owner, @selector(setDelegate:), first);
        SpeechObserverInheritedCallbacks *recipient = [SpeechObserverInheritedCallbacks new];
        setDelegate(owner, @selector(setDelegate:), recipient);
        assert(owner.delegate == recipient);
        assert(![recipient respondsToSelector:@selector(speechSynthesizer:didPauseSpeechUtterance:)]);
        AVSpeechUtterance *one = [AVSpeechUtterance speechUtteranceWithString:@"private text one"];
        AVSpeechUtterance *two = [AVSpeechUtterance speechUtteranceWithString:@"private text two"];
        speak(owner, @selector(speakUtterance:), one);
        speak(owner, @selector(speakUtterance:), two);
        // Complete the second object first: callback order must not assign request identities.
        [recipient speechSynthesizer:(id)owner didStartSpeechUtterance:two];
        [recipient speechSynthesizer:(id)owner didCancelSpeechUtterance:two];
        [recipient speechSynthesizer:(id)owner didStartSpeechUtterance:one];
        [recipient speechSynthesizer:(id)owner didFinishSpeechUtterance:one];
        assert(owner.requests == 2 && recipient.starts == 2 && recipient.finishes == 1 && recipient.cancellations == 1);
        NSMutableArray *requests = [NSMutableArray new], *callbacks = [NSMutableArray new];
        for (NSDictionary *record in records()) {
            if ([record[@"operation"] isEqual:@"speak"] && [record[@"phase"] isEqual:@"call"]) [requests addObject:record];
            if ([record[@"operation"] isEqual:@"delegate"] && ![record[@"phase"] isEqual:@"assigned"]) [callbacks addObject:record];
        }
        assert(requests.count == 2 && callbacks.count == 4); // Inherited methods must not double-count.
        assert([requests[0][@"utterance_id"] isEqual:callbacks[2][@"utterance_id"]]);
        assert([requests[0][@"utterance_id"] isEqual:callbacks[3][@"utterance_id"]]);
        assert([requests[1][@"utterance_id"] isEqual:callbacks[0][@"utterance_id"]]);
        assert([requests[1][@"utterance_id"] isEqual:callbacks[1][@"utterance_id"]]);
        assert(![requests[0][@"utterance_id"] isEqual:requests[1][@"utterance_id"]]);
        assert(![requests[0][@"implemented_callbacks"][@"pause"] boolValue]);
        SpeechObserverOverrideCallbacks *overriding = [SpeechObserverOverrideCallbacks new];
        setDelegate(owner, @selector(setDelegate:), overriding);
        NSUInteger beforeOverride = records().count;
        [overriding speechSynthesizer:(id)owner didStartSpeechUtterance:one];
        assert(overriding.starts == 1 && records().count == beforeOverride + 1);
        [overriding speechSynthesizer:(id)owner didStartSpeechUtterance:one];
        assert(overriding.starts == 2 && records().count == beforeOverride + 2);
        owner.fail = YES;
        BOOL threw = NO;
        @try { speak(owner, @selector(speakUtterance:), one); }
        @catch (NSException *error) { assert([error.name isEqual:@"native_failure"]); threw = YES; }
        assert(threw);
        NSDictionary *last = records().lastObject;
        assert([last[@"phase"] isEqual:@"threw"]); // Explicit failure, no fabricated successful return.
        for (NSUInteger i = 0; i < 70; i++) speechUtteranceFields([AVSpeechUtterance speechUtteranceWithString:@"bounded"]);
        NSDictionary *overflow = speechUtteranceFields([AVSpeechUtterance speechUtteranceWithString:@"overflow"]);
        assert(overflow[@"utterance_id"] == NSNull.null && [overflow[@"utterance_limit_reached"] boolValue]);
        assert([speechUtteranceFields(one)[@"utterance_id"] isEqual:requests[0][@"utterance_id"]]);
        for (NSUInteger i = 0; i < 40; i++) recordSpeech(owner, @"stop", @"return", @{@"accepted":@YES});
        owner.speaking = YES;
        NSDictionary *snapshot = speechObservationSnapshot();
        assert([snapshot[@"synthesizers"][0][@"speaking"] boolValue]);
        owner.speaking = NO;
        assert(![speechObservationSnapshot()[@"synthesizers"][0][@"speaking"] boolValue]);
        assert([snapshot[@"synthesizers"][0][@"speaking"] boolValue]);
        assert([snapshot[@"dropped"] unsignedIntegerValue] > 0);
        assert([snapshot[@"events"] count] == 32 && [snapshot[@"utterances"] count] == 2);
        NSDictionary *secondSummary = snapshot[@"utterances"][1];
        assert([secondSummary[@"starts"] unsignedIntegerValue] == 1);
        assert([secondSummary[@"cancellations"] unsignedIntegerValue] == 1);
        assert([secondSummary[@"lastSequence"] unsignedIntegerValue] < [snapshot[@"events"][0][@"sequence"] unsignedIntegerValue]);
        if (argc == 2) assert([[NSJSONSerialization dataWithJSONObject:snapshot options:0 error:NULL]
            writeToFile:[NSString stringWithUTF8String:argv[1]] atomically:YES]);
        for (NSUInteger i = 0; i < 520; i++) recordSpeech(owner, @"stop", @"return", @{@"accepted":@YES});
        NSDictionary *limited = speechObservationSnapshot();
        assert([limited[@"truncated"] boolValue]);
        assert(records().count == 512);
        assert([limited[@"received"] unsignedIntegerValue] <= 512);
        assert([snapshot[@"received"] unsignedIntegerValue] < [limited[@"received"] unsignedIntegerValue]);
        NSString *serialized = [[NSString alloc] initWithData:[NSJSONSerialization dataWithJSONObject:records() options:0 error:NULL] encoding:NSUTF8StringEncoding];
        assert(![serialized containsString:@"private text"]);
        assert([NSFileManager.defaultManager removeItemAtPath:fixtureHome error:NULL]);
        puts("speech request/callback correlation, inherited dispatch, native errors and identity bounds passed");
    }
    return 0;
}
