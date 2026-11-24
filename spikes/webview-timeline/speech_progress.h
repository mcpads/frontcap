#ifndef FRONTCAP_SPEECH_PROGRESS_H
#define FRONTCAP_SPEECH_PROGRESS_H
#include <stdbool.h>
#include <stdint.h>

// Serial, caller-owned state. No host clock reads, callbacks, allocation or generation ownership.
typedef enum {
    SPEECH_PREPARING, SPEECH_PLAYABLE, SPEECH_FINISHED, SPEECH_CANCELLED, SPEECH_FAILED
} SpeechPhase;

typedef struct {
    SpeechPhase phase;
    bool paused;
    uint32_t sample_rate;
    uint64_t total_frames;
    uint64_t frame;
    uint64_t fraction; // Sub-frame numerator, denominator 1,000,000,000.
} SpeechProgress;

typedef enum {
    SPEECH_BLOCKED, SPEECH_ADVANCED, SPEECH_COMPLETED, SPEECH_INACTIVE
} SpeechAdvanceKind;

typedef struct {
    SpeechAdvanceKind kind;
    uint64_t consumed_ns;
    uint64_t unused_ns; // Carry to another utterance only on completion, not when blocked.
} SpeechAdvance;

SpeechProgress speech_progress_new(void);
// A caller must finalize actual synthesis before preparing; a count estimate is not a duration.
bool speech_progress_prepare(SpeechProgress *speech, uint64_t frames, uint32_t sample_rate);
bool speech_progress_pause(SpeechProgress *speech, bool paused);
bool speech_progress_cancel(SpeechProgress *speech);
bool speech_progress_fail(SpeechProgress *speech);
SpeechAdvance speech_progress_advance(SpeechProgress *speech, uint64_t delta_ns);
#endif
