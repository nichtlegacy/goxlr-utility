#pragma once

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <stdexcept>

// One real-time writer and any number of readers, each with its own cursor.
class StereoHistory {
public:
    struct Cursor {
        uint64_t nextFrame = 0;
    };

    explicit StereoHistory(size_t capacityFrames)
        : slots_(new Slot[capacityFrames]), capacity_(capacityFrames), mask_(capacityFrames - 1) {
        if (capacityFrames < 2 || (capacityFrames & mask_) != 0) {
            throw std::invalid_argument("stereo history capacity must be a power of two");
        }
    }

    uint64_t publishedFrames() const {
        return published_.load(std::memory_order_acquire);
    }

    void push(const float* interleaved, size_t frames) {
        const uint64_t first = published_.load(std::memory_order_relaxed);
        for (size_t i = 0; i < frames; ++i) {
            const uint64_t frame = first + i;
            Slot& slot = slots_[frame & mask_];
            slot.generation.store(frame * 2 + 1, std::memory_order_seq_cst);
            slot.samples.store(pack(interleaved + i * 2), std::memory_order_seq_cst);
            slot.generation.store(frame * 2 + 2, std::memory_order_seq_cst);
        }
        published_.store(first + frames, std::memory_order_release);
    }

    void read(Cursor& cursor, float* interleaved, size_t frames) const {
        const uint64_t published = publishedFrames();
        const uint64_t oldest = published > capacity_ ? published - capacity_ : 0;
        if (cursor.nextFrame < oldest) {
            cursor.nextFrame = oldest;
        }

        for (size_t i = 0; i < frames; ++i) {
            float* output = interleaved + i * 2;
            if (cursor.nextFrame >= published) {
                output[0] = output[1] = 0;
                continue;
            }

            const Slot& slot = slots_[cursor.nextFrame & mask_];
            const uint64_t expected = cursor.nextFrame * 2 + 2;
            const uint64_t before = slot.generation.load(std::memory_order_seq_cst);
            const uint64_t samples = slot.samples.load(std::memory_order_seq_cst);
            const uint64_t after = slot.generation.load(std::memory_order_seq_cst);
            if (before == expected && after == expected) {
                unpack(samples, output);
            } else {
                output[0] = output[1] = 0;
            }
            ++cursor.nextFrame;
        }
    }

private:
    struct Slot {
        std::atomic<uint64_t> generation{0};
        std::atomic<uint64_t> samples{0};
    };

    static uint64_t pack(const float* samples) {
        uint32_t left;
        uint32_t right;
        std::memcpy(&left, samples, sizeof(left));
        std::memcpy(&right, samples + 1, sizeof(right));
        return uint64_t(left) | (uint64_t(right) << 32);
    }

    static void unpack(uint64_t packed, float* samples) {
        const uint32_t left = uint32_t(packed);
        const uint32_t right = uint32_t(packed >> 32);
        std::memcpy(samples, &left, sizeof(left));
        std::memcpy(samples + 1, &right, sizeof(right));
    }

    std::unique_ptr<Slot[]> slots_;
    const uint64_t capacity_;
    const uint64_t mask_;
    std::atomic<uint64_t> published_{0};
};
