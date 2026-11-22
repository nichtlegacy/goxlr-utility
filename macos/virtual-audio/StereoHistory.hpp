#pragma once

#include <algorithm>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <memory>
#include <stdexcept>

// One real-time writer and any number of readers, using cursors or absolute frame positions.
class StereoHistory {
public:
    struct Cursor {
        uint64_t nextFrame = 0;
    };

    explicit StereoHistory(size_t capacityFrames, size_t channels = 2)
        : slots_(new Slot[capacityFrames]), capacity_(capacityFrames), mask_(capacityFrames - 1),
          channels_(channels) {
        if (capacityFrames < 2 || (capacityFrames & mask_) != 0) {
            throw std::invalid_argument("history capacity must be a power of two");
        }
        if (channels < 1 || channels > 2) {
            throw std::invalid_argument("history channels must be one or two");
        }
    }

    uint64_t publishedFrames() const {
        return published_.load(std::memory_order_acquire);
    }

    void push(const float* interleaved, size_t frames) {
        writeAt(published_.load(std::memory_order_relaxed), interleaved, frames);
    }

    void writeAt(uint64_t first, const float* interleaved, size_t frames) {
        const uint64_t published = published_.load(std::memory_order_relaxed);
        const uint64_t end = first + frames;
        const uint64_t oldest = oldestFrame(std::max(published, end));
        const size_t skip = first < oldest ? std::min<uint64_t>(oldest - first, frames) : 0;
        for (size_t i = skip; i < frames; ++i) {
            const uint64_t frame = first + i;
            Slot& slot = slots_[frame & mask_];
            slot.generation.store(frame * 2 + 1, std::memory_order_seq_cst);
            slot.samples.store(pack(interleaved + i * channels_, channels_), std::memory_order_seq_cst);
            slot.generation.store(frame * 2 + 2, std::memory_order_seq_cst);
        }
        published_.store(std::max(published, end), std::memory_order_release);
    }

    void read(Cursor& cursor, float* interleaved, size_t frames) const {
        const uint64_t published = publishedFrames();
        cursor.nextFrame = std::max(cursor.nextFrame, oldestFrame(published));
        readFrames(cursor.nextFrame, interleaved, frames, published);
        if (cursor.nextFrame < published) {
            cursor.nextFrame += std::min<uint64_t>(frames, published - cursor.nextFrame);
        }
    }

    void readAt(uint64_t first, float* interleaved, size_t frames) const {
        readFrames(first, interleaved, frames, publishedFrames());
    }

private:
    uint64_t oldestFrame(uint64_t published) const {
        return published > capacity_ ? published - capacity_ : 0;
    }

    void readFrames(uint64_t first, float* interleaved, size_t frames, uint64_t published) const {
        const uint64_t oldest = oldestFrame(published);
        for (size_t i = 0; i < frames; ++i) {
            const uint64_t frame = first + i;
            float* output = interleaved + i * channels_;
            if (frame < oldest || frame >= published) {
                output[0] = 0;
                if (channels_ == 2) output[1] = 0;
                continue;
            }

            const Slot& slot = slots_[frame & mask_];
            const uint64_t expected = frame * 2 + 2;
            const uint64_t before = slot.generation.load(std::memory_order_seq_cst);
            const uint64_t samples = slot.samples.load(std::memory_order_seq_cst);
            const uint64_t after = slot.generation.load(std::memory_order_seq_cst);
            if (before == expected && after == expected) {
                unpack(samples, output, channels_);
            } else {
                output[0] = 0;
                if (channels_ == 2) output[1] = 0;
            }
        }
    }

    struct Slot {
        std::atomic<uint64_t> generation{0};
        std::atomic<uint64_t> samples{0};
    };

    static uint64_t pack(const float* samples, size_t channels = 2) {
        uint32_t left;
        std::memcpy(&left, samples, sizeof(left));
        uint32_t right = 0;
        if (channels == 2) std::memcpy(&right, samples + 1, sizeof(right));
        return uint64_t(left) | (uint64_t(right) << 32);
    }

    static void unpack(uint64_t packed, float* samples, size_t channels) {
        const uint32_t left = uint32_t(packed);
        std::memcpy(samples, &left, sizeof(left));
        if (channels == 2) {
            const uint32_t right = uint32_t(packed >> 32);
            std::memcpy(samples + 1, &right, sizeof(right));
        }
    }

    std::unique_ptr<Slot[]> slots_;
    const uint64_t capacity_;
    const uint64_t mask_;
    const size_t channels_;
    std::atomic<uint64_t> published_{0};
};
