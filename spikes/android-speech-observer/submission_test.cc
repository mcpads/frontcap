// Exercise call/return correlation and observer errors without starting a speech engine.
#include <cassert>
#include <cstdio>
#include <mutex>
#include <string>
#include <vector>
using jint = int;
using jthread = void*;
using jmethodID = void*;
using jboolean = bool;
using jvmtiError = int;
struct JNIEnv {};
union jvalue { jint i; };
#define JNICALL
constexpr int JVMTI_ERROR_NONE = 0, JVMTI_ENABLE = 1, JVMTI_DISABLE = 0,
    JVMTI_EVENT_METHOD_EXIT = 66;
struct jvmtiEnv {
    jint depth = 10;
    int query_error = 0, event_error = 0;
    bool enabled = false;
    int GetFrameCount(jthread, jint* value) { *value = depth; return query_error; }
    int SetEventNotificationMode(int mode, int, jthread) {
        if (!event_error) enabled = mode == JVMTI_ENABLE;
        return event_error;
    }
};
static FILE* output;
static std::mutex guard;
#include "submission.h"

int main() {
    output = tmpfile(); assert(output);
    jvmtiEnv ti;
    int method_object, other_object;
    auto method = static_cast<void*>(&method_object);
    jvalue rejected{}; rejected.i = -1;
    jvalue accepted{}; accepted.i = 0;
    watch_submission(&ti, nullptr, method, 1, 1);
    assert(ti.enabled);
    ti.depth = 12;
    watch_submission(&ti, nullptr, method, 2, 1); // Same token and method, distinct stack frame.
    ti.depth = 13;
    submission_returned(&ti, nullptr, nullptr, method, false, accepted); // Unobserved recursive frame.
    submission_returned(&ti, nullptr, nullptr, &other_object, false, accepted);
    assert(speech_submissions.size() == 2);
    ti.depth = 12;
    submission_returned(&ti, nullptr, nullptr, method, false, rejected);
    assert(ti.enabled && speech_submissions.size() == 1);
    ti.depth = 10;
    submission_returned(&ti, nullptr, nullptr, method, false, accepted);
    assert(!ti.enabled && speech_submissions.empty());
    watch_submission(&ti, nullptr, method, 3, 2);
    submission_returned(&ti, nullptr, nullptr, method, true, rejected);
    assert(!ti.enabled);
    ti.event_error = 99;
    watch_submission(&ti, nullptr, method, 4, 3);
    assert(!ti.enabled && speech_submissions.empty());
    ti.event_error = 0;
    watch_submission(&ti, nullptr, method, 5, 4);
    ti.query_error = 31;
    submission_returned(&ti, nullptr, nullptr, method, false, accepted);
    assert(!ti.enabled && speech_submissions.empty());
    assert(rejected.i == -1 && accepted.i == 0); // Observing never replaces the producer's value.
    rewind(output);
    std::string records;
    char line[512]; unsigned count = 0;
    while (fgets(line, sizeof(line), output)) { records += line; ++count; }
    assert(count == 5);
    assert(records.find("\"request_sequence\":2,\"identity\":1,\"returned\":-1") != std::string::npos);
    assert(records.find("\"request_sequence\":1,\"identity\":1,\"returned\":0") != std::string::npos);
    assert(records.find("\"request_sequence\":3,\"identity\":2,\"returned\":null,\"threw\":true") != std::string::npos);
    assert(records.find("\"observer_error\":99") != std::string::npos);
    assert(records.find("\"observer_error\":31") != std::string::npos);
    fclose(output);
    puts("submission correlation preserves nested calls, producer returns and observer failures");
}
