#ifndef FRONTCAP_SPEECH_ATTACHMENT_H
#define FRONTCAP_SPEECH_ATTACHMENT_H
#include <charconv>
#include <cstdint>
#include <string>
#include <string_view>

struct SpeechAttachment {
    std::string directory, id;
    uint64_t pid = 0, start_ticks = 0;
};
inline bool speech_number(std::string_view text, uint64_t& value) {
    if (text.empty() || text.front() == '0') return false;
    const auto result = std::from_chars(text.data(), text.data() + text.size(), value);
    return result.ec == std::errc{} && result.ptr == text.data() + text.size() && value > 0;
}
inline bool speech_attachment(std::string_view options, SpeechAttachment& result) {
    // Fenced flow mode: absolute directory | PID | process birth ticks | attachment UUID.
    const auto split = options.find('|');
    if (split == std::string_view::npos || split == 0 || options.size() > 4096) return false;
    result.directory = options.substr(0, split);
    if (result.directory.front() != '/' || result.directory.find("..") != std::string::npos) return false;
    for (char c : result.directory) {
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
            || c == '/' || c == '-' || c == '_' || c == '.')) return false;
    }
    options.remove_prefix(split + 1);
    const auto second = options.find('|');
    if (second == std::string_view::npos || !speech_number(options.substr(0, second), result.pid)
        || result.pid > 2147483647) return false;
    options.remove_prefix(second + 1);
    const auto third = options.find('|');
    if (third == std::string_view::npos || !speech_number(options.substr(0, third), result.start_ticks)) return false;
    options.remove_prefix(third + 1);
    if (options.size() != 36) return false;
    for (size_t i = 0; i < options.size(); ++i) {
        const char c = options[i];
        if (i == 8 || i == 13 || i == 18 || i == 23) { if (c != '-') return false; }
        else if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f'))) return false;
    }
    result.id = options;
    return true;
}
inline bool speech_process_matches(std::string_view stat, const SpeechAttachment& expected) {
    const auto name = stat.find(" (");
    const auto end = stat.rfind(") ");
    uint64_t pid = 0, ticks = 0;
    if (name == std::string_view::npos || end == std::string_view::npos || end < name
        || !speech_number(stat.substr(0, name), pid) || pid != expected.pid) return false;
    stat.remove_prefix(end + 2);
    for (unsigned field = 0; field <= 19; ++field) {
        const auto space = stat.find_first_of(" \n");
        const auto value = stat.substr(0, space);
        if (field == 0 && (value == "Z" || value == "X" || value == "x")) return false;
        if (field == 19) return speech_number(value, ticks) && ticks == expected.start_ticks;
        if (space == std::string_view::npos) return false;
        stat.remove_prefix(space + 1);
    }
    return false;
}
#endif
