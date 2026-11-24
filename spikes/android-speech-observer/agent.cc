#include <jni.h>
#include <jvmti.h>
#include <atomic>
#include <cstdio>
#include <cstring>
#include <map>
#include <mutex>
#include <string>
#include <vector>
#include <time.h>
#include <unistd.h>
#include "flow.h"
#include "attachment.h"
#ifdef FRONTCAP_SPEECH_FLOW
static SpeechRequestCounts flow_requests;
static bool observe_submissions;
#endif

// Opt-in, process-lifetime observation. The separate hold-worker build replaces one speak return.
struct Boundary {
    jmethodID method;
    std::string kind;
    jint identity_slot, rate_slot, format_slot, channels_slot, audio_slot, text_slot, params_slot;
};
static std::mutex guard;
static std::vector<Boundary> boundaries;
static std::map<std::string, unsigned> identities;
static std::atomic_flag attached = ATOMIC_FLAG_INIT;
static std::atomic<bool> enabled{false};
static FILE* output;
static unsigned records;
static jclass string_class;
static jclass byte_array_class;
#ifdef FRONTCAP_SPEECH_FLOW
#include "state-sampler.h"
#include "submission.h"
#endif
#ifdef FRONTCAP_SPEECH_FILE_WORKER
#include "file-worker.h"
#endif

static unsigned identity(JNIEnv* env, jobject value) {
    if (!value || !env->IsInstanceOf(value, string_class) ||
        env->GetStringUTFLength(static_cast<jstring>(value)) > 512) return 0;
    const char* bytes = env->GetStringUTFChars(static_cast<jstring>(value), nullptr);
    if (!bytes) return 0;
    std::string key(bytes);
    env->ReleaseStringUTFChars(static_cast<jstring>(value), bytes);
#ifdef FRONTCAP_SPEECH_FILE_WORKER
    if (key == worker_identity) return worker_ordinal;
#ifdef FRONTCAP_SPEECH_SEQUENTIAL
    if (key == worker_identity + "-2") return worker_ordinal;
#endif
#endif
    auto found = identities.find(key);
    if (found != identities.end()) return found->second;
    if (identities.size() >= 64) return 0;
    unsigned ordinal = identities.size() + 1;
    identities.emplace(std::move(key), ordinal);
    return ordinal;
}

static void JNICALL hit(jvmtiEnv* ti, JNIEnv* env, jthread thread, jmethodID method, jlocation) {
    if (!enabled.load() || env->ExceptionCheck()) return;
    std::unique_lock<std::mutex> locked(guard);
    if (!enabled.load() || records >= 256) return;
#ifdef FRONTCAP_SPEECH_FILE_WORKER
    if (observed_complete) return; // This experiment compares only the first ordinary utterance.
#endif
    for (const auto& boundary : boundaries) {
        if (boundary.method != method) continue;
#ifdef FRONTCAP_SPEECH_FILE_WORKER
        bool following = false;
#endif
#ifdef FRONTCAP_SPEECH_CONTROL
        if (worker_requested && (boundary.kind == "speak" || boundary.kind == "stop")) {
            jobject source = nullptr;
            bool same = ti->GetLocalInstance(thread, 0, &source) == JVMTI_ERROR_NONE &&
                source && controlled_source && env->IsSameObject(source, controlled_source);
            if (source) env->DeleteLocalRef(source);
            if (same) {
                if (boundary.kind == "stop") {
                    locked.unlock();
                    bool accepted = env->CallStaticBooleanMethod(worker_class, worker_stop);
                    bool exception = env->ExceptionCheck();
                    if (exception) env->ExceptionClear();
                    accepted = accepted && !exception;
                    if (!accepted) {
                        std::lock_guard<std::mutex> cursor_locked(cursor_guard);
                        speech_progress_fail(&cursor);
                    }
                    auto returned = ti->ForceEarlyReturnInt(thread, accepted ? 0 : -1);
                    locked.lock();
                    fprintf(output, "{\"event\":\"controlled_stop\",\"accepted\":%s,"
                        "\"observer_exception\":%s,\"early_return_error\":%d}\n",
                        accepted ? "true" : "false", exception ? "true" : "false", returned);
                    fflush(output);
                    return;
                }
                {
                    std::lock_guard<std::mutex> cursor_locked(cursor_guard);
#ifdef FRONTCAP_SPEECH_SEQUENTIAL
                    following = !worker_dispatching && worker_requests < 2 &&
                        (cursor.phase == SPEECH_FINISHED || cursor.phase == SPEECH_CANCELLED);
#endif
                    if (!following) speech_progress_fail(&cursor);
                }
                if (!following) {
                    auto rejected = ti->ForceEarlyReturnInt(thread, -1);
                    fprintf(output, "{\"event\":\"unsupported_control_request\",\"kind\":\"%s\","
                        "\"early_return_error\":%d}\n", boundary.kind.c_str(), rejected);
                    fflush(output);
                    return;
                }
            }
        }
#endif
        jobject value = nullptr;
        auto error = boundary.identity_slot < 0 ? JVMTI_ERROR_ABSENT_INFORMATION
            : ti->GetLocalObject(thread, 0, boundary.identity_slot, &value);
        unsigned ordinal = error == JVMTI_ERROR_NONE ? identity(env, value) : 0;
        bool exception = env->ExceptionCheck();
        if (exception) env->ExceptionClear(); // Only an exception introduced by this observer.
        if (value) env->DeleteLocalRef(value);
#ifdef FRONTCAP_SPEECH_FILE_WORKER
        if (ordinal == worker_ordinal) return; // Worker has its own bounded numeric receipt.
#endif
#ifdef FRONTCAP_SPEECH_FLOW
        const unsigned request_count = flow_requests.observe(ordinal, boundary.kind);
        const unsigned synthesizer = (boundary.kind == "speak" || boundary.kind == "stop")
            ? remember_synthesizer(ti, env, thread) : 0;
#endif
#ifndef FRONTCAP_SPEECH_FLOW
        jint rate = -1, format = -1, channels = -1, bytes = -1, nonzero = -1;
        jvmtiError pcm_error = JVMTI_ERROR_NONE;
        bool pcm_omitted = false;
        auto read_int = [&](jint slot, jint* destination) {
            auto read_error = slot < 0 ? JVMTI_ERROR_ABSENT_INFORMATION
                : ti->GetLocalInt(thread, 0, slot, destination);
            if (read_error != JVMTI_ERROR_NONE) pcm_error = read_error;
        };
        if (boundary.kind == "onBeginSynthesis") {
            read_int(boundary.rate_slot, &rate);
            read_int(boundary.format_slot, &format);
            read_int(boundary.channels_slot, &channels);
        } else if (boundary.kind == "onAudioAvailable") {
            jobject audio = nullptr;
            pcm_error = boundary.audio_slot < 0 ? JVMTI_ERROR_ABSENT_INFORMATION
                : ti->GetLocalObject(thread, 0, boundary.audio_slot, &audio);
            if (pcm_error == JVMTI_ERROR_NONE && audio && env->IsInstanceOf(audio, byte_array_class)) {
                auto array = static_cast<jbyteArray>(audio);
                bytes = env->GetArrayLength(array);
                if (bytes <= 1048576) {
                    // Transient bounded copy for a nonzero count; never persist audio bytes.
                    std::vector<jbyte> buffer(bytes);
                    if (bytes > 0) env->GetByteArrayRegion(array, 0, bytes, buffer.data());
                    nonzero = 0;
                    for (jbyte byte : buffer) if (byte != 0) ++nonzero;
                } else pcm_omitted = true;
            } else if (pcm_error == JVMTI_ERROR_NONE) pcm_error = JVMTI_ERROR_TYPE_MISMATCH;
            if (audio) env->DeleteLocalRef(audio);
        }
        if (env->ExceptionCheck()) {
            exception = true;
            env->ExceptionClear();
        }
#endif
        timespec now{};
        clock_gettime(CLOCK_MONOTONIC, &now);
        ++records;
#ifdef FRONTCAP_SPEECH_FLOW
        fprintf(output, "{\"event\":\"boundary\",\"sequence\":%u,\"kind\":\"%s\","
            "\"identity\":%u,\"request_count\":%u,\"correlation\":\"%s\",\"synthesizer\":%u,"
            "\"local_error\":%d,\"observer_exception\":%s,\"monotonic_ns\":%lld,\"limit_reached\":%s}\n",
            records, boundary.kind.c_str(), ordinal, request_count, speech_correlation(request_count), synthesizer,
            error, exception ? "true" : "false",
            static_cast<long long>(now.tv_sec) * 1000000000LL + now.tv_nsec,
            records == 256 ? "true" : "false");
        if (observe_submissions && boundary.kind == "speak") watch_submission(ti, thread, method, records, ordinal);
#else
        fprintf(output, "{\"event\":\"boundary\",\"sequence\":%u,\"kind\":\"%s\","
            "\"identity\":%u,\"local_error\":%d,\"observer_exception\":%s,\"monotonic_ns\":%lld,"
            "\"limit_reached\":%s,\"sample_rate\":%d,\"audio_format\":%d,\"channels\":%d,"
            "\"pcm_bytes\":%d,\"nonzero_bytes\":%d,\"pcm_error\":%d,\"pcm_omitted\":%s}\n",
            records, boundary.kind.c_str(), ordinal, error, exception ? "true" : "false",
            static_cast<long long>(now.tv_sec) * 1000000000LL + now.tv_nsec,
            records == 256 ? "true" : "false", rate, format, channels, bytes, nonzero,
            pcm_error, pcm_omitted ? "true" : "false");
#endif
        fflush(output);
#ifdef FRONTCAP_SPEECH_FILE_WORKER
        if (boundary.kind == "onSuccess" && ordinal == 1) observed_complete = true;
        if (boundary.kind == "speak" && (!worker_requested || following)) {
            worker_requested = true;
            worker_dispatching = true;
            ++worker_requests;
            std::string token = worker_identity + (worker_requests == 1 ? "" : "-2");
            jint text_slot = boundary.text_slot, params_slot = boundary.params_slot;
            jint identity_slot = boundary.identity_slot;
            locked.unlock();
            const char* stage = nullptr;
            bool ok = prepare_worker(ti, env, thread, text_slot, params_slot, identity_slot, token, &stage);
#ifdef FRONTCAP_SPEECH_HOLD
            // Apply only to this selected request, after its real preparation has been dispatched.
            // Returning SUCCESS admits the request while preventing ordinary engine playback.
            auto held = ti->ForceEarlyReturnInt(thread, ok ? 0 : -1);
#endif
            locked.lock();
            worker_dispatching = false;
            fprintf(output, "{\"event\":\"worker_dispatch\",\"passed\":%s,\"stage\":\"%s\"}\n",
                    ok ? "true" : "false", stage);
#ifdef FRONTCAP_SPEECH_HOLD
            fprintf(output, "{\"event\":\"speech_hold\",\"identity\":%u,\"early_return_error\":%d}\n",
                    ordinal, held);
#endif
            fflush(output);
        }
#endif
        return;
    }
}

static void inspect(jvmtiEnv* ti, jclass type) {
    char* signature = nullptr;
    if (ti->GetClassSignature(type, &signature, nullptr) != JVMTI_ERROR_NONE) return;
    bool outer = !strcmp(signature, "Landroid/speech/tts/TextToSpeech;");
    bool connection = !strncmp(signature, "Landroid/speech/tts/TextToSpeech$Connection",
                               strlen("Landroid/speech/tts/TextToSpeech$Connection"));
    ti->Deallocate(reinterpret_cast<unsigned char*>(signature));
    if (!outer && !connection) return;
    jint count = 0;
    jmethodID* methods = nullptr;
    if (ti->GetClassMethods(type, &count, &methods) != JVMTI_ERROR_NONE) return;
    for (int i = 0; i < count; ++i) {
        char *name = nullptr, *descriptor = nullptr;
        if (ti->GetMethodName(methods[i], &name, &descriptor, nullptr) != JVMTI_ERROR_NONE) continue;
        std::string kind;
        if (outer && !strcmp(name, "speak") &&
            !strcmp(descriptor, "(Ljava/lang/CharSequence;ILandroid/os/Bundle;Ljava/lang/String;)I")) kind = "speak";
        if (outer && !strcmp(name, "stop") && !strcmp(descriptor, "()I")) kind = "stop";
        if (connection && !strcmp(descriptor, "(Ljava/lang/String;)V")) {
            if (!strcmp(name, "onStart")) kind = "onStart";
            if (!strcmp(name, "onSuccess")) kind = "onSuccess";
        }
        if (connection && !strcmp(name, "onError") &&
            !strcmp(descriptor, "(Ljava/lang/String;I)V")) kind = "onError";
        if (connection && !strcmp(name, "onStop") &&
            !strcmp(descriptor, "(Ljava/lang/String;Z)V")) kind = "onStop";
        if (connection && !strcmp(name, "onBeginSynthesis") &&
            !strcmp(descriptor, "(Ljava/lang/String;III)V")) kind = "onBeginSynthesis";
        if (connection && !strcmp(name, "onAudioAvailable") &&
            !strcmp(descriptor, "(Ljava/lang/String;[B)V")) kind = "onAudioAvailable";
        ti->Deallocate(reinterpret_cast<unsigned char*>(name));
        ti->Deallocate(reinterpret_cast<unsigned char*>(descriptor));
        if (kind.empty()) continue;
#ifdef FRONTCAP_SPEECH_FLOW
        if (!speech_flow_boundary(kind)) continue;
#endif
        std::lock_guard<std::mutex> locked(guard);
        bool known = false;
        for (const auto& existing : boundaries) if (existing.method == methods[i]) known = true;
        if (known) continue;
        jlocation start = 0, end = 0;
        auto location_error = ti->GetMethodLocation(methods[i], &start, &end);
        jint slot = -1, locals_count = 0;
        jint rate_slot = -1, format_slot = -1, channels_slot = -1, audio_slot = -1;
        jint text_slot = -1, params_slot = -1;
        jvmtiLocalVariableEntry* locals = nullptr;
        auto locals_error = ti->GetLocalVariableTable(methods[i], &locals_count, &locals);
        if (locals_error == JVMTI_ERROR_NONE) {
            for (int j = 0; j < locals_count; ++j) {
                auto& local = locals[j];
                if (!strcmp(local.name, "utteranceId") && !strcmp(local.signature, "Ljava/lang/String;") &&
                    local.start_location <= start && start < local.start_location + local.length)
                    slot = local.slot;
                if (local.start_location <= start && start < local.start_location + local.length) {
                    if (!strcmp(local.signature, "I")) {
                        if (!strcmp(local.name, "sampleRateInHz")) rate_slot = local.slot;
                        if (!strcmp(local.name, "audioFormat")) format_slot = local.slot;
                        if (!strcmp(local.name, "channelCount")) channels_slot = local.slot;
                    }
                    if (!strcmp(local.signature, "[B") && !strcmp(local.name, "audio")) audio_slot = local.slot;
                    if (!strcmp(local.signature, "Ljava/lang/CharSequence;") && !strcmp(local.name, "text"))
                        text_slot = local.slot;
                    if (!strcmp(local.signature, "Landroid/os/Bundle;") && !strcmp(local.name, "params"))
                        params_slot = local.slot;
                }
                ti->Deallocate(reinterpret_cast<unsigned char*>(local.name));
                ti->Deallocate(reinterpret_cast<unsigned char*>(local.signature));
                if (local.generic_signature)
                    ti->Deallocate(reinterpret_cast<unsigned char*>(local.generic_signature));
            }
            ti->Deallocate(reinterpret_cast<unsigned char*>(locals));
        }
        auto error = location_error == JVMTI_ERROR_NONE ? ti->SetBreakpoint(methods[i], start) : location_error;
        fprintf(output, "{\"event\":\"bind\",\"kind\":\"%s\",\"local_table_error\":%d,"
            "\"identity_slot\":%d,\"breakpoint_error\":%d,\"rate_slot\":%d,\"format_slot\":%d,"
            "\"channels_slot\":%d,\"audio_slot\":%d,\"text_slot\":%d,\"params_slot\":%d}\n",
            kind.c_str(), locals_error, slot, error,
            rate_slot, format_slot, channels_slot, audio_slot, text_slot, params_slot);
        if (error == JVMTI_ERROR_NONE)
            boundaries.push_back({methods[i], kind, slot, rate_slot, format_slot, channels_slot,
                                  audio_slot, text_slot, params_slot});
        fflush(output);
    }
    ti->Deallocate(reinterpret_cast<unsigned char*>(methods));
}

static void JNICALL prepared(jvmtiEnv* ti, JNIEnv*, jthread, jclass type) {
    if (enabled.load()) inspect(ti, type);
}

extern "C" JNIEXPORT jint JNICALL Agent_OnAttach(JavaVM* vm, char* options, void*) {
    if (attached.test_and_set() || !options || options[0] != '/' || strstr(options, "..")) return JNI_ERR;
    SpeechAttachment attachment;
    std::string directory(options);
    if (strchr(options, '|')) {
#ifdef FRONTCAP_SPEECH_FLOW
        if (!speech_attachment(options, attachment) || attachment.pid != static_cast<uint64_t>(getpid())) return JNI_ERR;
        char stat[4096];
        FILE* process = fopen("/proc/self/stat", "r");
        if (!process) return JNI_ERR;
        const bool read = fgets(stat, sizeof(stat), process) && strchr(stat, '\n');
        fclose(process);
        if (!read || !speech_process_matches(stat, attachment)) return JNI_ERR;
        directory = attachment.directory;
#else
        return JNI_ERR;
#endif
    }
    std::string path = directory + "/speech-observer.jsonl";
    output = fopen(path.c_str(), "wx");
    if (!output) return JNI_ERR;
    jvmtiEnv* ti = nullptr;
    JNIEnv* env = nullptr;
    jint result = vm->GetEnv(reinterpret_cast<void**>(&ti), JVMTI_VERSION_1_2 | 0x40000000);
    if (attachment.id.empty()) {
        fprintf(output, "{\"event\":\"attach\",\"pid\":%d,\"get_env\":%d}\n", getpid(), result);
    } else {
        fprintf(output, "{\"event\":\"attach\",\"pid\":%d,\"get_env\":%d,\"attachment\":\"%s\",\"start_ticks\":%llu}\n",
            getpid(), result, attachment.id.c_str(), static_cast<unsigned long long>(attachment.start_ticks));
    }
    fflush(output);
    if (result != JNI_OK || vm->GetEnv(reinterpret_cast<void**>(&env), JNI_VERSION_1_6) != JNI_OK) return JNI_ERR;
    jclass strings = env->FindClass("java/lang/String");
    if (!strings) return JNI_ERR;
    string_class = static_cast<jclass>(env->NewGlobalRef(strings));
    env->DeleteLocalRef(strings);
    if (!string_class) return JNI_ERR;
    jclass bytes = env->FindClass("[B");
    if (!bytes) return JNI_ERR;
    byte_array_class = static_cast<jclass>(env->NewGlobalRef(bytes));
    env->DeleteLocalRef(bytes);
    if (!byte_array_class) return JNI_ERR;
#ifdef FRONTCAP_SPEECH_FILE_WORKER
    if (!load_worker(env, options)) {
        fprintf(output, "{\"event\":\"failed\",\"reason\":\"worker_load\"}\n");
        fflush(output);
        return JNI_ERR;
    }
#endif
#ifdef FRONTCAP_SPEECH_FLOW
    if (!attachment.id.empty() && !start_sampler(ti, env, attachment)) return JNI_ERR;
#endif
    jvmtiCapabilities caps{};
    caps.can_generate_breakpoint_events = 1;
    caps.can_access_local_variables = 1;
#ifdef FRONTCAP_SPEECH_HOLD
    caps.can_force_early_return = 1;
#endif
    auto capabilities = ti->AddCapabilities(&caps);
#ifdef FRONTCAP_SPEECH_FLOW
    jvmtiCapabilities submission_caps{};
    submission_caps.can_generate_method_exit_events = 1;
    observe_submissions = ti->AddCapabilities(&submission_caps) == JVMTI_ERROR_NONE;
#endif
    jvmtiEventCallbacks callbacks{};
    callbacks.Breakpoint = hit;
    callbacks.ClassPrepare = prepared;
#ifdef FRONTCAP_SPEECH_FLOW
    callbacks.MethodExit = submission_returned;
#endif
    auto callback_error = ti->SetEventCallbacks(&callbacks, sizeof(callbacks));
    auto breakpoint_error = ti->SetEventNotificationMode(JVMTI_ENABLE, JVMTI_EVENT_BREAKPOINT, nullptr);
    auto prepare_error = ti->SetEventNotificationMode(JVMTI_ENABLE, JVMTI_EVENT_CLASS_PREPARE, nullptr);
    jint count = 0;
    jclass* classes = nullptr;
    auto loaded_error = ti->GetLoadedClasses(&count, &classes);
    if (capabilities || callback_error || breakpoint_error || prepare_error || loaded_error) {
        fprintf(output, "{\"event\":\"failed\",\"capabilities\":%d,\"callbacks\":%d,"
            "\"breakpoints\":%d,\"class_prepare\":%d,\"loaded_classes\":%d}\n",
            capabilities, callback_error, breakpoint_error, prepare_error, loaded_error);
        fflush(output);
        return JNI_ERR; // Admission stays closed; retire this process, never reattach.
    }
    enabled.store(true);
    for (int i = 0; i < count; ++i) {
        inspect(ti, classes[i]);
        env->DeleteLocalRef(classes[i]);
    }
    ti->Deallocate(reinterpret_cast<unsigned char*>(classes));
    std::lock_guard<std::mutex> locked(guard);
#ifdef FRONTCAP_SPEECH_FLOW
    fprintf(output, "{\"event\":\"ready\",\"pid\":%d,\"boundaries\":%zu,\"flow_only\":true,\"sampling\":%s,\"submission_observed\":%s}\n", getpid(), boundaries.size(), attachment.id.empty() ? "false" : "true", observe_submissions ? "true" : "false");
#else
    fprintf(output, "{\"event\":\"ready\",\"pid\":%d,\"boundaries\":%zu}\n", getpid(), boundaries.size());
#endif
    fflush(output);
    return JNI_OK;
}
