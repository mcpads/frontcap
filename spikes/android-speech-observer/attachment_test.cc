#include "attachment.h"
#include "state-request.h"
#include <cassert>

int main() {
    SpeechAttachment attachment;
    const std::string option = "/data/user/0/dev.example/files/speech|123|456|12345678-1234-1234-1234-123456789abc";
    assert(speech_attachment(option, attachment));
    const std::string stat = "123 (name with ) spaces) S " + std::string("1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 ") + "456 999\n";
    assert(speech_process_matches(stat, attachment));
    attachment.start_ticks++;
    assert(!speech_process_matches(stat, attachment));
    attachment.start_ticks--;
    attachment.pid++;
    assert(!speech_process_matches(stat, attachment));
    for (const auto& bad : {option + "|extra", std::string("/tmp/a|123|0|12345678-1234-1234-1234-123456789abc"),
            std::string("/tmp/../a|123|456|12345678-1234-1234-1234-123456789abc"),
            std::string("/tmp/a|123|456|bad"), std::string("/tmp/a|+123|456|12345678-1234-1234-1234-123456789abc")}) {
        assert(!speech_attachment(bad, attachment));
    }
    assert(!speech_process_matches("123 (truncated) S 1 2", attachment));
    SpeechStateRequest request, last;
    assert(speech_state_request("1|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n", request));
    assert(admit_speech_state(request, last) == SpeechStateAdmission::Fresh);
    assert(admit_speech_state(request, last) == SpeechStateAdmission::Replay);
    auto conflicting = request; conflicting.id = std::string(32, 'b');
    assert(admit_speech_state(conflicting, last) == SpeechStateAdmission::Stale);
    for (uint64_t i = 2; i <= 10000; ++i) {
        const auto digits = std::to_string(i);
        SpeechStateRequest next{i, std::string(32 - digits.size(), '0') + digits};
        assert(admit_speech_state(next, last) == SpeechStateAdmission::Fresh);
    }
    assert(admit_speech_state(request, last) == SpeechStateAdmission::Stale);
    assert(last.index == 10000 && last.id.size() == 32);
    for (const auto& invalid : {"0|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n", "1|a\n",
        "1|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "1|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa|\n",
        "18446744073709551616|aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"}) {
        assert(!speech_state_request(invalid, request));
    }
}
