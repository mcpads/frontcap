#ifndef FRONTCAP_SPEECH_PREPARATION_H
#define FRONTCAP_SPEECH_PREPARATION_H
#include "speech_progress.h"

typedef enum {
    SPEECH_BUFFER_OK, SPEECH_BUFFER_FORMAT, SPEECH_BUFFER_AFTER_END, SPEECH_BUFFER_OVERFLOW
} SpeechBufferError;
typedef enum {
    SPEECH_PREPARATION_PENDING, SPEECH_PREPARATION_READY, SPEECH_PREPARATION_ERROR
} SpeechPreparationEvent;
typedef struct {
    SpeechProgress progress;
    uint64_t frames;
    uint32_t sample_rate, channels, format;
    bool end_seen, finish_seen, announced;
    SpeechBufferError error;
} SpeechPreparation;

SpeechPreparation speech_preparation_new(void);
SpeechPreparationEvent speech_preparation_buffer(SpeechPreparation *preparation, uint64_t frames,
    uint32_t sample_rate, uint32_t channels, uint32_t format);
SpeechPreparationEvent speech_preparation_finished(SpeechPreparation *preparation);
// An error after readiness must fence the caller; it cannot undo an already delivered callback.
#endif
