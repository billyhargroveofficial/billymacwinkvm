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

    /// Check whether Karabiner VirtualHID is present on macOS.
    MacHidProbe,

    /// Try to create our own native macOS virtual HID device.
    MacNativeHidProbe,

    /// Send a tiny no-click movement through Karabiner VirtualHID.
    MacHidSmoke,

    /// Type a tiny "a" key press through Karabiner VirtualHID.
    MacKeySmoke,

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
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum SinkKind {
    Log,
    Karabiner,
    NativeHid,
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
