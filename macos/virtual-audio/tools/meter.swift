import AVFoundation
import CoreAudio
import Foundation

func fail(_ message: String) -> Never {
    fputs("\(message)\n", stderr)
    exit(1)
}

func checked(_ status: OSStatus, _ operation: String) {
    if status != noErr { fail("\(operation): CoreAudio status \(status)") }
}

func property<T>(_ id: AudioObjectID, _ selector: AudioObjectPropertySelector,
                 _ value: inout T) -> OSStatus {
    var address = AudioObjectPropertyAddress(mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
    var size = UInt32(MemoryLayout<T>.size)
    return withUnsafeMutablePointer(to: &value) {
        AudioObjectGetPropertyData(id, &address, 0, nil, &size, $0)
    }
}

func objectList(_ id: AudioObjectID, _ selector: AudioObjectPropertySelector,
                _ scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal) -> [AudioObjectID] {
    var address = AudioObjectPropertyAddress(mSelector: selector, mScope: scope,
        mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(id, &address, 0, nil, &size) == noErr, size > 0 else {
        return []
    }
    var objects = [AudioObjectID](repeating: 0, count: Int(size) / 4)
    guard AudioObjectGetPropertyData(id, &address, 0, nil, &size, &objects) == noErr else {
        return []
    }
    return Array(objects.prefix(Int(size) / 4))
}

func textProperty(_ id: AudioObjectID, _ selector: AudioObjectPropertySelector) -> String {
    var value = "" as CFString
    _ = property(id, selector, &value)
    return value as String
}

func inputChannels(_ id: AudioObjectID) -> Int {
    var address = AudioObjectPropertyAddress(mSelector: kAudioDevicePropertyStreamConfiguration,
        mScope: kAudioObjectPropertyScopeInput, mElement: kAudioObjectPropertyElementMain)
    var size: UInt32 = 0
    guard AudioObjectGetPropertyDataSize(id, &address, 0, nil, &size) == noErr, size > 0 else {
        return 0
    }
    let buffer = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: 16)
    defer { buffer.deallocate() }
    guard AudioObjectGetPropertyData(id, &address, 0, nil, &size, buffer) == noErr else {
        return 0
    }
    let list = UnsafeMutableAudioBufferListPointer(buffer.assumingMemoryBound(to: AudioBufferList.self))
    return list.reduce(0) { $0 + Int($1.mNumberChannels) }
}

let args = CommandLine.arguments
guard args.count == 3, let duration = Double(args[1]), (1...60).contains(duration) else {
    fail("Usage: meter <seconds from 1 to 60> <output directory>")
}
let directory = URL(fileURLWithPath: args[2], isDirectory: true)
try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)

// Bound the meter itself even if a CoreAudio start/stop call or a permission request blocks.
DispatchQueue.global().asyncAfter(deadline: .now() + duration + 15) {
    fputs("Meter timed out\n", stderr)
    _exit(124)
}

final class Capture {
    let label: String
    let id: AudioObjectID
    let channels: [Int]
    var samples: [[Float]]
    var times: [(sample: Double, frames: Int, host: UInt64)] = []
    var proc: AudioDeviceIOProcID?

    init(_ label: String, _ id: AudioObjectID, _ channels: [Int], duration: Double) {
        self.label = label
        self.id = id
        self.channels = channels
        samples = channels.map { _ in [] }
        for i in samples.indices {
            samples[i].reserveCapacity(Int((duration + 1) * 48000))
        }
        times.reserveCapacity(Int((duration + 1) * 1500))
    }

    func capture(_ data: UnsafePointer<AudioBufferList>, _ timestamp: AudioTimeStamp) {
        let list = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: data))
        var channelBase = 0
        var frames = 0
        for buffer in list {
            let count = Int(buffer.mNumberChannels)
            defer { channelBase += count }
            guard count > 0, let p = buffer.mData?.assumingMemoryBound(to: Float.self) else { continue }
            frames = Int(buffer.mDataByteSize) / MemoryLayout<Float>.size / count
            for (index, channel) in channels.enumerated() where (channelBase..<(channelBase + count)).contains(channel) {
                for frame in 0..<frames {
                    samples[index].append(p[frame * count + channel - channelBase])
                }
            }
        }
        times.append((timestamp.mSampleTime, frames, timestamp.mHostTime))
    }
}

let system = AudioObjectID(kAudioObjectSystemObject)
let devices = objectList(system, kAudioHardwarePropertyDevices)
let virtualRoutes = [("DryMic", "virtual-drymic"), ("LineIn", "virtual-linein"),
                     ("Microphone", "virtual-microphone")]
guard let physical = devices.first(where: {
    textProperty($0, kAudioObjectPropertyName) == "GoXLR" && inputChannels($0) == 23
}) else { fail("Physical GoXLR Full is missing from CoreAudio") }
var captures = [Capture("physical", physical, Array(0..<23), duration: duration)]
for (route, label) in virtualRoutes {
    guard let device = devices.first(where: {
        textProperty($0, kAudioDevicePropertyDeviceUID) == "GoXLRVirtual::\(route)" && inputChannels($0) > 0
    }) else { fail("Enable GoXLR \(route) in Utility settings before measuring") }
    captures.append(Capture(label, device, [0], duration: duration))
}
for capture in captures {
    var rate: Float64 = 0
    checked(property(capture.id, kAudioDevicePropertyNominalSampleRate, &rate), "Read sample rate")
    guard rate == 48000 else { fail("\(capture.label) is running at \(rate) Hz, expected 48000") }
}

if AVCaptureDevice.authorizationStatus(for: .audio) == .notDetermined {
    let permission = DispatchSemaphore(value: 0)
    AVCaptureDevice.requestAccess(for: .audio) { _ in permission.signal() }
    permission.wait()
}
guard AVCaptureDevice.authorizationStatus(for: .audio) == .authorized else {
    fail("Allow GoXLR Meter under Privacy & Security > Microphone")
}

for capture in captures {
    checked(AudioDeviceCreateIOProcIDWithBlock(&capture.proc, capture.id, nil) { _, data, time, _, _ in
        capture.capture(data, time.pointee)
    }, "Create \(capture.label) IOProc")
    checked(AudioDeviceStart(capture.id, capture.proc), "Start \(capture.label)")
}
print("Meter PID \(getpid())")
fflush(stdout)
Thread.sleep(forTimeInterval: duration / 2)

// The process report is a snapshot taken while all meter streams are active.
if #available(macOS 14.2, *) {
    for capture in captures.dropFirst() {
        let clients = objectList(system, kAudioHardwarePropertyProcessObjectList).compactMap { process -> pid_t? in
            var active: UInt32 = 0
            var pid: pid_t = 0
            guard property(process, kAudioProcessPropertyIsRunningInput, &active) == noErr, active != 0,
                objectList(process, kAudioProcessPropertyDevices, kAudioObjectPropertyScopeInput)
                    .contains(capture.id),
                property(process, kAudioProcessPropertyPID, &pid) == noErr else { return nil }
            return pid
        }
        print("\(capture.label) input PIDs: \(clients.sorted())")
    }
}
Thread.sleep(forTimeInterval: duration / 2)
for capture in captures {
    checked(AudioDeviceStop(capture.id, capture.proc), "Stop \(capture.label)")
    checked(AudioDeviceDestroyIOProcID(capture.id, capture.proc!), "Destroy \(capture.label) IOProc")
}

func wav(_ samples: [Float], at path: URL) throws {
    var data = Data("RIFF".utf8)
    func append32(_ value: UInt32) {
        var le = value.littleEndian
        withUnsafeBytes(of: &le) { data.append(contentsOf: $0) }
    }
    func append16(_ value: UInt16) {
        var le = value.littleEndian
        withUnsafeBytes(of: &le) { data.append(contentsOf: $0) }
    }
    let byteCount = UInt32(samples.count * MemoryLayout<Float>.size)
    append32(36 + byteCount)
    data.append(contentsOf: "WAVEfmt ".utf8)
    append32(16)
    append16(3) // IEEE float PCM
    append16(1)
    append32(48000)
    append32(48000 * 4)
    append16(4)
    append16(32)
    data.append(contentsOf: "data".utf8)
    append32(byteCount)
    samples.withUnsafeBytes { data.append(contentsOf: $0) }
    try data.write(to: path, options: .atomic)
}

for capture in captures {
    guard capture.times.count > 1, capture.samples.allSatisfy({ !$0.isEmpty }) else {
        fail("No signal buffers received for \(capture.label)")
    }
    let gaps = zip(capture.times, capture.times.dropFirst()).filter {
        abs($1.sample - $0.sample - Double($0.frames)) > 0.5
    }.count
    let frames = Set(capture.times.map(\.frames)).sorted()
    print("\(capture.label): \(capture.samples[0].count) frames, buffers \(frames), time gaps \(gaps)")
    let csv = capture.times.map { "\($0.sample),\($0.frames),\($0.host)" }.joined(separator: "\n")
    try csv.write(to: directory.appendingPathComponent("\(capture.label)-times.csv"), atomically: true, encoding: .utf8)
    for (index, channel) in capture.channels.enumerated() {
        try wav(capture.samples[index], at: directory.appendingPathComponent("\(capture.label)-ch\(channel).wav"))
    }
}
print("Capture complete")
