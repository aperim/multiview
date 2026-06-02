//! # mosaic-core
//!
//! Shared types and traits for the **Mosaic** live video mosaic engine.
//! This crate is pure-Rust (no FFI) and is depended on by every other crate.
//! See `docs/architecture/conventions.md` for the canonical model and invariants.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error {
    //! Error taxonomy for the workspace.
    use thiserror::Error;

    /// Convenient result alias.
    pub type Result<T> = std::result::Result<T, Error>;

    /// Top-level error type spanning the Mosaic pipeline stages.
    #[derive(Debug, Error)]
    #[non_exhaustive]
    pub enum Error {
        /// An ingest/source failure.
        #[error("input error: {0}")]
        Input(String),
        /// A decode failure.
        #[error("decode error: {0}")]
        Decode(String),
        /// A compositing failure.
        #[error("compositor error: {0}")]
        Compositor(String),
        /// An encode failure.
        #[error("encode error: {0}")]
        Encode(String),
        /// An output/mux/serve failure.
        #[error("output error: {0}")]
        Output(String),
        /// A configuration error.
        #[error("config error: {0}")]
        Config(String),
        /// Functionality not yet implemented in this scaffold.
        #[error("not implemented: {0}")]
        NotImplemented(&'static str),
    }
}

pub mod time {
    //! Monotonic media time and exact rationals (never use float frame rates for timing).
    use serde::{Deserialize, Serialize};

    /// A point on the internal monotonic media timeline, in nanoseconds.
    #[derive(
        Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize,
    )]
    pub struct MediaTime(pub i64);

    impl MediaTime {
        /// The zero instant.
        pub const ZERO: Self = Self(0);
        /// Construct from nanoseconds.
        pub const fn from_nanos(ns: i64) -> Self {
            Self(ns)
        }
        /// The value in nanoseconds.
        pub const fn as_nanos(self) -> i64 {
            self.0
        }
    }

    /// An exact rational, e.g. a frame rate or timebase (`num/den`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Rational {
        /// Numerator.
        pub num: i64,
        /// Denominator.
        pub den: i64,
    }

    impl Rational {
        /// Construct a rational.
        pub const fn new(num: i64, den: i64) -> Self {
            Self { num, den }
        }
        /// 25 fps (PAL).
        pub const FPS_25: Self = Self::new(25, 1);
        /// 29.97 fps (NTSC, 30000/1001).
        pub const FPS_29_97: Self = Self::new(30000, 1001);
        /// 50 fps.
        pub const FPS_50: Self = Self::new(50, 1);
        /// 60 fps.
        pub const FPS_60: Self = Self::new(60, 1);
        /// Approximate floating value — for display/diagnostics only, never for timing math.
        pub fn as_f64(self) -> f64 {
            self.num as f64 / self.den as f64
        }
    }
}

pub mod pixel {
    //! Pixel formats. NV12 is the canonical working format throughout the pipeline.
    use serde::{Deserialize, Serialize};

    /// Supported pixel formats. Frames stay in NV12; RGBA is avoided per-tile.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum PixelFormat {
        /// 8-bit 4:2:0 semi-planar (canonical working format).
        #[default]
        Nv12,
        /// 10-bit 4:2:0 semi-planar.
        P010,
        /// Packed 8-bit RGBA (do not materialize per tile).
        Rgba,
    }
}

pub mod color {
    //! The four INDEPENDENT color axes. `Unspecified` means "not signalled" — apply the default policy.
    use serde::{Deserialize, Serialize};

    /// Color primaries (gamut).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum ColorPrimaries {
        /// Not signalled.
        #[default]
        Unspecified,
        /// ITU-R BT.709.
        Bt709,
        /// ITU-R BT.601 / SMPTE-170M.
        Bt601,
        /// ITU-R BT.2020.
        Bt2020,
    }

    /// Transfer characteristics (opto-electronic transfer / gamma).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum TransferCharacteristic {
        /// Not signalled.
        #[default]
        Unspecified,
        /// ITU-R BT.709.
        Bt709,
        /// sRGB.
        Srgb,
        /// SMPTE ST 2084 (PQ).
        Pq,
        /// ARIB STD-B67 (HLG).
        Hlg,
    }

    /// Matrix coefficients (YUV<->RGB).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum MatrixCoefficients {
        /// Not signalled.
        #[default]
        Unspecified,
        /// ITU-R BT.709.
        Bt709,
        /// ITU-R BT.601.
        Bt601,
        /// ITU-R BT.2020 non-constant luminance.
        Bt2020Ncl,
        /// Identity (samples are already RGB).
        Rgb,
    }

    /// Quantization range.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum ColorRange {
        /// Not signalled.
        #[default]
        Unspecified,
        /// Limited / "TV" / MPEG (e.g. 16-235 luma at 8-bit).
        Limited,
        /// Full / "PC" / JPEG (0-255 at 8-bit).
        Full,
    }

    /// The complete, independent color description of a frame (all four axes).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub struct ColorInfo {
        /// Primaries axis.
        pub primaries: ColorPrimaries,
        /// Transfer axis.
        pub transfer: TransferCharacteristic,
        /// Matrix axis.
        pub matrix: MatrixCoefficients,
        /// Range axis.
        pub range: ColorRange,
    }
}

pub mod frame {
    //! Frame metadata (pixel storage is backend-specific and lives in other crates).
    use crate::{color::ColorInfo, pixel::PixelFormat, time::MediaTime};

    /// Metadata describing one video frame on the internal timeline.
    #[derive(Debug, Clone)]
    pub struct FrameMeta {
        /// Presentation time on the internal monotonic timeline.
        pub pts: MediaTime,
        /// Width in pixels.
        pub width: u32,
        /// Height in pixels.
        pub height: u32,
        /// Pixel format.
        pub format: PixelFormat,
        /// Color description (four axes).
        pub color: ColorInfo,
    }
}

pub mod layout {
    //! Declarative layout/template model (canvas + cells).
    use serde::{Deserialize, Serialize};

    /// How a source is fitted into its cell rectangle.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum FitMode {
        /// Scale to fit entirely inside the cell (letterbox/pillarbox).
        #[default]
        Contain,
        /// Scale to cover the cell, cropping overflow.
        Cover,
        /// Stretch to the cell, ignoring aspect ratio.
        Fill,
    }

    /// One mosaic cell/tile: a normalized rectangle (`0.0..=1.0`) on the canvas.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Cell {
        /// Left edge (fraction of canvas width).
        pub x: f32,
        /// Top edge (fraction of canvas height).
        pub y: f32,
        /// Width (fraction of canvas width).
        pub w: f32,
        /// Height (fraction of canvas height).
        pub h: f32,
        /// Stacking order (higher draws on top).
        pub z: i32,
        /// Fit mode.
        pub fit: FitMode,
        /// Bound source id, if any.
        pub source: Option<String>,
    }

    /// The output canvas description.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Canvas {
        /// Width in pixels.
        pub width: u32,
        /// Height in pixels.
        pub height: u32,
        /// Output frame-rate numerator.
        pub fps_num: i64,
        /// Output frame-rate denominator (1001 for NTSC families).
        pub fps_den: i64,
    }

    /// A complete named layout/template.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Layout {
        /// Template name.
        pub name: String,
        /// Output canvas.
        pub canvas: Canvas,
        /// Cells in declaration order.
        pub cells: Vec<Cell>,
    }
}

pub mod traits {
    //! Minimal pipeline-stage traits (scaffold stubs; expanded during implementation).
    use crate::error::{Error, Result};
    use crate::frame::FrameMeta;

    /// A live input source.
    pub trait Source {
        /// Stable source identifier.
        fn id(&self) -> &str;
        /// Whether the source is currently connected/producing frames.
        fn is_connected(&self) -> bool;
    }

    /// An output sink/publisher.
    pub trait Sink {
        /// Stable sink identifier.
        fn id(&self) -> &str;
    }

    /// A hardware/software backend implementing a pipeline stage.
    pub trait Backend {
        /// Human-readable backend name (e.g. `"nvenc"`, `"videotoolbox"`, `"software"`).
        fn name(&self) -> &str;
    }

    /// Composites tiles into the output canvas.
    pub trait Compositor {
        /// Describe the next composited output frame's metadata.
        ///
        /// The scaffold returns [`Error::NotImplemented`]; real backends render on the GPU.
        fn describe_output(&self) -> Result<FrameMeta> {
            Err(Error::NotImplemented("Compositor::describe_output"))
        }
    }
}

pub use error::{Error, Result};
