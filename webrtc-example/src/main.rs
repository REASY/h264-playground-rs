use std::{env, fs};

use base64::Engine;
use clap::Parser;
use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::time::Duration;
use tracing_subscriber::EnvFilter;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_H264};
use webrtc::api::APIBuilder;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::media::io::h264_reader::H264Reader;
use webrtc::media::Sample;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use thiserror::Error;

use shadow_rs::shadow;
use tracing::{info, warn};

shadow!(build);

pub const APP_VERSION: &str = shadow_rs::formatcp!(
    "{} ({} {}), build_env: {}, {}, {}",
    build::PKG_VERSION,
    build::SHORT_COMMIT,
    build::BUILD_TIME,
    build::RUST_VERSION,
    build::RUST_CHANNEL,
    build::CARGO_VERSION
);

#[derive(Error, Debug)]
#[error(transparent)]
pub struct AppError(Box<ErrorKind>);

#[derive(Error, Debug)]
#[error(transparent)]
pub enum ErrorKind {
    #[error("SerdeJsonError: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
    #[error("IoError: {0}")]
    IoError(#[from] std::io::Error),
    #[error("WebRTCError: {0}")]
    WebRTCError(#[from] webrtc::Error),
}

impl<E> From<E> for AppError
where
    ErrorKind: From<E>,
{
    fn from(err: E) -> Self {
        AppError(Box::new(ErrorKind::from(err)))
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Parser, Debug, Clone)]
#[clap(author, about, long_version = APP_VERSION)]
struct AppArgs {
    /// Path to H264 frames
    #[clap(long)]
    path_to_h264_frames: String,
    /// Path to JSON encoded local RTCSessionDescription https://developer.mozilla.org/en-US/docs/Web/API/RTCPeerConnection/localDescription
    #[clap(long)]
    path_local_description_json: String,
}

async fn run(session_desc: RTCSessionDescription, path_to_h264_frames: &str) -> Result<()> {
    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;

    // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
    // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
    // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
    // for each PeerConnection.
    let mut registry = Registry::new();
    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Prepare the configuration
    let config = RTCConfiguration {
        ice_servers: vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    let notify_tx = Arc::new(Notify::new());
    let notify_video = notify_tx.clone();

    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);
    let video_done_tx = done_tx.clone();

    // Create a video track
    let video_track = Arc::new(TrackLocalStaticSample::new(
        RTCRtpCodecCapability {
            mime_type: MIME_TYPE_H264.to_owned(),
            ..Default::default()
        },
        "video".to_owned(),
        "webrtc-rs".to_owned(),
    ));

    // Add this newly created track to the PeerConnection
    let rtp_sender = peer_connection
        .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
        .await?;

    // Read incoming RTCP packets
    // Before these packets are returned they are processed by interceptors. For things
    // like NACK this needs to be called.
    tokio::spawn(async move {
        let mut rtcp_buf = vec![0u8; 1500];
        while let Ok((_, _)) = rtp_sender.read(&mut rtcp_buf).await {}
        Result::Ok(())
    });

    let path_to_h264_frames: String = path_to_h264_frames.to_string();
    let paths = fs::read_dir(path_to_h264_frames.clone())?;
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
    info!(
        "There are {} H264 frames in {} folder",
        files.len(),
        &path_to_h264_frames
    );

    tokio::spawn(async move {
        // Wait for connection established
        let _ = notify_video.notified().await;

        for file in files {
            // Open a H264 file and start reading using our H264Reader
            let path = format!("{path_to_h264_frames}/{file}");
            let file = File::open(path.clone())?;
            let reader = BufReader::new(file);
            let mut h264 = H264Reader::new(reader, 400 * 1024);

            // It is important to use a time.Ticker instead of time.Sleep because
            // * avoids accumulating skew, just calling time.Sleep didn't compensate for the time spent parsing the data
            // * works around latency issues with Sleep
            let mut ticker = tokio::time::interval(Duration::from_millis(25));
            loop {
                let nal = match h264.next_nal() {
                    Ok(nal) => nal,
                    Err(_err) => {
                        break;
                    }
                };
                video_track
                    .write_sample(&Sample {
                        data: nal.data.freeze(),
                        duration: Duration::from_secs(1),
                        ..Default::default()
                    })
                    .await?;
                let _ = ticker.tick().await;
            }
        }

        let _ = video_done_tx.try_send(());
        Result::Ok(())
    });

    // Set the handler for ICE connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            info!("Connection State has changed {}", connection_state);
            if connection_state == RTCIceConnectionState::Connected {
                notify_tx.notify_waiters();
            }
            Box::pin(async {})
        },
    ));

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
        info!("Peer Connection State has changed: {}", s);

        if s == RTCPeerConnectionState::Failed {
            // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
            // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
            // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
            warn!("Peer Connection has gone to failed exiting");
            let _ = done_tx.try_send(());
        }

        Box::pin(async {})
    }));

    // Set the remote SessionDescription
    peer_connection.set_remote_description(session_desc).await?;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;

    // Create channel that is blocked until ICE Gathering is complete
    let mut gather_complete = peer_connection.gathering_complete_promise().await;

    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    // Block until ICE Gathering is complete, disabling trickle ICE
    // we do this because we only can exchange one signaling message
    // in a production application you should exchange ICE Candidates via OnICECandidate
    let _ = gather_complete.recv().await;

    // Output the answer in base64 so we can paste it in browser
    if let Some(local_desc) = peer_connection.local_description().await {
        let json_str = serde_json::to_string(&local_desc)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(json_str);
        info!("Paste below base64 encoded string to `WebRTC base64 Session Description` text area");
        println!("{}", b64);
    } else {
        println!("generate local_description failed!");
    }

    println!("Press ctrl-c to stop");
    tokio::select! {
        _ = done_rx.recv() => {
            info!("received done signal!");
        }
        _ = tokio::signal::ctrl_c() => {
        }
    };

    peer_connection.close().await?;

    Result::Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if env::var_os("RUST_LOG").is_none() {
        let env = "webrtc_example=DEBUG".to_string();
        env::set_var("RUST_LOG", env);
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .init();

    let args = AppArgs::parse();

    let f = File::open(args.path_local_description_json)?;
    let session_desc: RTCSessionDescription = serde_json::from_reader(BufReader::new(f))?;

    run(session_desc, args.path_to_h264_frames.as_str()).await?;
    Ok(())
}
