use crate::util::event_adder::{deblur_image, BlurInfo, EventAdder};
use aedat::base::{Decoder, ParseError, Stream, StreamContent};

use crate::util::reconstructor::ReconstructorError::ArgumentError;
use crate::util::threaded_decoder::{setup_packet_threads, PacketReceiver, TimestampedPacket};
use aedat::events_generated::Event;
use cv_convert::TryFromCv;
use nalgebra::DMatrix;
use num_traits::FromPrimitive;
use opencv::core::{Mat, MatTrait, MatTraitConst, Size, CV_8S, NORM_MINMAX};
use opencv::highgui;
use opencv::imgproc::resize;
use simple_error::SimpleError;
use std::cmp::max;
use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::time::Instant;
use std::{io, mem};
use thiserror::Error;

pub type IterVal = (
    Mat,
    Option<Instant>,
    Option<(f64, Vec<Event>, Vec<Event>, i64, i64)>,
    Option<u128>,
);
pub type IterRet = Option<Result<IterVal, ReconstructionError>>;

#[derive(Default)]
pub struct BlurredInput {
    pub image: Mat,
    pub exposure_begin_t: i64,
    pub exposure_end_t: i64,
}
unsafe impl Sync for Reconstructor {}
unsafe impl Send for Reconstructor {}

pub struct Reconstructor {
    show_display: bool,
    show_blurred_display: bool,
    packet_receiver: PacketReceiver,
    pub height: u16,
    pub width: u16,
    packet_queue: VecDeque<TimestampedPacket>,
    pub event_adder: EventAdder,
    latent_image_queue: VecDeque<Mat>,
    pub output_fps: f64,
    optimize_c: bool,
    optimize_controller: bool,
    pub target_latency: f64,
    mode: String,
    events_return_before: Vec<Event>, // Events occurring before the deblurred frame
    events_return_after: Vec<Event>,  // Events occurring during & after the deblurred frame
}

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum ReconstructorError {
    #[error("Parse error")]
    ParseError(#[from] ParseError),

    #[error("OpenCV error")]
    OpenCVError(#[from] opencv::Error),

    #[error("Argument error: `{0}`")]
    ArgumentError(String),
}

impl Reconstructor {
    pub async fn new(
        directory: String,
        aedat_filename_0: String,
        aedat_filename_1: String,
        mode: String,
        start_c: f64,
        optimize_c: bool,
        optimize_c_frequency: u32,
        optimize_controller: bool,
        display: bool,
        blurred_display: bool,
        output_fps: f64,
        deblur_only: bool,
        events_only: bool,
        target_latency: f64,
        simulate_latency: bool,
    ) -> Result<Reconstructor, ReconstructorError> {
        // assert!(!(deblur_only && events_only));
        let mut decoder_0 = match mode.as_str() {
            "file" => {
                Decoder::new_from_file(Path::new(&(directory.clone() + "/" + &aedat_filename_0)))?
            }
            #[cfg(target_family = "unix")]
            "socket" => Decoder::new_from_unix_stream(Path::new(
                &(directory.clone() + "/" + &aedat_filename_0),
            ))?,
            "tcp" => Decoder::new_from_tcp_stream(&(directory.clone() + "/" + &aedat_filename_0))?,
            _ => return Err(ArgumentError("Invalid source mode".to_string())),
        };

        assert!(target_latency > 0.0);
        let (height, width) = split_camera_info(&decoder_0.id_to_stream[&0]);

        let decoder_1 = match mode.as_str() {
            "file" => None,
            #[cfg(target_family = "unix")]
            "socket" => Some(Decoder::new_from_unix_stream(Path::new(
                &(directory + "/" + &aedat_filename_1),
            ))?),
            "tcp" => Some(Decoder::new_from_tcp_stream(
                &(directory + "/" + &aedat_filename_1),
            )?),
            _ => return Err(ArgumentError("Invalid source mode".to_string())),
        };

        let mut event_counter = Mat::default();

        // Signed integers, to allow for negative polarities dominating the interval
        unsafe {
            event_counter.create_rows_cols(height as i32, width as i32, CV_8S)?;
        }

        let packet_queue: VecDeque<TimestampedPacket> = VecDeque::new();
        let output_frame_length = (1000000.0 / output_fps) as i64;
        println!(
            "EDI output frame length: {} microseconds",
            output_frame_length
        );

        // Get the first frame and ignore events before it
        if decoder_1.is_none() {
            loop {
                if let Ok(p) = decoder_0.next().unwrap() {
                    if matches!(
                        decoder_0.id_to_stream.get(&p.stream_id).unwrap().content,
                        StreamContent::Frame
                    ) {
                        match aedat::frame_generated::size_prefixed_root_as_frame(&p.buffer) {
                            Ok(result) => result,
                            Err(_) => {
                                panic!("the packet does not have a size prefix");
                            }
                        };
                        break;
                    }
                }
            }
        }

        let mut r = Reconstructor {
            show_display: display,
            show_blurred_display: blurred_display,
            packet_receiver: setup_packet_threads(decoder_0, decoder_1, simulate_latency),
            height,
            width,
            packet_queue,
            event_adder: EventAdder::new(
                height,
                width,
                output_frame_length,
                start_c,
                optimize_c,
                optimize_c_frequency,
                deblur_only,
                events_only,
            ),
            latent_image_queue: Default::default(),
            output_fps,
            optimize_c,
            optimize_controller,
            target_latency,
            mode,
            events_return_before: vec![],
            events_return_after: vec![],
        };
        let blur_info = fill_packet_queue_to_frame(
            &mut r.packet_receiver,
            &mut r.packet_queue,
            r.height as i32,
            r.width as i32,
        )
        .await
        .unwrap();

        let frame_exp_dt = blur_info.exposure_end_t - blur_info.exposure_begin_t;
        if frame_exp_dt < r.event_adder.interval_t && r.event_adder.deblur_only {
            r.event_adder.interval_t = max(frame_exp_dt, 1);
            r.output_fps = 1.0e6 / frame_exp_dt as f64;
        }
        r.event_adder.blur_info = Some(blur_info);

        Ok(r)
    }

    pub fn set_optimize_c(&mut self, optimize: bool, frequency: u32) {
        self.optimize_c = optimize;
        self.event_adder.optimize_c = optimize;
        self.event_adder.optimize_c_frequency = frequency;
    }

    /// Get the next reconstructed image
    pub async fn next(&mut self, with_events: bool) -> IterRet {
        if with_events {
            assert!(self.event_adder.deblur_only);
        }
        return match self.latent_image_queue.pop_front() {
            // If we have a queue of images already, just return the next one
            Some(image) => Some(Ok((image, None, None, None))), // TODO: what about event queues?

            // Else we need to rebuild the queue
            _ => {
                let now = Instant::now();

                if self.event_adder.next_blur_info.is_some() {
                    mem::swap(
                        &mut self.event_adder.blur_info,
                        &mut self.event_adder.next_blur_info,
                    );
                    self.event_adder.next_blur_info = None;
                }

                // let join_handle: thread::JoinHandle<_> = thread::spawn(|| {
                let latency = match self.get_more_images().await {
                    Ok(a) => a,
                    Err(_) => return None,
                };
                // });
                let running_fps = self.latent_image_queue.len() as f64
                    / now.elapsed().as_millis() as f64
                    * 1000.0;
                print!(
                    "\r{} frames in  {}ms -- Current FPS: {:.2}, Current c: {:.5}",
                    self.latent_image_queue.len(),
                    now.elapsed().as_millis(),
                    running_fps,
                    self.event_adder.current_c
                );
                if self.optimize_controller
                    && ((1000000.0 / running_fps) as i64 - self.event_adder.interval_t).abs()
                        > 1000000 / 50000
                {
                    // self.event_adder.interval_t =
                    //     (1000000.0 / running_fps).max(1000000.0 / self.output_fps) as i64;
                    // print!(" Target FPS: {}", 1000000 / self.event_adder.interval_t);
                    // self.event_adder.optimize_c = false;
                } else {
                    // self.event_adder.optimize_c = self.optimize_c;
                }
                io::stdout().flush().unwrap();
                match self.latent_image_queue.pop_front() {
                    None => {
                        panic!("No images in the returned queue")
                    }
                    Some(image) => {
                        // TODO: handle error
                        // debug_assert!(
                        //     self.event_adder
                        //         .blur_info
                        //         .as_ref()
                        //         .unwrap()
                        //         .exposure_begin_t
                        //         < self.event_adder.last_interval_start_timestamp
                        // );
                        //
                        // debug_assert!({
                        //     let true_frame_dt =
                        //         self.event_adder.blur_info.as_ref().unwrap().exposure_end_t
                        //             - self
                        //                 .event_adder
                        //                 .blur_info
                        //                 .as_ref()
                        //                 .unwrap()
                        //                 .exposure_begin_t;
                        //     let img_dt_secs = true_frame_dt as f64 / 1000000.0;
                        //     let frame_length_secs = 1.0 / self.output_fps as f64;
                        //     img_dt_secs >= frame_length_secs
                        // });

                        return match with_events {
                            true => Some(Ok((
                                image,
                                Some(
                                    self.event_adder
                                        .blur_info
                                        .as_ref()
                                        .unwrap()
                                        .packet_timestamp,
                                ),
                                Some((
                                    self.event_adder.current_c,
                                    self.events_return_before.clone(),
                                    self.events_return_after.clone(),
                                    self.event_adder
                                        .blur_info
                                        .as_ref()
                                        .unwrap()
                                        .exposure_begin_t,
                                    self.event_adder.last_interval_start_timestamp,
                                )),
                                Some(latency),
                            ))),
                            false => Some(Ok((
                                image,
                                Some(
                                    self.event_adder
                                        .blur_info
                                        .as_ref()
                                        .unwrap()
                                        .packet_timestamp,
                                ),
                                None,
                                Some(latency),
                            ))),
                        };
                    }
                }
            }
        };
    }

    /// Generates reconstructed images from the next packet of events
    async fn get_more_images(&mut self) -> Result<u128, SimpleError> {
        while let Some(p) = self.packet_queue.pop_front() {
            match FromPrimitive::from_u32(p.packet.stream_id) {
                Some(StreamContent::Frame) => {
                    panic!("Unhandled frame?")
                }
                Some(StreamContent::Events) => {
                    self.event_adder.sort_events(p.packet);
                }
                _ => {
                    println!("debug 2")
                }
            }
        }

        let deblur_res = {
            if self.show_blurred_display {
                let tmp_blurred_mat =
                    Mat::try_from_cv(&self.event_adder.blur_info.as_ref().unwrap().blurred_image)
                        .unwrap();
                _show_display_force("blurred input", &tmp_blurred_mat, 1, false);
            }
            deblur_image(&mut self.event_adder)
        };

        let latency = (Instant::now()
            - self
                .event_adder
                .blur_info
                .as_ref()
                .unwrap()
                .packet_timestamp)
            .as_millis();
        println!("  Latency is {}ms", latency);

        match (
            self.mode.as_str(),
            self.optimize_controller,
            self.optimize_c,
            latency > self.target_latency as u128,
            self.event_adder.optimize_c,
        ) {
            ("file", _, _, _, _) => {
                // Don't do anything, since latency doesn't make sense in this context. (File reads
                // happen instantaneously)
            }
            (_, true, true, true, true) => {
                println!("DISABLING C-OPTIMIZATION");
                self.event_adder.optimize_c = false;
            }
            (_, true, true, false, false) => {
                println!("ENABLING C-OPTIMIZATION");
                self.event_adder.optimize_c = true;
            }
            (_, _, _, _, _) => {}
        }

        let next_blur_info = match fill_packet_queue_to_frame(
            &mut self.packet_receiver,
            &mut self.packet_queue,
            self.height as i32,
            self.width as i32,
        )
        .await
        {
            Ok(blur_info) => {
                let frame_exp_dt = blur_info.exposure_end_t - blur_info.exposure_begin_t;
                if frame_exp_dt < self.event_adder.interval_t && self.event_adder.deblur_only {
                    self.event_adder.interval_t = max(frame_exp_dt, 1);
                    self.output_fps = 1.0e6 / frame_exp_dt as f64;
                }
                Some(blur_info)
            }

            Err(_) => None,
        };

        match (deblur_res, next_blur_info) {
            (None, _) => {
                panic!("No images returned from deblur call")
            }
            (Some(deblur_return), Some(next_blur_info)) => {
                self.event_adder.latent_image = deblur_return.ret_vec.last().unwrap().clone();
                self.event_adder.last_interval_start_timestamp =
                    deblur_return.last_interval_start_timestamp;
                self.latent_image_queue
                    .append(&mut VecDeque::from(deblur_return.ret_vec));

                {
                    /*
                    set the 'return after' queue. Keeps the events from during the exposure time,
                    and the events occurring afterwards.
                    */
                    let mut tmp_vec = vec![];
                    mem::swap(&mut tmp_vec, &mut self.event_adder.event_during_queue);
                    self.events_return_after = tmp_vec;
                    self.events_return_after
                        .append(&mut self.event_adder.event_after_queue.clone());
                }

                {
                    // set the 'return before' queue
                    let mut tmp_vec = vec![];
                    mem::swap(&mut tmp_vec, &mut self.event_adder.event_before_queue);
                    self.events_return_before = tmp_vec;
                }

                self.event_adder.reset_event_queues();
                self.event_adder.next_blur_info = Some(next_blur_info);
                self.event_adder.current_c = deblur_return.found_c;
            }
            _ => return Err(SimpleError::new("End of aedat file")),
        };

        Ok(latency)
    }
}

/// Read packets until the next APS frame is reached (inclusive)
async fn fill_packet_queue_to_frame(
    packet_receiver: &mut PacketReceiver,
    packet_queue: &mut VecDeque<TimestampedPacket>,
    height: i32,
    width: i32,
) -> Result<BlurInfo, SimpleError> {
    let blur_info = loop {
        match packet_receiver.next().await {
            Some(p) => {
                if matches!(
                    FromPrimitive::from_u32(p.packet.stream_id),
                    Some(StreamContent::Frame)
                ) {
                    let frame =
                        match aedat::frame_generated::size_prefixed_root_as_frame(&p.packet.buffer)
                        {
                            Ok(result) => result,
                            Err(_) => {
                                panic!("the packet does not have a size prefix");
                            }
                        };

                    let frame_px = frame.pixels().unwrap();
                    let mut image = DMatrix::<f64>::zeros(height as usize, width as usize);
                    for (row_idx, mut im_row) in image.row_iter_mut().enumerate() {
                        for (col_idx, im_px) in im_row.iter_mut().enumerate() {
                            *im_px = frame_px[row_idx * width as usize + col_idx] as f64 / 255.0;
                        }
                    }

                    let blur_info = BlurInfo::new(
                        image,
                        frame.exposure_begin_t(),
                        frame.exposure_end_t(),
                        p.timestamp,
                    );

                    break blur_info;
                } else if matches!(
                    FromPrimitive::from_u32(p.packet.stream_id),
                    Some(StreamContent::Events)
                ) {
                    packet_queue.push_back(p);
                }
            }
            None => return Err(SimpleError::new("End of aedat file")),
        }
    };

    match packet_receiver.next().await {
        Some(p) => {
            if matches!(
                FromPrimitive::from_u32(p.packet.stream_id),
                Some(StreamContent::Events)
            ) {
                packet_queue.push_back(p);
            } else if p.packet.stream_id == 2 || p.packet.stream_id == 3 {
                // Do nothing
            } else {
                return Err(SimpleError::new("TODO handle sparse events"));
            }
        }
        None => return Err(SimpleError::new("End of aedat file")),
    };

    Ok(blur_info)
}

#[derive(Debug)]
pub struct ReconstructionError {
    message: String,
}

impl ReconstructionError {
    pub fn _new(message: &str) -> ReconstructionError {
        ReconstructionError {
            message: message.to_string(),
        }
    }
}

impl std::fmt::Display for ReconstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::convert::From<ParseError> for ReconstructionError {
    fn from(error: ParseError) -> Self {
        ReconstructionError {
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LatentImage {
    pub frame: Mat,
}

// use async_trait::async_trait;
//
// #[async_trait]
// impl Iterator for Reconstructor {
//     type Item = Result<Mat, ReconstructionError>;
//
//     /// Get the next reconstructed image
//     async fn next(&mut self) -> Option<Self::Item> {
//         return match self.latent_image_queue.pop_front() {
//             // If we have a queue of images already, just return the next one
//             Some(image) => Some(Ok(image)),
//
//             // Else we need to rebuild the queue
//             _ => {
//                 let now = Instant::now();
//
//                 if self.event_adder.next_blur_info.is_some() {
//                     mem::swap(&mut self.event_adder.blur_info, &mut self.event_adder.next_blur_info);
//                     self.event_adder.next_blur_info = None;
//                 }
//                 //
//                 //     self.fill_packet_queue_to_frame()
//
//
//                 // let join_handle: thread::JoinHandle<_> = thread::spawn(|| {
//                 match self.get_more_images().await {
//                     Ok(_) => {}
//                     Err(_) => return None
//                 };
//                 // });
//                 let running_fps = self.latent_image_queue.len() as f64
//                     / now.elapsed().as_millis() as f64 * 1000.0;
//                 print!(
//                     "\r{} frames in  {}ms -- Current FPS: {:.2}, Current c: {:.5}",
//                     self.latent_image_queue.len(),
//                     now.elapsed().as_millis(),
//                     running_fps,
//                     self.event_adder.current_c
//                 );
//                 io::stdout().flush().unwrap();
//                 match self.latent_image_queue.pop_front() {
//                     None => {
//                         panic!("No images in the returned queue")
//                     }
//                     Some(image) => {
//                         return Some(Ok(image));
//                     }
//                 }
//             }
//         };
//     }
// }

fn split_camera_info(stream: &Stream) -> (u16, u16) {
    (stream.height, stream.width)
}

/// If [`MyArgs`]`.show_display`, shows the given [`Mat`] in an OpenCV window
pub fn show_display(window_name: &str, mat: &Mat, wait: i32, reconstructor: &Reconstructor) -> i32 {
    if reconstructor.show_display {
        let mut tmp = Mat::default();

        if mat.rows() != 540 {
            let factor = mat.rows() as f32 / 540.0;
            resize(
                mat,
                &mut tmp,
                Size {
                    width: (mat.cols() as f32 / factor) as i32,
                    height: 540,
                },
                0.0,
                0.0,
                0,
            )
            .unwrap();
            highgui::imshow(window_name, &tmp).unwrap();
        } else {
            highgui::imshow(window_name, mat).unwrap();
        }
        return highgui::wait_key(wait).unwrap();
    }
    -1
}

/// TODO: Remove. Just for debugging.
pub fn _show_display_force(window_name: &str, mat: &Mat, wait: i32, normalize: bool) {
    let mut normed = mat.clone();
    let mut tmp = Mat::default();
    if normalize {
        opencv::core::normalize(
            &mat,
            &mut normed,
            0.0,
            1.0,
            NORM_MINMAX,
            -1,
            &opencv::core::no_array(),
        )
        .unwrap();
    }

    if mat.rows() != 540 {
        let factor = mat.rows() as f32 / 540.0;
        resize(
            &normed,
            &mut tmp,
            Size {
                width: (mat.cols() as f32 / factor) as i32,
                height: 540,
            },
            0.0,
            0.0,
            0,
        )
        .unwrap();
        highgui::imshow(window_name, &tmp).unwrap();
    } else {
        highgui::imshow(window_name, mat).unwrap();
    }
    highgui::wait_key(wait).unwrap();
}
