#include "speech_preparation.h"

SpeechPreparation speech_preparation_new(void) {
    return (SpeechPreparation){.progress = speech_progress_new()};
}

static SpeechPreparationEvent report(SpeechPreparation *preparation) {
    if (preparation->error != SPEECH_BUFFER_OK) {
        speech_progress_fail(&preparation->progress);
        return SPEECH_PREPARATION_ERROR;
    }
    if (!preparation->announced && preparation->end_seen && preparation->finish_seen) {
        if (!speech_progress_prepare(&preparation->progress, preparation->frames, preparation->sample_rate))
            return SPEECH_PREPARATION_PENDING; // Cancelled/failed preparation cannot revive.
        preparation->announced = true;
        return SPEECH_PREPARATION_READY;
    }
    return SPEECH_PREPARATION_PENDING;
}

SpeechPreparationEvent speech_preparation_buffer(SpeechPreparation *preparation, uint64_t frames,
    uint32_t sample_rate, uint32_t channels, uint32_t format) {
    if (preparation->error != SPEECH_BUFFER_OK) return report(preparation);
    if (!sample_rate || !channels || !format ||
        (preparation->sample_rate && (preparation->sample_rate != sample_rate ||
         preparation->channels != channels || preparation->format != format))) {
        preparation->error = SPEECH_BUFFER_FORMAT;
    } else if (frames && preparation->end_seen) {
        preparation->error = SPEECH_BUFFER_AFTER_END;
    } else if (UINT64_MAX - preparation->frames < frames) {
        preparation->error = SPEECH_BUFFER_OVERFLOW;
    } else {
        preparation->sample_rate = sample_rate;
        preparation->channels = channels;
        preparation->format = format;
        preparation->frames += frames;
        if (!frames) preparation->end_seen = true;
    }
    return report(preparation);
}

SpeechPreparationEvent speech_preparation_finished(SpeechPreparation *preparation) {
    preparation->finish_seen = true;
    return report(preparation);
}
