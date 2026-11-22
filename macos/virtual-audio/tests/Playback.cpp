// Exercise the real driver handlers without loading a HAL plug-in into coreaudiod.
#include "../Driver.cpp"

#include <cassert>
#include <cstdlib>

namespace {

std::shared_ptr<aspl::Device> findDevice(const aspl::Plugin& plugin, const std::string& uid) {
    for (UInt32 i = 0; i < plugin.GetDeviceCount(); ++i) {
        auto device = plugin.GetDeviceByIndex(i);
        if (device->GetDeviceUID() == uid) return device;
    }
    std::abort();
}

void checkPlayback(bool withBackground) {
    constexpr UInt32 block = 512;
    constexpr UInt32 blocks = 17;
    constexpr UInt32 lastBlock = 193;

    const auto driver = createDriver();
    const auto plugin = driver->GetPlugin();
    const auto output = findDevice(*plugin, "GoXLRVirtual::System");
    const auto input = findDevice(*plugin, "GoXLRVirtual::System::Bridge");
    const auto outputStream = output->GetStreamByIndex(aspl::Direction::Output, 0);
    const auto inputStream = input->GetStreamByIndex(aspl::Direction::Input, 0);

    const AudioServerPlugInClientInfo bridge{1, 10, true, CFSTR("test.bridge")};
    const AudioServerPlugInClientInfo background{2, 20, true, CFSTR("test.background")};
    const AudioServerPlugInClientInfo sound{3, 30, true, CFSTR("test.short-sound")};
    assert(input->AddClient(input->GetID(), &bridge) == noErr);
    assert(input->StartIO(input->GetID(), bridge.mClientID) == noErr);

    AudioServerPlugInIOCycleInfo cycle{};
    const auto io = [&](aspl::Device& device, const aspl::Stream& stream, UInt32 client,
                        UInt32 operation, UInt32 frames, float* buffer) {
        assert(device.DoIOOperation(device.GetID(), stream.GetID(),
            client, operation, frames, &cycle, buffer, nullptr) == noErr);
    };
    std::array<float, block * 2> written{}, read{};
    if (withBackground) {
        assert(output->AddClient(output->GetID(), &background) == noErr);
        assert(output->StartIO(output->GetID(), background.mClientID) == noErr);
        written.fill(0.125f);
        io(*output, *outputStream, background.mClientID, kAudioServerPlugInIOOperationWriteMix,
           block, written.data());
    }
    io(*input, *inputStream, bridge.mClientID, kAudioServerPlugInIOOperationReadInput,
       block, read.data());
    assert(read == written);

    assert(output->AddClient(output->GetID(), &sound) == noErr);
    assert(output->StartIO(output->GetID(), sound.mClientID) == noErr);
    for (UInt32 i = 0; i < blocks; ++i) {
        const bool last = i + 1 == blocks;
        const UInt32 frames = last ? lastBlock : block;
        cycle.mOutputTime.mSampleTime = 8192 + i * block;
        cycle.mInputTime.mSampleTime = 16384 + i * block;
        written.fill(0.0f);
        for (UInt32 sample = 0; sample < frames * 2; ++sample) {
            written[sample] = float(1 + i * block * 2 + sample) / 65536.0f;
        }
        const auto expected = written;
        io(*output, *outputStream, sound.mClientID, kAudioServerPlugInIOOperationProcessOutput,
           frames, written.data());
        assert(written == expected);

        if (withBackground) {
            // CoreAudio supplies the mix to WriteMix. Model the already-running client's tone.
            for (UInt32 sample = 0; sample < frames * 2; ++sample) written[sample] += 0.125f;
        }
        io(*output, *outputStream, sound.mClientID, kAudioServerPlugInIOOperationWriteMix,
           frames, written.data());
        if (last) {
            // Frames already written must outlive the source client.
            assert(output->StopIO(output->GetID(), sound.mClientID) == noErr);
            assert(output->RemoveClient(output->GetID(), &sound) == noErr);
        }
        io(*input, *inputStream, bridge.mClientID, kAudioServerPlugInIOOperationReadInput,
           block, read.data());
        assert(read == written);
    }

    if (withBackground) {
        assert(output->StopIO(output->GetID(), background.mClientID) == noErr);
        assert(output->RemoveClient(output->GetID(), &background) == noErr);
    }
    assert(input->StopIO(input->GetID(), bridge.mClientID) == noErr);
    assert(input->RemoveClient(input->GetID(), &bridge) == noErr);
}

} // namespace

int main() {
    checkPlayback(false);
    checkPlayback(true);
}
