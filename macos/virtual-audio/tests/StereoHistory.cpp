#include "StereoHistory.hpp"

#include <cassert>
#include <atomic>
#include <thread>
#include <vector>

int main() {
    StereoHistory history(128);
    std::vector<float> input(128 * 2);
    for (size_t frame = 0; frame < 128; ++frame) {
        input[frame * 2] = float(frame);
        input[frame * 2 + 1] = -float(frame);
    }

    StereoHistory::Cursor first;
    StereoHistory::Cursor second;
    float empty[2] = {1, 1};
    history.read(first, empty, 1);
    assert(empty[0] == 0 && empty[1] == 0);

    history.push(input.data(), 128);
    std::vector<float> output(input.size());
    history.read(first, output.data(), 128);
    assert(output == input);
    history.read(second, output.data(), 128);
    assert(output == input);

    std::vector<float> wrapped = {1000, -1000, 1001, -1001};
    history.push(wrapped.data(), 2);
    history.read(first, output.data(), 2);
    assert(output[0] == 1000 && output[1] == -1000);
    assert(output[2] == 1001 && output[3] == -1001);

    StereoHistory::Cursor lagged;
    history.read(lagged, output.data(), 2);
    assert(output[0] == 2 && output[1] == -2);
    assert(output[2] == 3 && output[3] == -3);

    float underflow[4] = {1, 1, 1, 1};
    history.read(first, underflow, 2);
    assert(underflow[0] == 0 && underflow[1] == 0);
    assert(underflow[2] == 0 && underflow[3] == 0);

    StereoHistory concurrent(128);
    std::atomic<bool> done{false};
    auto reader = [&] {
        StereoHistory::Cursor cursor;
        float frames[64 * 2];
        do {
            concurrent.read(cursor, frames, 64);
            for (size_t i = 0; i < 64; ++i) {
                assert(frames[i * 2] == -frames[i * 2 + 1]);
            }
        } while (!done.load());
    };
    std::thread firstReader(reader);
    std::thread secondReader(reader);
    std::thread writer([&] {
        for (size_t i = 1; i <= 10000; ++i) {
            float pair[2] = {float(i), -float(i)};
            concurrent.push(pair, 1);
        }
        done.store(true);
    });
    writer.join();
    firstReader.join();
    secondReader.join();
}
