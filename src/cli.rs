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
