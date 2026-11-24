// Optional simulator instrumentation. Observes ordinary API calls without replacing their results.
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#ifdef FRONTCAP_SPEECH_PAUSE_PROBE
#include <stdatomic.h>
static _Atomic(unsigned long) speechEpoch;
static _Atomic(bool) pauseProbeStarted;
#endif

static IMP originalSpeak, originalStop, originalPause, originalContinue;
static NSMapTable *synthesizers;
static NSUInteger nextSynthesizer, sequence;
static NSMapTable *speechUtterances;
static NSUInteger nextSpeechUtterance;

// Shared by API and delegate observations. IDs name objects, never spoken text.
NSDictionary *speechUtteranceFields(AVSpeechUtterance *utterance) {
    @synchronized(AVSpeechSynthesizer.class) {
        if (!speechUtterances) speechUtterances = [NSMapTable
            mapTableWithKeyOptions:NSPointerFunctionsWeakMemory | NSPointerFunctionsObjectPointerPersonality
            valueOptions:NSPointerFunctionsStrongMemory];
        NSNumber *identity = utterance ? [speechUtterances objectForKey:utterance] : nil;
        if (!identity && utterance && nextSpeechUtterance < 64) {
            identity = @(++nextSpeechUtterance);
            [speechUtterances setObject:identity forKey:utterance];
        }
        return @{@"utterance_id":identity ?: (id)NSNull.null,
            @"utterance_limit_reached":@(!identity && utterance != nil && nextSpeechUtterance >= 64)};
    }
}
#ifdef FRONTCAP_SPEECH_PCM_PROBE
void probeSpeechPCM(AVSpeechSynthesizer *consumer, AVSpeechUtterance *utterance);
BOOL isSpeechPCMWorker(AVSpeechSynthesizer *synthesizer);
#endif

#include "speech-flow.h"

void recordSpeech(id synthesizer, NSString *operation, NSString *phase, NSDictionary *extra) {
    @try {
        @synchronized(AVSpeechSynthesizer.class) {
            if (sequence >= 512) return;
            NSNumber *identity = [synthesizers objectForKey:synthesizer];
            if (!identity) {
                identity = @(++nextSynthesizer);
                [synthesizers setObject:identity forKey:synthesizer];
            }
            NSMutableDictionary *record = [@{
                @"sequence":@(++sequence), @"synthesizer_id":identity,
                @"pid":@(NSProcessInfo.processInfo.processIdentifier),
                @"uptime_s":@(NSProcessInfo.processInfo.systemUptime),
                @"operation":operation, @"phase":phase,
                @"speaking":@([synthesizer isSpeaking]), @"paused":@([synthesizer isPaused])
            } mutableCopy];
            [record addEntriesFromDictionary:extra];
            recordSpeechFlow(identity, operation, phase, extra, [synthesizer isSpeaking], [synthesizer isPaused]);
            if (sequence == 512) record[@"limit_reached"] = @YES;
            NSData *data = [NSJSONSerialization dataWithJSONObject:record options:0 error:NULL];
            NSString *path = [NSHomeDirectory() stringByAppendingPathComponent:@"Documents/native-speech-api.jsonl"];
            if (![NSFileManager.defaultManager fileExistsAtPath:path])
                [NSFileManager.defaultManager createFileAtPath:path contents:nil attributes:nil];
            NSFileHandle *file = [NSFileHandle fileHandleForWritingAtPath:path];
            if (file && data) {
                @try {
                    [file seekToEndOfFile];
                    [file writeData:data];
                    [file writeData:[@"\n" dataUsingEncoding:NSUTF8StringEncoding]];
                } @finally { [file closeFile]; }
            } else NSLog(@"FRONTCAP_SPEECH observation unavailable");
        }
    } @catch (NSException *error) {
        @synchronized(AVSpeechSynthesizer.class) { speechFlowFaults++; }
        NSLog(@"FRONTCAP_SPEECH observation failed: %@", error.reason);
    }
}

static void speak(id self, SEL selector, AVSpeechUtterance *utterance) {
#ifdef FRONTCAP_SPEECH_PCM_PROBE
    probeSpeechPCM(self, utterance);
#endif
#ifdef FRONTCAP_SPEECH_PAUSE_PROBE
    unsigned long epoch = atomic_fetch_add(&speechEpoch, 1) + 1;
#endif
    // Deliberately omit text and voice-specific assumptions from the observation boundary.
    float requestedVolume = utterance.volume;
    BOOL muted = [NSProcessInfo.processInfo.environment[@"FRONTCAP_MUTE_AUDIO"] isEqual:@"1"];
#ifdef FRONTCAP_SPEECH_PCM_PROBE
    // writeUtterance invokes speak internally on this runtime. Buffer generation is not output.
    if (isSpeechPCMWorker(self)) muted = NO;
#endif
    if (muted) utterance.volume = 0;
    NSMutableDictionary *fields = [speechUtteranceFields(utterance) mutableCopy];
    fields[@"muted_output"] = @(muted);
    fields[@"requested_volume"] = @(requestedVolume);
    fields[@"submitted_volume"] = @(utterance.volume);
    id<AVSpeechSynthesizerDelegate> recipient = [(AVSpeechSynthesizer *)self delegate];
    fields[@"implemented_callbacks"] = @{
        @"start":@([recipient respondsToSelector:@selector(speechSynthesizer:didStartSpeechUtterance:)]),
        @"finish":@([recipient respondsToSelector:@selector(speechSynthesizer:didFinishSpeechUtterance:)]),
        @"cancel":@([recipient respondsToSelector:@selector(speechSynthesizer:didCancelSpeechUtterance:)]),
        @"pause":@([recipient respondsToSelector:@selector(speechSynthesizer:didPauseSpeechUtterance:)]),
        @"resume":@([recipient respondsToSelector:@selector(speechSynthesizer:didContinueSpeechUtterance:)])};
    recordSpeech(self, @"speak", @"call", fields);
    @try {
        ((void (*)(id, SEL, AVSpeechUtterance *))originalSpeak)(self, selector, utterance);
    } @catch (NSException *exception) {
        recordSpeech(self, @"speak", @"threw", speechUtteranceFields(utterance));
        @throw;
    }
    recordSpeech(self, @"speak", @"return", speechUtteranceFields(utterance));
#ifdef FRONTCAP_SPEECH_PAUSE_PROBE
    if (!atomic_exchange(&pauseProbeStarted, true)) {
        __weak AVSpeechSynthesizer *weakSynthesizer = self;
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC / 2),
                       dispatch_get_main_queue(), ^{
            AVSpeechSynthesizer *synthesizer = weakSynthesizer;
            if (!synthesizer || atomic_load(&speechEpoch) != epoch) return;
            recordSpeech(synthesizer, @"pause_probe", @"before", @{});
            if (!synthesizer.isSpeaking || ![synthesizer pauseSpeakingAtBoundary:AVSpeechBoundaryImmediate])
                return;
            for (int sample = 1; sample <= 6; sample++) {
                dispatch_after(dispatch_time(DISPATCH_TIME_NOW, sample * NSEC_PER_SEC / 2),
                               dispatch_get_main_queue(), ^{
                    AVSpeechSynthesizer *current = weakSynthesizer;
                    if (!current || atomic_load(&speechEpoch) != epoch) return;
                    recordSpeech(current, @"pause_probe", @"held", @{@"sample":@(sample)});
                    if (sample == 6 && current.isPaused) [current continueSpeaking];
                });
            }
        });
    }
#endif
}

static BOOL stop(id self, SEL selector, AVSpeechBoundary boundary) {
#ifdef FRONTCAP_SPEECH_PAUSE_PROBE
    atomic_fetch_add(&speechEpoch, 1);
#endif
    recordSpeech(self, @"stop", @"call", @{@"boundary":@(boundary)});
    BOOL accepted = ((BOOL (*)(id, SEL, AVSpeechBoundary))originalStop)(self, selector, boundary);
    recordSpeech(self, @"stop", @"return", @{@"accepted":@(accepted)});
    return accepted;
}

static BOOL pauseSpeech(id self, SEL selector, AVSpeechBoundary boundary) {
    recordSpeech(self, @"pause", @"call", @{@"boundary":@(boundary)});
    BOOL accepted = ((BOOL (*)(id, SEL, AVSpeechBoundary))originalPause)(self, selector, boundary);
    recordSpeech(self, @"pause", @"return", @{@"accepted":@(accepted)});
    return accepted;
}

static BOOL continueSpeech(id self, SEL selector) {
    recordSpeech(self, @"resume", @"call", @{});
    BOOL accepted = ((BOOL (*)(id, SEL))originalContinue)(self, selector);
    recordSpeech(self, @"resume", @"return", @{@"accepted":@(accepted)});
    return accepted;
}

__attribute__((constructor)) static void observeSpeech(void) {
    @autoreleasepool {
        NSString *mode = NSProcessInfo.processInfo.environment[@"FRONTCAP_WEBVIEW_CONTROL"];
        if (![mode isEqual:@"1"] && ![mode isEqual:@"pages"]) return;
        Method speakMethod = class_getInstanceMethod(AVSpeechSynthesizer.class, @selector(speakUtterance:));
        Method stopMethod = class_getInstanceMethod(AVSpeechSynthesizer.class, @selector(stopSpeakingAtBoundary:));
        Method pauseMethod = class_getInstanceMethod(AVSpeechSynthesizer.class, @selector(pauseSpeakingAtBoundary:));
        Method continueMethod = class_getInstanceMethod(AVSpeechSynthesizer.class, @selector(continueSpeaking));
        if (!speakMethod || !stopMethod || !pauseMethod || !continueMethod) {
            NSLog(@"FRONTCAP_SPEECH API unavailable");
            return;
        }
        synthesizers = [NSMapTable weakToStrongObjectsMapTable];
        originalSpeak = method_setImplementation(speakMethod, (IMP)speak);
        originalStop = method_setImplementation(stopMethod, (IMP)stop);
        originalPause = method_setImplementation(pauseMethod, (IMP)pauseSpeech);
        originalContinue = method_setImplementation(continueMethod, (IMP)continueSpeech);
    }
}
