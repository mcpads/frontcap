#include "speech_progress.h"
#include "speech_preparation.h"
#include <assert.h>
#include <stdio.h>

static void preparation_and_pause(void) {
    SpeechProgress speech = speech_progress_new();
    assert(speech_progress_advance(&speech, UINT64_MAX).kind == SPEECH_BLOCKED);
    assert(speech_progress_pause(&speech, true));
    assert(speech_progress_prepare(&speech, 22050, 22050));
    assert(!speech_progress_prepare(&speech, 1, 1));
    assert(speech_progress_advance(&speech, UINT64_MAX).kind == SPEECH_BLOCKED);
    assert(speech.frame == 0 && speech.fraction == 0);
    assert(speech_progress_pause(&speech, false));
    assert(speech_progress_advance(&speech, 1).kind == SPEECH_ADVANCED);
    assert(speech.frame == 0 && speech.fraction == 22050);
    assert(speech_progress_pause(&speech, true));
    assert(speech_progress_advance(&speech, 1000000000).kind == SPEECH_BLOCKED);
    assert(speech_progress_pause(&speech, false));
    SpeechAdvance end = speech_progress_advance(&speech, 1000000000);
    assert(end.kind == SPEECH_COMPLETED && end.consumed_ns == 999999999 && end.unused_ns == 1);
    assert(speech_progress_advance(&speech, 0).kind == SPEECH_INACTIVE);
    assert(!speech_progress_pause(&speech, true) && !speech_progress_cancel(&speech));
}

static void partition_and_boundaries(void) {
    // Use actual native synthesis frame count; expected boundary is ceil(frames * 1e9 / rate).
    SpeechProgress whole = speech_progress_new(), split = speech_progress_new();
    assert(speech_progress_prepare(&whole, 96341, 22050));
    assert(speech_progress_prepare(&split, 96341, 22050));
    const uint64_t before_end = 4369206349;
    assert(speech_progress_advance(&whole, before_end).kind == SPEECH_ADVANCED);
    uint64_t left = before_end;
    uint64_t seed = 19;
    while (left) {
        seed = seed * 6364136223846793005ULL + 1;
        uint64_t step = 1 + seed % 10000000;
        if (step > left) step = left;
        assert(speech_progress_advance(&split, step).kind == SPEECH_ADVANCED);
        left -= step;
    }
    assert(whole.frame == split.frame && whole.fraction == split.fraction);
    assert(whole.frame == 96340);
    assert(speech_progress_advance(&whole, 1).kind == SPEECH_COMPLETED);
    SpeechAdvance end = speech_progress_advance(&split, 100);
    assert(end.kind == SPEECH_COMPLETED && end.consumed_ns == 1 && end.unused_ns == 99);
    assert(split.frame == whole.frame && split.fraction == 0);
    SpeechProgress next = speech_progress_new();
    assert(speech_progress_prepare(&next, 1, 10000000));
    assert(speech_progress_advance(&next, end.unused_ns).kind == SPEECH_ADVANCED);
    assert(speech_progress_advance(&next, 1).kind == SPEECH_COMPLETED);

    SpeechProgress large = speech_progress_new();
    assert(speech_progress_prepare(&large, UINT64_MAX, UINT32_MAX));
    SpeechAdvance result = speech_progress_advance(&large, UINT64_MAX);
    assert(result.kind == SPEECH_COMPLETED && result.unused_ns > 0);
    assert(large.frame == UINT64_MAX);
    SpeechProgress slow = speech_progress_new();
    assert(speech_progress_prepare(&slow, UINT64_MAX, 1));
    assert(speech_progress_advance(&slow, UINT64_MAX).kind == SPEECH_ADVANCED);
    assert(slow.frame == UINT64_MAX / 1000000000);
}

static void terminal_dispositions(void) {
    SpeechProgress cancelled = speech_progress_new();
    assert(speech_progress_cancel(&cancelled));
    assert(!speech_progress_prepare(&cancelled, 1, 22050));
    assert(!speech_progress_cancel(&cancelled));
    assert(speech_progress_advance(&cancelled, UINT64_MAX).kind == SPEECH_INACTIVE);
    SpeechProgress interrupted = speech_progress_new();
    assert(speech_progress_prepare(&interrupted, 22050, 22050));
    assert(speech_progress_advance(&interrupted, 500000000).kind == SPEECH_ADVANCED);
    assert(speech_progress_cancel(&interrupted));
    assert(speech_progress_advance(&interrupted, UINT64_MAX).kind == SPEECH_INACTIVE);
    assert(interrupted.frame == 11025);
    SpeechProgress invalid = speech_progress_new();
    assert(!speech_progress_prepare(&invalid, 100, 0));
    assert(invalid.phase == SPEECH_FAILED);
    assert(!speech_progress_prepare(&invalid, 100, 22050));
    SpeechProgress empty = speech_progress_new();
    assert(speech_progress_prepare(&empty, 0, 22050));
    assert(speech_progress_advance(&empty, 0).kind == SPEECH_COMPLETED);
    assert(speech_progress_advance(&empty, 0).kind == SPEECH_INACTIVE);
    SpeechProgress failed = speech_progress_new();
    assert(speech_progress_prepare(&failed, 100, 22050));
    assert(speech_progress_fail(&failed));
    assert(!speech_progress_fail(&failed));
    assert(speech_progress_advance(&failed, UINT64_MAX).kind == SPEECH_INACTIVE);
}

static void synthesis_boundaries(void) {
    SpeechPreparation first = speech_preparation_new(), second = speech_preparation_new();
    assert(speech_preparation_buffer(&first, 96341, 22050, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_buffer(&first, 0, 22050, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(first.progress.phase == SPEECH_PREPARING);
    assert(speech_preparation_finished(&first) == SPEECH_PREPARATION_READY);
    assert(speech_preparation_buffer(&first, 0, 22050, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_finished(&first) == SPEECH_PREPARATION_PENDING);
    assert(first.progress.total_frames == 96341 && first.progress.frame == 0);
    assert(speech_preparation_finished(&second) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_buffer(&second, 96341, 22050, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_buffer(&second, 0, 22050, 1, 1) == SPEECH_PREPARATION_READY);
    assert(second.progress.total_frames == first.progress.total_frames);
    assert(speech_preparation_buffer(&second, 1, 22050, 1, 1) == SPEECH_PREPARATION_ERROR);
    assert(second.error == SPEECH_BUFFER_AFTER_END && second.progress.phase == SPEECH_FAILED);
    assert(speech_progress_advance(&second.progress, UINT64_MAX).kind == SPEECH_INACTIVE);
    SpeechPreparation changed = speech_preparation_new();
    assert(speech_preparation_buffer(&changed, 100, 22050, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_buffer(&changed, 0, 44100, 1, 1) == SPEECH_PREPARATION_ERROR);
    assert(speech_preparation_finished(&changed) == SPEECH_PREPARATION_ERROR);
    SpeechPreparation overflow = speech_preparation_new();
    assert(speech_preparation_buffer(&overflow, UINT64_MAX, 22050, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_buffer(&overflow, 1, 22050, 1, 1) == SPEECH_PREPARATION_ERROR);
    assert(overflow.error == SPEECH_BUFFER_OVERFLOW);
    SpeechPreparation cancelled = speech_preparation_new();
    assert(speech_progress_cancel(&cancelled.progress));
    assert(speech_preparation_buffer(&cancelled, 0, 22050, 1, 1) == SPEECH_PREPARATION_PENDING);
    assert(speech_preparation_finished(&cancelled) == SPEECH_PREPARATION_PENDING);
    assert(cancelled.progress.phase == SPEECH_CANCELLED);
}

int main(void) {
    preparation_and_pause();
    partition_and_boundaries();
    terminal_dispositions();
    synthesis_boundaries();
    puts("speech preparation and progress: buffer, partition, pause and terminal boundaries passed");
}
