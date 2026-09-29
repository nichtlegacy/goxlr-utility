#include "AppRouting.hpp"
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
#include <mutex>
#include <optional>
#include <string>

namespace {

constexpr UInt32 kSampleRate = 48000;
constexpr size_t kHistoryFrames = 2048;
constexpr UInt32 kRouteMaskSelector = 0x67787274; // 'gxrt'
constexpr UInt32 kAppRulesSelector = 0x67786170; // 'gxap'
constexpr UInt32 kAppLevelsSelector = 0x67786c76; // 'gxlv'
constexpr UInt32 kRouteMaskAll = (1u << 17) - 1;
constexpr UInt32 kDefaultRouteMask = 0xF002;
constexpr size_t kRouteCount = 17;
// Playback routes (System, Game, Chat, Music, Sample) follow the 12 capture routes.
constexpr size_t kFirstPlaybackRoute = 12;

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
        : aspl::Plugin(std::move(context)), router_(std::make_shared<app_routing::Router>()) {
        RegisterCustomProperty(kRouteMaskSelector, *this,
                               &RoutePlugin::GetRouteMask, &RoutePlugin::SetRouteMask);
        RegisterCustomProperty(kAppRulesSelector, *this,
                               &RoutePlugin::GetAppRules, &RoutePlugin::SetAppRules);
        RegisterCustomProperty(kAppLevelsSelector, *this, &RoutePlugin::GetAppLevels);
    }

    const std::shared_ptr<app_routing::Router>& router() const { return router_; }

    // Bit n is set when playback route n is enabled, and so bridged to the GoXLR.
    UInt32 enabledPlaybackRoutes() const {
        return routeMask_.load(std::memory_order_acquire) >> kFirstPlaybackRoute;
    }

    CFStringRef GetAppRules() const {
        std::lock_guard lock(appRulesMutex_);
        return CFStringCreateWithCString(kCFAllocatorDefault, appRules_.c_str(),
                                         kCFStringEncodingASCII);
    }

    void SetAppRules(CFStringRef value) {
        if (!value) return;
        const CFIndex length = CFStringGetMaximumSizeForEncoding(CFStringGetLength(value),
                                                                 kCFStringEncodingASCII) + 1;
        std::string text(size_t(length), '\0');
        if (!CFStringGetCString(value, text.data(), length, kCFStringEncodingASCII)) return;
        text.resize(std::char_traits<char>::length(text.c_str()));

        auto rules = app_routing::parseRules(text);
        if (!rules) return;
        {
            std::lock_guard lock(appRulesMutex_);
            if (appRules_ == text) return;
            appRules_ = text;
        }
        router_->setRules(*rules);
        NotifyPropertyChanged(kAppRulesSelector);
    }

    // Read-only. Each read returns the peaks since the previous read and resets them.
    CFStringRef GetAppLevels() const {
        const std::string levels = router_->takeLevels();
        return CFStringCreateWithCString(kCFAllocatorDefault, levels.c_str(),
                                         kCFStringEncodingASCII);
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
    std::shared_ptr<app_routing::Router> router_;
    mutable std::mutex appRulesMutex_;
    std::string appRules_;
};

class AudioClient final : public aspl::Client {
public:
    AudioClient(const aspl::ClientInfo& info, uint64_t firstFrame,
                app_routing::Router::Cursors injected)
        : aspl::Client(info), cursor{firstFrame}, injected(injected) {}

    StereoHistory::Cursor cursor;
    app_routing::Router::Cursors injected;
};

class LoopbackHandler final : public aspl::ControlRequestHandler, public aspl::IORequestHandler {
public:
    // `playbackRoute` is the route's index among the playback routes, or nothing for capture.
    LoopbackHandler(std::shared_ptr<StereoHistory> history, UInt32 channels,
                    std::shared_ptr<app_routing::Router> router,
                    std::optional<size_t> playbackRoute)
        : history_(std::move(history)), channels_(channels), router_(std::move(router)),
          playbackRoute_(playbackRoute) {}

    std::shared_ptr<aspl::Client> OnAddClient(const aspl::ClientInfo& info) override {
        auto injected = playbackRoute_ ? router_->cursorsFor(*playbackRoute_)
                                       : app_routing::Router::Cursors{};
        return std::make_shared<AudioClient>(info, history_->publishedFrames(), injected);
    }

    void OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>&,
                            Float64, Float64 timestamp, const void* bytes,
                            UInt32 bytesCount) override {
        history_->push(static_cast<const float*>(bytes), bytesCount / (channels_ * sizeof(float)));
        if (playbackRoute_) {
            router_->flush(*playbackRoute_, timestamp);
        }
    }

    void OnReadClientInput(const std::shared_ptr<aspl::Client>& client,
                           const std::shared_ptr<aspl::Stream>&,
                           Float64, Float64, void* bytes, UInt32 bytesCount) override {
        auto audioClient = std::static_pointer_cast<AudioClient>(client);
        const UInt32 frames = bytesCount / (channels_ * sizeof(float));
        history_->read(audioClient->cursor, static_cast<float*>(bytes), frames);
        // Only a playback route's bridge reads input from this handler, and the HAL serves its
        // clients one at a time on the device's IO thread, so the scratch buffer isn't shared.
        if (playbackRoute_) {
            router_->mixInjected(*playbackRoute_, audioClient->injected,
                                 static_cast<float*>(bytes), frames, injectedScratch_.data());
        }
    }

private:
    std::shared_ptr<StereoHistory> history_;
    const UInt32 channels_;
    std::shared_ptr<app_routing::Router> router_;
    const std::optional<size_t> playbackRoute_;
    std::array<float, app_routing::kMaxFrames * 2> injectedScratch_{};
};

// macOS refuses Voice Isolation on virtual input devices and then reconfigures every input
// device. Apps such as Discord answer by negotiating again, which loops and stalls coreaudiod.
// These devices carry the GoXLR's USB audio, so report that transport instead.
class GoXLRDevice final : public aspl::Device {
public:
    using aspl::Device::Device;

    UInt32 GetTransportType() const override { return kAudioDeviceTransportTypeUSB; }

    // Visible playback devices ask the HAL for each client's samples before it mixes them, so
    // per-app rules can scale them or move them to another route.
    void EnableAppRouting(std::shared_ptr<RoutePlugin> plugin, size_t playbackRoute) {
        plugin_ = std::move(plugin);
        playbackRoute_ = playbackRoute;
    }

protected:
    OSStatus WillDoIOOperationImpl(UInt32 clientID, UInt32 operationID, Boolean* outWillDo,
                                   Boolean* outWillDoInPlace) override {
        if (plugin_ && operationID == kAudioServerPlugInIOOperationProcessOutput) {
            *outWillDo = true;
            *outWillDoInPlace = true;
            return kAudioHardwareNoError;
        }
        return aspl::Device::WillDoIOOperationImpl(clientID, operationID, outWillDo,
                                                   outWillDoInPlace);
    }

    OSStatus DoIOOperationImpl(AudioObjectID streamID, UInt32 clientID, UInt32 operationID,
                               UInt32 ioFrameCount, const AudioServerPlugInIOCycleInfo* ioCycleInfo,
                               void* ioMainBuffer, void* ioSecondaryBuffer) override {
        if (plugin_ && operationID == kAudioServerPlugInIOOperationProcessOutput) {
            if (auto client = GetClientByID(clientID)) {
                plugin_->router()->processClientOutput(
                    playbackRoute_, client->GetProcessID(), static_cast<Float32*>(ioMainBuffer),
                    ioFrameCount, ioCycleInfo->mOutputTime.mSampleTime,
                    plugin_->enabledPlaybackRoutes());
            }
            return kAudioHardwareNoError;
        }
        return aspl::Device::DoIOOperationImpl(streamID, clientID, operationID, ioFrameCount,
                                               ioCycleInfo, ioMainBuffer, ioSecondaryBuffer);
    }

private:
    std::shared_ptr<RoutePlugin> plugin_;
    size_t playbackRoute_ = 0;
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

std::shared_ptr<GoXLRDevice> addDevice(const std::shared_ptr<aspl::Context>& context,
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

    auto device = std::make_shared<GoXLRDevice>(context, params);
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
        const auto playbackRoute = index >= kFirstPlaybackRoute
                                       ? std::optional<size_t>(index - kFirstPlaybackRoute)
                                       : std::nullopt;
        auto handler = std::make_shared<LoopbackHandler>(
            std::make_shared<StereoHistory>(kHistoryFrames, route.channels), route.channels,
            plugin->router(), playbackRoute);
        const bool visibleByDefault = (kDefaultRouteMask & (1u << index)) != 0;
        auto visibleDevice = addDevice(context, plugin, std::string("GoXLR ") + route.name,
                                       route.uid, route.visibleDirection, route.channels,
                                       !visibleByDefault, handler);
        if (playbackRoute) {
            visibleDevice->EnableAppRouting(plugin, *playbackRoute);
        }
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
