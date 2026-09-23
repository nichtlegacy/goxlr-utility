#include "StereoHistory.hpp"

#include <aspl/ControlRequestHandler.hpp>
#include <aspl/Device.hpp>
#include <aspl/Driver.hpp>
#include <aspl/IORequestHandler.hpp>
#include <aspl/Plugin.hpp>
#include <aspl/Stream.hpp>

#include <CoreAudio/AudioServerPlugIn.h>

#include <array>
#include <memory>
#include <string>

namespace {

constexpr UInt32 kSampleRate = 48000;
constexpr size_t kHistoryFrames = 2048;

class AudioClient final : public aspl::Client {
public:
    AudioClient(const aspl::ClientInfo& info, uint64_t firstFrame)
        : aspl::Client(info), cursor{firstFrame} {}

    StereoHistory::Cursor cursor;
};

class LoopbackHandler final : public aspl::ControlRequestHandler, public aspl::IORequestHandler {
public:
    explicit LoopbackHandler(std::shared_ptr<StereoHistory> history) : history_(std::move(history)) {}

    std::shared_ptr<aspl::Client> OnAddClient(const aspl::ClientInfo& info) override {
        return std::make_shared<AudioClient>(info, history_->publishedFrames());
    }

    void OnWriteMixedOutput(const std::shared_ptr<aspl::Stream>&,
                            Float64, Float64, const void* bytes, UInt32 bytesCount) override {
        history_->push(static_cast<const float*>(bytes), bytesCount / (2 * sizeof(float)));
    }

    void OnReadClientInput(const std::shared_ptr<aspl::Client>& client,
                           const std::shared_ptr<aspl::Stream>&,
                           Float64, Float64, void* bytes, UInt32 bytesCount) override {
        auto audioClient = std::static_pointer_cast<AudioClient>(client);
        history_->read(audioClient->cursor, static_cast<float*>(bytes),
                       bytesCount / (2 * sizeof(float)));
    }

private:
    std::shared_ptr<StereoHistory> history_;
};

aspl::StreamParameters stereoStream(aspl::Direction direction) {
    aspl::StreamParameters params;
    params.Direction = direction;
    params.Format = {
        .mSampleRate = kSampleRate,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagsNativeEndian |
                        kAudioFormatFlagIsPacked,
        .mBytesPerPacket = 2 * sizeof(Float32),
        .mFramesPerPacket = 1,
        .mBytesPerFrame = 2 * sizeof(Float32),
        .mChannelsPerFrame = 2,
        .mBitsPerChannel = 32,
    };
    return params;
}

void addDevice(const std::shared_ptr<aspl::Context>& context,
               const std::shared_ptr<aspl::Plugin>& plugin,
               const std::string& name,
               const std::string& uid,
               aspl::Direction direction,
               bool hidden,
               const std::shared_ptr<LoopbackHandler>& handler) {
    aspl::DeviceParameters params;
    params.Name = name;
    params.Manufacturer = "GoXLR Utility";
    params.DeviceUID = uid;
    params.ModelUID = "GoXLRVirtual";
    params.SampleRate = kSampleRate;
    params.ChannelCount = 2;
    params.CanBeDefault = !hidden;
    params.CanBeDefaultForSystemSounds = !hidden;

    auto device = std::make_shared<aspl::Device>(context, params);
    device->AddStreamAsync(stereoStream(direction));
    device->SetControlHandler(handler);
    device->SetIOHandler(handler);
    if (hidden) {
        device->SetIsHidden(true);
    }
    plugin->AddDevice(device);
}

std::shared_ptr<aspl::Driver> createDriver() {
    auto context = std::make_shared<aspl::Context>();
    auto plugin = std::make_shared<aspl::Plugin>(context);

    struct Route {
        const char* name;
        const char* uid;
        aspl::Direction visibleDirection;
        aspl::Direction bridgeDirection;
    };
    constexpr std::array<Route, 3> routes = {{
        {"Microphone", "GoXLRVirtual::Microphone", aspl::Direction::Input, aspl::Direction::Output},
        {"Chat", "GoXLRVirtual::Chat", aspl::Direction::Output, aspl::Direction::Input},
        {"Music", "GoXLRVirtual::Music", aspl::Direction::Output, aspl::Direction::Input},
    }};

    for (const auto& route : routes) {
        auto handler = std::make_shared<LoopbackHandler>(
            std::make_shared<StereoHistory>(kHistoryFrames));
        addDevice(context, plugin, std::string("GoXLR ") + route.name, route.uid,
                  route.visibleDirection, false, handler);
        addDevice(context, plugin, std::string("GoXLR ") + route.name + " Bridge",
                  std::string(route.uid) + "::Bridge", route.bridgeDirection, true, handler);
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
