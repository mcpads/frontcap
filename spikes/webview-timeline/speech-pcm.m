// One parallel synthesis measurement. It does not replace consumer speech or emit audio.
#import <AVFoundation/AVFoundation.h>
#import <Foundation/Foundation.h>
#include <stdatomic.h>
#include <math.h>
#include "speech_preparation.h"

void recordSpeech(id synthesizer, NSString *operation, NSString *phase, NSDictionary *extra);

@interface FrontcapSpeechPCM : NSObject <AVSpeechSynthesizerDelegate> {
    SpeechPreparation _preparation;
    BOOL _reportedPreparationError;
}
@property(nonatomic, strong) AVSpeechSynthesizer *worker;
@property(nonatomic, weak) AVSpeechSynthesizer *consumer;
@property(nonatomic) uint64_t frames;
@property(nonatomic) NSUInteger buffers;
@property(nonatomic) NSUInteger markers;
@property(nonatomic) NSUInteger emptyBuffers;
@property(nonatomic) double sampleRate;
@property(nonatomic) BOOL formatChanged;
@property(nonatomic) BOOL nonzeroSamples;
@end

@implementation FrontcapSpeechPCM
- (instancetype)init {
    self = [super init];
    if (self) _preparation = speech_preparation_new();
    return self;
}
- (void)reportPreparation:(SpeechPreparationEvent)event {
    if (event == SPEECH_PREPARATION_READY) {
        recordSpeech(self.consumer, @"pcm", @"prepared", @{
            @"frames":@(_preparation.progress.total_frames),
            @"sample_rate":@(_preparation.progress.sample_rate),
            @"cursor_frame":@(_preparation.progress.frame),
            @"cursor_fraction":@(_preparation.progress.fraction)});
    } else if (event == SPEECH_PREPARATION_ERROR && !_reportedPreparationError) {
        _reportedPreparationError = YES;
        recordSpeech(self.consumer, @"pcm", @"preparation_error", @{@"reason":@(_preparation.error)});
    }
}
- (void)speechSynthesizer:(AVSpeechSynthesizer *)synthesizer didFinishSpeechUtterance:(AVSpeechUtterance *)utterance {
    @synchronized(self) {
        recordSpeech(self.consumer, @"pcm", @"delegate_done", @{});
        [self reportPreparation:speech_preparation_finished(&_preparation)];
    }
}
- (void)buffer:(AVAudioBuffer *)buffer {
    @synchronized(self) {
        if (![buffer isKindOfClass:AVAudioPCMBuffer.class]) {
            recordSpeech(self.consumer, @"pcm", @"unsupported_buffer", @{});
            [self reportPreparation:speech_preparation_buffer(&_preparation, 0, 0, 0, 0)];
            return;
        }
        AVAudioPCMBuffer *pcm = (AVAudioPCMBuffer *)buffer;
        double rate = pcm.format.sampleRate;
        uint32_t integerRate = isfinite(rate) && rate > 0 && rate <= UINT32_MAX && floor(rate) == rate
            ? (uint32_t)rate : 0;
        SpeechPreparationEvent preparationEvent = speech_preparation_buffer(&_preparation,
            pcm.frameLength, integerRate, pcm.format.channelCount,
            ((uint32_t)pcm.format.commonFormat << 1) | (pcm.format.isInterleaved ? 1 : 0));
        self.buffers++;
        if (self.sampleRate && self.sampleRate != pcm.format.sampleRate) self.formatChanged = YES;
        self.sampleRate = pcm.format.sampleRate;
        self.frames = _preparation.frames;
        if (pcm.floatChannelData) {
            for (AVAudioFrameCount frame = 0; frame < pcm.frameLength; frame++)
                if (pcm.floatChannelData[0][frame * pcm.stride] != 0) { self.nonzeroSamples = YES; break; }
        }
        if (!pcm.frameLength) self.emptyBuffers++;
        if (self.buffers == 1 || !pcm.frameLength) {
            recordSpeech(self.consumer, @"pcm", pcm.frameLength ? @"first_buffer" : @"empty_buffer", @{
                @"frames":@(self.frames), @"buffers":@(self.buffers), @"sample_rate":@(self.sampleRate),
                @"channels":@(pcm.format.channelCount), @"common_format":@(pcm.format.commonFormat),
                @"duration_s":@(self.sampleRate ? self.frames / self.sampleRate : 0),
                @"format_changed":@(self.formatChanged), @"nonzero_samples":@(self.nonzeroSamples),
                @"marker_count":@(self.markers)});
        }
        [self reportPreparation:preparationEvent];
    }
}
- (void)receiveMarkers:(NSArray<AVSpeechSynthesisMarker *> *)markers {
    @synchronized(self) {
        for (AVSpeechSynthesisMarker *marker in markers) {
            self.markers++;
            if (self.markers <= 64)
                recordSpeech(self.consumer, @"pcm", @"marker", @{
                    @"mark":@(marker.mark), @"byte_sample_offset":@(marker.byteSampleOffset),
                    @"location":@(marker.textRange.location), @"length":@(marker.textRange.length)});
        }
    }
}
@end

static FrontcapSpeechPCM *probe;

BOOL isSpeechPCMWorker(AVSpeechSynthesizer *synthesizer) {
    return synthesizer == probe.worker;
}

void probeSpeechPCM(AVSpeechSynthesizer *consumer, AVSpeechUtterance *utterance) {
    static _Atomic(bool) started;
    if (!atomic_exchange(&started, true)) {
        probe = [FrontcapSpeechPCM new];
        probe.consumer = consumer;
        probe.worker = [AVSpeechSynthesizer new];
        probe.worker.delegate = probe;
        AVSpeechUtterance *copy = [utterance copy];
        recordSpeech(consumer, @"pcm", @"requested", @{
            @"rate":@(copy.rate), @"volume":@(copy.volume),
            @"pre_delay_s":@(copy.preUtteranceDelay), @"post_delay_s":@(copy.postUtteranceDelay)});
        [probe.worker writeUtterance:copy toBufferCallback:^(AVAudioBuffer *buffer) {
            [probe buffer:buffer];
        } toMarkerCallback:^(NSArray<AVSpeechSynthesisMarker *> *markers) {
            [probe receiveMarkers:markers];
        }];
    }
}
