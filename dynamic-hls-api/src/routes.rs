use crate::errors;
use crate::mpegts::TransportStream;
use axum::extract::Path;
use axum::http::{header, HeaderName};
use axum::response::IntoResponse;
use axum::{debug_handler, extract::Query, routing::get, Router};
use bytes::Bytes;
use lazy_static::lazy_static;
use mp4::{AvcConfig, MediaConfig, Mp4Config, Mp4Sample, TrackConfig, TrackType};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::{env, fs};
use tracing::info;

pub fn get_frames(path_to_h264_frames: &str) -> Result<Vec<String>, errors::AppError> {
    let paths = fs::read_dir(path_to_h264_frames)?;
    let mut files: Vec<String> = paths
        .map(|x| {
            x.unwrap()
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        })
        .filter(|x| x.ends_with(".ts"))
        .collect();
    files.sort_by(|a, b| {
        let f0: i64 = a.replace(".ts", "").parse::<i64>().unwrap();
        let f1: i64 = b.replace(".ts", "").parse::<i64>().unwrap();
        f0.cmp(&f1)
    });
    Ok(files)
}

fn h264streams_concat(base_path: &str, streams: &[&String]) -> errors::Result<Vec<u8>> {
    let mut data2 = Vec::<u8>::new();
    for p in streams {
        let path = format!("{}/{}", base_path, p);
        let mut bytes = fs::read(path)?;
        data2.append(&mut bytes);
    }
    Ok(data2)
}

fn h264streams_to_mp4(base_path: &str, streams: &[&String]) -> errors::Result<Vec<u8>> {
    let config = Mp4Config {
        major_brand: str::parse("isom").unwrap(),
        minor_version: 512,
        compatible_brands: vec![
            str::parse("isom").unwrap(),
            str::parse("iso2").unwrap(),
            str::parse("avc1").unwrap(),
            str::parse("mp41").unwrap(),
        ],
        timescale: 1000,
    };
    let data: Cursor<Vec<u8>> = Cursor::new(Vec::<u8>::new());
    let mut wrt = mp4::Mp4Writer::write_start(data, &config)?;
    let avc_config = AvcConfig {
        width: 2816,
        height: 1856,
        seq_param_set: vec![
            0x27, 0x64, 0x00, 0x32, 0xac, 0x1b, 0x1a, 0x80, 0x2c, 0x00, 0xe9, 0x30, 0x16, 0xc8,
            0x00, 0x00, 0x1f, 0x40, 0x00, 0x04, 0xe2, 0x07, 0x43, 0x00, 0x01, 0x7d, 0x78, 0x00,
            0x00, 0x5f, 0x5e, 0x15, 0xde, 0x5c, 0x68, 0x60, 0x00, 0x2f, 0xaf, 0x00, 0x00, 0x0b,
            0xeb, 0xc2, 0xbb, 0xcb, 0x85, 0x00,
        ],
        pic_param_set: vec![0x28, 0xee, 0x38, 0x30],
    };
    let track_cfg = TrackConfig {
        track_type: TrackType::Video,
        timescale: 1200000,
        language: "und".to_string(),
        media_conf: MediaConfig::AvcConfig(avc_config),
    };
    wrt.add_track(&track_cfg)?;

    let mut start_time: u64 = 0;
    let duration = 60000;
    let track_id = 1;
    for p in streams {
        let path = format!("{}/{}", base_path, p);
        let bytes = fs::read(path)?;
        let sample = Mp4Sample {
            start_time,
            duration,
            rendering_offset: 0,
            is_sync: false,
            bytes: Bytes::from(bytes),
        };
        wrt.write_sample(track_id, &sample)?;
        start_time += duration as u64;
    }
    wrt.write_end()?;
    Ok(wrt.into_writer().into_inner())
}

fn h264streams_to_mpegts(
    base_path: &str,
    streams: &[&String],
    duration: u32,
) -> errors::Result<Vec<u8>> {
    let mut ts: TransportStream = TransportStream::new();
    let mut start_time: u64 = 0;
    for p in streams {
        let path = format!("{}/{}", base_path, p);
        let bytes = std::fs::read(path)?;

        ts.push_video(start_time, 0, false, bytes)?;
        start_time += duration as u64;
    }
    let wrt = ts.write_to(Cursor::new(Vec::<u8>::new()))?;
    Ok(wrt.into_inner())
}

const MP2T_CONTENT_TYPE: [(HeaderName, &str); 1] = [(header::CONTENT_TYPE, "video/MP2T")];
const MP4_CONTENT_TYPE: [(HeaderName, &str); 1] = [(header::CONTENT_TYPE, "video/mp4")];
const PLAYLIST_CONTENT_TYPE: [(HeaderName, &str); 1] =
    [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")];

#[derive(Debug, Serialize, Deserialize, Default)]
enum VideoType {
    #[default]
    MpegTs,
    Mp4,
    Raw,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    #[serde(rename = "offset")]
    offset_ms: usize,
    #[serde(rename = "length")]
    length_ms: usize,
    #[serde(default)]
    video_type: VideoType,
}

#[debug_handler]
#[tracing::instrument(level = "INFO")]
async fn get_segment(
    Path(log_name): Path<String>,
    pagination: Query<Pagination>,
) -> errors::Result<impl IntoResponse> {
    let path_to_h264_frames: String = get_h264_path(&log_name);
    let files = get_frames(&path_to_h264_frames)?;

    // Camera sensors have 20 FPS, so it is a frame every 50 ms
    let offset_frames = pagination.offset_ms / 50;
    let frames = pagination.length_ms / 50;

    let frame_files: Vec<&String> = files.iter().skip(offset_frames).take(frames).collect();

    let video_bytes = match pagination.video_type {
        VideoType::MpegTs => {
            h264streams_to_mpegts(&path_to_h264_frames, frame_files.as_slice(), 50)?
        }
        VideoType::Mp4 => h264streams_to_mp4(&path_to_h264_frames, frame_files.as_slice())?,
        VideoType::Raw => h264streams_concat(&path_to_h264_frames, frame_files.as_slice())?,
    };
    let body = bytes::Bytes::from(video_bytes);

    match pagination.video_type {
        VideoType::MpegTs => Ok((MP2T_CONTENT_TYPE, body)),
        VideoType::Mp4 => Ok((MP4_CONTENT_TYPE, body)),
        VideoType::Raw => Ok((MP2T_CONTENT_TYPE, body)),
    }
}

const DEFAULT_BASE_PATH: &str = "/data/testing/camera";

lazy_static! {
    static ref BASE_PATH: String = {
        match env::var("BASE_PATH") {
            Ok(p) => {
                info!("`BASE_PATH` env variable is set to {}", p);
                p
            }
            Err(_) => {
                info!(
                    "`BASE_PATH` env variable is not set, use {}",
                    DEFAULT_BASE_PATH
                );
                DEFAULT_BASE_PATH.to_string()
            }
        }
    };
}

fn get_h264_path(log_name: &str) -> String {
    format!("{}/{}", *BASE_PATH, log_name)
}

const PLAYLIST_HEADER: &str = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:10
#EXT-X-MEDIA-SEQUENCE:0"#;

#[debug_handler]
#[tracing::instrument(level = "INFO")]
async fn get_playlist(Path(log_name): Path<String>) -> errors::Result<impl IntoResponse> {
    let path_to_h264_frames: String = get_h264_path(&log_name);
    let files = get_frames(&path_to_h264_frames)?;

    let mut playlist = PLAYLIST_HEADER.to_string();

    let mut offset_ms = 0;
    let mut length_ms = 0;

    for _f in files {
        if length_ms == 5000 {
            let duration_secs = length_ms / 1000;
            playlist += format!("#EXTINF:{duration_secs}.0,\n").as_str();
            playlist += format!("http://127.0.0.1:18080/v1/segment/{log_name}?offset={offset_ms}&length={length_ms}\n").as_str();
            offset_ms += length_ms;
            length_ms = 0
        }

        length_ms += 50;
    }

    if length_ms != 0 {
        let duration_secs = length_ms / 1000;
        playlist += format!("#EXTINF:{duration_secs}.0,\n").as_str();
        playlist += format!(
            "http://127.0.0.1:18080/v1/segment/{log_name}?offset={offset_ms}&length={length_ms}\n"
        )
        .as_str();
    }
    playlist += "#EXT-X-ENDLIST";

    Ok((PLAYLIST_CONTENT_TYPE, playlist))
}

pub async fn create_route() -> Router {
    let get_layer_route = Router::new()
        .route("/v1/segment/:log_name", get(get_segment))
        .route("/v1/playlist/:log_name", get(get_playlist));
    Router::new().merge(get_layer_route)
}
