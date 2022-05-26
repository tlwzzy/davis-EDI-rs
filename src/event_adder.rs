use std::collections::VecDeque;
use aedat::base::Packet;
use aedat::events_generated::Event;
use opencv::core::{bitwise_or, BORDER_DEFAULT, count_non_zero, CV_64F, ElemMul, exp, log, Mat, MatExprTraitConst, MatTrait, MatTraitConst, min_max_idx, no_array, NORM_MINMAX, Point, Size, sqrt, sum_elems};
use opencv::imgproc::{erode, get_structuring_element, MORPH_CROSS, MORPH_OPEN, morphology_ex, sobel, THRESH_BINARY, threshold};
use crate::reconstructor::{BlurredInput, show_display_force};

#[derive(Default)]
struct Interval {
    pub idx: i32,
    pub e_accumuluator: Mat,
    pub c_accumuluator: Mat,
    pub latent_image: Mat,
}

#[derive(Default, Clone)]
struct BlurryBookend {
    pub output_interval_idx: usize, // corresponding output_interval
    pub interval_timestamp: i64, // at what point in the interval does the image start (or end)
    pub image_accumulated_events: Mat,
    pub nonimage_accumulated_events: Mat, // events during this interval which are not during the blurry image exposure time

}

#[derive(Default, Clone)]
pub struct BlurInfo {
    pub blurred_image: Mat,
    latent_image: Mat,
    exposure_begin_t: i64,
    exposure_end_t: i64,
    begin_bookend: BlurryBookend,
    end_bookend: BlurryBookend,
    mid_idx: usize,
    pub init: bool, // TODO: not very rusty
}

impl BlurInfo {
    pub fn new(image: Mat,
               exposure_begin_t: i64,
               exposure_end_t: i64,
                t_shift: i64,
                interval_t: i64,
                height: i32,
                width: i32,
                intervals_popped: i32,
    ) -> BlurInfo {
        let begin_bookend = BlurryBookend {
            output_interval_idx: ((exposure_begin_t - t_shift) / interval_t) as usize,
            interval_timestamp: (exposure_begin_t - t_shift) % interval_t,
            image_accumulated_events: Mat::zeros(height, width, CV_64F).unwrap().to_mat().unwrap(),
            nonimage_accumulated_events: Mat::zeros(height, width, CV_64F).unwrap().to_mat().unwrap(),
        };

        let end_bookend = BlurryBookend {
            output_interval_idx: ((exposure_end_t - t_shift) / interval_t) as usize,
            interval_timestamp: (exposure_end_t - t_shift) % interval_t,
            image_accumulated_events: Mat::zeros(height, width, CV_64F).unwrap().to_mat().unwrap(),
            nonimage_accumulated_events: Mat::zeros(height, width, CV_64F).unwrap().to_mat().unwrap(),
        };

        let mid_idx = (end_bookend.output_interval_idx - begin_bookend.output_interval_idx)/2 + 1 + intervals_popped as usize;

        BlurInfo {
            blurred_image: image,
            latent_image: Mat::zeros(height, width, CV_64F).unwrap().to_mat().unwrap(),
            exposure_begin_t,
            exposure_end_t,
            begin_bookend,
            end_bookend,
            mid_idx,
            init: true,
        }
    }
}

pub struct EventAdder {
    t_shift: i64,
    interval_t: i64,
    height: usize,
    width: usize,
    interval_count: i32,
    pub intervals_popped: i32,
    sum_mat: Mat,
    latent_image: Mat,
    return_queue: VecDeque<Mat>,
    event_intervals: VecDeque<Interval>,
    pub blur_info: BlurInfo,
    pub next_blur_info: BlurInfo,
    edge_boundary: Mat,
    current_c: f64,
}

impl EventAdder {
    pub fn new(height: usize, width:usize, t_shift: i64, output_frame_length: i64) -> EventAdder {
        EventAdder {
            t_shift,
            interval_t: output_frame_length,
            height,
            width,
            interval_count: 0,
            intervals_popped: 0,
            sum_mat: Default::default(),
            latent_image: Mat::zeros(height as i32, width as i32, CV_64F).unwrap().to_mat().unwrap(),
            return_queue: VecDeque::new(),
            event_intervals: VecDeque::new(),
            blur_info: Default::default(),
            next_blur_info: Default::default(),
            edge_boundary: Mat::zeros(height as i32, width as i32, CV_64F).unwrap().to_mat().unwrap(),
            current_c: 0.3,
        }
    }
    fn push_interval(&mut self) {
        self.event_intervals.push_back(Interval {
            idx: self.interval_count,
            e_accumuluator: Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap(),
            c_accumuluator: Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap(),
            latent_image: Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap(),
        });
        self.interval_count += 1;
    }


    // TODO: make sure it's called at the right time for deblurred intervals
    fn pop_interval(&mut self) {
        let mut interval = match self.event_intervals.pop_front() {
            None => {panic!("No interval to pop")}
            Some(a) => {a}
        };
        self.intervals_popped += 1;
        interval.c_accumuluator = (interval.e_accumuluator.clone() * self.current_c).into_result().unwrap().to_mat().unwrap();
        interval.latent_image = (self.latent_image.clone() + interval.c_accumuluator).into_result().unwrap().to_mat().unwrap();
        self.latent_image = interval.latent_image;
        let mut l = Mat::default();
        exp(&self.latent_image, &mut l).unwrap();
        self.return_queue.push_back(l.clone());
    }

    /// to do: need separate notion of blurred intervals for the starting and ending of blurred image.
    /// Have one big vecdequeue of event accumulator intervals. Each item holds the output frame index
    /// and the accumulated event array.
    /// Have struct members blurry_interval_start, blurry_interval_end. Each is a BlurryBookend
    /// BlurryBookend {
    ///     output_interval_idx: i64, // corresponding output_interval
    ///     image_accumulated_events
    ///     nonimage_accumulated_events // events during this interval which are not during the blurry image exposure time
    ///     interval_timestamp      // at what point in the interval does the image start (or end)
    /// }
    ///
    /// The T for getting the starting latent image:
    /// T = non_bookend_interval_count +
    ///     ((t_shift - blurry_interval_start.interval_timestamp) + blurry_interval_end.interval_timestamp) / t_shift

    pub fn add_events(&mut self, packet: Packet, current_blurred_image: &mut BlurredInput) -> Option<VecDeque<Mat>> {
        if self.event_intervals.len() == 0 {
            self.push_interval();
        }

        self.return_queue = VecDeque::new();
        let event_packet= match aedat::events_generated::size_prefixed_root_as_event_packet(&packet.buffer) {
            Ok(result) => result,
            Err(_) => {
                panic!("the packet does not have a size prefix");
            }
        };

        let event_arr = match event_packet.elements() {
            None => { return None}
            Some(events) => { events }
        };

        for event in event_arr {
            self.process_event(event);
        }

        return match self.return_queue.len() {
            0 => { None},
            _ => {Some(self.return_queue.clone())}
        }
    }

    fn process_event(&mut self, event: &Event) {
        if event.t() < self.t_shift {
            return;
        }

        let local_t = event.t() - self.t_shift;
        let interval_idx = (local_t / self.interval_t) as usize;
        while interval_idx - self.intervals_popped as usize >= self.event_intervals.len() {
            self.push_interval();
        }

        // First, add it to a blurry bookend if it falls in one of those intervals
        if self.blur_info.init {
            if event.t() >= self.blur_info.exposure_begin_t && event.t() < self.blur_info.exposure_end_t {
                self.add_to_edge_boundary(event);
            }

            match interval_idx {
                a if a == self.blur_info.begin_bookend.output_interval_idx as usize => {
                    match local_t {
                        t if t <= self.blur_info.begin_bookend.interval_timestamp => {
                            add_to_event_counter(&mut self.blur_info.begin_bookend.nonimage_accumulated_events, event);
                        }
                        _ => {
                            add_to_event_counter(&mut self.blur_info.begin_bookend.image_accumulated_events, event);
                        }
                    }
                },
                a if a == self.blur_info.end_bookend.output_interval_idx as usize => {
                    match local_t {
                        t if t < self.blur_info.end_bookend.interval_timestamp => {
                            add_to_event_counter(&mut self.blur_info.end_bookend.image_accumulated_events, event);
                        }
                        _ => {
                            add_to_event_counter(&mut self.blur_info.end_bookend.nonimage_accumulated_events, event);
                        }
                    }
                },
                a if a > self.blur_info.end_bookend.output_interval_idx as usize => {
                    // Then we have everything we need to deblur the image
                    for _ in 0..self.blur_info.begin_bookend.output_interval_idx - self.event_intervals[0].idx as usize {
                        self.pop_interval();
                    }
                    assert_eq!(self.event_intervals[0].idx as usize, self.blur_info.begin_bookend.output_interval_idx);
                    // self.deblur_image();
                    self.optimize_c();

                    // First, get the return images from the backwards direction of the middle of the blurry image
                    let mut temp_latent_image = self.latent_image.clone();
                    let mut temp_return_queue = VecDeque::new();

                    for i in (0..self.blur_info.mid_idx - self.intervals_popped as usize).rev() {
                        let interval = &mut self.event_intervals[i];
                        interval.c_accumuluator = (interval.e_accumuluator.clone() * self.current_c).into_result().unwrap().to_mat().unwrap();
                        interval.latent_image = (&temp_latent_image - &interval.c_accumuluator).into_result().unwrap().to_mat().unwrap();
                        temp_latent_image = interval.latent_image.clone();
                        let mut l = Mat::default();
                        exp(&temp_latent_image, &mut l).unwrap();
                        temp_return_queue.push_front(l.clone());
                    }
                    for i in 0..self.blur_info.mid_idx - self.intervals_popped as usize {
                        self.event_intervals.pop_front().unwrap();
                        self.return_queue.push_back(temp_return_queue[i].clone());  // TODO: don't use so many temporaries
                        self.intervals_popped += 1;
                    }
                    let mut l = Mat::default();
                    exp(&self.latent_image, &mut l).unwrap();
                    self.return_queue.push_back(l.clone());

                    for _ in self.blur_info.mid_idx..self.blur_info.end_bookend.output_interval_idx {
                        self.pop_interval();
                    }
                    self.blur_info = self.next_blur_info.clone();
                    self.next_blur_info = Default::default();
                    self.edge_boundary = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();
                },
                _ => {}
            }
        }

        // Then add it to its regular interval
        let interval = &mut self.event_intervals[interval_idx - self.intervals_popped as usize];
        add_to_event_counter(&mut interval.e_accumuluator, event);
        return
    }

    fn deblur_image(&mut self, c_threshold: f64) {
        if self.blur_info.end_bookend.output_interval_idx == self.blur_info.begin_bookend.output_interval_idx + 1 {
            self.blur_info.mid_idx = self.intervals_popped as usize;
        }

        self.sum_mat = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();
        let mut temp_exp = Mat::default();

        let mut interval_count = 0.0;
        let mut c_sum = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();
        let mut exp_sum = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();
        if self.blur_info.begin_bookend.output_interval_idx != self.blur_info.end_bookend.output_interval_idx - 1 {
            for i in (self.blur_info.begin_bookend.output_interval_idx + 1..self.blur_info.mid_idx).rev() {
                let interval = &mut self.event_intervals[i as usize - self.intervals_popped as usize];
                interval.c_accumuluator =
                    (&interval.e_accumuluator * &c_threshold).into_result().unwrap().to_mat().unwrap();
                c_sum = (c_sum - &interval.c_accumuluator).into_result().unwrap().to_mat().unwrap();
                exp(&c_sum, &mut temp_exp).unwrap();
                exp_sum = (exp_sum + &temp_exp).into_result().unwrap().to_mat().unwrap();
                interval_count += 1.0;
            }
        }
        let interval = &mut self.event_intervals[self.blur_info.begin_bookend.output_interval_idx as usize - self.intervals_popped as usize];
        interval.c_accumuluator =
            (&interval.e_accumuluator * &c_threshold).into_result().unwrap().to_mat().unwrap();
        c_sum = (c_sum - &interval.c_accumuluator).into_result().unwrap().to_mat().unwrap();
        exp(&c_sum, &mut temp_exp).unwrap();
        let proportion1 = (self.interval_t - self.blur_info.begin_bookend.interval_timestamp) as f64 / self.interval_t as f64;
        temp_exp = (temp_exp * proportion1).into_result().unwrap().to_mat().unwrap();
        exp_sum = (exp_sum + &temp_exp).into_result().unwrap().to_mat().unwrap();
        interval_count += proportion1;


        c_sum = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();
        if self.blur_info.begin_bookend.output_interval_idx != self.blur_info.end_bookend.output_interval_idx - 1 {
            for i in self.blur_info.mid_idx..self.blur_info.end_bookend.output_interval_idx {
                let interval = &mut self.event_intervals[i as usize - self.intervals_popped as usize];
                // assert_eq!(interval.idx, (self.blur_info.begin_bookend.output_interval_idx + i) as i32);
                interval.c_accumuluator =
                    (&interval.e_accumuluator * &c_threshold).into_result().unwrap().to_mat().unwrap();
                c_sum = (c_sum + &interval.c_accumuluator).into_result().unwrap().to_mat().unwrap();
                exp(&c_sum, &mut temp_exp).unwrap();
                exp_sum = (exp_sum + &temp_exp).into_result().unwrap().to_mat().unwrap();
                interval_count += 1.0;
            }
        }
        let interval = &mut self.event_intervals[self.blur_info.end_bookend.output_interval_idx as usize - self.intervals_popped as usize];
        interval.c_accumuluator =
            (&interval.e_accumuluator * &c_threshold).into_result().unwrap().to_mat().unwrap();
        c_sum = (c_sum + &interval.c_accumuluator).into_result().unwrap().to_mat().unwrap();
        exp(&c_sum, &mut temp_exp).unwrap();
        let proportion2 = self.blur_info.end_bookend.interval_timestamp as f64 / self.interval_t as f64;
        temp_exp = (temp_exp * proportion2).into_result().unwrap().to_mat().unwrap();
        exp_sum = (exp_sum + &temp_exp).into_result().unwrap().to_mat().unwrap();
        interval_count += proportion2;

        self.sum_mat = exp_sum;
        self.sum_mat = (self.sum_mat.clone() / interval_count).into_result().unwrap().to_mat().unwrap();

        let mut log_sub = Mat::default();
        log(&self.sum_mat, &mut log_sub).unwrap();

        let mut log_b = Mat::default();
        log(&self.blur_info.blurred_image, &mut log_b).unwrap();

        let log_l = (log_b - log_sub).into_result().unwrap().to_mat().unwrap();
        self.latent_image = log_l.clone();

        let mut l = Mat::default();
        exp(&log_l, &mut l).unwrap();
    }

    fn optimize_c(&mut self) {



        let (mut min_c, mut max_c, n_points) = (0.0, 0.5, 60);
        let (mut energy1, mut energy2, mut c1, mut c2) = (0.0, 0.0, 0.0, 0.0);
        let (mut latent1, mut latent2) = (Mat::default(), Mat::default());

        let mut cec_norm = Mat::default();

        // Uncomment the lines below to use the optimized c search. NOT currently yielding good
        // results!

        // create fibonacci sequence
        // let mut fib = vec![1.0; 22];
        // for i in 2..fib.len() {
        //     fib[i] = fib[i-1] + fib[i-2];
        // }
        //
        // let mut fib_index = 2;
        // while fib[fib_index-1] < n_points as f64 {
        //     fib_index += 1;
        // }
        //
        //
        // for k in 0..fib_index-1 {
        //     if k == 0 {
        //         c1 = min_c + fib[fib_index - k - 1]  / fib[fib_index-k+1] * (max_c - min_c);
        //         c2 = max_c - fib[fib_index - k - 1]  / fib[fib_index-k+1] * (max_c - min_c);
        //         match self.get_energy(c1) {
        //             (a, b) => { energy1 = a; latent1 = b; }
        //         };
        //         opencv::core::normalize(&latent1, &mut cec_norm, 0.0, 1.0, NORM_MINMAX, -1, &opencv::core::no_array());
        //         // show_display_force("latent1", &cec_norm, 1);
        //         match self.get_energy(c2) {
        //             (a, b) => { energy2 = a; latent2 = b; }
        //         }
        //         opencv::core::normalize(&latent2, &mut cec_norm, 0.0, 1.0, NORM_MINMAX, -1, &opencv::core::no_array());
        //         // show_display_force("latent2", &cec_norm, 0);
        //     }
        //     if energy1 < energy2 {
        //         max_c = c2;
        //         c2 = c1;
        //         energy2 = energy1;
        //         latent2 = latent1;
        //         c1 = min_c + fib[fib_index - k - 2] / fib[fib_index - k + 1] * (max_c - min_c);
        //         match self.get_energy(c1) {
        //             (a, b) => { energy1 = a; latent1 = b; }
        //         };
        //         opencv::core::normalize(&latent1, &mut cec_norm, 0.0, 1.0, NORM_MINMAX, -1, &opencv::core::no_array());
        //         // show_display_force("latent1", &cec_norm, 0);
        //     } else {
        //         min_c = c1;
        //         c1 = c2;
        //         energy1 = energy2;
        //         latent1 = latent2;
        //         c2 = max_c - fib[fib_index - k - 1]  / fib[fib_index-k+1] * (max_c - min_c);
        //         match self.get_energy(c2) {
        //             (a, b) => { energy2 = a; latent2 = b; }
        //         };
        //         opencv::core::normalize(&latent2, &mut cec_norm, 0.0, 1.0, NORM_MINMAX, -1, &opencv::core::no_array());
        //         // show_display_force("latent2", &cec_norm, 0);
        //     }
        // }
        // if energy1 < energy2 {
        //     self.current_c = c1;
        //     self.latent_image = latent1;
        // } else {
        //     self.current_c = c2;
        //     self.latent_image = latent2;
        // }
        // println!("Optimal c is: {}", self.current_c);

        match self.get_energy(0.3) {
            (a, b) => { energy2 = a; latent2 = b; }
        };
        self.current_c = 0.3;
        self.latent_image = latent2;
        opencv::core::normalize(
            &self.latent_image,
            &mut cec_norm,
            0.0,
            1.0,
            NORM_MINMAX,
            -1,
            &opencv::core::no_array(),
        ).unwrap();
        show_display_force("LATENT", &cec_norm, 0);
    }

    fn get_energy(&mut self, c_threshold: f64) -> (f64, Mat) {
        self.deblur_image(c_threshold);

        let mut edge_thresh_f64 = Mat::default();
        let mut latent_thresh_f64 = Mat::default();

        let edge_boundary_grad = self.get_gradient_magnitude(&self.edge_boundary);
        let cutoff = 4.0 * sum_elems(&edge_boundary_grad).unwrap()[0] / (self.width as f64 * self.height as f64);
        let t1 = threshold(&edge_boundary_grad, &mut edge_thresh_f64, cutoff, 1.0, THRESH_BINARY).unwrap();
        // show_display_force("edge grad", &edge_thresh_f64, 0);
        let edge_thinned = self.thin(&mut edge_thresh_f64);

        // scale mat to values [0,1]

        let mut latent_image_exp = Mat::default();
        exp(&self.latent_image, &mut latent_image_exp).unwrap();
        let mat_f1 = (&latent_image_exp / 255.0).into_result().unwrap().to_mat().unwrap();
        let latent_image_grad = self.get_gradient_magnitude(&mat_f1);
        let cutoff = 4.0 * sum_elems(&latent_image_grad).unwrap()[0] / (self.width as f64 * self.height as f64);
        threshold(&latent_image_grad, &mut latent_thresh_f64, cutoff, 1.0, THRESH_BINARY).unwrap();
        // show_display_force("latent grad", &latent_image_grad, 0);
        let latent_thinned = self.thin(&mut latent_thresh_f64.clone());

        // let edge_bytes = edge_thresh_f64.data_bytes_mut().unwrap();
        // let latent_bytes = latent_thresh_f64.data_bytes_mut().unwrap();
        // let latent_grad_bytes = latent_image_grad.data_bytes_mut().unwrap();
        let mut sharpness = 0;
        let mut tv = 0.0;
        for i in 0..self.height as i32 {
            for j in 0..self.width as i32 {
                // let t : &f64 = edge_thinned.at_2d(i, j).unwrap();
                if *edge_thinned.at_2d::<f64>(i, j).unwrap() == 1.0 && *latent_thinned.at_2d::<f64>(i, j).unwrap() == 1.0 {
                    sharpness += 1;
                }
                // sharpness += edge_bytes[i] as i64 * latent_bytes[i] as i64;
                tv += *latent_thresh_f64.at_2d::<f64>(i, j).unwrap();
            }
        }

        // Assume for now that lambda = 0.2 (TODO)
        let energy = 0.03 * tv - sharpness as f64;
        // println!("c {}, tv {}, Sharpness {}, energy {}", c_threshold, 0.03*tv, sharpness, energy);
        // assert!(energy >= 0.0);
        (energy, self.latent_image.clone())
    }

    fn get_gradient_magnitude(&self, mat: &Mat) -> Mat {
        let mut max = 0.0;
        min_max_idx(&mat, None, Some(&mut max), None, None, &no_array()).unwrap();

        // show_display_force("mat", &mat, 0);

        let mut sobel_dx = Mat::default();
        let mut sobel_dy = Mat::default();
        let mut grad = Mat::default();
        // ksize = 1 means no gaussian smoothing is done
        sobel(&mat, &mut sobel_dx, -1, 1, 0, 1, 1.0, 0.0, BORDER_DEFAULT).unwrap();
        // show_display_force("dx", &sobel_dx, 0);
        sobel(&mat, &mut sobel_dy, -1, 0, 1, 1, 1.0, 0.0, BORDER_DEFAULT).unwrap();
        // show_display_force("dy", &sobel_dy, 0);
        sobel_dx = sobel_dx.clone().elem_mul(sobel_dx.clone()).into_result().unwrap().to_mat().unwrap();
        sobel_dy = sobel_dy.clone().elem_mul(sobel_dy.clone()).into_result().unwrap().to_mat().unwrap();
        sobel_dx = (sobel_dx + sobel_dy).into_result().unwrap().to_mat().unwrap();
        sqrt(&sobel_dx, &mut grad).unwrap();    // This is the gradient magnitude image
        grad
    }

    /// The original Matlab implementation used thinning. I adapted this function
    /// from https://theailearner.com/tag/thinning-opencv/
    fn thin(&self, mat: &mut Mat) -> Mat {

        let kernel = get_structuring_element(MORPH_CROSS, Size { width: 3, height: 3 }, Point { x: -1, y: -1 }).unwrap();
            // get_structuring_element(MORPH_CROSS, Size { width: 3, height: 3 }).unwrap();
        let mut thinned = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();

        while count_non_zero(mat).unwrap() != 0 {
            let mut eroded = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();
            erode(mat, &mut eroded, &kernel, Point { x: -1, y: -1 }, 1, BORDER_DEFAULT, Default::default()).unwrap();
            let mut opened = Mat::zeros(self.height as i32, self.width as i32, CV_64F).unwrap().to_mat().unwrap();
            morphology_ex(&eroded, &mut opened, MORPH_OPEN, &kernel, Point { x: -1, y: -1 }, 1, BORDER_DEFAULT, Default::default()).unwrap();
            *mat = eroded.clone();
            let subset = (eroded - opened).into_result().unwrap().to_mat().unwrap();
            bitwise_or(&subset, &thinned.clone(), &mut thinned, &no_array()).unwrap();
        }
        // show_display_force("thinned", &thinned, 1);
        thinned
    }



    fn add_to_edge_boundary(&mut self, event: &Event) {
        let px: &mut f64 = self.edge_boundary.at_2d_mut(event.y() as i32, event.x() as i32).unwrap();
        let mid_t = (self.blur_info.mid_idx * self.interval_t as usize) as i64;
        let inner = match (mid_t - (event.t() - self.t_shift)) as f64 / self.interval_t as f64 {
            a if a > 0.0 => { -a }
            a => { a }
        } as f64;
        let outer = inner.exp();
        *px += match event.on() {
            true => { outer }
            false => { -outer }
        }
    }
}




fn add_to_event_counter(mat: &mut Mat, event: &Event) {
    let px: &mut f64 = mat.at_2d_mut(event.y() as i32, event.x() as i32).unwrap();
    *px += match event.on() {
        true => { 1.0 }
        false => { -1.0 }
    }
}