//! Producer side of oxideav-core's mid-stream output-shape contract
//! (`registry` feature): the [`Decoder::output_params`] /
//! [`Decoder::output_params_generation`] answers and the in-band
//! [`Frame::StreamParams`] event, for this crate's own decoders (the
//! `"heif"` still decoder, the VVC item decoder).
//!
//! Each queued picture carries the description of its own shape; the
//! queue announces a change exactly where it takes effect:
//!
//! * no event for the first shape (that is what `output_params()`
//!   answers up front — the shape of the first queued picture);
//! * one event immediately before the first picture of a new shape,
//!   with `output_params()` and the generation already moved when it
//!   is returned and the payload equal to `output_params()`;
//! * pictures queued before a change still come out first (the queue
//!   is FIFO), so draining after `flush()` keeps the ordering;
//! * after [`ShapedQueue::clear`] (a decoder `reset()`) the rule holds
//!   relative to the shape of the last picture returned before it.
//!
//! [`Decoder::output_params`]: oxideav_core::Decoder::output_params
//! [`Decoder::output_params_generation`]: oxideav_core::Decoder::output_params_generation

use std::collections::VecDeque;

use oxideav_core::{CodecId, CodecParameters, ColorSignal, Frame, PixelFormat, VideoFrame};

/// What `output_params()` describes for a picture: geometry, pixel
/// layout, colour signal. Two pictures of equal shape need no event
/// between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub color_signal: ColorSignal,
}

impl Shape {
    /// The shape as a video [`CodecParameters`] under `codec_id`.
    pub fn to_params(self, codec_id: &CodecId) -> CodecParameters {
        let mut p = CodecParameters::video(codec_id.clone());
        p.width = Some(self.width);
        p.height = Some(self.height);
        p.pixel_format = Some(self.pixel_format);
        p.color_signal = self.color_signal;
        p
    }
}

/// A FIFO of decoded pictures, each with its [`Shape`], that inserts
/// the [`Frame::StreamParams`] events and keeps the generation counter
/// (see the module docs).
#[derive(Debug, Default)]
pub(crate) struct ShapedQueue {
    queue: VecDeque<(VideoFrame, Shape)>,
    /// Shape of the last picture returned (or of the last event).
    current: Option<Shape>,
    /// A media frame has been returned since construction.
    started: bool,
    generation: u64,
}

impl ShapedQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a picture of `shape`.
    pub fn push(&mut self, frame: VideoFrame, shape: Shape) {
        self.queue.push_back((frame, shape));
    }

    /// The next frame out: the picture at the head, or the event
    /// announcing its shape when that differs from the shape of the
    /// picture returned last. `None` when nothing is queued.
    pub fn pop(&mut self, codec_id: &CodecId) -> Option<Frame> {
        let next = self.queue.front()?.1;
        if self.started && self.current != Some(next) {
            self.current = Some(next);
            self.generation = self.generation.wrapping_add(1);
            return Some(Frame::StreamParams(Box::new(next.to_params(codec_id))));
        }
        let (vf, shape) = self.queue.pop_front()?;
        self.current = Some(shape);
        self.started = true;
        Some(Frame::Video(vf))
    }

    /// The shape `output_params()` answers: the last picture's (or
    /// event's) once one was returned; before that, the first queued
    /// picture's.
    pub fn shape(&self) -> Option<Shape> {
        if self.started {
            self.current
        } else {
            self.queue.front().map(|(_, s)| *s).or(self.current)
        }
    }

    /// [`Self::shape`] as `output_params()` answers it.
    pub fn output_params(&self, codec_id: &CodecId) -> Option<CodecParameters> {
        self.shape().map(|s| s.to_params(codec_id))
    }

    /// The `output_params_generation()` counter.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Drop every queued picture (a `reset()`), keeping the shape of
    /// the last one returned so a change after the seek is announced.
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(w: u32, h: u32, pf: PixelFormat) -> Shape {
        Shape {
            width: w,
            height: h,
            pixel_format: pf,
            color_signal: ColorSignal::default(),
        }
    }

    fn pic(tag: u8) -> VideoFrame {
        VideoFrame {
            pts: Some(tag as i64),
            planes: vec![oxideav_core::VideoPlane {
                stride: 1,
                data: vec![tag],
            }],
        }
    }

    /// Pull everything: `Ok(tag)` for a picture, `Err((w, h))` for an
    /// event (checked against `output_params()` and the generation).
    fn drain(q: &mut ShapedQueue, id: &CodecId) -> Vec<Result<u8, (u32, u32)>> {
        let mut out = Vec::new();
        while let Some(f) = q.pop(id) {
            match f {
                Frame::Video(v) => out.push(Ok(v.planes[0].data[0])),
                Frame::StreamParams(p) => {
                    let now = q.output_params(id).unwrap();
                    assert_eq!(now.width, p.width);
                    assert_eq!(now.height, p.height);
                    assert_eq!(now.pixel_format, p.pixel_format);
                    out.push(Err((p.width.unwrap(), p.height.unwrap())));
                }
                _ => unreachable!(),
            }
        }
        out
    }

    #[test]
    fn constant_shape_has_no_event() {
        let id = CodecId::new("heif");
        let mut q = ShapedQueue::new();
        assert!(q.output_params(&id).is_none());
        for t in 0..3 {
            q.push(pic(t), shape(4, 4, PixelFormat::Yuv420P));
        }
        assert_eq!(q.output_params(&id).unwrap().width, Some(4));
        assert_eq!(drain(&mut q, &id), vec![Ok(0), Ok(1), Ok(2)]);
        assert_eq!(q.generation(), 0);
    }

    #[test]
    fn one_event_per_change_before_the_new_shape() {
        let id = CodecId::new("heif");
        let mut q = ShapedQueue::new();
        // The first shape differs from nothing announced: no event.
        q.push(pic(0), shape(8, 8, PixelFormat::Yuv420P));
        q.push(pic(1), shape(8, 8, PixelFormat::Yuv420P));
        q.push(pic(2), shape(2, 2, PixelFormat::Yuv420P));
        q.push(pic(3), shape(2, 2, PixelFormat::Gray8));
        q.push(pic(4), shape(2, 2, PixelFormat::Gray8));
        assert_eq!(q.output_params(&id).unwrap().width, Some(8));
        assert_eq!(
            drain(&mut q, &id),
            vec![Ok(0), Ok(1), Err((2, 2)), Ok(2), Err((2, 2)), Ok(3), Ok(4)]
        );
        assert_eq!(q.generation(), 2);
        assert_eq!(
            q.output_params(&id).unwrap().pixel_format,
            Some(PixelFormat::Gray8)
        );
    }

    #[test]
    fn clear_keeps_the_last_returned_shape() {
        let id = CodecId::new("heif");
        let mut q = ShapedQueue::new();
        q.push(pic(0), shape(8, 8, PixelFormat::Yuv420P));
        q.push(pic(1), shape(4, 4, PixelFormat::Yuv420P));
        assert!(matches!(q.pop(&id), Some(Frame::Video(_))));
        q.clear();
        assert!(q.is_empty());
        // Same shape as the last returned: no event after the reset.
        q.push(pic(2), shape(8, 8, PixelFormat::Yuv420P));
        assert_eq!(drain(&mut q, &id), vec![Ok(2)]);
        // A different one: announced.
        q.push(pic(3), shape(4, 4, PixelFormat::Yuv420P));
        assert_eq!(drain(&mut q, &id), vec![Err((4, 4)), Ok(3)]);
        assert_eq!(q.generation(), 1);
    }
}
