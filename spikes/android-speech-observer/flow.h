#ifndef FRONTCAP_SPEECH_FLOW_H
#define FRONTCAP_SPEECH_FLOW_H
#include <array>
#include <string_view>

// Lifecycle evidence deliberately excludes PCM callbacks and audio-buffer inspection.
inline bool speech_flow_boundary(std::string_view kind) {
    return kind == "speak" || kind == "stop" || kind == "onStart" || kind == "onSuccess"
        || kind == "onError" || kind == "onStop";
}
struct SpeechRequestCounts {
    std::array<unsigned, 65> requests{};
    unsigned observe(unsigned identity, std::string_view kind) {
        if (identity == 0 || identity >= requests.size()) return 0;
        if (kind == "speak") ++requests[identity];
        return requests[identity];
    }
};
inline const char* speech_correlation(unsigned requests) {
    // The same consumer ID may denote repeated submissions or different synthesizers.
    // Keep its token identity but never invent a per-request callback association.
    return requests == 0 ? "unattributed" : requests == 1 ? "single-observed-request" : "reused-token";
}
#endif
