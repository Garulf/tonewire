use pipewire as pw;
use pw::spa;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Value};
use pw::spa::sys;

/// Serialized `EnumFormat` pod for interleaved f32 at `rate` with `channels`.
pub fn audio_format(rate: u32, channels: u32) -> Vec<u8> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(rate);
    info.set_channels(channels);
    let mut position = [0; MAX_CHANNELS];
    for (i, slot) in position.iter_mut().enumerate().take(channels as usize) {
        *slot = channel_position(channels, i);
    }
    info.set_position(position);
    PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: sys::SPA_TYPE_OBJECT_Format,
            id: sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .expect("audio format pod serializes")
    .0
    .into_inner()
}

fn channel_position(channels: u32, index: usize) -> u32 {
    if channels == 1 {
        return sys::SPA_AUDIO_CHANNEL_MONO;
    }
    const LAYOUT: [u32; 8] = [
        sys::SPA_AUDIO_CHANNEL_FL,
        sys::SPA_AUDIO_CHANNEL_FR,
        sys::SPA_AUDIO_CHANNEL_FC,
        sys::SPA_AUDIO_CHANNEL_LFE,
        sys::SPA_AUDIO_CHANNEL_RL,
        sys::SPA_AUDIO_CHANNEL_RR,
        sys::SPA_AUDIO_CHANNEL_SL,
        sys::SPA_AUDIO_CHANNEL_SR,
    ];
    LAYOUT
        .get(index)
        .copied()
        .unwrap_or(sys::SPA_AUDIO_CHANNEL_AUX0 + index as u32)
}

pub fn pod(bytes: &[u8]) -> &spa::pod::Pod {
    spa::pod::Pod::from_bytes(bytes).expect("serialized pod is valid")
}
