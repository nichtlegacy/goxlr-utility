use anyhow::Result;
use coreaudio_sys::AudioDeviceID;
use log::{debug, error, warn};
use std::collections::HashMap;
use std::collections::hash_map::Entry::Vacant;
use std::time::Duration;
use strum::IntoEnumIterator;

use crate::HANDLE_MACOS_AGGREGATES;
use crate::events::EventTriggers;
use crate::platform::macos::app_audio::AppAudioHandle;
use crate::platform::macos::audio_bridge;
use crate::platform::macos::core_audio::{
    CoreAudioDevice, add_sub_device, create_aggregate_device, destroy_aggregate_device,
    find_all_existing_aggregates, get_goxlr_devices, set_active_channels,
};
use crate::platform::macos::device::{Inputs, Outputs};
use crate::settings::SettingsHandle;
use crate::shutdown::Shutdown;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task;
use tokio::time::sleep;
use tokio::{select, time};

/*
   The main use of the runtime here is to detect, and manage, Aggregate devices for the
   GoXLR. When we start we'll clear any stale devices then create new ones, when we stop
   we'll destroy the devices. During runtime, we'll periodically check to see if new devices
   have appeared, or old devices have disappeared, and manage accordingly.
*/
pub async fn run(
    tx: mpsc::Sender<EventTriggers>,
    settings: SettingsHandle,
    app_audio: AppAudioHandle,
    mut stop: Shutdown,
) -> Result<()> {
    let bridge_stop = stop.clone();
    let bridge = tokio::spawn(async move {
        if let Err(error) = audio_bridge::run(settings, app_audio, bridge_stop).await {
            warn!("GoXLR virtual audio bridge stopped: {error}");
        }
    });

    // Before we start, we should destroy any existing aggregate devices as they're unmanaged.
    // CoreAudio calls can block while coreaudiod is busy, so keep them off the tokio workers.
    let _ = task::spawn_blocking(|| {
        if let Ok(devices) = find_all_existing_aggregates() {
            for device in devices {
                if destroy_aggregate_device(device).is_err() {
                    warn!("Unable to Destroy Aggregate Device {}", device);
                }
            }
        }
    })
    .await;

    if !HANDLE_MACOS_AGGREGATES.lock().unwrap().unwrap() {
        let _ = bridge.await;
        return Ok(());
    }

    // Ticker to monitor for device changes..
    let mut ticker = time::interval(Duration::from_secs(2));

    // Shutdown Handlers..
    let mut stream = signal(SignalKind::terminate())?;

    // A list of devices, and a list of their associated AudioDeviceIDs..
    let mut device_map: HashMap<String, Vec<AudioDeviceID>> = HashMap::new();

    loop {
        select! {
            _ = ticker.tick() => {
                let map = std::mem::take(&mut device_map);
                match task::spawn_blocking(move || refresh_devices(map)).await {
                    Ok(map) => device_map = map,
                    Err(error) => error!("Aggregate Device Refresh Failed: {}", error),
                }
            },

            Some(_) = stream.recv() => {
                // Trigger a Shutdown
                debug!("TERM Signal Received, Triggering STOP");
                let _ = tx.send(EventTriggers::Stop(false)).await;
            },

            _ = stop.recv() => {
                debug!("Destroying Aggregates and Stopping..");

                // Destroy existing devices..
                let map = std::mem::take(&mut device_map);
                let _ = task::spawn_blocking(move || {
                    for devices in map.values() {
                        if let Err(error) = destroy_devices(devices) {
                            error!("Error Removing Device: {}", error);
                        }
                    }
                })
                .await;

                // Wait a second so CoreAudio can clean up..
                sleep(Duration::from_secs(1)).await;

                let _ = bridge.await;
                debug!("Runtime Ended");
                break;
            }
        }
    }

    Ok(())
}

fn refresh_devices(
    mut device_map: HashMap<String, Vec<AudioDeviceID>>,
) -> HashMap<String, Vec<AudioDeviceID>> {
    if let Ok(devices) = get_goxlr_devices() {
        let mut remove_keys: Vec<String> = vec![];

        // Iterate the device map to check for things..
        for uid in device_map.keys() {
            // Is this device still present?
            if !devices.iter().any(|d| d.uid == *uid) {
                debug!("{} No longer Present in Map..", uid);
                if destroy_devices(device_map.get(uid).unwrap()).is_err() {
                    warn!("Error Removing Aggregate Devices");
                }
                remove_keys.push(uid.clone());
            }
        }

        // Remove the devices from the map..
        device_map.retain(|uid, _| !remove_keys.contains(uid));

        for device in devices {
            if let Vacant(entry) = device_map.entry(device.uid.clone()) {
                debug!("Creating Aggregates for {}", device.uid.clone());
                match create_devices(device) {
                    Ok(devices) => {
                        entry.insert(devices);
                    }
                    Err(error) => {
                        error!("Unable to Create Device: {}", error)
                    }
                }
            }
        }
    }
    device_map
}

fn create_devices(device: CoreAudioDevice) -> Result<Vec<AudioDeviceID>> {
    let mut devices = vec![];
    let result = (|| -> Result<()> {
        for output in Outputs::iter() {
            let aggregate = create_aggregate_device(output.get_name(), &device)?;
            devices.push(aggregate);
            add_sub_device(aggregate, device.uid.clone())?;
            set_active_channels(aggregate, false, output.get_channels())?;
        }

        for input in Inputs::iter() {
            let aggregate = create_aggregate_device(input.get_name(), &device)?;
            devices.push(aggregate);
            add_sub_device(aggregate, device.uid.clone())?;
            set_active_channels(aggregate, true, input.get_channels())?;
        }
        Ok(())
    })();

    if let Err(error) = result {
        for aggregate in devices {
            if let Err(cleanup_error) = destroy_aggregate_device(aggregate) {
                warn!("Unable to Remove Partial Aggregate {aggregate}: {cleanup_error}");
            }
        }
        return Err(error);
    }

    Ok(devices)
}

fn destroy_devices(devices: &Vec<AudioDeviceID>) -> Result<()> {
    for device in devices {
        debug!("Removing: {}", device);
        destroy_aggregate_device(*device)?;
    }

    Ok(())
}
