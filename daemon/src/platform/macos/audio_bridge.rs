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

use crate::platform::macos::core_audio::{get_device_id_for_uid, get_goxlr_devices};
use crate::shutdown::Shutdown;

const MICROPHONE_BRIDGE_UID: &str = "GoXLRVirtual::Microphone::Bridge";
const CHAT_BRIDGE_UID: &str = "GoXLRVirtual::Chat::Bridge";
const MUSIC_BRIDGE_UID: &str = "GoXLRVirtual::Music::Bridge";
const RING_FRAMES: usize = 2048;
const INPUT_CHANNELS: usize = 23;
const OUTPUT_CHANNELS: usize = 10;

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

fn microphone_pair(frame: &[f32]) -> Stereo {
    [frame[2], frame[3]]
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

    fn fill(&mut self, output: &mut [f32]) {
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
        for frame in output.chunks_exact_mut(2) {
            frame[0] = self.current[0] + (self.next[0] - self.current[0]) * self.phase;
            frame[1] = self.current[1] + (self.next[1] - self.current[1]) * self.phase;
            self.phase += rate;
            while self.phase >= 1.0 {
                self.phase -= 1.0;
                self.current = self.next;
                self.next = self.queue.pop().unwrap_or([0.0; 2]);
            }
        }
    }
}

struct Bridge {
    physical_uid: String,
    physical_id: AudioDeviceID,
    units: Vec<AudioUnit>,
}

impl Bridge {
    fn start(
        physical_uid: String,
        physical: AudioDeviceID,
        virtual_ids: [AudioDeviceID; 3],
    ) -> Result<Self> {
        let [microphone, chat, music] = virtual_ids;
        let mut physical_input = input_unit(physical, INPUT_CHANNELS)?;
        let mut physical_output = output_unit(physical, OUTPUT_CHANNELS)?;
        let mut microphone_output = output_unit(microphone, 2)?;
        let mut chat_input = input_unit(chat, 2)?;
        let mut music_input = input_unit(music, 2)?;

        let (mut mic_producer, mic_consumer) = RingBuffer::<Stereo>::new(RING_FRAMES);
        let (mut chat_producer, chat_consumer) = RingBuffer::<Stereo>::new(RING_FRAMES);
        let (mut music_producer, music_consumer) = RingBuffer::<Stereo>::new(RING_FRAMES);
        let mut mic_reader = StereoReader::new(mic_consumer);
        let mut chat_reader = StereoReader::new(chat_consumer);
        let mut music_reader = StereoReader::new(music_consumer);

        physical_input.set_input_callback(move |args: AudioArgs| {
            for frame in args
                .data
                .buffer
                .chunks_exact(INPUT_CHANNELS)
                .take(args.num_frames)
            {
                let _ = mic_producer.push(microphone_pair(frame));
            }
            Ok(())
        })?;
        microphone_output.set_render_callback(move |args: AudioArgs| {
            mic_reader.fill(args.data.buffer);
            Ok(())
        })?;

        chat_input.set_input_callback(move |args: AudioArgs| {
            for frame in args.data.buffer.chunks_exact(2).take(args.num_frames) {
                let _ = chat_producer.push([frame[0], frame[1]]);
            }
            Ok(())
        })?;
        music_input.set_input_callback(move |args: AudioArgs| {
            for frame in args.data.buffer.chunks_exact(2).take(args.num_frames) {
                let _ = music_producer.push([frame[0], frame[1]]);
            }
            Ok(())
        })?;
        physical_output.set_render_callback(move |args: AudioArgs| {
            args.data.buffer.fill(0.0);
            // Process arbitrary callback sizes in fixed stack buffers.
            let mut chat_block = [0.0f32; 1024];
            let mut music_block = [0.0f32; 1024];
            for chunk in args.data.buffer.chunks_mut(512 * OUTPUT_CHANNELS) {
                let frames = chunk.len() / OUTPUT_CHANNELS;
                chat_reader.fill(&mut chat_block[..frames * 2]);
                music_reader.fill(&mut music_block[..frames * 2]);
                for (index, frame) in chunk.chunks_exact_mut(OUTPUT_CHANNELS).enumerate() {
                    set_pair(frame, 4, [chat_block[index * 2], chat_block[index * 2 + 1]]);
                    set_pair(
                        frame,
                        6,
                        [music_block[index * 2], music_block[index * 2 + 1]],
                    );
                }
            }
            Ok(())
        })?;

        let mut bridge = Self {
            physical_uid,
            physical_id: physical,
            units: Vec::with_capacity(5),
        };
        for mut unit in [
            chat_input,
            music_input,
            physical_input,
            microphone_output,
            physical_output,
        ] {
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

fn virtual_ids() -> Result<Option<[AudioDeviceID; 3]>> {
    let (Some(microphone), Some(chat), Some(music)) = (
        get_device_id_for_uid(MICROPHONE_BRIDGE_UID)?,
        get_device_id_for_uid(CHAT_BRIDGE_UID)?,
        get_device_id_for_uid(MUSIC_BRIDGE_UID)?,
    ) else {
        return Ok(None);
    };
    Ok(Some([microphone, chat, music]))
}

fn report_error(last_error: &mut Option<String>, message: String) {
    if last_error.as_deref() != Some(&message) {
        warn!("GoXLR virtual audio bridge unavailable: {message}");
        *last_error = Some(message);
    }
}

pub async fn run(mut stop: Shutdown) -> Result<()> {
    let Some(mut ids) = virtual_ids()? else {
        return Ok(());
    };

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
                let Some(current_ids) = current_ids else {
                    active = None;
                    break;
                };
                if current_ids != ids {
                    active = None;
                    ids = current_ids;
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
                    match Bridge::start(physical_uid.clone(), physical_id, ids) {
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
    use super::{StereoReader, microphone_pair, set_pair};
    use rtrb::RingBuffer;

    #[test]
    fn routes_only_the_requested_physical_pairs() {
        let input: Vec<f32> = (0..23).map(|value| value as f32).collect();
        assert_eq!(microphone_pair(&input), [2.0, 3.0]);

        let mut output = [0.0; 10];
        set_pair(&mut output, 4, [50.0, 60.0]);
        set_pair(&mut output, 6, [70.0, 80.0]);
        assert_eq!(
            output,
            [0.0, 0.0, 0.0, 0.0, 50.0, 60.0, 70.0, 80.0, 0.0, 0.0]
        );
    }

    #[test]
    fn stereo_reader_waits_for_audio_and_keeps_channels_together() {
        let (mut producer, consumer) = RingBuffer::new(2048);
        let mut reader = StereoReader::new(consumer);
        let mut output = [1.0; 128];
        reader.fill(&mut output);
        assert!(output.iter().all(|sample| *sample == 0.0));

        for frame in 0..512 {
            producer.push([frame as f32, -(frame as f32)]).unwrap();
        }
        reader.fill(&mut output);
        for (index, frame) in output.chunks_exact(2).enumerate() {
            assert!((frame[0] - index as f32).abs() < 0.1);
            assert!((frame[1] + index as f32).abs() < 0.1);
        }
    }
}
