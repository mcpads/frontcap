// Method-exit observation is enabled only while this thread has an observed speak call.
// Return values are evidence of submission, never evidence of playback or completion.
struct SpeechSubmission {
    jmethodID method;
    jint depth;
    unsigned sequence;
    unsigned identity;
};
static thread_local std::vector<SpeechSubmission> speech_submissions;

static void write_submission(const SpeechSubmission& call, const jint* returned,
                             bool threw, jvmtiError error) {
    char value[32];
    if (returned) snprintf(value, sizeof(value), "%d", *returned);
    else snprintf(value, sizeof(value), "null");
    fprintf(output, "{\"event\":\"submission\",\"request_sequence\":%u,\"identity\":%u,"
        "\"returned\":%s,\"threw\":%s,\"observer_error\":%d}\n",
        call.sequence, call.identity, value, threw ? "true" : "false", error);
    fflush(output);
}

static void watch_submission(jvmtiEnv* ti, jthread thread, jmethodID method,
                             unsigned sequence, unsigned identity) {
    SpeechSubmission call{method, 0, sequence, identity};
    auto error = ti->GetFrameCount(thread, &call.depth);
    if (error == JVMTI_ERROR_NONE && speech_submissions.empty())
        error = ti->SetEventNotificationMode(JVMTI_ENABLE, JVMTI_EVENT_METHOD_EXIT, thread);
    if (error != JVMTI_ERROR_NONE) {
        write_submission(call, nullptr, false, error);
        return;
    }
    // At most 256 entry records exist in the entire process. References to Java objects
    // are not retained, and the per-thread vector is released with its native thread.
    speech_submissions.push_back(call);
}

static void JNICALL submission_returned(jvmtiEnv* ti, JNIEnv*, jthread thread,
                                        jmethodID method, jboolean threw, jvalue value) {
    if (speech_submissions.empty() || speech_submissions.back().method != method) return;
    jint depth = 0;
    auto error = ti->GetFrameCount(thread, &depth);
    if (error == JVMTI_ERROR_NONE && depth != speech_submissions.back().depth) return;
    std::lock_guard<std::mutex> locked(guard);
    if (error != JVMTI_ERROR_NONE) {
        // A failed stack query cannot safely associate this exit with any nested call.
        for (const auto& call : speech_submissions) write_submission(call, nullptr, false, error);
        speech_submissions.clear();
    } else {
        auto call = speech_submissions.back();
        speech_submissions.pop_back();
        write_submission(call, threw ? nullptr : &value.i, threw, JVMTI_ERROR_NONE);
    }
    if (speech_submissions.empty())
        ti->SetEventNotificationMode(JVMTI_DISABLE, JVMTI_EVENT_METHOD_EXIT, thread);
}
