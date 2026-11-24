#include "flow.h"
#include <cassert>
#include <cstring>
#include <cstdio>
int main() {
    SpeechRequestCounts counts;
    assert(counts.observe(1, "onStart") == 0); // Attachment may miss the request.
    assert(counts.observe(0, "speak") == 0);
    assert(counts.observe(65, "speak") == 0);
    assert(counts.observe(1, "speak") == 1);
    assert(counts.observe(1, "onStart") == 1);
    assert(counts.observe(1, "onSuccess") == 1);
    assert(counts.observe(2, "speak") == 1); // A different token stays independent.
    assert(counts.observe(1, "speak") == 2);
    assert(counts.observe(1, "onStop") == 2); // Do not assign a late callback to the latest request.
    assert(!strcmp(speech_correlation(0), "unattributed"));
    assert(!strcmp(speech_correlation(1), "single-observed-request"));
    assert(!strcmp(speech_correlation(2), "reused-token"));
    unsigned retained = 0;
    auto boundary = [&](std::string_view kind) { if (speech_flow_boundary(kind)) retained++; };
    boundary("speak");boundary("onStart");
    for (unsigned i = 0; i < 10000; ++i) boundary("onAudioAvailable");
    boundary("onBeginSynthesis");boundary("onSuccess");
    assert(retained == 3); // Audio chunk volume must not consume the lifecycle evidence budget.
    assert(speech_flow_boundary("stop") && speech_flow_boundary("onStop") && speech_flow_boundary("onError"));
    assert(!speech_flow_boundary("unrecognized"));
    puts("speech flow excludes PCM traffic and reports reused or unobserved request identity");
}
