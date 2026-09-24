use anyhow::Result;
use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::macos_helpers::audio_unit_from_device_id;
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, Element, SampleFormat, Scope, StreamFormat};
use coreaudio_sys::{AudioDeviceID, kAudioUnitProperty_StreamFormat};
use goxlr_usb::PID_GOXLR_FULL;
use log::{info, warn};
use rtrb::{Consumer, RingBuffer};
use std::time::Duration;
use tokio::time;

use crate::platform::macos::core_audio::{
    get_device_id_for_uid, get_goxlr_devices, set_virtual_audio_routes,
};
use crate::settings::SettingsHandle;
use crate::shutdown::Shutdown;

const RING_FRAMES: usize = 2048;
const INPUT_CHANNELS: usize = 23;
const OUTPUT_CHANNELS: usize = 10;
const CAPTURE_COUNT: usize = 12;
const PLAYBACK_COUNT: usize = 5;

#[derive(Clone, Copy)]
struct CaptureRoute {
    bridge_uid: &'static str,
    physical_channel: usize,
    channels: usize,
}

const CAPTURE_ROUTES: [CaptureRoute; CAPTURE_COUNT] = [
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::BroadcastMix::Bridge",
        physical_channel: 0,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::Microphone::Bridge",
        physical_channel: 2,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::SamplerCapture::Bridge",
        physical_channel: 4,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::ChatMic::Bridge",
        physical_channel: 6,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::SystemCapture::Bridge",
        physical_channel: 8,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::GameCapture::Bridge",
        physical_channel: 10,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::ChatCapture::Bridge",
        physical_channel: 12,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::MusicCapture::Bridge",
        physical_channel: 14,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::SampleCapture::Bridge",
        physical_channel: 16,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::LineIn::Bridge",
        physical_channel: 18,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::Console::Bridge",
        physical_channel: 20,
        channels: 2,
    },
    CaptureRoute {
        bridge_uid: "GoXLRVirtual::DryMic::Bridge",
        physical_channel: 22,
        channels: 1,
    },
];

#[derive(Clone, Copy)]
struct PlaybackRoute {
    bridge_uid: &'static str,
    physical_channel: usize,
}

const PLAYBACK_ROUTES: [PlaybackRoute; PLAYBACK_COUNT] = [
    PlaybackRoute {
        bridge_uid: "GoXLRVirtual::System::Bridge",
        physical_channel: 0,
    },
    PlaybackRoute {
        bridge_uid: "GoXLRVirtual::Game::Bridge",
        physical_channel: 2,
    },
    PlaybackRoute {
        bridge_uid: "GoXLRVirtual::Chat::Bridge",
        physical_channel: 4,
    },
    PlaybackRoute {
        bridge_uid: "GoXLRVirtual::Music::Bridge",
        physical_channel: 6,
    },
    PlaybackRoute {
        bridge_uid: "GoXLRVirtual::Sample::Bridge",
        physical_channel: 8,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
struct VirtualIds {
    captures: [AudioDeviceID; CAPTURE_COUNT],
    playbacks: [AudioDeviceID; PLAYBACK_COUNT],
}

type Stereo = [f32; 2];
type AudioArgs = render_callback::Args<data::Interleaved<f32>>;

fn stream_format(channels: usize) -> StreamFormat {
    StreamFormat {
        sample_rate: 48_000.0,
        sample_format: SampleFormat::F32,
        flags: LinearPcmFlags::IS_FLOAT | LinearPcmFlags::IS_PACKED,
        channels: channels as u32,
    }
}

fn input_unit(id: AudioDeviceID, channels: usize) -> Result<AudioUnit> {
    let mut unit = audio_unit_from_device_id(id, true)?;
    unit.set_property(
        kAudioUnitProperty_StreamFormat,
        Scope::Output,
        Element::Input,
        Some(&stream_format(channels).to_asbd()),
    )?;
    Ok(unit)
}

fn output_unit(id: AudioDeviceID, channels: usize) -> Result<AudioUnit> {
    let mut unit = audio_unit_from_device_id(id, false)?;
    unit.set_property(
        kAudioUnitProperty_StreamFormat,
        Scope::Input,
        Element::Output,
        Some(&stream_format(channels).to_asbd()),
    )?;
    Ok(unit)
}

fn capture_pair(frame: &[f32], route: CaptureRoute) -> Stereo {
    [
        frame[route.physical_channel],
        if route.channels == 2 {
            frame[route.physical_channel + 1]
        } else {
            0.0
        },
    ]
}

fn set_pair(frame: &mut [f32], left: usize, pair: Stereo) {
    frame[left] = pair[0];
    frame[left + 1] = pair[1];
}

// The GoXLR and virtual devices have independent clocks. Keep a small backlog
// and interpolate at a slightly adjusted rate instead of periodically losing a
// whole audio block when their clocks drift apart.
struct StereoReader {
    queue: Consumer<Stereo>,
    current: Stereo,
    next: Stereo,
    phase: f32,
    started: bool,
}

impl StereoReader {
    fn new(queue: Consumer<Stereo>) -> Self {
        Self {
            queue,
            current: [0.0; 2],
            next: [0.0; 2],
            phase: 0.0,
            started: false,
        }
    }

    fn fill(&mut self, output: &mut [f32], channels: usize) {
        const TARGET: usize = 512;
        if !self.started {
            if self.queue.slots() < TARGET {
                output.fill(0.0);
                return;
            }
            self.current = self.queue.pop().unwrap_or([0.0; 2]);
            self.next = self.queue.pop().unwrap_or(self.current);
            self.started = true;
        }

        let backlog = self.queue.slots() as f32;
        let rate = (1.0 + (backlog - TARGET as f32) / TARGET as f32 * 0.002).clamp(0.998, 1.002);
        let mut frames = output.chunks_exact_mut(channels);
        while let Some(frame) = frames.next() {
            frame[0] = self.current[0] + (self.next[0] - self.current[0]) * self.phase;
            if channels == 2 {
                frame[1] = self.current[1] + (self.next[1] - self.current[1]) * self.phase;
            }
            self.phase += rate;
            while self.phase >= 1.0 {
                self.phase -= 1.0;
                self.current = self.next;
                let Ok(next) = self.queue.pop() else {
                    self.started = false;
                    self.current = [0.0; 2];
                    self.next = [0.0; 2];
                    self.phase = 0.0;
                    for frame in frames {
                        frame.fill(0.0);
                    }
                    return;
                };
                self.next = next;
            }
        }
    }
}

struct Bridge {
    physical_uid: String,
    physical_id: AudioDeviceID,
    routes: u32,
    units: Vec<AudioUnit>,
}

impl Bridge {
    fn start(
        physical_uid: String,
        physical: AudioDeviceID,
        virtual_ids: VirtualIds,
        routes: u32,
    ) -> Result<Self> {
        let mut physical_input = input_unit(physical, INPUT_CHANNELS)?;
        let mut physical_output = output_unit(physical, OUTPUT_CHANNELS)?;

        let mut capture_producers = Vec::with_capacity(CAPTURE_COUNT);
        let mut capture_units = Vec::with_capacity(CAPTURE_COUNT);
        for (index, (route, id)) in CAPTURE_ROUTES
            .into_iter()
            .zip(virtual_ids.captures)
            .enumerate()
        {
            if routes & (1 << index) == 0 {
                continue;
            }
            let (producer, consumer) = RingBuffer::<Stereo>::new(RING_FRAMES);
            capture_producers.push((route, producer));
            let mut reader = StereoReader::new(consumer);
            let mut unit = output_unit(id, route.channels)?;
            unit.set_render_callback(move |args: AudioArgs| {
                reader.fill(args.data.buffer, route.channels);
                Ok(())
            })?;
            capture_units.push(unit);
        }

        physical_input.set_input_callback(move |args: AudioArgs| {
            for frame in args
                .data
                .buffer
                .chunks_exact(INPUT_CHANNELS)
                .take(args.num_frames)
            {
                for (route, producer) in &mut capture_producers {
                    let _ = producer.push(capture_pair(frame, *route));
                }
            }
            Ok(())
        })?;

        let mut playback_readers = Vec::with_capacity(PLAYBACK_COUNT);
        let mut playback_units = Vec::with_capacity(PLAYBACK_COUNT);
        for (index, id) in virtual_ids.playbacks.into_iter().enumerate() {
            if routes & (1 << (CAPTURE_COUNT + index)) == 0 {
                continue;
            }
            let (mut producer, consumer) = RingBuffer::<Stereo>::new(RING_FRAMES);
            let mut unit = input_unit(id, 2)?;
            unit.set_input_callback(move |args: AudioArgs| {
                for frame in args.data.buffer.chunks_exact(2).take(args.num_frames) {
                    let _ = producer.push([frame[0], frame[1]]);
                }
                Ok(())
            })?;
            playback_readers.push((index, StereoReader::new(consumer)));
            playback_units.push(unit);
        }

        let mut output_blocks = Box::new([[0.0f32; 1024]; PLAYBACK_COUNT]);
        physical_output.set_render_callback(move |args: AudioArgs| {
            args.data.buffer.fill(0.0);
            // Process arbitrary callback sizes without allocating on the audio thread.
            for chunk in args.data.buffer.chunks_mut(512 * OUTPUT_CHANNELS) {
                let frames = chunk.len() / OUTPUT_CHANNELS;
                for (route_index, reader) in &mut playback_readers {
                    reader.fill(&mut output_blocks[*route_index][..frames * 2], 2);
                }
                for (index, frame) in chunk.chunks_exact_mut(OUTPUT_CHANNELS).enumerate() {
                    for (route_index, _) in &playback_readers {
                        let route = PLAYBACK_ROUTES[*route_index];
                        let block = &output_blocks[*route_index];
                        set_pair(
                            frame,
                            route.physical_channel,
                            [block[index * 2], block[index * 2 + 1]],
                        );
                    }
                }
            }
            Ok(())
        })?;

        let mut units = Vec::with_capacity(2 + routes.count_ones() as usize);
        units.push(physical_input);
        units.extend(capture_units);
        units.extend(playback_units);
        units.push(physical_output);
        let mut bridge = Self {
            physical_uid,
            physical_id: physical,
            routes,
            units: Vec::with_capacity(units.len()),
        };
        for mut unit in units {
            unit.start()?;
            bridge.units.push(unit);
        }
        Ok(bridge)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        for unit in self.units.iter_mut().rev() {
            let _ = unit.stop();
        }
    }
}

fn route_ids<const N: usize>(
    uids: impl IntoIterator<Item = &'static str>,
) -> Result<Option<[AudioDeviceID; N]>> {
    let mut ids = Vec::with_capacity(N);
    for uid in uids {
        let Some(id) = get_device_id_for_uid(uid)? else {
            return Ok(None);
        };
        ids.push(id);
    }
    Ok(Some(
        ids.try_into()
            .expect("route table length must match device count"),
    ))
}

fn virtual_ids() -> Result<Option<VirtualIds>> {
    let Some(captures) = route_ids(CAPTURE_ROUTES.map(|route| route.bridge_uid))? else {
        return Ok(None);
    };
    let Some(playbacks) = route_ids(PLAYBACK_ROUTES.map(|route| route.bridge_uid))? else {
        return Ok(None);
    };
    Ok(Some(VirtualIds {
        captures,
        playbacks,
    }))
}

fn report_error(last_error: &mut Option<String>, message: String) {
    if last_error.as_deref() != Some(&message) {
        warn!("GoXLR virtual audio bridge unavailable: {message}");
        *last_error = Some(message);
    }
}

pub async fn run(settings: SettingsHandle, mut stop: Shutdown) -> Result<()> {
    let mut ids: Option<VirtualIds> = None;
    let mut active: Option<Bridge> = None;
    let mut last_error: Option<String> = None;
    let mut ticker = time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = stop.recv() => break,
            _ = ticker.tick() => {
                let current_ids = match virtual_ids() {
                    Ok(ids) => ids,
                    Err(error) => {
                        active = None;
                        report_error(&mut last_error, error.to_string());
                        continue;
                    }
                };
                if current_ids != ids {
                    active = None;
                    ids = current_ids;
                }
                let Some(ids) = ids else {
                    continue;
                };
                let routes = settings.get_macos_virtual_audio_routes().await;
                if let Err(error) = set_virtual_audio_routes(routes) {
                    active = None;
                    report_error(&mut last_error, error.to_string());
                    continue;
                }
                if active.as_ref().is_some_and(|bridge| bridge.routes != routes) {
                    active = None;
                }
                if routes == 0 {
                    active = None;
                    continue;
                }

                let devices = match get_goxlr_devices() {
                    Ok(devices) => devices,
                    Err(error) => {
                        active = None;
                        report_error(&mut last_error, error.to_string());
                        continue;
                    }
                };
                let full_devices: Vec<_> = devices
                    .into_iter()
                    .filter(|device| device.product_id == PID_GOXLR_FULL)
                    .collect();
                if full_devices.len() != 1 {
                    active = None;
                    if full_devices.len() > 1 {
                        report_error(&mut last_error, "multiple GoXLR Full devices found".into());
                    }
                    continue;
                }
                let physical_uid = &full_devices[0].uid;
                let physical_id = match get_device_id_for_uid(physical_uid) {
                    Ok(id) => id,
                    Err(error) => {
                        active = None;
                        report_error(&mut last_error, error.to_string());
                        continue;
                    }
                };
                if active.as_ref().is_some_and(|bridge| bridge.physical_uid != *physical_uid || Some(bridge.physical_id) != physical_id) {
                    active = None;
                }
                if active.is_none() {
                    let Some(physical_id) = physical_id else {
                        continue;
                    };
                    match Bridge::start(physical_uid.clone(), physical_id, ids, routes) {
                        Ok(bridge) => {
                            info!("GoXLR virtual audio bridge started");
                            active = Some(bridge);
                            last_error = None;
                        }
                        Err(error) => {
                            report_error(&mut last_error, error.to_string());
                        }
                    }
                }
            }
        }
    }
    drop(active);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CAPTURE_ROUTES, PLAYBACK_ROUTES, StereoReader, capture_pair, set_pair};
    use rtrb::RingBuffer;

    #[test]
    fn routes_every_physical_channel_once() {
        let input: Vec<f32> = (0..23).map(|value| value as f32).collect();
        for (index, route) in CAPTURE_ROUTES.iter().copied().enumerate() {
            assert_eq!(route.physical_channel, index * 2);
            assert_eq!(
                capture_pair(&input, route),
                [
                    (index * 2) as f32,
                    if index == 11 {
                        0.0
                    } else {
                        (index * 2 + 1) as f32
                    },
                ]
            );
        }
        assert_eq!(
            CAPTURE_ROUTES[1].bridge_uid,
            "GoXLRVirtual::Microphone::Bridge"
        );
        assert_eq!(CAPTURE_ROUTES[11].channels, 1);

        let mut output = [0.0; 10];
        for (index, route) in PLAYBACK_ROUTES.iter().enumerate() {
            assert_eq!(route.physical_channel, index * 2);
            set_pair(
                &mut output,
                route.physical_channel,
                [(index * 2) as f32, (index * 2 + 1) as f32],
            );
        }
        assert_eq!(output, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert_eq!(PLAYBACK_ROUTES[2].bridge_uid, "GoXLRVirtual::Chat::Bridge");
        assert_eq!(PLAYBACK_ROUTES[3].bridge_uid, "GoXLRVirtual::Music::Bridge");
    }

    #[test]
    fn stereo_reader_waits_for_audio_and_keeps_channels_together() {
        let (mut producer, consumer) = RingBuffer::new(2048);
        let mut reader = StereoReader::new(consumer);
        let mut output = [1.0; 128];
        reader.fill(&mut output, 2);
        assert!(output.iter().all(|sample| *sample == 0.0));

        for frame in 0..512 {
            producer.push([frame as f32, -(frame as f32)]).unwrap();
        }
        reader.fill(&mut output, 2);
        for (index, frame) in output.chunks_exact(2).enumerate() {
            assert!((frame[0] - index as f32).abs() < 0.1);
            assert!((frame[1] + index as f32).abs() < 0.1);
        }
    }

    #[test]
    fn dry_mic_reader_writes_one_channel() {
        let (mut producer, consumer) = RingBuffer::new(2048);
        let mut reader = StereoReader::new(consumer);
        for frame in 0..512 {
            producer.push([frame as f32, 999.0]).unwrap();
        }
        let mut output = [0.0; 64];
        reader.fill(&mut output, 1);
        for (index, sample) in output.iter().enumerate() {
            assert!((*sample - index as f32).abs() < 0.1);
        }
    }

    #[test]
    fn stereo_reader_rebuffers_after_underrun() {
        let (mut producer, consumer) = RingBuffer::new(2048);
        let mut reader = StereoReader::new(consumer);
        for _ in 0..512 {
            producer.push([1.0, -1.0]).unwrap();
        }

        let mut output = [0.0; 1024];
        reader.fill(&mut output, 2);

        for _ in 0..128 {
            producer.push([2.0, -2.0]).unwrap();
        }
        reader.fill(&mut output, 2);
        assert!(output.iter().all(|sample| *sample == 0.0));

        for _ in 0..384 {
            producer.push([2.0, -2.0]).unwrap();
        }
        reader.fill(&mut output, 2);
        assert_eq!(&output[..2], &[2.0, -2.0]);
    }
}
