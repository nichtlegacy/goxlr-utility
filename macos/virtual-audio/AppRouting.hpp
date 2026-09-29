#pragma once

#include "StereoHistory.hpp"

#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <memory>
#include <optional>
#include <string>
#include <sys/types.h>
#include <utility>
#include <vector>

// Per-app routing between the stereo playback routes (System, Game, Chat, Music, Sample).
//
// Each playback device sees every client's samples before the HAL mixes them. A client whose
// PID has a rule is scaled by the rule's gain; if the rule names another route, its samples
// move into a per-cycle scratch buffer for that route and are zeroed in place. When the source
// device writes its mix, the scratch buffers are published to injection histories, which the
// target route's bridge reader sums with its own history.
//
// Every client's peak level after gain is recorded per PID, so the daemon can show meters.
namespace app_routing {

constexpr size_t kRoutes = 5;
constexpr size_t kMaxRules = 128;
constexpr uint32_t kMaxFrames = 4096;
constexpr uint32_t kKeepRoute = 0xFF;
constexpr uint32_t kUnityGain = 1000;
constexpr uint32_t kMaxGain = 2000;
// Boosted samples above this magnitude are softly saturated so they never pass 1.0.
constexpr float kLimiterKnee = 0.8f;
constexpr size_t kLevelSlots = 128;
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
// gain is per mille (0-2000). Returns nothing if any entry is malformed.
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
        if (gainText.empty() || *rest != '\0' || gain < 0 || gain > long(kMaxGain)) {
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

// Identity up to the knee, then a tanh curve that approaches 1.0 without reaching past it.
inline float softLimit(float sample) {
    const float magnitude = std::fabs(sample);
    if (!(magnitude > kLimiterKnee)) return sample;
    const float range = 1.0f - kLimiterKnee;
    const float limited = kLimiterKnee + range * std::tanh((magnitude - kLimiterKnee) / range);
    return std::copysign(std::min(limited, 1.0f), sample);
}

// Scales `samples` in place by a per mille gain. Mute writes exact zeros; boosts are limited.
inline void applyGain(float* samples, uint32_t count, uint32_t gain) {
    if (gain == kUnityGain) return;
    if (gain == 0) {
        std::fill(samples, samples + count, 0.0f);
        return;
    }
    const float scale = float(gain) / float(kUnityGain);
    if (gain < kUnityGain) {
        for (uint32_t i = 0; i < count; ++i) samples[i] *= scale;
        return;
    }
    for (uint32_t i = 0; i < count; ++i) samples[i] = softLimit(samples[i] * scale);
}

// The largest sample magnitude in per mille, clamped to 0-1000.
inline uint32_t peakPerMille(const float* samples, uint32_t count) {
    float peak = 0.0f;
    for (uint32_t i = 0; i < count; ++i) {
        const float magnitude = std::fabs(samples[i]);
        if (magnitude > peak) peak = magnitude;
    }
    return uint32_t(std::min(peak, 1.0f) * float(kUnityGain) + 0.5f);
}

// Peak level per PID since the last take, in fixed slots packing pid << 32 | peak. IO threads
// claim an empty slot or raise their own with compare-and-swap. A take racing a record may
// leave one PID in two slots; take() merges them. When every slot is taken, new PIDs are
// dropped until the next take.
class LevelTable {
public:
    void record(pid_t pid, uint32_t peak) {
        if (pid <= 0 || peak == 0) return;
        const uint64_t packed = (uint64_t(uint32_t(pid)) << 32) | peak;
        for (auto& slot : slots_) {
            uint64_t current = slot.load(std::memory_order_relaxed);
            while (true) {
                if (current == 0) {
                    if (slot.compare_exchange_weak(current, packed, std::memory_order_acq_rel,
                                                   std::memory_order_relaxed)) {
                        return;
                    }
                    continue;
                }
                if (pid_t(current >> 32) != pid) break;
                if (uint32_t(current) >= peak) return;
                if (slot.compare_exchange_weak(current, packed, std::memory_order_acq_rel,
                                               std::memory_order_relaxed)) {
                    return;
                }
            }
        }
    }

    // Returns "pid:peak;..." for every recorded PID and clears the table. Not real-time safe.
    std::string take() {
        std::vector<std::pair<pid_t, uint32_t>> peaks;
        for (auto& slot : slots_) {
            const uint64_t packed = slot.exchange(0, std::memory_order_acq_rel);
            if (packed == 0) continue;
            const pid_t pid = pid_t(packed >> 32);
            const uint32_t peak = std::min(uint32_t(packed), kUnityGain);
            auto existing = std::find_if(peaks.begin(), peaks.end(),
                                         [pid](const auto& entry) { return entry.first == pid; });
            if (existing == peaks.end()) peaks.emplace_back(pid, peak);
            else existing->second = std::max(existing->second, peak);
        }
        std::string text;
        for (const auto& [pid, peak] : peaks) {
            if (peak == 0) continue;
            text += std::to_string(pid) + ':' + std::to_string(peak) + ';';
        }
        return text;
    }

private:
    std::array<std::atomic<uint64_t>, kLevelSlots> slots_{};
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
    // Clients without a rule keep their route and gain but still have their level recorded.
    void processClientOutput(size_t source, pid_t pid, float* frames, uint32_t frameCount,
                             double sampleTime, uint32_t enabledRoutes) {
        if (source >= kRoutes) return;
        uint32_t route = kKeepRoute;
        uint32_t gain = kUnityGain;
        rules_.lookup(pid, route, gain);

        applyGain(frames, frameCount * 2, gain);
        levels_.record(pid, peakPerMille(frames, frameCount * 2));

        const bool moves = route < kRoutes && route != source && frameCount <= kMaxFrames &&
                           (enabledRoutes & (1u << route)) != 0;
        if (!moves) return;

        Scratch& scratch = scratch_[source][route];
        if (!scratch.used || scratch.sampleTime != sampleTime || scratch.frames != frameCount) {
            std::fill(scratch.samples.begin(), scratch.samples.begin() + frameCount * 2, 0.0f);
            scratch.sampleTime = sampleTime;
            scratch.frames = frameCount;
            scratch.used = true;
        }
        for (uint32_t i = 0; i < frameCount * 2; ++i) {
            scratch.samples[i] += frames[i];
            frames[i] = 0.0f;
        }
    }

    // Peak levels since the previous call as "pid:peak;...". Not real-time safe.
    std::string takeLevels() { return levels_.take(); }

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
    LevelTable levels_;
    std::array<std::array<Scratch, kRoutes>, kRoutes> scratch_{};
    std::array<std::array<std::unique_ptr<StereoHistory>, kRoutes>, kRoutes> injections_{};
};

} // namespace app_routing
