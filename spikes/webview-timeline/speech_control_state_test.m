// Focused queue-state fixture: prepared buffers are seeded, not synthesized.
// This tests native bridge transitions without installing or modifying a consumer app.
#import "speech-controlled.m"
#include <assert.h>
#include <stdio.h>

@interface SpeechFixtureOwner : NSObject
@property(nonatomic, weak) id delegate;
@end
@implementation SpeechFixtureOwner
@end

@interface SpeechFixtureCallbacks : NSObject <AVSpeechSynthesizerDelegate>
@property(nonatomic, strong) NSMutableArray *events;
@property(nonatomic, copy) void (^onStart)(void);
@end
@implementation SpeechFixtureCallbacks
- (instancetype)init {
    self = [super init];
    if (self) _events = [NSMutableArray new];
    return self;
}
#define EVENT(method, name) \
- (void)speechSynthesizer:(AVSpeechSynthesizer *)synthesizer method:(AVSpeechUtterance *)utterance { \
    [self.events addObject:name]; \
}
- (void)speechSynthesizer:(AVSpeechSynthesizer *)synthesizer didStartSpeechUtterance:(AVSpeechUtterance *)utterance {
    [self.events addObject:@"start"];
    if (self.onStart) self.onStart();
}
EVENT(didPauseSpeechUtterance, @"pause")
EVENT(didContinueSpeechUtterance, @"resume")
EVENT(didFinishSpeechUtterance, @"finish")
#undef EVENT
@end

static void resetFixture(void) {
    queue = [NSMutableArray new];
    retained = [NSMutableArray new];
    owner = nil;
    failure = nil;
    lastTerminal = nil;
    nextIdentity = completed = cancelledCount = 0;
}

static FrontcapQueuedSpeech *preparedEntry(void) {
    FrontcapQueuedSpeech *entry = [FrontcapQueuedSpeech new];
    entry.identity = ++nextIdentity;
    assert(speech_preparation_buffer(&entry->preparation, 1000, 1000, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_finished(&entry->preparation) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_buffer(&entry->preparation, 0, 1000, 1, 1) == SPEECH_PREPARATION_READY);
    [queue addObject:entry];
    [retained addObject:entry];
    return entry;
}

static void advance(uint64_t delta) {
    uint64_t identity;
    assert(speechControlPreflight(delta, &identity) == nil);
    speechControlAdvance(delta, identity);
}

static void pauseAndQueuedCancellation(void) {
    resetFixture();
    SpeechFixtureOwner *consumer = [SpeechFixtureOwner new];
    SpeechFixtureCallbacks *callbacks = [SpeechFixtureCallbacks new];
    consumer.delegate = callbacks;
    owner = (AVSpeechSynthesizer *)consumer;
    FrontcapQueuedSpeech *first = preparedEntry();
    FrontcapQueuedSpeech *second = preparedEntry();
    FrontcapQueuedSpeech *third = preparedEntry();
    assert(pauseSpeech(consumer, @selector(pauseSpeakingAtBoundary:), AVSpeechBoundaryImmediate));
    advance(5000000000);
    assert(first->preparation.progress.frame == 0 && !first.started && callbacks.events.count == 0);
    assert(resumeSpeech(consumer, @selector(continueSpeaking)));
    advance(0);
    advance(300000000);
    assert(first->preparation.progress.frame == 300);
    assert(pauseSpeech(consumer, @selector(pauseSpeakingAtBoundary:), AVSpeechBoundaryImmediate));
    advance(5000000000);
    assert(first->preparation.progress.frame == 300 && completed == 0);
    assert(!pauseSpeech(consumer, @selector(pauseSpeakingAtBoundary:), AVSpeechBoundaryImmediate));
    assert(resumeSpeech(consumer, @selector(continueSpeaking)));
    assert(!resumeSpeech(consumer, @selector(continueSpeaking)));
    advance(700000000);
    assert(completed == 1 && queue.firstObject == second);
    assert(second->preparation.progress.frame == 0 && !second.started);
    assert(([callbacks.events isEqual:@[@"start", @"pause", @"resume", @"finish"]]));
    advance(0);
    advance(100000000);
    assert(stop(consumer, @selector(stopSpeakingAtBoundary:), AVSpeechBoundaryImmediate));
    assert(cancelledCount == 2 && completed == 1 && queue.count == 0);
    assert(second->preparation.progress.phase == SPEECH_CANCELLED);
    assert(third->preparation.progress.phase == SPEECH_CANCELLED);
    assert([lastTerminal[@"frame"] unsignedLongLongValue] == 100);
    NSArray *events = [callbacks.events copy];
    assert(!stop(consumer, @selector(stopSpeakingAtBoundary:), AVSpeechBoundaryImmediate));
    advance(10000000000);
    assert([callbacks.events isEqual:events] && completed == 1 && cancelledCount == 2);
    assert(([events isEqual:@[@"start", @"pause", @"resume", @"finish", @"start", @"finish"]]));
}

static void liveOwnerReplacement(void) {
    resetFixture();
    SpeechFixtureOwner *consumer = [SpeechFixtureOwner new];
    SpeechFixtureOwner *replacement = [SpeechFixtureOwner new];
    owner = (AVSpeechSynthesizer *)consumer;
    FrontcapQueuedSpeech *entry = preparedEntry();
    speak(replacement, @selector(speakUtterance:), nil);
    assert([failure isEqual:@"speech_owner_changed"]);
    uint64_t identity = 999;
    assert([speechControlPreflight(100, &identity)[@"error"] isEqual:@"speech_owner_changed"]);
    assert(identity == 0);
    speechControlAdvance(100, entry.identity);
    assert(entry->preparation.progress.frame == 0 && nextIdentity == 1 && queue.count == 1);
    assert(!pauseSpeech(consumer, @selector(pauseSpeakingAtBoundary:), AVSpeechBoundaryImmediate));
    assert(!resumeSpeech(consumer, @selector(continueSpeaking)));
    assert(!stop(consumer, @selector(stopSpeakingAtBoundary:), AVSpeechBoundaryImmediate));
}

static void lostOwnerCannotRebind(void) {
    resetFixture();
    FrontcapQueuedSpeech *entry;
    @autoreleasepool {
        SpeechFixtureOwner *consumer = [SpeechFixtureOwner new];
        owner = (AVSpeechSynthesizer *)consumer;
        entry = preparedEntry();
        assert(owner != nil);
    }
    assert(owner == nil);
    assert([speechControlSnapshot()[@"failure"] isEqual:@"speech_owner_lost"]);
    SpeechFixtureOwner *replacement = [SpeechFixtureOwner new];
    speak(replacement, @selector(speakUtterance:), nil);
    assert([failure isEqual:@"speech_owner_lost"] && nextIdentity == 1);
    uint64_t identity = 999;
    assert([speechControlPreflight(1, &identity)[@"error"] isEqual:@"speech_owner_lost"]);
    speechControlAdvance(1, entry.identity);
    assert(identity == 0 && entry->preparation.progress.frame == 0 && !entry.started);
}

static void ownerReleasedByStartCallback(void) {
    resetFixture();
    __block SpeechFixtureOwner *consumer = [SpeechFixtureOwner new];
    SpeechFixtureCallbacks *callbacks = [SpeechFixtureCallbacks new];
    callbacks.onStart = ^{ consumer = nil; };
    consumer.delegate = callbacks;
    owner = (AVSpeechSynthesizer *)consumer;
    FrontcapQueuedSpeech *entry = preparedEntry();
    advance(300000000);
    assert(consumer == nil && owner == nil);
    assert([failure isEqual:@"speech_owner_lost"]);
    assert(entry->preparation.progress.frame == 0 && completed == 0);
    assert(([callbacks.events isEqual:@[@"start"]]));
}

int main(void) {
    @autoreleasepool {
        pauseAndQueuedCancellation();
        liveOwnerReplacement();
        lostOwnerCannotRebind();
        ownerReleasedByStartCallback();
        puts("speech queue state: pause, FIFO, cancellation and owner fencing passed");
    }
}
