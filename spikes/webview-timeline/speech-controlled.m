// Experimental muted speech queue. Link instead of the observer/parallel PCM modules.
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#import <objc/runtime.h>
#include <math.h>
#include "speech_preparation.h"

static IMP originalSpeak, originalStop, originalPause, originalResume, originalSpeaking, originalPaused;
static char workerKey;
static NSMutableArray *queue, *retained;
static __weak AVSpeechSynthesizer *owner;
static NSString *failure;
static uint64_t nextIdentity, completed, cancelledCount;
static NSDictionary *lastTerminal;

static BOOL validateOwner(void) {
    if (!failure && nextIdentity && !owner) failure = @"speech_owner_lost";
    return failure == nil;
}

@interface FrontcapQueuedSpeech : NSObject <AVSpeechSynthesizerDelegate> {
@public
    SpeechPreparation preparation;
}
@property(nonatomic, strong) AVSpeechSynthesizer *worker;
@property(nonatomic, strong) AVSpeechUtterance *utterance;
@property(nonatomic) uint64_t identity;
@property(nonatomic) BOOL started;
@end

static void preparationResult(FrontcapQueuedSpeech *entry, SpeechPreparationEvent result) {
    if (result == SPEECH_PREPARATION_ERROR && entry->preparation.progress.phase != SPEECH_CANCELLED)
        failure = @"speech_preparation_failed";
}

@implementation FrontcapQueuedSpeech
- (instancetype)init {
    self = [super init];
    if (self) preparation = speech_preparation_new();
    return self;
}
- (void)speechSynthesizer:(AVSpeechSynthesizer *)synthesizer didFinishSpeechUtterance:(AVSpeechUtterance *)utterance {
    dispatch_async(dispatch_get_main_queue(), ^{
        preparationResult(self, speech_preparation_finished(&self->preparation));
    });
}
@end

static void notify(FrontcapQueuedSpeech *entry, SEL selector) {
    if (!validateOwner()) return;
    AVSpeechSynthesizer *consumer = owner;
    id delegate = consumer.delegate;
    if ([delegate respondsToSelector:selector]) {
        ((void (*)(id, SEL, id, id))[delegate methodForSelector:selector])
            (delegate, selector, consumer, entry.utterance);
    }
}

static BOOL controlled(id synthesizer) {
    return ![objc_getAssociatedObject(synthesizer, &workerKey) boolValue];
}

static void speak(id self, SEL selector, AVSpeechUtterance *utterance) {
    if (!controlled(self)) {
        ((void (*)(id, SEL, id))originalSpeak)(self, selector, utterance);
        return;
    }
    if (!NSThread.isMainThread) { failure = @"speech_off_main_thread"; return; }
    if (!validateOwner()) return;
    if (nextIdentity && owner != self) {
        failure = @"speech_owner_changed"; return;
    }
    if (queue.count >= 8 || nextIdentity >= 64) {
        failure = @"speech_capacity"; return;
    }
    if (utterance.preUtteranceDelay || utterance.postUtteranceDelay ||
        [((AVSpeechSynthesizer *)self).delegate respondsToSelector:@selector(speechSynthesizer:willSpeakRangeOfSpeechString:utterance:)]) {
        failure = @"speech_delay_or_range_callback_unsupported"; return;
    }
    owner = self;
    FrontcapQueuedSpeech *entry = [FrontcapQueuedSpeech new];
    entry.identity = ++nextIdentity;
    entry.utterance = utterance;
    entry.worker = [AVSpeechSynthesizer new];
    objc_setAssociatedObject(entry.worker, &workerKey, @YES, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
    entry.worker.delegate = entry;
    [queue addObject:entry];
    [retained addObject:entry]; // Bounded lifetime evidence; retirement releases the process.
    [entry.worker writeUtterance:[utterance copy] toBufferCallback:^(AVAudioBuffer *buffer) {
        uint64_t frames = 0;
        uint32_t rate = 0, channels = 0, format = 0;
        if ([buffer isKindOfClass:AVAudioPCMBuffer.class]) {
            AVAudioPCMBuffer *pcm = (AVAudioPCMBuffer *)buffer;
            double sampledRate = pcm.format.sampleRate;
            frames = pcm.frameLength;
            rate = isfinite(sampledRate) && sampledRate > 0 && sampledRate <= UINT32_MAX &&
                floor(sampledRate) == sampledRate ? (uint32_t)sampledRate : 0;
            channels = pcm.format.channelCount;
            format = ((uint32_t)pcm.format.commonFormat << 1) | (pcm.format.isInterleaved ? 1 : 0);
        }
        dispatch_async(dispatch_get_main_queue(), ^{
            if (entry->preparation.progress.phase == SPEECH_CANCELLED) return;
            preparationResult(entry, speech_preparation_buffer(&entry->preparation, frames, rate, channels, format));
        });
    }];
}

static BOOL speaking(id self, SEL selector) {
    if (!controlled(self)) return ((BOOL (*)(id, SEL))originalSpeaking)(self, selector);
    return owner == self && queue.count > 0;
}
static BOOL paused(id self, SEL selector) {
    if (!controlled(self)) return ((BOOL (*)(id, SEL))originalPaused)(self, selector);
    FrontcapQueuedSpeech *entry = queue.firstObject;
    return owner == self && entry && entry->preparation.progress.paused;
}
static BOOL stop(id self, SEL selector, AVSpeechBoundary boundary) {
    if (!controlled(self)) return ((BOOL (*)(id, SEL, AVSpeechBoundary))originalStop)(self, selector, boundary);
    if (!validateOwner()) return NO;
    if (owner != self || !queue.count) return NO;
    if (boundary != AVSpeechBoundaryImmediate) { failure = @"speech_word_boundary_unsupported"; return NO; }
    NSArray *cancelled = [queue copy];
    [queue removeAllObjects];
    for (FrontcapQueuedSpeech *entry in cancelled) {
        if (speech_progress_cancel(&entry->preparation.progress)) cancelledCount++;
        [entry.worker stopSpeakingAtBoundary:AVSpeechBoundaryImmediate];
    }
    // This selected simulator's measured immediate-stop result is didFinish, not didCancel.
    FrontcapQueuedSpeech *active = cancelled.firstObject;
    lastTerminal = @{@"utterance_id":@(active.identity), @"outcome":@"cancelled",
        @"frame":@(active->preparation.progress.frame),
        @"frames":@(active->preparation.progress.total_frames)};
    if (active.started) notify(active, @selector(speechSynthesizer:didFinishSpeechUtterance:));
    return YES;
}
static BOOL pauseSpeech(id self, SEL selector, AVSpeechBoundary boundary) {
    if (!controlled(self)) return ((BOOL (*)(id, SEL, AVSpeechBoundary))originalPause)(self, selector, boundary);
    if (!validateOwner()) return NO;
    if (boundary != AVSpeechBoundaryImmediate) { failure = @"speech_word_boundary_unsupported"; return NO; }
    FrontcapQueuedSpeech *entry = queue.firstObject;
    if (owner != self || !entry || !speech_progress_pause(&entry->preparation.progress, true)) return NO;
    if (entry.started) notify(entry, @selector(speechSynthesizer:didPauseSpeechUtterance:));
    return YES;
}
static BOOL resumeSpeech(id self, SEL selector) {
    if (!controlled(self)) return ((BOOL (*)(id, SEL))originalResume)(self, selector);
    if (!validateOwner()) return NO;
    FrontcapQueuedSpeech *entry = queue.firstObject;
    if (owner != self || !entry || !speech_progress_pause(&entry->preparation.progress, false)) return NO;
    if (entry.started) notify(entry, @selector(speechSynthesizer:didContinueSpeechUtterance:));
    return YES;
}

NSDictionary *speechControlSnapshot(void) {
    validateOwner();
    FrontcapQueuedSpeech *entry = queue.firstObject;
    SpeechProgress progress = entry ? entry->preparation.progress : (SpeechProgress){0};
    return @{@"available":@(queue != nil), @"failure":failure ?: (id)NSNull.null,
        @"queued":@(queue.count), @"accepted":@(nextIdentity), @"completed":@(completed),
        @"cancelled":@(cancelledCount), @"last_terminal":lastTerminal ?: (id)NSNull.null,
        @"utterance_id":entry ? @(entry.identity) : (id)NSNull.null,
        @"phase":entry ? @(progress.phase) : (id)NSNull.null,
        @"paused":@(progress.paused), @"started":@(entry.started),
        @"frames":@(progress.total_frames), @"frame":@(progress.frame),
        @"fraction":@(progress.fraction), @"sample_rate":@(progress.sample_rate)};
}

NSDictionary *speechControlPreflight(uint64_t delta, uint64_t *identity) {
    *identity = 0;
    if (!validateOwner()) return @{@"status":@"rejected", @"error":failure, @"speech":speechControlSnapshot()};
    FrontcapQueuedSpeech *entry = queue.firstObject;
    if (!entry) return nil;
    SpeechProgress progress = entry->preparation.progress;
    if (progress.phase == SPEECH_PREPARING)
        return @{@"status":@"rejected", @"error":@"speech_preparing", @"speech":speechControlSnapshot()};
    if (progress.phase != SPEECH_PLAYABLE)
        return @{@"status":@"rejected", @"error":@"speech_unavailable", @"speech":speechControlSnapshot()};
    if (!progress.paused) {
        __uint128_t units = (__uint128_t)(progress.total_frames - progress.frame) * 1000000000 - progress.fraction;
        __uint128_t remaining = (units + progress.sample_rate - 1) / progress.sample_rate;
        if (remaining < delta)
            return @{@"status":@"rejected", @"error":@"speech_boundary",
                @"remaining_ns":@((uint64_t)remaining), @"speech":speechControlSnapshot()};
    }
    *identity = entry.identity;
    return nil;
}

void speechControlAdvance(uint64_t delta, uint64_t identity) {
    FrontcapQueuedSpeech *entry = queue.firstObject;
    if (!validateOwner() || !entry || entry.identity != identity) return;
    if (!entry.started && !entry->preparation.progress.paused) {
        entry.started = YES;
        notify(entry, @selector(speechSynthesizer:didStartSpeechUtterance:));
        if (queue.firstObject != entry || !validateOwner()) return;
    }
    SpeechAdvance advance = speech_progress_advance(&entry->preparation.progress, delta);
    if (advance.kind == SPEECH_COMPLETED) {
        [queue removeObjectAtIndex:0];
        completed++;
        lastTerminal = @{@"utterance_id":@(entry.identity), @"outcome":@"completed",
            @"frame":@(entry->preparation.progress.frame), @"frames":@(entry->preparation.progress.total_frames)};
        notify(entry, @selector(speechSynthesizer:didFinishSpeechUtterance:));
    }
}

__attribute__((constructor)) static void installSpeechControl(void) {
    @autoreleasepool {
        if (![NSProcessInfo.processInfo.environment[@"FRONTCAP_MUTE_AUDIO"] isEqual:@"1"]) {
            failure = @"controlled_speech_requires_muted_output"; return;
        }
        queue = [NSMutableArray new];
        retained = [NSMutableArray new];
#define REPLACE(selector, target, original) \
        original = method_setImplementation(class_getInstanceMethod(AVSpeechSynthesizer.class, selector), (IMP)target)
        REPLACE(@selector(speakUtterance:), speak, originalSpeak);
        REPLACE(@selector(stopSpeakingAtBoundary:), stop, originalStop);
        REPLACE(@selector(pauseSpeakingAtBoundary:), pauseSpeech, originalPause);
        REPLACE(@selector(continueSpeaking), resumeSpeech, originalResume);
        REPLACE(@selector(isSpeaking), speaking, originalSpeaking);
        REPLACE(@selector(isPaused), paused, originalPaused);
#undef REPLACE
    }
}
