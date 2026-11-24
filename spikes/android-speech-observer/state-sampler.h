#ifndef FRONTCAP_SPEECH_STATE_SAMPLER_H
#define FRONTCAP_SPEECH_STATE_SAMPLER_H
#include <sys/inotify.h>
#include <cerrno>
#include "state-request.h"

// Framework queries run off the consumer thread, without holding the breakpoint mutex.
static std::mutex sample_guard;
static std::vector<jweak> sampled_synthesizers;
static jclass speech_type;
static jmethodID speaking_method;
static unsigned sample_faults;
static bool sample_reference_limit;
static int sample_notifications = -1;
static SpeechAttachment sample_attachment;

static unsigned remember_synthesizer(jvmtiEnv* ti, JNIEnv* env, jthread thread) {
    jobject source = nullptr;
    const auto error = ti->GetLocalInstance(thread, 0, &source);
    std::lock_guard<std::mutex> locked(sample_guard);
    if (error || !source) {
        ++sample_faults;
        if (source) env->DeleteLocalRef(source);
        if (env->ExceptionCheck()) env->ExceptionClear();
        return 0;
    }
    unsigned id = 0;
    for (size_t i = 0; i < sampled_synthesizers.size(); ++i) {
        if (env->IsSameObject(source, sampled_synthesizers[i])) { id = i + 1; break; }
    }
    if (!id && sampled_synthesizers.size() == 8) sample_reference_limit = true;
    else if (!id) {
        auto weak = env->NewWeakGlobalRef(source);
        if (weak) { sampled_synthesizers.push_back(weak); id = sampled_synthesizers.size(); }
        else ++sample_faults;
    }
    if (env->ExceptionCheck()) { env->ExceptionClear(); ++sample_faults; }
    env->DeleteLocalRef(source);
    return id;
}
static unsigned sample_sequence() {
    std::lock_guard<std::mutex> locked(guard);
    return records;
}
static unsigned long long sample_clock() {
    timespec value{};
    clock_gettime(CLOCK_MONOTONIC, &value);
    return static_cast<unsigned long long>(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}
static void sample_state(JNIEnv* env, const SpeechStateRequest& request) {
    const auto before = sample_sequence();
    const auto began = sample_clock();
    std::vector<jobject> sources;
    std::vector<bool> reference_errors;
    unsigned faults;
    bool limited;
    {
        std::lock_guard<std::mutex> locked(sample_guard);
        for (auto weak : sampled_synthesizers) {
            sources.push_back(env->NewLocalRef(weak));
            const bool failed = env->ExceptionCheck();
            reference_errors.push_back(failed);
            if (failed) { env->ExceptionClear(); ++sample_faults; }
        }
        faults = sample_faults; limited = sample_reference_limit;
    }
    std::string rows;
    for (size_t i = 0; i < sources.size(); ++i) {
        const char* state = reference_errors[i] ? "failed" : "collected";
        const char* speaking = "null";
        if (sources[i]) {
            const auto active = env->CallNonvirtualBooleanMethod(sources[i], speech_type, speaking_method);
            if (env->ExceptionCheck()) { env->ExceptionClear(); ++faults; state = "failed"; }
            else { state = "sampled"; speaking = active ? "true" : "false"; }
            env->DeleteLocalRef(sources[i]);
        }
        if (i) rows += ",";
        rows += "{\"id\":" + std::to_string(i + 1) + ",\"state\":\"" + state + "\",\"speaking\":" + speaking + "}";
    }
    const auto ended = sample_clock();
    const auto after = sample_sequence();
    const auto temporary = sample_attachment.directory + "/state-response.tmp";
    FILE* file = fopen(temporary.c_str(), "w");
    if (!file) return;
    fprintf(file, "{\"request\":\"%s\",\"attachment\":\"%s\",\"pid\":%d,\"start_ticks\":%llu,"
        "\"request_index\":%llu,\"sequence_before\":%u,\"sequence_after\":%u,\"began_ns\":%llu,\"ended_ns\":%llu,"
        "\"faults\":%u,\"reference_limit_reached\":%s,\"synthesizers\":[%s]}\n",
        request.id.c_str(), sample_attachment.id.c_str(), getpid(), static_cast<unsigned long long>(sample_attachment.start_ticks),
        static_cast<unsigned long long>(request.index), before, after, began, ended, faults, limited ? "true" : "false", rows.c_str());
    const bool written = !ferror(file) && fflush(file) == 0 && fsync(fileno(file)) == 0;
    const bool closed = fclose(file) == 0;
    if (written && closed) rename(temporary.c_str(), (sample_attachment.directory + "/state-response.json").c_str());
}
static void JNICALL sample_requests(jvmtiEnv*, JNIEnv* env, void*) {
    SpeechStateRequest last;
    char notifications[4096];
    while (true) {
        const auto count = read(sample_notifications, notifications, sizeof(notifications));
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) return;
        // Notifications are only wakeups; read the bounded atomically published request.
        FILE* file = fopen((sample_attachment.directory + "/state-request").c_str(), "r");
        if (!file) continue;
        char bytes[55]{};
        const auto length = fread(bytes, 1, sizeof(bytes), file);
        fclose(file);
        SpeechStateRequest request;
        if (!speech_state_request(std::string_view(bytes, length), request)
            || admit_speech_state(request, last) != SpeechStateAdmission::Fresh) continue;
        // Monotonic admission uses constant memory; old requests cannot resample current state.
        sample_state(env, request);
    }
}
static bool start_sampler(jvmtiEnv* ti, JNIEnv* env, const SpeechAttachment& attachment) {
    auto type = env->FindClass("android/speech/tts/TextToSpeech");
    if (!type) return false;
    speech_type = static_cast<jclass>(env->NewGlobalRef(type));
    env->DeleteLocalRef(type);
    if (!speech_type) return false;
    speaking_method = env->GetMethodID(speech_type, "isSpeaking", "()Z");
    if (!speaking_method) return false;
    sample_attachment = attachment;
    sample_notifications = inotify_init1(IN_CLOEXEC);
    if (sample_notifications < 0) return false;
    if (inotify_add_watch(sample_notifications, attachment.directory.c_str(), IN_MOVED_TO) < 0) {
        close(sample_notifications); sample_notifications = -1; return false;
    }
    auto thread_type = env->FindClass("java/lang/Thread");
    if (!thread_type) return false;
    auto constructor = env->GetMethodID(thread_type, "<init>", "()V");
    auto thread = constructor ? env->NewObject(thread_type, constructor) : nullptr;
    env->DeleteLocalRef(thread_type);
    if (!thread) return false;
    const auto result = ti->RunAgentThread(thread, sample_requests, nullptr, JVMTI_THREAD_NORM_PRIORITY);
    env->DeleteLocalRef(thread);
    return result == JVMTI_ERROR_NONE;
}
#endif
