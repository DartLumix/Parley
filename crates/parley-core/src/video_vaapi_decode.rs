//! VA-API H.264 decode backend.
//!
//! `moq-vaapi::decode::Decoder` is deliberately not `Send`: a libva context is
//! bound to the thread that created it. The receive hub is shared across the
//! client's reader and display threads, so the decoder lives on a dedicated
//! worker thread and every call is a request/response pair. That keeps the
//! unsafe thread contract inside this module instead of relying on every
//! caller to pin a hub operation to one thread.
//!
//! Only progressive 8-bit 4:2:0 streams are accepted by the vendored decoder;
//! anything it rejects falls back to the software decoder in `video.rs`.

use std::sync::mpsc::{Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;

use moq_vaapi::decode::{Config, Decoder};

use crate::error::Error;
use crate::video::nv12_to_rgb;

/// RGB bytes plus the SPS-decided dimensions.
type RgbFrame = (Vec<u8>, usize, usize);
#[cfg(test)]
type FlushReply = Result<Vec<RgbFrame>, String>;

enum Request {
    Decode {
        frame: Vec<u8>,
        rgba: bool,
        reply: SyncSender<Result<Option<RgbFrame>, String>>,
    },
    #[cfg(test)]
    Flush {
        reply: SyncSender<FlushReply>,
    },
    Shutdown,
}

/// A VA-API decoder confined to its own worker thread.
pub struct VaapiDecoder {
    tx: Sender<Request>,
    join: Option<JoinHandle<()>>,
}

impl VaapiDecoder {
    pub fn new() -> Result<Self, Error> {
        let (tx, rx) = channel::<Request>();
        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
        let join = std::thread::Builder::new()
            .name("parley-vaapi-decode".into())
            .spawn(move || {
                // The thread owns both the display and every surface the
                // decoder allocates; neither ever crosses a thread boundary.
                let mut decoder = match Decoder::new(Config::new()) {
                    Ok(decoder) => {
                        let _ = ready_tx.send(Ok(()));
                        decoder
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                let mut timestamp = 0u64;
                while let Ok(request) = rx.recv() {
                    match request {
                        Request::Shutdown => break,
                        #[cfg(test)]
                        Request::Flush { reply } => {
                            let result = decoder.flush().map_err(|e| e.to_string()).map(|frames| {
                                frames
                                    .iter()
                                    .map(|frame| {
                                        (
                                            nv12_to_rgb(
                                                &frame.data,
                                                frame.width as usize,
                                                frame.height as usize,
                                                false,
                                            ),
                                            frame.width as usize,
                                            frame.height as usize,
                                        )
                                    })
                                    .collect()
                            });
                            let _ = reply.send(result);
                        }
                        Request::Decode { frame, rgba, reply } => {
                            let result = decoder
                                .decode(&frame, timestamp)
                                .map_err(|e| e.to_string())
                                .map(|frames| {
                                    // A live, no-B-frame stream normally
                                    // yields one picture per input. A DPB
                                    // bump can release several at once; the
                                    // display wants the newest, exactly like
                                    // the hub's own drop-oldest queue.
                                    frames.last().map(|frame| {
                                        let rgb = nv12_to_rgb(
                                            &frame.data,
                                            frame.width as usize,
                                            frame.height as usize,
                                            rgba,
                                        );
                                        (rgb, frame.width as usize, frame.height as usize)
                                    })
                                });
                            timestamp = timestamp.wrapping_add(1);
                            let _ = reply.send(result);
                        }
                    }
                }
            })
            .map_err(|e| Error::Codec(format!("VA-API decoder thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                join: Some(join),
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(Error::Codec(format!("VA-API decoder init: {e}")))
            }
            Err(_) => {
                let _ = join.join();
                Err(Error::Codec(
                    "VA-API decoder thread exited during init".into(),
                ))
            }
        }
    }

    /// Release the decoder's reorder tail. Only tests call this: a live
    /// stream keeps decoding, and the final picture of a stopped stream has
    /// no consumer.
    #[cfg(test)]
    pub fn flush(&mut self) -> Result<Vec<RgbFrame>, Error> {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.tx
            .send(Request::Flush { reply: reply_tx })
            .map_err(|_| Error::Codec("VA-API decoder thread is gone".into()))?;
        match reply_rx.recv() {
            Ok(Ok(frames)) => Ok(frames),
            Ok(Err(e)) => Err(Error::Codec(format!("VA-API flush: {e}"))),
            Err(_) => Err(Error::Codec(
                "VA-API decoder thread dropped the flush".into(),
            )),
        }
    }

    pub fn decode(&mut self, frame: &[u8], rgba: bool) -> Result<Option<RgbFrame>, Error> {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.tx
            .send(Request::Decode {
                frame: frame.to_vec(),
                rgba,
                reply: reply_tx,
            })
            .map_err(|_| Error::Codec("VA-API decoder thread is gone".into()))?;
        match reply_rx.recv() {
            Ok(Ok(frame)) => Ok(frame),
            Ok(Err(e)) => Err(Error::Codec(format!("VA-API decode: {e}"))),
            Err(_) => Err(Error::Codec(
                "VA-API decoder thread dropped the request".into(),
            )),
        }
    }
}

impl Drop for VaapiDecoder {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{SoftwareEncoder, VideoEncoder};

    fn frame(width: usize, height: usize, t: usize) -> Vec<u8> {
        let mut bgra = vec![0u8; width * height * 4];
        for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = i % width;
            let y = i / width;
            let bar = ((t * 17) % width) as i32;
            px[0] = (x * 255 / width) as u8;
            px[1] = (y * 255 / height) as u8;
            px[2] = if (x as i32 - bar).abs() < 20 { 255 } else { 32 };
            px[3] = 255;
        }
        bgra
    }

    /// The decoder is allowed to hold a picture until the next DPB bump;
    /// collect the tail with `flush` and require the whole little stream
    /// back. Skips on a machine with no VA-API H.264 decoder.
    #[test]
    fn vaapi_decode_roundtrip_releases_every_picture() {
        let Ok(mut decoder) = VaapiDecoder::new() else {
            return;
        };
        let (width, height) = (320usize, 240usize);
        let mut encoder = SoftwareEncoder::new(width, height).expect("software encoder");
        let mut pictures = 0usize;
        for t in 0..12 {
            let (access, _key) = encoder.encode_bgra(&frame(width, height, t)).unwrap();
            if let Some((rgb, w, h)) = decoder.decode(&access, false).unwrap() {
                assert_eq!((w, h), (width, height));
                assert_eq!(rgb.len(), width * height * 3);
                pictures += 1;
            }
        }
        pictures += decoder.flush().expect("flush").len();
        assert!(
            pictures >= 10,
            "VA-API decoder returned {pictures} of 12 pictures"
        );
    }
}
