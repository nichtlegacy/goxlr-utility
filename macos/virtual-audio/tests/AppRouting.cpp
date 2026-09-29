#include "AppRouting.hpp"

#include <cassert>
#include <vector>

using namespace app_routing;

namespace {

std::vector<float> constant(uint32_t frames, float left, float right) {
    std::vector<float> samples(frames * 2);
    for (uint32_t i = 0; i < frames; ++i) {
        samples[i * 2] = left;
        samples[i * 2 + 1] = right;
    }
    return samples;
}

constexpr uint32_t kAllRoutes = (1u << kRoutes) - 1;

} // namespace

int main() {
    // Parsing.
    auto rules = parseRules("101:2:500;202:-:1000;");
    assert(rules && rules->size() == 2);
    assert((*rules)[0].pid == 101 && (*rules)[0].route == 2 && (*rules)[0].gain == 500);
    assert((*rules)[1].route == kKeepRoute && (*rules)[1].gain == kUnityGain);
    assert(parseRules("")->empty());
    assert(!parseRules("101:5:500"));
    assert(!parseRules("101:1:1001"));
    assert(!parseRules("0:1:500"));
    assert(!parseRules("abc:1:500"));
    assert(!parseRules("101:1"));

    // Rule table lookup, including shrinking.
    RuleTable table;
    table.apply(*rules);
    uint32_t route = 0, gain = 0;
    assert(table.lookup(202, route, gain) && route == kKeepRoute && gain == kUnityGain);
    assert(!table.lookup(303, route, gain));
    table.apply({{202, 1, 250}});
    assert(!table.lookup(101, route, gain));
    assert(table.lookup(202, route, gain) && route == 1 && gain == 250);

    // Clients without a rule, or kept on their route, stay in the mix (scaled by their gain).
    Router router;
    router.setRules({{10, kKeepRoute, 500}, {20, 2, 1000}, {30, 2, 500}, {40, 3, 1000}});
    auto untouched = constant(4, 1, -1);
    router.processClientOutput(0, 99, untouched.data(), 4, 0, kAllRoutes);
    assert(untouched == constant(4, 1, -1));
    auto quieter = constant(4, 1, -1);
    router.processClientOutput(0, 10, quieter.data(), 4, 0, kAllRoutes);
    assert(quieter == constant(4, 0.5f, -0.5f));

    // Two clients moved from System (0) to Chat (2) in one cycle are summed and removed from
    // System's mix; Chat's reader sees them once published.
    auto chatCursors = router.cursorsFor(2);
    auto first = constant(4, 1, -1);
    auto second = constant(4, 1, -1);
    router.processClientOutput(0, 20, first.data(), 4, 128, kAllRoutes);
    router.processClientOutput(0, 30, second.data(), 4, 128, kAllRoutes);
    assert(first == constant(4, 0, 0) && second == constant(4, 0, 0));

    std::vector<float> scratch(kMaxFrames * 2);
    auto chat = constant(4, 0.25f, 0.25f);
    router.mixInjected(2, chatCursors, chat.data(), 4, scratch.data());
    assert(chat == constant(4, 0.25f, 0.25f)); // nothing published yet

    router.flush(0, 128);
    router.mixInjected(2, chatCursors, chat.data(), 4, scratch.data());
    assert(chat == constant(4, 1.75f, -1.25f));

    // Other routes don't see Chat's injection.
    auto musicCursors = router.cursorsFor(3);
    auto music = constant(4, 0, 0);
    router.mixInjected(3, musicCursors, music.data(), 4, scratch.data());
    assert(music == constant(4, 0, 0));

    // A stale scratch buffer (flushed for another cycle) is dropped, not published.
    auto stale = constant(4, 1, 1);
    router.processClientOutput(0, 40, stale.data(), 4, 256, kAllRoutes);
    router.flush(0, 512);
    router.mixInjected(3, musicCursors, music.data(), 4, scratch.data());
    assert(music == constant(4, 0, 0));

    // A disabled target route leaves the client where it is.
    auto kept = constant(4, 1, 1);
    router.processClientOutput(0, 40, kept.data(), 4, 768, kAllRoutes & ~(1u << 3));
    assert(kept == constant(4, 1, 1));

    // A reader that fell far behind skips ahead instead of replaying old audio.
    Router lagging;
    lagging.setRules({{50, 1, 1000}});
    auto gameCursors = lagging.cursorsFor(1);
    for (uint32_t cycle = 0; cycle < 12; ++cycle) {
        auto block = constant(256, float(cycle), 0);
        lagging.processClientOutput(0, 50, block.data(), 256, cycle * 256.0, kAllRoutes);
        lagging.flush(0, cycle * 256.0);
    }
    auto game = constant(4, 0, 0);
    lagging.mixInjected(1, gameCursors, game.data(), 4, scratch.data());
    assert(gameCursors.fromRoute[0].nextFrame == 12 * 256 - kResyncLag + 4);
    assert(game[0] == 11.0f);

    return 0;
}
