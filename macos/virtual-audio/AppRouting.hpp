#pragma once

#include "StereoHistory.hpp"

#include <algorithm>
#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <optional>
#include <string>
#include <sys/types.h>
#include <vector>

// Per-app routing between the stereo playback routes (System, Game, Chat, Music, Sample).
//
// Each playback device sees every client's samples before the HAL mixes them. A client whose
// PID has a rule is scaled by the rule's gain; if the rule names another route, its samples
// move into a per-cycle scratch buffer for that route and are zeroed in place. When the source
// device writes its mix, the scratch buffers are published to injection histories, which the
// target route's bridge reader sums with its own history.
namespace app_routing {

constexpr size_t kRoutes = 5;
constexpr size_t kMaxRules = 128;
constexpr uint32_t kMaxFrames = 4096;
constexpr uint32_t kKeepRoute = 0xFF;
constexpr uint32_t kUnityGain = 1000;
constexpr size_t kInjectionFrames = 4096;
// A reader this far behind an injection history (e.g. after it stalled) skips ahead.
constexpr uint64_t kMaxReaderLag = 1024;
constexpr uint64_t kResyncLag = 256;

struct Rule {
    pid_t pid = 0;
    uint32_t route = kKeepRoute;
    uint32_t gain = kUnityGain;
};

// Parses "pid:route:gain;..." where route is 0-4 or '-' to keep the device the app chose, and
// gain is per mille (0-1000). Returns nothing if any entry is malformed.
inline std::optional<std::vector<Rule>> parseRules(const std::string& text) {
    std::vector<Rule> rules;
    size_t start = 0;
    while (start < text.size()) {
        size_t end = text.find(';', start);
        if (end == std::string::npos) end = text.size();
        const std::string entry = text.substr(start, end - start);
        start = end + 1;
        if (entry.empty()) continue;

        const size_t first = entry.find(':');
        const size_t second = first == std::string::npos ? first : entry.find(':', first + 1);
        if (second == std::string::npos) return std::nullopt;

        char* rest = nullptr;
        const std::string pidText = entry.substr(0, first);
        const long pid = std::strtol(pidText.c_str(), &rest, 10);
        if (pidText.empty() || *rest != '\0' || pid <= 0) return std::nullopt;

        const std::string routeText = entry.substr(first + 1, second - first - 1);
        uint32_t route = kKeepRoute;
        if (routeText != "-") {
            const long parsed = std::strtol(routeText.c_str(), &rest, 10);
            if (routeText.empty() || *rest != '\0' || parsed < 0 || parsed >= long(kRoutes)) {
                return std::nullopt;
            }
            route = uint32_t(parsed);
        }

        const std::string gainText = entry.substr(second + 1);
        const long gain = std::strtol(gainText.c_str(), &rest, 10);
        if (gainText.empty() || *rest != '\0' || gain < 0 || gain > long(kUnityGain)) {
            return std::nullopt;
        }

        if (rules.size() == kMaxRules) return std::nullopt;
        rules.push_back({pid_t(pid), route, uint32_t(gain)});
    }
    return rules;
}

// Fixed slots the IO threads scan without locks. An update may be seen half-applied for one
// cycle, which at worst routes one buffer with an old rule.
class RuleTable {
public:
    void apply(const std::vector<Rule>& rules) {
        for (size_t i = 0; i < kMaxRules; ++i) {
            const uint64_t packed = i < rules.size() ? pack(rules[i]) : 0;
            slots_[i].store(packed, std::memory_order_release);
        }
    }

    bool lookup(pid_t pid, uint32_t& route, uint32_t& gain) const {
        for (const auto& slot : slots_) {
            const uint64_t packed = slot.load(std::memory_order_acquire);
            if (packed == 0) return false;
            if (pid_t(packed >> 32) == pid) {
                route = uint32_t(packed >> 16) & 0xFF;
                gain = uint32_t(packed) & 0xFFFF;
                return true;
            }
        }
        return false;
    }

private:
    static uint64_t pack(const Rule& rule) {
        return (uint64_t(uint32_t(rule.pid)) << 32) | (uint64_t(rule.route & 0xFF) << 16) |
               uint64_t(rule.gain & 0xFFFF);
    }

    std::array<std::atomic<uint64_t>, kMaxRules> slots_{};
};

class Router {
public:
    struct Cursors {
        std::array<StereoHistory::Cursor, kRoutes> fromRoute{};
    };

    Router() {
        for (auto& row : injections_) {
            for (auto& history : row) {
                history = std::make_unique<StereoHistory>(kInjectionFrames);
            }
        }
    }

    void setRules(const std::vector<Rule>& rules) { rules_.apply(rules); }

    // Called on the source route's IO thread for one client's stereo samples, before the mix.
    // `enabledRoutes` has bit n set when playback route n is being bridged to the GoXLR.
    void processClientOutput(size_t source, pid_t pid, float* frames, uint32_t frameCount,
                             double sampleTime, uint32_t enabledRoutes) {
        uint32_t route = kKeepRoute;
        uint32_t gain = kUnityGain;
        if (source >= kRoutes || !rules_.lookup(pid, route, gain)) return;

        const float scale = float(gain) / float(kUnityGain);
        const bool moves = route < kRoutes && route != source && frameCount <= kMaxFrames &&
                           (enabledRoutes & (1u << route)) != 0;
        if (!moves) {
            if (gain != kUnityGain) {
                for (uint32_t i = 0; i < frameCount * 2; ++i) frames[i] *= scale;
            }
            return;
        }

        Scratch& scratch = scratch_[source][route];
        if (!scratch.used || scratch.sampleTime != sampleTime || scratch.frames != frameCount) {
            std::fill(scratch.samples.begin(), scratch.samples.begin() + frameCount * 2, 0.0f);
            scratch.sampleTime = sampleTime;
            scratch.frames = frameCount;
            scratch.used = true;
        }
        for (uint32_t i = 0; i < frameCount * 2; ++i) {
            scratch.samples[i] += frames[i] * scale;
            frames[i] = 0.0f;
        }
    }

    // Called on the source route's IO thread when it writes its mix for `sampleTime`.
    void flush(size_t source, double sampleTime) {
        if (source >= kRoutes) return;
        for (size_t target = 0; target < kRoutes; ++target) {
            Scratch& scratch = scratch_[source][target];
            if (!scratch.used) continue;
            if (scratch.sampleTime == sampleTime) {
                injections_[source][target]->push(scratch.samples.data(), scratch.frames);
            }
            scratch.used = false;
        }
    }

    Cursors cursorsFor(size_t target) const {
        Cursors cursors;
        if (target >= kRoutes) return cursors;
        for (size_t source = 0; source < kRoutes; ++source) {
            cursors.fromRoute[source].nextFrame = injections_[source][target]->publishedFrames();
        }
        return cursors;
    }

    // Adds every other route's injected audio for `target` into `output` (stereo, interleaved).
    // Called on the target's bridge reader thread; `scratch` must hold `frameCount` frames.
    void mixInjected(size_t target, Cursors& cursors, float* output, uint32_t frameCount,
                     float* scratch) const {
        if (target >= kRoutes || frameCount > kMaxFrames) return;
        for (size_t source = 0; source < kRoutes; ++source) {
            if (source == target) continue;
            const StereoHistory& history = *injections_[source][target];
            StereoHistory::Cursor& cursor = cursors.fromRoute[source];
            const uint64_t published = history.publishedFrames();
            if (cursor.nextFrame >= published) continue;
            if (published - cursor.nextFrame > kMaxReaderLag) {
                cursor.nextFrame = published - kResyncLag;
            }
            history.read(cursor, scratch, frameCount);
            for (uint32_t i = 0; i < frameCount * 2; ++i) output[i] += scratch[i];
        }
    }

private:
    struct Scratch {
        std::array<float, kMaxFrames * 2> samples{};
        double sampleTime = 0;
        uint32_t frames = 0;
        bool used = false;
    };

    RuleTable rules_;
    std::array<std::array<Scratch, kRoutes>, kRoutes> scratch_{};
    std::array<std::array<std::unique_ptr<StereoHistory>, kRoutes>, kRoutes> injections_{};
};

} // namespace app_routing
