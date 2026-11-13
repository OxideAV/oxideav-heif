//! A stub codec [`Decoder`] for the `Frame::StreamParams` /
//! layer-identity consumer tests (oxideav-core 0.1.43): registered
//! under a real codec id in a caller-supplied `CodecRegistry`, it
//! ignores the payload and answers every packet with a flat picture of
//! the shape its `CodecParameters` announce — every luma (first-plane)
//! sample [`LUMA`], every other plane's [`CHROMA`] — carrying a
//! layer-identity and a colour-signal side-channel plane, surrounded by
//! `StreamParams` events.

use std::collections::VecDeque;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, ColorSignal, Decoder,
    Error, Frame, LayerIdentity, Packet, PixelFormat, Result, VideoFrame, VideoPlane,
};
use oxideav_heif::HeifPixelFormat;

/// Sample value of the stub's first plane.
pub const LUMA: u16 = 0x51;
/// Sample value of the stub's other planes.
pub const CHROMA: u16 = 0x7a;

/// A flat `w × h` picture in `pf`, tagged with `layer` (and a colour
/// signal), its planes tight.
pub fn picture(w: u32, h: u32, pf: PixelFormat, layer: LayerIdentity) -> VideoFrame {
    let fmt = HeifPixelFormat::from_core(pf).expect("planar layout");
    let bps = fmt.bytes_per_sample();
    let planes = (0..fmt.plane_count())
        .map(|p| {
            let (pw, ph) = fmt.plane_dims(p, w, h);
            let v = if p == 0 { LUMA } else { CHROMA };
            let sample: Vec<u8> = if bps == 1 {
                vec![v as u8]
            } else {
                v.to_le_bytes().to_vec()
            };
            VideoPlane {
                stride: pw as usize * bps,
                data: sample.repeat((pw * ph) as usize),
            }
        })
        .collect();
    VideoFrame {
        pts: Some(0),
        planes,
    }
    .with_layer(layer)
    .with_color_signal(ColorSignal::default())
}

/// The stub's shape description for `w × h` in `pf`.
fn shape(codec_id: &CodecId, w: u32, h: u32, pf: PixelFormat) -> CodecParameters {
    let mut p = CodecParameters::video(codec_id.clone());
    p.width = Some(w);
    p.height = Some(h);
    p.pixel_format = Some(pf);
    p
}

/// What the stub queues per packet.
#[derive(Clone, Copy)]
enum Script {
    /// An event, the picture, another event (a consumer must neither
    /// count the events as pictures nor fail on them).
    Surrounded,
    /// For a multi-layer item (`ols` option): the base layer at the
    /// announced size, then an event and the second output layer at
    /// half the size (both tagged); a single-layer item otherwise
    /// gets [`Script::Surrounded`].
    Layered,
    /// A codec whose output layer set selects only its highest layer:
    /// under the `ols` option layer 1 alone comes out; under
    /// `layerset` (and no `ols`) every layer of the set does — layer
    /// 0, then layer 1 — each at the announced size.
    HighestOnly,
}

struct Stub {
    id: CodecId,
    params: CodecParameters,
    script: Script,
    queue: VecDeque<Frame>,
    flushed: bool,
    current: Option<CodecParameters>,
    generation: u64,
}

impl Decoder for Stub {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, _packet: &Packet) -> Result<()> {
        let w = self.params.width.unwrap_or(16);
        let h = self.params.height.unwrap_or(16);
        let pf = self.params.pixel_format.unwrap_or(PixelFormat::Yuv420P);
        let full = shape(&self.id, w, h, pf);
        let layered =
            matches!(self.script, Script::Layered) && self.params.options.get("ols").is_some();
        if matches!(self.script, Script::HighestOnly) {
            let whole_set = self.params.options.get("ols").is_none()
                && self.params.options.get("layerset").is_some();
            for layer in [0u16, 1] {
                if layer == 1 || whole_set {
                    self.queue.push_back(Frame::Video(picture(
                        w,
                        h,
                        pf,
                        LayerIdentity::new(layer).with_access_unit(0),
                    )));
                }
            }
        } else if layered {
            let half = shape(&self.id, w / 2, h / 2, pf);
            self.queue.push_back(Frame::Video(picture(
                w,
                h,
                pf,
                LayerIdentity::new(0).with_access_unit(0),
            )));
            self.queue.push_back(Frame::StreamParams(Box::new(half)));
            self.queue.push_back(Frame::Video(picture(
                w / 2,
                h / 2,
                pf,
                LayerIdentity::new(1).with_access_unit(0),
            )));
        } else {
            self.queue
                .push_back(Frame::StreamParams(Box::new(full.clone())));
            self.queue.push_back(Frame::Video(picture(
                w,
                h,
                pf,
                LayerIdentity::new(0).with_access_unit(0),
            )));
            self.queue.push_back(Frame::StreamParams(Box::new(full)));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.queue.pop_front() {
            Some(f) => {
                if let Some(p) = f.stream_params() {
                    self.current = Some(p.clone());
                    self.generation += 1;
                }
                Ok(f)
            }
            None if self.flushed => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        Ok(())
    }

    fn output_params(&self) -> Option<CodecParameters> {
        self.current.clone()
    }

    fn output_params_generation(&self) -> u64 {
        self.generation
    }
}

fn make(params: &CodecParameters, script: Script) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Stub {
        id: params.codec_id.clone(),
        params: params.clone(),
        script,
        queue: VecDeque::new(),
        flushed: false,
        current: None,
        generation: 0,
    }))
}

fn make_surrounded(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    make(params, Script::Surrounded)
}

fn make_layered(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    make(params, Script::Layered)
}

fn make_highest_only(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    make(params, Script::HighestOnly)
}

/// A registry with the stub under `"h265"`, scripted as a codec whose
/// output layer set outputs only its highest layer while its layer set
/// holds two (the `layerset` option yields both).
pub fn registry_highest_only() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    reg.register(
        CodecInfo::new(CodecId::new("h265"))
            .capabilities(CodecCapabilities::video("stream_params_stub").with_decode())
            .decoder(make_highest_only),
    );
    reg
}

/// A registry with the stub under `"h265"`, `"av1"` and `"h264"`;
/// `layered` picks the multi-layer script.
pub fn registry(layered: bool) -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    for id in ["h265", "av1", "h264"] {
        reg.register(
            CodecInfo::new(CodecId::new(id))
                .capabilities(CodecCapabilities::video("stream_params_stub").with_decode())
                .decoder(if layered {
                    make_layered
                } else {
                    make_surrounded
                }),
        );
    }
    reg
}

/// The next picture `dec` returns, skipping `StreamParams` events;
/// `None` at `NeedMore` / `Eof` or on an error.
pub fn next_video<D: Decoder + ?Sized>(dec: &mut D) -> Option<VideoFrame> {
    loop {
        match dec.receive_frame() {
            Ok(Frame::Video(v)) => return Some(v),
            Ok(f) if f.is_stream_params() => continue,
            _ => return None,
        }
    }
}

/// `true` when every sample of `frame`'s first plane is [`LUMA`] and
/// every sample of its other planes [`CHROMA`].
pub fn is_flat(frame: &oxideav_heif::HeifFrame) -> bool {
    let bps = frame.format.bytes_per_sample();
    (0..frame.format.plane_count()).all(|p| {
        let want = if p == 0 { LUMA } else { CHROMA };
        let (pw, ph) = frame.format.plane_dims(p, frame.width, frame.height);
        let plane = &frame.planes[p];
        (0..ph as usize).all(|y| {
            let row = &plane.data[y * plane.stride..y * plane.stride + pw as usize * bps];
            if bps == 1 {
                row.iter().all(|s| *s as u16 == want)
            } else {
                row.as_chunks::<2>()
                    .0
                    .iter()
                    .all(|c| u16::from_le_bytes(*c) == want)
            }
        })
    })
}
