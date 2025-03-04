// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright © 2021-2022 Adrian <adrian.eddy at gmail>

mod ffmpeg_audio;
mod ffmpeg_video;
pub mod ffmpeg_processor;
pub mod ffmpeg_hw;

pub use self::ffmpeg_processor::FfmpegProcessor;
pub use self::ffmpeg_processor::FFmpegError;
use crate::core::{StabilizationManager, undistortion::*};
use ffmpeg_next::{ format::Pixel, frame::Video, codec, Error, ffi };
use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::{Arc, atomic::AtomicBool};
use parking_lot::RwLock;

#[derive(Debug, PartialEq, Clone, Copy)]
enum GpuType {
    NVIDIA, AMD, Intel, Unknown
}
lazy_static::lazy_static! {
    static ref GPU_TYPE: RwLock<GpuType> = RwLock::new(GpuType::Unknown);
    pub static ref GPU_DECODING: RwLock<bool> = RwLock::new(true);
}
pub fn set_gpu_type_from_name(name: &str) {
    let name = name.to_ascii_lowercase();
         if name.contains("nvidia") { *GPU_TYPE.write() = GpuType::NVIDIA; }
    else if name.contains("amd") || name.contains("advanced micro devices") { *GPU_TYPE.write() = GpuType::AMD; }
    else if name.contains("intel") && !name.contains("intel(r) core(tm)") { *GPU_TYPE.write() = GpuType::Intel; }
    else {
        log::warn!("Unknown GPU {}", name);
    }

    let gpu_type = *GPU_TYPE.read();
    if gpu_type == GpuType::NVIDIA {
        ffmpeg_hw::initialize_ctx(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA);
    }
    if gpu_type == GpuType::AMD {
        ffmpeg_hw::initialize_ctx(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    ffmpeg_hw::initialize_ctx(ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX);

    dbg!(gpu_type);
}

pub fn get_possible_encoders(codec: &str, use_gpu: bool) -> Vec<(&'static str, bool)> { // -> (name, is_gpu)
    if codec.contains("PNG") || codec.contains("png") { return vec![("png", false)]; }
    
    let mut encoders = if use_gpu {
        match codec {
            "x264" => vec![
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                ("h264_videotoolbox", true),
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                ("h264_nvenc",        true),
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                ("nvenc",             true),
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                ("nvenc_h264",        true),
                #[cfg(target_os = "windows")]
                ("h264_amf",          true),
                #[cfg(target_os = "windows")]
                ("h264_mf",           true),
                #[cfg(target_os = "linux")]
                ("h264_vaapi",        true),
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                ("h264_qsv",          true),
                #[cfg(target_os = "linux")]
                ("h264_v4l2m2m",      true),
                ("libx264",           false),
            ],
            "x265" => vec![
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                ("hevc_videotoolbox", true),
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                ("hevc_nvenc",        true),
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                ("nvenc_hevc",        true),
                #[cfg(target_os = "windows")]
                ("hevc_amf",          true),
                #[cfg(target_os = "windows")]
                ("hevc_mf",           true),
                #[cfg(target_os = "linux")]
                ("hevc_vaapi",        true),
                #[cfg(any(target_os = "windows", target_os = "linux"))]
                ("hevc_qsv",          true),
                #[cfg(target_os = "linux")]
                ("hevc_v4l2m2m",      true),
                ("libx265",           false),
            ],
            "ProRes" => vec![("prores_ks", false)],
            _        => vec![]
        }
    } else {
        match codec {
            "x264"   => vec![("libx264", false)],
            "x265"   => vec![("libx265", false)],
            "ProRes" => vec![("prores_ks", false)],
            _        => vec![]
        }
    };

    let gpu_type = *GPU_TYPE.read();
    if gpu_type != GpuType::NVIDIA {
        encoders = encoders.into_iter().filter(|x| !x.0.contains("nvenc")).collect();
    }
    if gpu_type != GpuType::AMD {
        encoders = encoders.into_iter().filter(|x| !x.0.contains("_amf")).collect();
    }
    log::debug!("Possible encoders with {:?}: {:?}", gpu_type, encoders);
    encoders
}

pub fn render<T: PixelType, F>(stab: Arc<StabilizationManager<T>>, progress: F, video_path: &str, codec: &str, codec_options: &str, output_path: &str, trim_start: f64, trim_end: f64, 
                               output_width: usize, output_height: usize, bitrate: f64, use_gpu: bool, audio: bool, gpu_decoder_index: i32, cancel_flag: Arc<AtomicBool>) -> Result<(), FFmpegError>
    where F: Fn((f64, usize, usize, bool)) + Send + Sync + Clone
{
    log::debug!("ffmpeg_hw::supported_gpu_backends: {:?}", ffmpeg_hw::supported_gpu_backends());

    let params = stab.params.read();
    let trim_ratio = trim_end - trim_start;
    let total_frame_count = params.frame_count;
    let _fps = params.fps;
    let fps_scale = params.fps_scale;

    let duration_ms = params.duration_ms;

    let render_duration = params.duration_ms * trim_ratio;
    let render_frame_count = (total_frame_count as f64 * trim_ratio).round() as usize;

    drop(params);

    let mut proc = FfmpegProcessor::from_file(video_path, *GPU_DECODING.read() && gpu_decoder_index >= 0, gpu_decoder_index as usize)?;

    log::debug!("proc.gpu_device: {:?}", &proc.gpu_device);
    let encoder = ffmpeg_hw::find_working_encoder(&get_possible_encoders(codec, use_gpu));
    proc.video_codec = Some(encoder.0.to_owned());
    proc.video.gpu_encoding = encoder.1;
    proc.video.hw_device_type = encoder.2;
    proc.video.codec_options.set("threads", "auto");
    log::debug!("proc.video_codec: {:?}", &proc.video_codec);

    if trim_start > 0.0 { proc.start_ms = Some(trim_start * duration_ms); }
    if trim_end   < 1.0 { proc.end_ms   = Some(trim_end   * duration_ms); }

    match proc.video_codec.as_deref() {
        Some("prores_ks") => {
            let profiles = ["Proxy", "LT", "Standard", "HQ", "4444", "4444XQ"];
            let pix_fmts = [Pixel::YUV422P10LE, Pixel::YUV422P10LE, Pixel::YUV422P10LE, Pixel::YUV422P10LE, Pixel::YUVA444P10LE, Pixel::YUVA444P10LE];
            if let Some(profile) = profiles.iter().position(|&x| x == codec_options) {
                proc.video.codec_options.set("profile", &format!("{}", profile));
                proc.video.encoder_pixel_format = Some(pix_fmts[profile]);
            }
        }
        Some("png") => {
            if codec_options.contains("16-bit") {
                proc.video.encoder_pixel_format = Some(Pixel::RGB48BE);
            } else {
                proc.video.encoder_pixel_format = Some(Pixel::RGB24);
            }
        }
        _ => { }
    }

    //proc.video.codec_options.set("preset", "medium");
    proc.video.codec_options.set("allow_sw", "1");

    let start_us = (proc.start_ms.unwrap_or_default() * 1000.0) as i64;

    if !audio {
        proc.audio_codec = codec::Id::None;
    }

    log::debug!("start_us: {}, render_duration: {}, render_frame_count: {}", start_us, render_duration, render_frame_count);

    let mut planes = Vec::<Box<dyn FnMut(i64, &mut Video, &mut Video, usize)>>::new();

    let progress2 = progress.clone();
    let mut process_frame = 0;
    proc.on_frame(move |mut timestamp_us, input_frame, output_frame, converter| {
        process_frame += 1;
        log::debug!("process_frame: {}, timestamp_us: {}", process_frame, timestamp_us);
            
        if let Some(scale) = fps_scale {
            timestamp_us = (timestamp_us as f64 / scale).round() as i64;
        }

        let output_frame = output_frame.unwrap();

        macro_rules! create_planes_proc {
            ($planes:ident, $(($t:tt, $in_frame:expr, $out_frame:expr, $ind:expr, $yuvi:expr), )*) => {
                $({
                    let in_size  = ($in_frame .plane_width($ind) as usize, $in_frame .plane_height($ind) as usize, $in_frame .stride($ind) as usize);
                    let out_size = ($out_frame.plane_width($ind) as usize, $out_frame.plane_height($ind) as usize, $out_frame.stride($ind) as usize);
                    let bg = {
                        let mut params = stab.params.write();
                        params.size        = (in_size.0,  in_size.1);
                        params.output_size = (out_size.0, out_size.1);
                        params.background
                    };
                    let mut plane = Undistortion::<$t>::default();
                    plane.init_size(<$t as PixelType>::from_rgb_color(bg, &$yuvi), (in_size.0, in_size.1), in_size.2, (out_size.0, out_size.1), out_size.2);
                    plane.set_compute_params(ComputeParams::from_manager(&stab));
                    $planes.push(Box::new(move |timestamp_us: i64, in_frame_data: &mut Video, out_frame_data: &mut Video, plane_index: usize| {
                        let (w, h, s)    = ( in_frame_data.plane_width(plane_index) as usize,  in_frame_data.plane_height(plane_index) as usize,  in_frame_data.stride(plane_index) as usize);
                        let (ow, oh, os) = (out_frame_data.plane_width(plane_index) as usize, out_frame_data.plane_height(plane_index) as usize, out_frame_data.stride(plane_index) as usize);

                        let (buffer, out_buffer) = (in_frame_data.data_mut(plane_index), out_frame_data.data_mut(plane_index));

                        plane.process_pixels(timestamp_us, w, h, s, ow, oh, os, buffer, out_buffer);
                    }));
                })*
            };
        }

        if planes.is_empty() {
            // Good reference about video formats: https://source.chromium.org/chromium/chromium/src/+/master:media/base/video_frame.cc
            // https://gist.github.com/Jim-Bar/3cbba684a71d1a9d468a6711a6eddbeb
            match input_frame.format() {
                Pixel::NV12 => {
                    create_planes_proc!(planes, 
                        (Luma8, input_frame, output_frame, 0, [0]),
                        (UV8,   input_frame, output_frame, 1, [1,2]),
                    );
                },
                Pixel::NV21 => {
                    create_planes_proc!(planes, 
                        (Luma8, input_frame, output_frame, 0, [0]),
                        (UV8,   input_frame, output_frame, 1, [2,1]),
                    );
                },
                Pixel::P010LE | Pixel::P016LE => {
                    create_planes_proc!(planes, 
                        (Luma16, input_frame, output_frame, 0, [0]),
                        (UV16,   input_frame, output_frame, 1, [1,2]),
                    );
                },
                Pixel::YUV420P | Pixel::YUVJ420P => {
                    create_planes_proc!(planes, 
                        (Luma8, input_frame, output_frame, 0, [0]),
                        (Luma8, input_frame, output_frame, 1, [1]),
                        (Luma8, input_frame, output_frame, 2, [2]),
                    );
                },
                Pixel::YUV420P10LE | Pixel::YUV420P16LE => {
                    create_planes_proc!(planes, 
                        (Luma16, input_frame, output_frame, 0, [0]),
                        (Luma16, input_frame, output_frame, 1, [1]),
                        (Luma16, input_frame, output_frame, 2, [2]),
                    );
                },
                format => { // All other convert to YUV444P16LE
                    ::log::info!("Unknown format {:?}, converting to YUV444P16LE", format);
                    // Go through 4:4:4 because of even plane dimensions
                    converter.convert_pixel_format(input_frame, output_frame, Pixel::YUV444P16LE, |converted_frame, converted_output| {
                        create_planes_proc!(planes, 
                            (Luma16, converted_frame, converted_output, 0, [0]), 
                            (Luma16, converted_frame, converted_output, 1, [1]), 
                            (Luma16, converted_frame, converted_output, 2, [2]), 
                        );
                    })?;
                }
            }
        }
        if planes.is_empty() {
            return Err(FFmpegError::UnknownPixelFormat(input_frame.format()));
        }

        let mut undistort_frame = |frame: &mut Video, out_frame: &mut Video| {
            for (i, cb) in planes.iter_mut().enumerate() {
                (*cb)(timestamp_us, frame, out_frame, i);
            }
            progress2((process_frame as f64 / render_frame_count as f64, process_frame, render_frame_count, false));
        };

        match input_frame.format() {
            Pixel::NV12 | Pixel::NV21 | Pixel::YUV420P | Pixel::YUVJ420P | Pixel::P010LE | Pixel::P016LE | Pixel::YUV420P10LE | Pixel::YUV420P16LE => {
                undistort_frame(input_frame, output_frame)
            },
            _ => {
                converter.convert_pixel_format(input_frame, output_frame, Pixel::YUV444P16LE, |converted_frame, converted_output| {
                    undistort_frame(converted_frame, converted_output);
                })?;
            }
        }
        
        Ok(())
    });

    proc.render(&output_path, (output_width as u32, output_height as u32), if bitrate > 0.0 { Some(bitrate) } else { None }, cancel_flag)?;

    progress((1.0, render_frame_count, render_frame_count, true));

    Ok(())
}

pub fn init() -> Result<(), Error> {
	unsafe {
        ffi::av_log_set_level(ffi::AV_LOG_INFO);
        ffi::av_log_set_callback(Some(ffmpeg_log));
    }

    Ok(())
}

lazy_static::lazy_static! {
    pub static ref FFMPEG_LOG: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
    pub static ref LAST_PREFIX: Arc<RwLock<i32>> = Arc::new(RwLock::new(1));
}

#[cfg(not(any(target_os = "linux", all(target_os = "macos", target_arch = "x86_64"))))]
type VaList = ffi::va_list;
#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "x86_64")))]
type VaList = *mut ffi::__va_list_tag;

#[allow(improper_ctypes_definitions)]
unsafe extern "C" fn ffmpeg_log(avcl: *mut c_void, level: i32, fmt: *const c_char, vl: VaList) {
    if level <= ffi::av_log_get_level() {
        let mut line = vec![0u8; 2048];
        let mut prefix: i32 = *LAST_PREFIX.read();
        
        ffi::av_log_default_callback(avcl, level, fmt, vl);
        #[cfg(target_os = "android")]
        let written = ffi::av_log_format_line2(avcl, level, fmt, vl, line.as_mut_ptr() as *mut u8, line.len() as i32, &mut prefix);
        #[cfg(not(target_os = "android"))]
        let written = ffi::av_log_format_line2(avcl, level, fmt, vl, line.as_mut_ptr() as *mut i8, line.len() as i32, &mut prefix);
        if written > 0 { 
            line.resize(written as usize, 0u8);
        }

        *LAST_PREFIX.write() = prefix;

        if let Ok(mut line) = String::from_utf8(line) {
            match level {
                ffi::AV_LOG_PANIC | ffi::AV_LOG_FATAL | ffi::AV_LOG_ERROR => {
                    line = format!("<font color=\"#d82626\">{}</font>", line);
                },
                ffi::AV_LOG_WARNING => {
                    line = format!("<font color=\"#f6a10c\">{}</font>", line);
                },
                _ => { }
            }
            FFMPEG_LOG.write().push_str(&line);
        }
    }
}

pub fn append_log(msg: &str) { ::log::debug!("{}", msg); FFMPEG_LOG.write().push_str(msg); }
pub fn get_log() -> String { FFMPEG_LOG.read().clone() }
pub fn clear_log() { FFMPEG_LOG.write().clear() }

/*
pub fn test() {
    log::debug!("FfmpegProcessor::supported_gpu_backends: {:?}", FfmpegProcessor::supported_gpu_backends());

    let mut stab = StabilizationManager::default();
    let duration_ms = 15015.0;
    let frame_count = 900;
    let fps = 60000.0/1001.0;
    let video_size = (3840, 2160);

    stab.init_from_video_data("E:/clips/GoPro/rs/C0752.MP4", duration_ms, fps, frame_count, video_size);
    stab.gyro.set_offset(0, -26.0);
    stab.gyro.integration_method = 1;
    stab.gyro.integrate();
    stab.load_lens_profile("E:/clips/GoPro/rs/Sony_A7s3_Tamron_28-200_4k60p.json");
    stab.init_size(video_size.0, video_size.1);
    stab.smoothing_id = 1;
    stab.smoothing_algs[1].as_mut().set_parameter("time_constant", 0.4);
    stab.frame_readout_time = 8.9;
    stab.fov = 1.0;
    stab.background = nalgebra::Vector4::new(0.0, 0.0, 0.0, 0.0);
    stab.recompute_blocking();

    render(
        stab, 
        move |params: (f64, usize, usize)| {
            ::log::debug!("frame {}/{}", params.1, params.2);
        }, 
        "E:/clips/GoPro/rs/C0752.MP4".into(),
        "x265".into(),
        "E:/clips/GoPro/rs/C0752-test.MP4".into(), 
        0.0,
        1.0,
        video_size.0,
        video_size.1,
        true, 
        true,
        Arc::new(AtomicBool::new(false))
    );
}
// use opencv::core::{Mat, Size, CV_8UC1};
// use std::os::raw::c_void;
        
pub fn test_decode() {
    let mut proc = FfmpegProcessor::from_file("E:/clips/GoPro/rs/C0752.MP4", true).unwrap();

    // TODO: gpu scaling in filters, example here https://github.com/zmwangx/rust-ffmpeg/blob/master/examples/transcode-audio.rs, filter scale_cuvid or scale_npp
    proc.on_frame(move |timestamp_us, input_frame, converter| {
        let small_frame = converter.scale(input_frame, Pixel::GRAY8, 1280, 720);
        ::log::debug!("ts: {} width: {}", timestamp_us, small_frame.plane_width(0));

        /*let (w, h) = (small_frame.plane_width(0) as i32, small_frame.plane_height(0) as i32);
        let mut bytes = small_frame.data_mut(0);
        let inp = unsafe { Mat::new_size_with_data(Size::new(w, h), CV_8UC1, bytes.as_mut_ptr() as *mut c_void, w as usize) }.unwrap();
        opencv::imgcodecs::imwrite("D:/test.jpg", &inp, &opencv::types::VectorOfi32::new());*/
        
    });
    let _ = proc.start_decoder_only(vec![
        (100, 2000),
        (3000, 5000),
        (11000, 999999)
    ], Arc::new(AtomicBool::new(false)));
}
*/