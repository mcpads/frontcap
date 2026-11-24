extern "C" {
#include "../webview-timeline/speech_progress.h"
}
static std::mutex cursor_guard;
static SpeechProgress cursor = speech_progress_new();
static jfieldID speech_listener_field;
static jweak controlled_source;

static jboolean JNICALL cursor_reset(JNIEnv*, jclass) {
    std::lock_guard<std::mutex> locked(cursor_guard);
    if (cursor.phase != SPEECH_FINISHED && cursor.phase != SPEECH_CANCELLED) return false;
    cursor = speech_progress_new();
    return true;
}

static jboolean JNICALL cursor_prepare(JNIEnv*, jclass, jlong frames, jint rate) {
    std::lock_guard<std::mutex> locked(cursor_guard);
    return frames > 0 && rate > 0 && speech_progress_prepare(&cursor, frames, rate);
}

static jlongArray JNICALL cursor_step(JNIEnv* env, jclass, jlong delta) {
    std::lock_guard<std::mutex> locked(cursor_guard);
    auto remaining = [&]() -> uint64_t {
        if (cursor.phase != SPEECH_PLAYABLE) return 0;
        __uint128_t numerator = (__uint128_t)(cursor.total_frames - cursor.frame) * 1000000000
            - cursor.fraction;
        return static_cast<uint64_t>((numerator + cursor.sample_rate - 1) / cursor.sample_rate);
    };
    jint kind = -1; // Read, or a rejected operation with no cursor mutation.
    if (delta == -2) speech_progress_fail(&cursor);
    else if (delta == -3) speech_progress_cancel(&cursor);
    else if (delta >= 0) {
        if (cursor.phase == SPEECH_PLAYABLE && static_cast<uint64_t>(delta) > remaining()) kind = -2;
        else kind = speech_progress_advance(&cursor, delta).kind;
    }
    jlong values[] = {cursor.phase, static_cast<jlong>(cursor.total_frames), cursor.sample_rate,
        static_cast<jlong>(cursor.frame), static_cast<jlong>(cursor.fraction), kind,
        static_cast<jlong>(remaining())};
    auto result = env->NewLongArray(7);
    if (result) env->SetLongArrayRegion(result, 0, 7, values);
    return result;
}

static jboolean JNICALL cursor_owner(JNIEnv* env, jclass, jobject source, jobject listener) {
    if (!source || !listener || !speech_listener_field || !controlled_source ||
        !env->IsSameObject(source, controlled_source)) return false;
    jobject current = env->GetObjectField(source, speech_listener_field);
    bool matches = !env->ExceptionCheck() && env->IsSameObject(current, listener);
    if (current) env->DeleteLocalRef(current);
    return matches;
}

static bool register_cursor(JNIEnv* env, jclass type) {
    JNINativeMethod methods[] = {
        {const_cast<char*>("cursorReset"), const_cast<char*>("()Z"),
         reinterpret_cast<void*>(cursor_reset)},
        {const_cast<char*>("cursorPrepare"), const_cast<char*>("(JI)Z"),
         reinterpret_cast<void*>(cursor_prepare)},
        {const_cast<char*>("cursorStep"), const_cast<char*>("(J)[J"),
         reinterpret_cast<void*>(cursor_step)},
        {const_cast<char*>("cursorOwner"),
         const_cast<char*>("(Landroid/speech/tts/TextToSpeech;Landroid/speech/tts/UtteranceProgressListener;)Z"),
         reinterpret_cast<void*>(cursor_owner)}
    };
    return env->RegisterNatives(type, methods, 4) == JNI_OK;
}
