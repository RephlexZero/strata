//! CLI definition (clap derive). The flag surface is frozen — scripts, wiki
//! examples and the sender/receiver daemons all invoke these exact flags.

use clap::{Args, Parser, Subcommand};

const SENDER_AFTER_HELP: &str = r#"SOURCE HOT-SWAP:
  While the pipeline is running, send JSON commands to the control socket
  to switch video sources without stopping the stream:

    echo '{"cmd":"switch_source","mode":"test","pattern":"ball"}' | \
      socat - UNIX:/tmp/strata-pipeline.sock

  Supported commands:
    {"cmd":"switch_source","mode":"test","pattern":"<pattern>"}
      Patterns: smpte, ball, snow, black, white, red, green, blue
    {"cmd":"switch_source","mode":"v4l2","device":"/dev/video0"}
    {"cmd":"switch_source","mode":"uri","uri":"file:///path/to/video.mp4"}

EXAMPLES:
  # Test pattern over two cellular links
  strata-pipeline sender --dest server:5000,server:5002 \
    --config sender.toml

  # 1080p30 with audio for YouTube relay via receiver
  strata-pipeline sender --source test --framerate 30 --audio --bitrate 2000 \
    --dest receiver:5000,receiver:5002,receiver:5004

  # HDMI capture card to cloud receiver
  strata-pipeline sender --source v4l2 --device /dev/video0 \
    --dest cloud.example.com:5000,cloud.example.com:5002 \
    --bitrate 2000 --config sender.toml
"#;

const RECEIVER_AFTER_HELP: &str = r#"EXAMPLES:
  # Receive and monitor (no file output)
  strata-pipeline receiver --bind 0.0.0.0:5000

  # Receive bonded stream and relay to YouTube
  strata-pipeline receiver --bind 0.0.0.0:5000,0.0.0.0:5002,0.0.0.0:5004 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # Receive H.265 stream and relay
  strata-pipeline receiver --bind 0.0.0.0:5000 --codec h265 \
    --relay-url "rtmp://a.rtmp.youtube.com/live2/YOUR_STREAM_KEY"

  # Receive and record to MPEG-TS file
  strata-pipeline receiver --bind 0.0.0.0:5000 --output capture.ts

  # Receive with config
  strata-pipeline receiver --bind 0.0.0.0:5000 --config receiver.toml
"#;

#[derive(Parser)]
#[command(
    name = "strata-pipeline",
    about = "Bonded video transport pipeline (GStreamer)",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) mode: Mode,
}

#[derive(Subcommand)]
pub(crate) enum Mode {
    /// Encode and transmit video over bonded Strata links
    Sender(SenderArgs),
    /// Receive and reassemble bonded Strata stream
    Receiver(ReceiverArgs),
}

#[derive(Args)]
#[command(after_help = SENDER_AFTER_HELP)]
pub(crate) struct SenderArgs {
    /// Comma-separated destination addresses, e.g. 192.168.1.100:5000,10.0.0.100:5000
    #[arg(long, required = true)]
    pub(crate) dest: String,

    /// Initial video source mode: test (SMPTE bars), v4l2 (camera/HDMI capture), uri
    #[arg(long, default_value = "test")]
    pub(crate) source: String,

    /// V4L2 device path (used with --source v4l2)
    #[arg(long, default_value = "/dev/video0")]
    pub(crate) device: String,

    /// Media URI for uridecodebin (used with --source uri), e.g. file:///home/user/video.mp4
    #[arg(long, default_value = "")]
    pub(crate) uri: String,

    /// Target encoder bitrate in kbps
    #[arg(long, default_value_t = 1000)]
    pub(crate) bitrate: u32,

    /// Video codec: h265 or h264
    #[arg(long, default_value = "h265")]
    pub(crate) codec: String,

    /// Minimum bitrate for adaptation in kbps (default: from profile)
    #[arg(long)]
    pub(crate) min_bitrate: Option<u32>,

    /// Maximum bitrate for adaptation in kbps (default: from profile)
    #[arg(long)]
    pub(crate) max_bitrate: Option<u32>,

    /// Gently ramp the encoder from a low floor up to --bitrate over this
    /// window so a cold link isn't blasted with full rate (0 = disabled)
    #[arg(long, default_value_t = 0)]
    pub(crate) startup_ramp_ms: u32,

    /// Bitrate the startup ramp begins at (clamped to >= --min-bitrate; 0 = adapter default)
    #[arg(long, default_value_t = 0)]
    pub(crate) startup_floor_kbps: u32,

    /// Video framerate
    #[arg(long, default_value_t = 30)]
    pub(crate) framerate: u32,

    /// Video resolution as WxH
    #[arg(long, default_value = "1280x720")]
    pub(crate) resolution: String,

    /// Add silent AAC audio track (required for relay targets)
    #[arg(long)]
    pub(crate) audio: bool,

    /// Forward the source TS unchanged (requires --uri); no encoder
    #[arg(long)]
    pub(crate) passthrough: bool,

    /// Path to TOML config file (see Configuration Reference)
    #[arg(long, default_value = "")]
    pub(crate) config: String,

    /// UDP address to relay stats JSON (e.g. 127.0.0.1:9100)
    #[arg(long, default_value = "")]
    pub(crate) stats_dest: String,

    /// Start Prometheus metrics endpoint on this port (serves /metrics on 0.0.0.0:<port>)
    #[arg(long)]
    pub(crate) metrics_port: Option<u16>,

    /// Unix socket path for hot-swap commands
    #[arg(long, default_value = "/tmp/strata-pipeline.sock")]
    pub(crate) control: String,
}

#[derive(Args)]
#[command(after_help = RECEIVER_AFTER_HELP)]
pub(crate) struct ReceiverArgs {
    /// Bind address(es), e.g. 0.0.0.0:5000 or 0.0.0.0:5000,0.0.0.0:5002
    #[arg(long, required = true)]
    pub(crate) bind: String,

    /// Record to file (.ts = raw MPEG-TS, .mp4 = remuxed)
    #[arg(long, default_value = "")]
    pub(crate) output: String,

    /// URL to relay the received stream to (rtmp://... or YouTube HLS https://...)
    #[arg(long, default_value = "")]
    pub(crate) relay_url: String,

    /// Relay protocol (inferred from the URL scheme when omitted: rtmp:///rtmps:// -> rtmp, https:// -> hls)
    #[arg(long, value_parser = ["rtmp", "hls"])]
    pub(crate) relay_type: Option<String>,

    /// Codec of incoming stream: h265 or h264
    #[arg(long, default_value = "h265")]
    pub(crate) codec: String,

    /// Path to TOML config file (see Configuration Reference)
    #[arg(long, default_value = "")]
    pub(crate) config: String,

    /// Relay per-second stats JSON via UDP to this address
    /// (links + HLS egress heartbeat; used by the strata-receiver daemon)
    #[arg(long, default_value = "")]
    pub(crate) stats_dest: String,

    /// Start Prometheus metrics endpoint on this port (serves /metrics on 0.0.0.0:<port>)
    #[arg(long)]
    pub(crate) metrics_port: Option<u16>,
}
