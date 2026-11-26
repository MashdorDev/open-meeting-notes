use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use anyhow::Result;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use log::{error, info, warn};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};

use super::devices::{device_from_id, get_device_and_config, DeviceType};

#[derive(Debug, Serialize, Clone)]
pub struct AudioLevelData {
    pub device_name: String,
    pub device_type: String,
    pub rms_level: f32,
    pub peak_level: f32,
    pub is_active: bool,
}

#[derive(Debug, Serialize, Clone)]
pub struct AudioLevelUpdate {
    pub timestamp: u64,
    pub levels: Vec<AudioLevelData>,
}

static IS_MONITORING: AtomicBool = AtomicBool::new(false);
/// Bumped on every start, so a stop or restart ends the previous run's
/// threads even if they have not seen the flag change yet.
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// RMS above this counts as sound (about -46 dBFS): room tone stays under it.
const ACTIVE_RMS: f32 = 0.005;
const EMIT_EVERY: std::time::Duration = std::time::Duration::from_millis(100);

/// Loudness since the last emit: (sum of squares, sample count, peak).
type Meter = Arc<Mutex<(f64, usize, f32)>>;

fn input_stream<T>(device: &cpal::Device, config: &cpal::StreamConfig, meter: Meter) -> Result<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _| {
            let mut m = meter.lock().unwrap_or_else(|e| e.into_inner());
            for &s in data {
                let v = f32::from_sample(s);
                m.0 += (v * v) as f64;
                m.2 = m.2.max(v.abs());
            }
            m.1 += data.len();
        },
        |e| warn!("Level monitor stream error: {}", e),
        None,
    )?;
    Ok(stream)
}

/// Opens each microphone and reports its real loudness every 100 ms as
/// `audio-levels`, until stop_monitoring. (This used to send a made-up sine
/// wave, so the level bars never reflected the microphone.) An empty list
/// means the system default microphone, the one a recording would use.
pub async fn start_monitoring<R: Runtime>(app_handle: AppHandle<R>, mut device_names: Vec<String>) -> Result<()> {
    if device_names.is_empty() {
        device_names.push(super::devices::default_input_device()?.name);
    }
    info!("Starting audio level monitoring for {} microphone(s)", device_names.len());
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    IS_MONITORING.store(true, Ordering::SeqCst);
    let running = move || IS_MONITORING.load(Ordering::SeqCst) && GENERATION.load(Ordering::SeqCst) == generation;

    let mut devices = Vec::new();
    for name in &device_names {
        match device_from_id(name, DeviceType::Input) {
            Ok(audio_device) => match get_device_and_config(&audio_device).await {
                Ok((device, config)) => devices.push((name.clone(), device, config)),
                Err(e) => warn!("Level monitor: cannot open microphone {}: {}", name, e),
            },
            Err(e) => warn!("Level monitor: bad device name {}: {}", name, e),
        }
    }
    if devices.is_empty() {
        IS_MONITORING.store(false, Ordering::SeqCst);
        return Err(anyhow::anyhow!("No microphone could be opened"));
    }

    let meters: HashMap<String, Meter> = devices.iter().map(|(n, ..)| (n.clone(), Meter::default())).collect();

    // cpal streams can't move between threads, so they live on their own
    // thread and are dropped (closing the microphone) when monitoring stops.
    let stream_meters = meters.clone();
    std::thread::spawn(move || {
        let mut streams = Vec::new();
        for (name, device, supported) in devices {
            let config: cpal::StreamConfig = supported.config();
            let meter = stream_meters[&name].clone();
            let built = match supported.sample_format() {
                cpal::SampleFormat::F32 => input_stream::<f32>(&device, &config, meter),
                cpal::SampleFormat::I16 => input_stream::<i16>(&device, &config, meter),
                cpal::SampleFormat::U16 => input_stream::<u16>(&device, &config, meter),
                cpal::SampleFormat::I32 => input_stream::<i32>(&device, &config, meter),
                other => Err(anyhow::anyhow!("unsupported sample format {:?}", other)),
            };
            match built.and_then(|s| s.play().map(|_| s).map_err(Into::into)) {
                Ok(stream) => streams.push(stream),
                Err(e) => warn!("Level monitor: microphone {} not started: {}", name, e),
            }
        }
        while running() {
            std::thread::sleep(EMIT_EVERY);
        }
        drop(streams);
        info!("Audio level monitoring closed its microphones");
    });

    tokio::spawn(async move {
        while running() {
            tokio::time::sleep(EMIT_EVERY).await;
            let levels = meters
                .iter()
                .map(|(name, meter)| {
                    let (sum, count, peak) = std::mem::take(&mut *meter.lock().unwrap_or_else(|e| e.into_inner()));
                    let rms = if count == 0 { 0.0 } else { (sum / count as f64).sqrt() as f32 };
                    AudioLevelData {
                        device_name: name.clone(),
                        device_type: "input".to_string(),
                        rms_level: rms.min(1.0),
                        peak_level: peak.min(1.0),
                        is_active: rms > ACTIVE_RMS,
                    }
                })
                .collect();
            let update = AudioLevelUpdate {
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                levels,
            };
            if let Err(e) = app_handle.emit("audio-levels", &update) {
                error!("Failed to emit audio levels: {}", e);
                break;
            }
        }
    });

    Ok(())
}

pub async fn stop_monitoring() -> Result<()> {
    info!("Stopping audio level monitoring");
    IS_MONITORING.store(false, Ordering::SeqCst);
    Ok(())
}

pub fn is_monitoring() -> bool {
    IS_MONITORING.load(Ordering::SeqCst)
}
