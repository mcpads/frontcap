#include "speech_progress.h"

static const uint64_t second_ns = 1000000000;

SpeechProgress speech_progress_new(void) {
    return (SpeechProgress){.phase = SPEECH_PREPARING};
}

bool speech_progress_prepare(SpeechProgress *speech, uint64_t frames, uint32_t sample_rate) {
    if (speech->phase != SPEECH_PREPARING) return false;
    if (!sample_rate) {
        speech->phase = SPEECH_FAILED;
        return false;
    }
    speech->total_frames = frames;
    speech->sample_rate = sample_rate;
    speech->phase = SPEECH_PLAYABLE;
    return true;
}

bool speech_progress_pause(SpeechProgress *speech, bool paused) {
    if (speech->phase != SPEECH_PREPARING && speech->phase != SPEECH_PLAYABLE) return false;
    if (speech->paused == paused) return false;
    speech->paused = paused;
    return true;
}

bool speech_progress_cancel(SpeechProgress *speech) {
    if (speech->phase != SPEECH_PREPARING && speech->phase != SPEECH_PLAYABLE) return false;
    speech->phase = SPEECH_CANCELLED;
    return true;
}

bool speech_progress_fail(SpeechProgress *speech) {
    if (speech->phase != SPEECH_PREPARING && speech->phase != SPEECH_PLAYABLE) return false;
    speech->phase = SPEECH_FAILED;
    return true;
}

SpeechAdvance speech_progress_advance(SpeechProgress *speech, uint64_t delta_ns) {
    SpeechAdvance result = {.kind = SPEECH_INACTIVE, .unused_ns = delta_ns};
    if (speech->phase == SPEECH_PREPARING ||
        (speech->phase == SPEECH_PLAYABLE && speech->paused)) {
        result.kind = SPEECH_BLOCKED;
        return result;
    }
    if (speech->phase != SPEECH_PLAYABLE) return result;

    // Full-width arithmetic keeps long durations and large advances from wrapping.
    __uint128_t remaining = (__uint128_t)(speech->total_frames - speech->frame) * second_ns
        - speech->fraction;
    __uint128_t required_ns = (remaining + speech->sample_rate - 1) / speech->sample_rate;
    if (required_ns <= delta_ns) {
        speech->frame = speech->total_frames;
        speech->fraction = 0;
        speech->phase = SPEECH_FINISHED;
        result.kind = SPEECH_COMPLETED;
        result.consumed_ns = (uint64_t)required_ns;
        result.unused_ns = delta_ns - result.consumed_ns;
        return result;
    }
    __uint128_t progress = (__uint128_t)delta_ns * speech->sample_rate + speech->fraction;
    speech->frame += (uint64_t)(progress / second_ns);
    speech->fraction = (uint64_t)(progress % second_ns);
    return (SpeechAdvance){.kind = SPEECH_ADVANCED, .consumed_ns = delta_ns};
}
