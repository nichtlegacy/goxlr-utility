#include "StereoHistory.hpp"

#include <aspl/ControlRequestHandler.hpp>
#include <aspl/Device.hpp>
#include <aspl/Driver.hpp>
#include <aspl/IORequestHandler.hpp>
#include <aspl/Plugin.hpp>
#include <aspl/Stream.hpp>

#include <CoreAudio/AudioServerPlugIn.h>

#include <array>
#include <atomic>
#include <memory>
#include <string>

namespace {

constexpr UInt32 kSampleRate = 48000;
constexpr size_t kHistoryFrames = 2048;
constexpr UInt32 kRouteMaskSelector = 0x67787274; // 'gxrt'
constexpr UInt32 kRouteMaskAll = (1u << 17) - 1;
constexpr UInt32 kDefaultRouteMask = 0xF002;
constexpr size_t kRouteCount = 17;

bool parseRouteMask(CFStringRef value, UInt32& mask) {
    if (!value) return false;
    char text[9];
    if (!CFStringGetCString(value, text, sizeof(text), kCFStringEncodingASCII)) return false;

    const size_t length = std::char_traits<char>::length(text);
    if (length == 0 || length > 8) return false;

    UInt32 parsed = 0;
    for (size_t i = 0; i < length; ++i) {
        const char c = text[i];
        UInt32 digit;
        if (c >= '0' && c <= '9') digit = UInt32(c - '0');
        else if (c >= 'a' && c <= 'f') digit = UInt32(c - 'a' + 10);
        else if (c >= 'A' && c <= 'F') digit = UInt32(c - 'A' + 10);
        else return false;
        parsed = (parsed << 4) | digit;
    }
    if ((parsed & ~kRouteMaskAll) != 0) return false;
    mask = parsed;
    return true;
}

class RoutePlugin final : public aspl::Plugin {
public:
    explicit RoutePlugin(std::shared_ptr<const aspl::Context> context)
        : aspl::Plugin(std::move(context)) {
        RegisterCustomProperty(kRouteMaskSelector, *this,
                               &RoutePlugin::GetRouteMask, &RoutePlugin::SetRouteMask);
    }

    CFStringRef GetRouteMask() const {
        return CFStringCreateWithFormat(kCFAllocatorDefault, nullptr, CFSTR("%X"),
                                        routeMask_.load(std::memory_order_acquire));
    }

    void SetRouteMask(CFStringRef value) {
        UInt32 mask;
        if (!parseRouteMask(value, mask)) return;

        if (routeMask_.exchange(mask, std::memory_order_acq_rel) == mask) return;
        for (size_t i = 0; i < visibleDevices_.size(); ++i) {
            if (visibleDevices_[i]) {
                const bool enabled = (mask & (1u << i)) != 0;
                visibleDevices_[i]->SetCanBeDefaultDevice(enabled);
                visibleDevices_[i]->SetCanBeDefaultSystemDevice(enabled);
                visibleDevices_[i]->SetIsHidden(!enabled);
            }
        }
        NotifyPropertyChanged(kRouteMaskSelector);
    }

    void SetVisibleDevice(size_t index, const std::shared_ptr<aspl::Device>& device) {
        visibleDevices_[index] = device;
        device->SetIsHidden((routeMask_.load(std::memory_order_acquire) & (1u << index)) == 0);
    }

private:
    std::atomic<UInt32> routeMask_{kDefaultRouteMask};
    std::array<std::shared_ptr<aspl::Device>, kRouteCount> visibleDevices_{};
};

class AudioClient final : public aspl::Client {
public:
    AudioClient(const aspl::ClientInfo& info, uint64_t firstFrame)
        : aspl::Client(info), cursor{firstFrame} {}

    StereoHistory::Cursor cursor;
};

class LoopbackHandler final : public aspl::ControlRequestHandler, public aspl::IORequestHandler {
public:
    LoopbackHandler(std::shared_ptr<StereoHistory> history, UInt32 channels)
        : history_(std::move(history)), channels_(channels) {}

    std::shared_ptr<aspl::Client> OnAddClient(const aspl::ClientInfo& info) override {
        return std::make_shared<AudioClient>(info, history_->publishedFrames());
    }

    void OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>&,
                            Float64, Float64, const void* bytes, UInt32 bytesCount) override {
        history_->push(static_cast<const float*>(bytes), bytesCount / (channels_ * sizeof(float)));
    }

    void OnReadClientInput(const std::shared_ptr<aspl::Client>& client,
                           const std::shared_ptr<aspl::Stream>&,
                           Float64, Float64, void* bytes, UInt32 bytesCount) override {
        auto audioClient = std::static_pointer_cast<AudioClient>(client);
        history_->read(audioClient->cursor, static_cast<float*>(bytes),
                       bytesCount / (channels_ * sizeof(float)));
    }

private:
    std::shared_ptr<StereoHistory> history_;
    const UInt32 channels_;
};

aspl::StreamParameters audioStream(aspl::Direction direction, UInt32 channels) {
    aspl::StreamParameters params;
    params.Direction = direction;
    params.Format = {
        .mSampleRate = kSampleRate,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian |
                        kAudioFormatFlagIsPacked,
        .mBytesPerPacket = channels * static_cast<UInt32>(sizeof(Float32)),
        .mFramesPerPacket = 1,
        .mBytesPerFrame = channels * static_cast<UInt32>(sizeof(Float32)),
        .mChannelsPerFrame = channels,
        .mBitsPerChannel = 32,
    };
    return params;
}

std::shared_ptr<aspl::Device> addDevice(const std::shared_ptr<aspl::Context>& context,
               const std::shared_ptr<aspl::Plugin>& plugin,
               const std::string& name,
               const std::string& uid,
               aspl::Direction direction,
               UInt32 channels,
               bool hidden,
               const std::shared_ptr<LoopbackHandler>& handler) {
    aspl::DeviceParameters params;
    params.Name = name;
    params.Manufacturer = "GoXLR Utility";
    params.DeviceUID = uid;
    params.ModelUID = "GoXLRVirtual";
    params.SampleRate = kSampleRate;
    params.ChannelCount = channels;
    params.CanBeDefault = !hidden;
    params.CanBeDefaultForSystemSounds = !hidden;

    auto device = std::make_shared<aspl::Device>(context, params);
    device->AddStreamAsync(audioStream(direction, channels));
    device->SetControlHandler(handler);
    device->SetIOHandler(handler);
    if (hidden) {
        device->SetIsHidden(true);
    }
    plugin->AddDevice(device);
    return device;
}

std::shared_ptr<aspl::Driver> createDriver() {
    auto context = std::make_shared<aspl::Context>();
    auto plugin = std::make_shared<RoutePlugin>(context);

    struct Route {
        const char* name;
        const char* uid;
        aspl::Direction visibleDirection;
        aspl::Direction bridgeDirection;
        UInt32 channels;
    };
    constexpr std::array<Route, 17> routes = {{
        {"Broadcast Mix", "GoXLRVirtual::BroadcastMix", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Microphone", "GoXLRVirtual::Microphone", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Sampler Capture", "GoXLRVirtual::SamplerCapture", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Chat Mic", "GoXLRVirtual::ChatMic", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"System Capture", "GoXLRVirtual::SystemCapture", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Game Capture", "GoXLRVirtual::GameCapture", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Chat Capture", "GoXLRVirtual::ChatCapture", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Music Capture", "GoXLRVirtual::MusicCapture", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Sample Capture", "GoXLRVirtual::SampleCapture", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Line-In", "GoXLRVirtual::LineIn", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Console", "GoXLRVirtual::Console", aspl::Direction::Input, aspl::Direction::Output, 2},
        {"Dry Mic", "GoXLRVirtual::DryMic", aspl::Direction::Input, aspl::Direction::Output, 1},
        {"System", "GoXLRVirtual::System", aspl::Direction::Output, aspl::Direction::Input, 2},
        {"Game", "GoXLRVirtual::Game", aspl::Direction::Output, aspl::Direction::Input, 2},
        {"Chat", "GoXLRVirtual::Chat", aspl::Direction::Output, aspl::Direction::Input, 2},
        {"Music", "GoXLRVirtual::Music", aspl::Direction::Output, aspl::Direction::Input, 2},
        {"Sample", "GoXLRVirtual::Sample", aspl::Direction::Output, aspl::Direction::Input, 2},
    }};

    for (size_t index = 0; index < routes.size(); ++index) {
        const auto& route = routes[index];
        auto handler = std::make_shared<LoopbackHandler>(
            std::make_shared<StereoHistory>(kHistoryFrames, route.channels), route.channels);
        const bool visibleByDefault = (kDefaultRouteMask & (1u << index)) != 0;
        auto visibleDevice = addDevice(context, plugin, std::string("GoXLR ") + route.name,
                                       route.uid, route.visibleDirection, route.channels,
                                       !visibleByDefault, handler);
        plugin->SetVisibleDevice(index, visibleDevice);
        addDevice(context, plugin, std::string("GoXLR ") + route.name + " Bridge",
                  std::string(route.uid) + "::Bridge", route.bridgeDirection, route.channels, true, handler);
    }
    return std::make_shared<aspl::Driver>(context, plugin);
}

} // namespace

extern "C" void* GoXLRVirtualEntryPoint(CFAllocatorRef, CFUUIDRef typeUUID) {
    if (!CFEqual(typeUUID, kAudioServerPlugInTypeUUID)) {
        return nullptr;
    }
    try {
        static auto driver = createDriver();
        return driver->GetReference();
    } catch (...) {
        return nullptr;
    }
}
