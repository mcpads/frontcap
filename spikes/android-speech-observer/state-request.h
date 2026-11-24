#ifndef FRONTCAP_SPEECH_STATE_REQUEST_H
#define FRONTCAP_SPEECH_STATE_REQUEST_H
#include "attachment.h"

struct SpeechStateRequest {
    uint64_t index = 0;
    std::string id;
};
inline bool speech_state_request(std::string_view raw, SpeechStateRequest& request) {
    if (raw.empty() || raw.size() > 54 || raw.back() != '\n') return false;
    raw.remove_suffix(1);
    const auto split = raw.find('|');
    if (split == std::string_view::npos || !speech_number(raw.substr(0, split), request.index)) return false;
    raw.remove_prefix(split + 1);
    if (raw.size() != 32) return false;
    for (char c : raw) if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
    request.id = raw;
    return true;
}
enum class SpeechStateAdmission { Fresh, Replay, Stale };
inline SpeechStateAdmission admit_speech_state(const SpeechStateRequest& next, SpeechStateRequest& last) {
    if (next.index == 0 || next.index < last.index || (next.index == last.index && next.id != last.id))
        return SpeechStateAdmission::Stale;
    if (next.index == last.index) return SpeechStateAdmission::Replay;
    last = next;
    return SpeechStateAdmission::Fresh;
}
#endif
