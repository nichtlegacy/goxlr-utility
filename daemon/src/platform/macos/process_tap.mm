#import <Foundation/Foundation.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#include <atomic>
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstring>
#include <thread>
#include <vector>

struct GoXLRProcessTap {
    AudioObjectID tap = kAudioObjectUnknown;
    AudioObjectID aggregate = kAudioObjectUnknown;
    AudioDeviceIOProcID io = nullptr;
    bool started = false;
    std::atomic<float> gain{1.0f};
    std::atomic<float> peak{0.0f};
};

static void destroy(GoXLRProcessTap *tap) {
    if (!tap) return;
    if (tap->aggregate != kAudioObjectUnknown) {
        if (tap->io) {
            if (tap->started) AudioDeviceStop(tap->aggregate, tap->io);
            AudioDeviceDestroyIOProcID(tap->aggregate, tap->io);
        }
        AudioHardwareDestroyAggregateDevice(tap->aggregate);
    }
    if (tap->tap != kAudioObjectUnknown) AudioHardwareDestroyProcessTap(tap->tap);
    delete tap;
}

// The aggregate has only one input (the stereo tap) and one stereo output.
// No allocation, Objective-C, or locks run on the real-time thread.
static OSStatus render(AudioObjectID, const AudioTimeStamp *, const AudioBufferList *input,
                       const AudioTimeStamp *, AudioBufferList *output,
                       const AudioTimeStamp *, void *context) {
    auto *tap = static_cast<GoXLRProcessTap *>(context);
    const float gain = tap->gain.load(std::memory_order_relaxed);
    float peak = 0.0f;
    for (UInt32 i = 0; i < output->mNumberBuffers; ++i) {
        AudioBuffer &out = output->mBuffers[i];
        if (!out.mData) continue;
        const AudioBuffer *in = input && i < input->mNumberBuffers ? &input->mBuffers[i] : nullptr;
        const UInt32 bytes = in && in->mData ? std::min(in->mDataByteSize, out.mDataByteSize) : 0;
        const float *source = in ? static_cast<const float *>(in->mData) : nullptr;
        float *target = static_cast<float *>(out.mData);
        for (UInt32 sample = 0; sample < bytes / sizeof(float); ++sample) {
            float value = source[sample] * gain;
            if (gain > 1.0f && std::fabs(value) > 0.8f) {
                const float magnitude = 0.8f + 0.2f * std::tanh((std::fabs(value) - 0.8f) / 0.2f);
                value = std::copysign(std::min(magnitude, 1.0f), value);
            }
            target[sample] = value;
            peak = std::max(peak, std::fabs(value));
        }
        if (bytes < out.mDataByteSize)
            std::memset(static_cast<char *>(out.mData) + bytes, 0, out.mDataByteSize - bytes);
    }
    float recorded = tap->peak.load(std::memory_order_relaxed);
    while (recorded < peak && !tap->peak.compare_exchange_weak(recorded, peak,
                                                                std::memory_order_relaxed)) {}
    return noErr;
}

static bool floatStereo(AudioObjectID object, AudioObjectPropertySelector selector,
                        AudioObjectPropertyScope scope) {
    AudioObjectPropertyAddress address{selector, scope, kAudioObjectPropertyElementMain};
    AudioStreamBasicDescription format{};
    UInt32 size = sizeof(format);
    if (AudioObjectGetPropertyData(object, &address, 0, nullptr, &size, &format) != noErr)
        return false;
    return format.mFormatID == kAudioFormatLinearPCM && format.mBitsPerChannel == 32 &&
           format.mChannelsPerFrame == 2 &&
           (format.mFormatFlags & (kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked)) ==
               (kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked) &&
           format.mBytesPerFrame == 2 * sizeof(float);
}

extern "C" GoXLRProcessTap *goxlr_process_tap_create(const int32_t *pids, size_t count,
                                                       const char *outputUID, float gain,
                                                       OSStatus *error) {
    @autoreleasepool {
        *error = kAudioHardwareBadObjectError;
        if (!pids || !count || !outputUID) return nullptr;
        AudioObjectPropertyAddress listAddress{kAudioHardwarePropertyProcessObjectList,
                                                kAudioObjectPropertyScopeGlobal,
                                                kAudioObjectPropertyElementMain};
        UInt32 size = 0;
        *error = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &listAddress, 0,
                                                 nullptr, &size);
        if (*error != noErr) return nullptr;
        NSMutableArray<NSNumber *> *processes = [NSMutableArray array];
        auto ids = std::vector<AudioObjectID>(size / sizeof(AudioObjectID));
        *error = AudioObjectGetPropertyData(kAudioObjectSystemObject, &listAddress, 0,
                                            nullptr, &size, ids.data());
        if (*error != noErr) return nullptr;
        ids.resize(size / sizeof(AudioObjectID));
        AudioObjectPropertyAddress pidAddress{kAudioProcessPropertyPID,
                                               kAudioObjectPropertyScopeGlobal,
                                               kAudioObjectPropertyElementMain};
        for (AudioObjectID id : ids) {
            pid_t pid = 0;
            UInt32 pidSize = sizeof(pid);
            if (AudioObjectGetPropertyData(id, &pidAddress, 0, nullptr, &pidSize, &pid) == noErr &&
                std::find(pids, pids + count, pid) != pids + count)
                [processes addObject:@(id)];
        }
        if (!processes.count) {
            *error = kAudioHardwareBadObjectError;
            return nullptr;
        }

        auto *tap = new GoXLRProcessTap;
        tap->gain.store(gain, std::memory_order_relaxed);
        CATapDescription *description = [[CATapDescription alloc] initStereoMixdownOfProcesses:processes];
        description.UUID = [NSUUID UUID];
        description.muteBehavior = CATapMutedWhenTapped;
        description.privateTap = YES;
        description.name = @"GoXLR app route";
        *error = AudioHardwareCreateProcessTap(description, &tap->tap);
        if (*error != noErr) { destroy(tap); return nullptr; }
        if (!floatStereo(tap->tap, kAudioTapPropertyFormat, kAudioObjectPropertyScopeGlobal)) {
            *error = kAudioHardwareUnsupportedOperationError;
            destroy(tap);
            return nullptr;
        }

        NSString *uid = [NSString stringWithUTF8String:outputUID];
        NSString *tapUID = description.UUID.UUIDString;
        // AudioDeviceStart must return even when the app is paused. The tap and output
        // device can have different clocks, so compensate their drift in the aggregate.
        NSDictionary *aggregate = @{
            @kAudioAggregateDeviceNameKey: @"GoXLR app route",
            @kAudioAggregateDeviceUIDKey: [NSUUID UUID].UUIDString,
            @kAudioAggregateDeviceMainSubDeviceKey: uid,
            @kAudioAggregateDeviceClockDeviceKey: uid,
            @kAudioAggregateDeviceIsPrivateKey: @YES,
            @kAudioAggregateDeviceTapAutoStartKey: @NO,
            @kAudioAggregateDeviceSubDeviceListKey: @[@{@kAudioSubDeviceUIDKey: uid}],
            @kAudioAggregateDeviceTapListKey: @[@{@kAudioSubTapUIDKey: tapUID,
                                                   @kAudioSubTapDriftCompensationKey: @YES}],
        };
        *error = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)aggregate,
                                                     &tap->aggregate);
        if (*error != noErr) { destroy(tap); return nullptr; }
        // Aggregate properties can appear shortly after Create returns.
        bool outputReady = false;
        for (int attempt = 0; attempt < 25; ++attempt) {
            if (floatStereo(tap->aggregate, kAudioDevicePropertyStreamFormat,
                            kAudioObjectPropertyScopeOutput)) {
                outputReady = true;
                break;
            }
            std::this_thread::sleep_for(std::chrono::milliseconds(20));
        }
        if (!outputReady) {
            *error = kAudioHardwareUnsupportedOperationError;
            destroy(tap);
            return nullptr;
        }
        *error = AudioDeviceCreateIOProcID(tap->aggregate, render, tap, &tap->io);
        if (*error != noErr) { destroy(tap); return nullptr; }
        *error = AudioDeviceStart(tap->aggregate, tap->io);
        if (*error != noErr) { destroy(tap); return nullptr; }
        tap->started = true;
        return tap;
    }
}

extern "C" void goxlr_process_tap_gain(GoXLRProcessTap *tap, float gain) {
    tap->gain.store(gain, std::memory_order_relaxed);
}

extern "C" float goxlr_process_tap_take_peak(GoXLRProcessTap *tap) {
    return tap->peak.exchange(0.0f, std::memory_order_relaxed);
}

extern "C" bool goxlr_process_tap_alive(GoXLRProcessTap *tap) {
    AudioObjectPropertyAddress address{kAudioDevicePropertyDeviceIsAlive,
                                        kAudioObjectPropertyScopeGlobal,
                                        kAudioObjectPropertyElementMain};
    UInt32 alive = 0, size = sizeof(alive);
    return AudioObjectGetPropertyData(tap->aggregate, &address, 0, nullptr, &size, &alive) == noErr && alive;
}

extern "C" void goxlr_process_tap_destroy(GoXLRProcessTap *tap) { destroy(tap); }
