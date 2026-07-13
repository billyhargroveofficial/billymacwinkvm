use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "softkvm")]
#[command(about = "Low-latency local software KVM prototype")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print build and transport details.
    BuildInfo,

    /// Generate a 256-bit pairing key for later authenticated transports.
    GenPsk,

    /// Try to create our own native macOS virtual HID device.
    MacNativeHidProbe,

    /// Run the controlled machine receiver.
    Client {
        #[arg(long, default_value = "0.0.0.0:49321")]
        listen: String,

        #[arg(long, value_enum, default_value_t = SinkKind::Log)]
        sink: SinkKind,
    },

    /// Send synthetic input to a client. Useful before Windows capture exists.
    Probe {
        #[arg(long)]
        peer: String,
    },

    /// Send timed synthetic motion for latency/jitter diagnosis.
    MotionBench {
        #[arg(long)]
        peer: String,

        #[arg(long, value_enum, default_value_t = BenchTransport::Udp)]
        transport: BenchTransport,

        #[arg(long, value_enum, default_value_t = BenchTiming::Spin)]
        timing: BenchTiming,

        #[arg(long, default_value_t = 200)]
        hz: u32,

        #[arg(long, default_value_t = 8)]
        seconds: u32,

        #[arg(long, default_value_t = 8)]
        dx: i32,
    },

    /// macOS-only: drive the exact CGEvent writer from a local synthetic
    /// source (no network) and report per-stage stall statistics. Uses
    /// alternating +/-amp deltas so the cursor stays roughly in place.
    MacCgBench {
        #[arg(long, default_value_t = 20)]
        seconds: u32,

        #[arg(long, default_value_t = 250)]
        hz: u32,

        /// Delta magnitude in pixels for each alternating step.
        #[arg(long, default_value_t = 1)]
        amp: i32,

        /// Also write a binary .sktrace dump for offline analysis.
        #[arg(long)]
        dump: bool,
    },

    /// Analyze .sktrace ring dumps (one file, or a Windows + macOS pair for
    /// cross-machine freeze correlation by sequence number).
    TraceAnalyze {
        /// Dump files produced with SOFTKVM_TRACE=1 (freeze/session-end dumps).
        files: Vec<std::path::PathBuf>,

        /// Gap threshold in milliseconds to count as a stall.
        #[arg(long, default_value_t = 100.0)]
        stall_ms: f64,

        /// How many worst stalls to print.
        #[arg(long, default_value_t = 25)]
        top: usize,
    },

    /// Windows-only: diagnose outbound WSAEADDRINUSE 10048 (ephemeral-port
    /// allocation failure) without needing PowerShell. Tests allocation,
    /// prints netsh dynamicport/excludedportrange, counts dynamic-range
    /// occupancy per process, and (with --peer) tries both connect paths.
    WinPortDoctor {
        /// Optional peer to test real outbound connects against.
        #[arg(long)]
        peer: Option<String>,
    },

    /// Measure real Windows Raw Input cadence without involving macOS.
    WinRawCadence {
        #[arg(long, default_value_t = 60)]
        seconds: u32,

        #[arg(long, value_enum, default_value_t = WinRawCadenceMode::RawOnly)]
        mode: WinRawCadenceMode,
    },

    /// Run Windows host capture.
    Host {
        #[arg(long)]
        peer: String,

        #[arg(long, default_value = "mac-left")]
        layout: String,

        #[arg(long)]
        activate_on_start: bool,

        #[arg(long, default_value_t = 0.5)]
        entry_x_ratio: f64,

        #[arg(long, default_value_t = 0.5)]
        entry_y_ratio: f64,

        #[arg(long)]
        no_local_capture: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum SinkKind {
    Log,
    NativeHid,
    CgEvent,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BenchTransport {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BenchTiming {
    Sleep,
    Spin,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum WinRawCadenceMode {
    /// Raw Input only: no low-level mouse hook.
    RawOnly,
    /// Install WH_MOUSE_LL but always pass events through.
    HooksPassive,
    /// Install WH_MOUSE_LL and suppress mouse events for the timed run.
    HooksSuppress,
}
