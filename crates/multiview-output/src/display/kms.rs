//! The real DRM/KMS backend (feature `display-kms`): drm-rs 0.15 atomic
//! commits + GBM/dumb-buffer scanout allocation, implementing [`KmsBackend`]
//! for the hardware-free sink loop in [`super::sink`].
//!
//! **Hardware-only module.** Everything here speaks real ioctls against
//! `/dev/dri/cardN`; CI compiles it (the feature build gate) but exercises it
//! only on hardware — the sink's behaviour (conflation, EBUSY, modeset
//! discipline) is CI-proven through the scripted mock instead. All code is
//! safe Rust: drm-rs is a pure-ioctl safe wrapper, and GBM GEM handles are
//! obtained via prime-fd export/import rather than the C union accessor.
//!
//! v1 frame path (DEV-B1): CPU NV12→XRGB conversion
//! ([`super::canvas::nv12_to_xrgb`]) into a double-buffered XRGB8888 scanout
//! pool — GBM-allocated (`SCANOUT | WRITE | LINEAR`) where the device has a
//! Mesa GBM backend, with KMS **dumb buffers** as the universal fallback
//! (NVIDIA's proprietary driver documents GBM scanout allocation as
//! unsupported). DEV-B3 replaces this with NV12 direct scanout (Intel/vc4)
//! and the wgpu render pass (AMD DCE11 class), per the module-level spike
//! verdict.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use drm::buffer::{Buffer as DrmBuffer, DrmFourcc};
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{
    atomic::AtomicModeReq, connector, crtc, framebuffer, plane, property, AtomicCommitFlags,
    Device as ControlDevice, Mode, ModeFlags, ModeTypeFlags,
};
use drm::{ClientCapability, Device as BaseDevice};
use rustix::event::{PollFd, PollFlags};

use super::canvas::{nv12_to_xrgb, DisplayCanvas};
use super::device::{
    ConnectorDesc, ConnectorSelector, DisplayError, FlipEvent, HeadSetup, KmsBackend, SubmitError,
};
use super::mode::DisplayModeInfo;

/// `DRM_PLANE_TYPE_PRIMARY` (uapi `drm_mode.h`): the value of a plane's
/// `type` property identifying the primary plane.
const PLANE_TYPE_PRIMARY: u64 = 1;

/// The minimal DRM device wrapper drm-rs needs (the classic `Card(File)`
/// pattern). Opening the primary node while no other KMS master exists makes
/// this fd the implicit DRM master (brief §10).
#[derive(Debug)]
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BaseDevice for Card {}
impl ControlDevice for Card {}

/// One probed connector kept with its native handles/modes so a later
/// [`HeadSetup`] (plain data) can be resolved back onto kernel objects.
#[derive(Debug)]
struct ProbedConnector {
    handle: connector::Handle,
    modes: Vec<Mode>,
}

/// One scanout buffer: the framebuffer the plane flips to, plus how to map
/// and write it.
enum ScanoutBuffer {
    /// GBM-allocated (`SCANOUT | WRITE | LINEAR`), preferred where available.
    Gbm {
        /// Keeps the BO (and its GEM handle) alive for the framebuffer.
        bo: gbm::BufferObject<()>,
        fb: framebuffer::Handle,
    },
    /// KMS dumb buffer — the universal CPU-scanout fallback.
    Dumb {
        db: DumbBuffer,
        fb: framebuffer::Handle,
        pitch: u32,
    },
}

impl ScanoutBuffer {
    fn framebuffer(&self) -> framebuffer::Handle {
        match self {
            ScanoutBuffer::Gbm { fb, .. } | ScanoutBuffer::Dumb { fb, .. } => *fb,
        }
    }
}

/// The atomic property handles a lit head commits against.
#[derive(Debug, Clone, Copy)]
struct HeadProps {
    conn_crtc_id: property::Handle,
    crtc_mode_id: property::Handle,
    crtc_active: property::Handle,
    plane_fb_id: property::Handle,
    plane_crtc_id: property::Handle,
    plane_src_x: property::Handle,
    plane_src_y: property::Handle,
    plane_src_w: property::Handle,
    plane_src_h: property::Handle,
    plane_crtc_x: property::Handle,
    plane_crtc_y: property::Handle,
    plane_crtc_w: property::Handle,
    plane_crtc_h: property::Handle,
}

/// A prepared (validated and/or lit) head: resolved kernel objects, the mode
/// blob, and the double-buffered scanout pool.
struct PreparedHead {
    connector: connector::Handle,
    crtc: crtc::Handle,
    plane: plane::Handle,
    /// The `MODE_ID` blob id created for the selected timing.
    mode_blob_id: u64,
    props: HeadProps,
    buffers: Vec<ScanoutBuffer>,
    /// Index of the buffer currently (or about to be) on glass; the other is
    /// the write target. With commit-only-when-idle, two buffers suffice.
    front: usize,
    width: u32,
    height: u32,
}

/// The real KMS backend: owns the card fd (and the GBM device dup'd onto it)
/// for the lifetime of the sink. After
/// [`DisplaySink::start`](super::sink::DisplaySink::start) hands it to the
/// flip thread, that thread is the only user — "a dedicated thread owns the
/// DRM fd" (ADR-0044).
///
/// Kernel-side resources (framebuffers, dumb buffers, the mode blob) are
/// released by the kernel when the fd closes at drop; no explicit teardown
/// ioctls are needed for a process-lifetime sink.
pub struct KmsDisplayDevice {
    card: Card,
    card_path: PathBuf,
    /// The GBM allocator over a dup of the same fd; `None` when the driver
    /// has no GBM backend (the dumb-buffer fallback is used instead).
    gbm: Option<gbm::Device<Card>>,
    probed: HashMap<String, ProbedConnector>,
    head: Option<PreparedHead>,
}

impl std::fmt::Debug for KmsDisplayDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KmsDisplayDevice")
            .field("card_path", &self.card_path)
            .field("gbm", &self.gbm.is_some())
            .field("probed", &self.probed.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Wrap an ioctl error with context.
fn dev_err(context: &str, e: &std::io::Error) -> DisplayError {
    DisplayError::Device(format!("{context}: {e}"))
}

impl KmsDisplayDevice {
    /// Open one DRM primary node (`/dev/dri/cardN`) and enable the atomic +
    /// universal-planes client capabilities.
    ///
    /// # Errors
    ///
    /// [`DisplayError::Device`] when the node cannot be opened or the driver
    /// does not support atomic modesetting.
    pub fn open_path(path: &Path) -> Result<Self, DisplayError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| dev_err(&format!("opening {}", path.display()), &e))?;
        let card = Card(file);
        card.set_client_capability(ClientCapability::UniversalPlanes, true)
            .map_err(|e| dev_err("enabling universal planes", &e))?;
        card.set_client_capability(ClientCapability::Atomic, true)
            .map_err(|e| dev_err("enabling atomic modesetting", &e))?;
        // A GBM device over a dup of the same fd; absence (e.g. the NVIDIA
        // proprietary driver) selects the dumb-buffer pool instead.
        let gbm = card
            .0
            .try_clone()
            .ok()
            .and_then(|dup| gbm::Device::new(Card(dup)).ok());
        if gbm.is_none() {
            tracing::info!(
                card = %path.display(),
                "no GBM backend; using KMS dumb buffers for scanout"
            );
        }
        Ok(Self {
            card,
            card_path: path.to_path_buf(),
            gbm,
            probed: HashMap::new(),
            head: None,
        })
    }

    /// Scan `/dev/dri/card*` for the device exposing the selected connector
    /// (`Auto` = the first card with any connected connector).
    ///
    /// # Errors
    ///
    /// [`DisplayError`] naming what was probed when no card matches.
    pub fn open_for_connector(selector: &ConnectorSelector) -> Result<Self, DisplayError> {
        let mut card_paths: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .map_err(|e| dev_err("listing /dev/dri", &e))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card"))
            })
            .collect();
        card_paths.sort();
        let mut seen: Vec<String> = Vec::new();
        for path in &card_paths {
            let Ok(mut device) = Self::open_path(path) else {
                continue;
            };
            let Ok(connectors) = device.probe_connectors() else {
                continue;
            };
            let hit = match selector {
                ConnectorSelector::Auto => connectors.iter().any(|c| c.connected),
                ConnectorSelector::Name(name) => connectors.iter().any(|c| &c.name == name),
            };
            if hit {
                return Ok(device);
            }
            seen.extend(connectors.into_iter().map(|c| c.name));
        }
        match selector {
            ConnectorSelector::Auto => Err(DisplayError::NoneConnected { probed: seen }),
            ConnectorSelector::Name(name) => Err(DisplayError::ConnectorNotFound {
                requested: name.clone(),
                available: seen,
            }),
        }
    }

    /// Resolve a probed connector by kernel name.
    fn probed(&self, name: &str) -> Result<&ProbedConnector, DisplayError> {
        self.probed
            .get(name)
            .ok_or_else(|| DisplayError::ConnectorNotFound {
                requested: name.to_owned(),
                available: self.probed.keys().cloned().collect(),
            })
    }

    /// Find the primary plane that can drive `crtc` and scans out XRGB8888.
    fn find_primary_plane(&self, crtc: crtc::Handle) -> Result<plane::Handle, DisplayError> {
        let res = self
            .card
            .resource_handles()
            .map_err(|e| dev_err("reading resources", &e))?;
        let planes = self
            .card
            .plane_handles()
            .map_err(|e| dev_err("listing planes", &e))?;
        // The XRGB8888 fourcc code (`XR24`), as planes report formats raw.
        let xrgb = u32::from_le_bytes(*b"XR24");
        for ph in planes {
            let info = self
                .card
                .get_plane(ph)
                .map_err(|e| dev_err("reading plane", &e))?;
            if !res.filter_crtcs(info.possible_crtcs()).contains(&crtc) {
                continue;
            }
            if !info.formats().contains(&xrgb) {
                continue;
            }
            if self.plane_type(ph)? == PLANE_TYPE_PRIMARY {
                return Ok(ph);
            }
        }
        Err(DisplayError::Device(
            "no XRGB8888-capable primary plane for the chosen CRTC".to_owned(),
        ))
    }

    /// Read a plane's `type` property value.
    fn plane_type(&self, plane: plane::Handle) -> Result<u64, DisplayError> {
        let props = self
            .card
            .get_properties(plane)
            .map_err(|e| dev_err("reading plane properties", &e))?;
        let (handles, values) = props.as_props_and_values();
        for (ph, value) in handles.iter().zip(values.iter()) {
            let info = self
                .card
                .get_property(*ph)
                .map_err(|e| dev_err("reading property", &e))?;
            if info.name().to_str() == Ok("type") {
                return Ok(*value);
            }
        }
        Err(DisplayError::Device(
            "plane exposes no `type` property".to_owned(),
        ))
    }

    /// Look up a named property on a KMS object.
    fn find_prop<T>(&self, handle: T, name: &str) -> Result<property::Handle, DisplayError>
    where
        T: drm::control::ResourceHandle,
    {
        let props = self
            .card
            .get_properties(handle)
            .map_err(|e| dev_err("reading properties", &e))?;
        let (handles, _) = props.as_props_and_values();
        for ph in handles {
            let info = self
                .card
                .get_property(*ph)
                .map_err(|e| dev_err("reading property", &e))?;
            if info.name().to_str() == Ok(name) {
                return Ok(*ph);
            }
        }
        Err(DisplayError::Device(format!(
            "required KMS property {name:?} not found"
        )))
    }

    /// Allocate one XRGB8888 scanout buffer: GBM first, dumb-buffer fallback.
    fn allocate_buffer(&self, width: u32, height: u32) -> Result<ScanoutBuffer, DisplayError> {
        if let Some(gbm) = &self.gbm {
            match Self::allocate_gbm(gbm, &self.card, width, height) {
                Ok(buffer) => return Ok(buffer),
                Err(e) => {
                    tracing::info!(
                        error = %e,
                        "GBM scanout allocation unavailable; falling back to dumb buffers"
                    );
                }
            }
        }
        let mut db = self
            .card
            .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
            .map_err(|e| dev_err("creating dumb buffer", &e))?;
        let pitch = db.pitch();
        // Light the buffer black before first use (dumb buffers are
        // zero-filled by the kernel; the explicit clear keeps the contract
        // independent of that detail).
        {
            let mut mapping = self
                .card
                .map_dumb_buffer(&mut db)
                .map_err(|e| dev_err("mapping dumb buffer", &e))?;
            mapping.as_mut().fill(0);
        }
        let fb = self
            .card
            .add_framebuffer(&db, 24, 32)
            .map_err(|e| dev_err("adding dumb framebuffer", &e))?;
        Ok(ScanoutBuffer::Dumb { db, fb, pitch })
    }

    /// Allocate a GBM scanout BO and import it as a framebuffer via prime-fd
    /// export → GEM import (the safe path; no C union access).
    fn allocate_gbm(
        gbm: &gbm::Device<Card>,
        card: &Card,
        width: u32,
        height: u32,
    ) -> Result<ScanoutBuffer, DisplayError> {
        use gbm::BufferObjectFlags;
        let mut bo = gbm
            .create_buffer_object::<()>(
                width,
                height,
                gbm::Format::Xrgb8888,
                BufferObjectFlags::SCANOUT | BufferObjectFlags::WRITE | BufferObjectFlags::LINEAR,
            )
            .map_err(|e| dev_err("creating GBM scanout BO", &e))?;
        // Clear to black before first scanout (GBM memory is uninitialized).
        bo.map_mut(0, 0, width, height, |mapping| {
            mapping.buffer_mut().fill(0);
        })
        .map_err(|e| dev_err("mapping GBM BO", &e))?;
        let prime = bo
            .fd()
            .map_err(|e| DisplayError::Device(format!("exporting GBM BO fd: {e}")))?;
        let handle = card
            .prime_fd_to_buffer(prime.as_fd())
            .map_err(|e| dev_err("importing GBM BO", &e))?;
        let adapter = PrimeBuffer {
            size: (width, height),
            pitch: bo.stride(),
            handle,
        };
        let fb = card
            .add_framebuffer(&adapter, 24, 32)
            .map_err(|e| dev_err("adding GBM framebuffer", &e))?;
        Ok(ScanoutBuffer::Gbm { bo, fb })
    }

    /// Prepare (or reuse) the head state for `setup`: resolve kernel objects,
    /// build the mode blob, allocate the scanout pool, gather property
    /// handles. Idempotent across `validate_setup` → `apply_modeset`.
    fn prepare_head(&mut self, setup: &HeadSetup) -> Result<(), DisplayError> {
        if self.head.is_some() {
            return Ok(());
        }
        if self.probed.is_empty() {
            self.probe_connectors()?;
        }
        let probed = self.probed(&setup.connector)?;
        let conn_handle = probed.handle;
        // The native mode: the EDID mode whose timings equal the selection,
        // or a user-defined mode built from the (CVT-RB) timings.
        let native = probed
            .modes
            .iter()
            .find(|m| mode_to_info(m) == setup.mode)
            .copied()
            .map_or_else(|| native_mode_from_info(&setup.mode), Ok)?;
        // CRTC: the connector's current encoder's CRTC when lit, else the
        // first CRTC any of its encoders can drive.
        let conn_info = self
            .card
            .get_connector(conn_handle, false)
            .map_err(|e| dev_err("reading connector", &e))?;
        let res = self
            .card
            .resource_handles()
            .map_err(|e| dev_err("reading resources", &e))?;
        let mut chosen_crtc: Option<crtc::Handle> = None;
        if let Some(enc) = conn_info.current_encoder() {
            if let Ok(info) = self.card.get_encoder(enc) {
                chosen_crtc = info.crtc();
            }
        }
        if chosen_crtc.is_none() {
            for enc in conn_info.encoders() {
                let Ok(info) = self.card.get_encoder(*enc) else {
                    continue;
                };
                if let Some(c) = res.filter_crtcs(info.possible_crtcs()).first() {
                    chosen_crtc = Some(*c);
                    break;
                }
            }
        }
        let crtc = chosen_crtc.ok_or_else(|| {
            DisplayError::Device(format!("no CRTC can drive connector {}", setup.connector))
        })?;
        let plane = self.find_primary_plane(crtc)?;
        let blob_value = self
            .card
            .create_property_blob(&native)
            .map_err(|e| dev_err("creating mode blob", &e))?;
        let property::Value::Blob(mode_blob_id) = blob_value else {
            return Err(DisplayError::Device(
                "mode blob creation returned a non-blob value".to_owned(),
            ));
        };
        let props = HeadProps {
            conn_crtc_id: self.find_prop(conn_handle, "CRTC_ID")?,
            crtc_mode_id: self.find_prop(crtc, "MODE_ID")?,
            crtc_active: self.find_prop(crtc, "ACTIVE")?,
            plane_fb_id: self.find_prop(plane, "FB_ID")?,
            plane_crtc_id: self.find_prop(plane, "CRTC_ID")?,
            plane_src_x: self.find_prop(plane, "SRC_X")?,
            plane_src_y: self.find_prop(plane, "SRC_Y")?,
            plane_src_w: self.find_prop(plane, "SRC_W")?,
            plane_src_h: self.find_prop(plane, "SRC_H")?,
            plane_crtc_x: self.find_prop(plane, "CRTC_X")?,
            plane_crtc_y: self.find_prop(plane, "CRTC_Y")?,
            plane_crtc_w: self.find_prop(plane, "CRTC_W")?,
            plane_crtc_h: self.find_prop(plane, "CRTC_H")?,
        };
        let buffers = vec![
            self.allocate_buffer(setup.mode.width, setup.mode.height)?,
            self.allocate_buffer(setup.mode.width, setup.mode.height)?,
        ];
        self.head = Some(PreparedHead {
            connector: conn_handle,
            crtc,
            plane,
            mode_blob_id,
            props,
            buffers,
            front: 0,
            width: setup.mode.width,
            height: setup.mode.height,
        });
        Ok(())
    }

    /// Build the full-state atomic request (connector→CRTC→plane→fb) used by
    /// both the `TEST_ONLY` validation and the real modeset.
    fn full_state_request(head: &PreparedHead, fb: framebuffer::Handle) -> AtomicModeReq {
        let mut req = AtomicModeReq::new();
        let p = head.props;
        req.add_property(
            head.connector,
            p.conn_crtc_id,
            property::Value::CRTC(Some(head.crtc)),
        );
        req.add_property(
            head.crtc,
            p.crtc_mode_id,
            property::Value::Blob(head.mode_blob_id),
        );
        req.add_property(head.crtc, p.crtc_active, property::Value::Boolean(true));
        req.add_property(
            head.plane,
            p.plane_fb_id,
            property::Value::Framebuffer(Some(fb)),
        );
        req.add_property(
            head.plane,
            p.plane_crtc_id,
            property::Value::CRTC(Some(head.crtc)),
        );
        req.add_property(head.plane, p.plane_src_x, property::Value::UnsignedRange(0));
        req.add_property(head.plane, p.plane_src_y, property::Value::UnsignedRange(0));
        // SRC_W/H are 16.16 fixed point.
        req.add_property(
            head.plane,
            p.plane_src_w,
            property::Value::UnsignedRange(u64::from(head.width) << 16),
        );
        req.add_property(
            head.plane,
            p.plane_src_h,
            property::Value::UnsignedRange(u64::from(head.height) << 16),
        );
        req.add_property(head.plane, p.plane_crtc_x, property::Value::SignedRange(0));
        req.add_property(head.plane, p.plane_crtc_y, property::Value::SignedRange(0));
        req.add_property(
            head.plane,
            p.plane_crtc_w,
            property::Value::UnsignedRange(u64::from(head.width)),
        );
        req.add_property(
            head.plane,
            p.plane_crtc_h,
            property::Value::UnsignedRange(u64::from(head.height)),
        );
        req
    }

    /// Write `frame` into scanout buffer `index` (CPU NV12→XRGB v1 path).
    fn write_frame(&mut self, index: usize, frame: &dyn DisplayCanvas) -> Result<(), DisplayError> {
        let Some(head) = self.head.as_mut() else {
            return Err(DisplayError::Device("no lit head".to_owned()));
        };
        let (width, height) = (head.width, head.height);
        match head.buffers.get_mut(index) {
            Some(ScanoutBuffer::Gbm { bo, .. }) => bo
                .map_mut(0, 0, width, height, |mapping| {
                    let stride = mapping.stride();
                    nv12_to_xrgb(frame, mapping.buffer_mut(), width, height, stride)
                })
                .map_err(|e| dev_err("mapping GBM BO", &e))?
                .map_err(|e| DisplayError::Device(format!("converting frame: {e}"))),
            Some(ScanoutBuffer::Dumb { db, pitch, .. }) => {
                let pitch = *pitch;
                let mut mapping = self
                    .card
                    .map_dumb_buffer(db)
                    .map_err(|e| dev_err("mapping dumb buffer", &e))?;
                nv12_to_xrgb(frame, mapping.as_mut(), width, height, pitch)
                    .map_err(|e| DisplayError::Device(format!("converting frame: {e}")))
            }
            None => Err(DisplayError::Device("scanout buffer missing".to_owned())),
        }
    }
}

impl KmsBackend for KmsDisplayDevice {
    fn probe_connectors(&mut self) -> Result<Vec<ConnectorDesc>, DisplayError> {
        let res = self
            .card
            .resource_handles()
            .map_err(|e| dev_err("reading resources", &e))?;
        let mut out = Vec::new();
        self.probed.clear();
        for handle in res.connectors() {
            // force_probe: this runs at startup only (never the frame path),
            // exactly when fresh EDID/connection state is wanted.
            let info = self
                .card
                .get_connector(*handle, true)
                .map_err(|e| dev_err("probing connector", &e))?;
            let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
            let connected = info.state() == connector::State::Connected;
            let modes: Vec<DisplayModeInfo> = info.modes().iter().map(mode_to_info).collect();
            self.probed.insert(
                name.clone(),
                ProbedConnector {
                    handle: *handle,
                    modes: info.modes().to_vec(),
                },
            );
            out.push(ConnectorDesc {
                name,
                connected,
                modes,
            });
        }
        Ok(out)
    }

    fn validate_setup(&mut self, setup: &HeadSetup) -> Result<(), DisplayError> {
        self.prepare_head(setup)?;
        let Some(head) = self.head.as_ref() else {
            return Err(DisplayError::Device("head preparation lost".to_owned()));
        };
        let fb = head
            .buffers
            .first()
            .map(ScanoutBuffer::framebuffer)
            .ok_or_else(|| DisplayError::Device("scanout pool empty".to_owned()))?;
        let req = Self::full_state_request(head, fb);
        self.card
            .atomic_commit(
                AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
                req,
            )
            .map_err(|e| dev_err("TEST_ONLY validation rejected the configuration", &e))
    }

    fn apply_modeset(&mut self, setup: &HeadSetup) -> Result<(), DisplayError> {
        self.prepare_head(setup)?;
        let Some(head) = self.head.as_mut() else {
            return Err(DisplayError::Device("head preparation lost".to_owned()));
        };
        let fb = head
            .buffers
            .first()
            .map(ScanoutBuffer::framebuffer)
            .ok_or_else(|| DisplayError::Device("scanout pool empty".to_owned()))?;
        let req = Self::full_state_request(head, fb);
        head.front = 0;
        // The ONE blocking ALLOW_MODESET commit (startup / Class-2 only).
        self.card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .map_err(|e| dev_err("modeset commit failed", &e))
    }

    fn submit_frame(&mut self, frame: &dyn DisplayCanvas) -> Result<(), SubmitError> {
        let back = match self.head.as_ref() {
            Some(head) => 1 - head.front,
            None => {
                return Err(SubmitError::Device(DisplayError::Device(
                    "no lit head".to_owned(),
                )))
            }
        };
        self.write_frame(back, frame).map_err(SubmitError::Device)?;
        let Some(head) = self.head.as_mut() else {
            return Err(SubmitError::Device(DisplayError::Device(
                "no lit head".to_owned(),
            )));
        };
        let fb = head
            .buffers
            .get(back)
            .map(ScanoutBuffer::framebuffer)
            .ok_or_else(|| {
                SubmitError::Device(DisplayError::Device("scanout buffer missing".to_owned()))
            })?;
        // The flip commit: just the new framebuffer on the primary plane —
        // NONBLOCK + PAGE_FLIP_EVENT, never ALLOW_MODESET (ADR-0044 §1).
        let mut req = AtomicModeReq::new();
        req.add_property(
            head.plane,
            head.props.plane_fb_id,
            property::Value::Framebuffer(Some(fb)),
        );
        match self.card.atomic_commit(
            AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT,
            req,
        ) {
            Ok(()) => {
                head.front = back;
                Ok(())
            }
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::BUSY.raw_os_error()) => {
                Err(SubmitError::Busy)
            }
            Err(e) => Err(SubmitError::Device(dev_err("flip commit failed", &e))),
        }
    }

    fn wait_events(&mut self, timeout: Duration) -> Result<Vec<FlipEvent>, DisplayError> {
        let timeout_ms = i32::try_from(timeout.as_millis())
            .unwrap_or(i32::MAX)
            .max(1);
        let mut fds = [PollFd::new(&self.card, PollFlags::IN)];
        match rustix::event::poll(&mut fds, timeout_ms) {
            Ok(0) => Ok(Vec::new()),
            Ok(_) => {
                let events = self
                    .card
                    .receive_events()
                    .map_err(|e| dev_err("reading DRM events", &e))?;
                Ok(events
                    .filter_map(|event| match event {
                        drm::control::Event::PageFlip(flip) => Some(FlipEvent {
                            crtc_frame: flip.frame,
                            timestamp: flip.duration,
                        }),
                        _ => None,
                    })
                    .collect())
            }
            // A signal interrupting the wait is not an error; the loop simply
            // re-checks the mailbox/stop flag.
            Err(e) if e == rustix::io::Errno::INTR => Ok(Vec::new()),
            Err(e) => Err(DisplayError::Device(format!("polling the DRM fd: {e}"))),
        }
    }
}

/// A drm `Buffer` view over a prime-imported GBM BO (handle + geometry),
/// enough for `add_framebuffer`.
struct PrimeBuffer {
    size: (u32, u32),
    pitch: u32,
    handle: drm::buffer::Handle,
}

impl DrmBuffer for PrimeBuffer {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn pitch(&self) -> u32 {
        self.pitch
    }
    fn handle(&self) -> drm::buffer::Handle {
        self.handle
    }
}

/// Convert a kernel mode into the plain [`DisplayModeInfo`] the pure policy
/// layer consumes.
fn mode_to_info(mode: &Mode) -> DisplayModeInfo {
    let (width, height) = mode.size();
    let (hsync_start, hsync_end, htotal) = mode.hsync();
    let (vsync_start, vsync_end, vtotal) = mode.vsync();
    DisplayModeInfo {
        width: u32::from(width),
        height: u32::from(height),
        clock_khz: mode.clock(),
        hsync_start: u32::from(hsync_start),
        hsync_end: u32::from(hsync_end),
        htotal: u32::from(htotal),
        vsync_start: u32::from(vsync_start),
        vsync_end: u32::from(vsync_end),
        vtotal: u32::from(vtotal),
        hsync_positive: mode.flags().contains(ModeFlags::PHSYNC),
        vsync_positive: mode.flags().contains(ModeFlags::PVSYNC),
        preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
    }
}

/// Build a user-defined kernel mode from computed (CVT-RB forced) timings.
fn native_mode_from_info(info: &DisplayModeInfo) -> Result<Mode, DisplayError> {
    let geometry_err =
        |what: &str| DisplayError::Device(format!("forced mode {what} exceeds the KMS u16 range"));
    let mut raw = drm_ffi::drm_mode_modeinfo {
        clock: info.clock_khz,
        hdisplay: u16::try_from(info.width).map_err(|_| geometry_err("width"))?,
        hsync_start: u16::try_from(info.hsync_start).map_err(|_| geometry_err("hsync_start"))?,
        hsync_end: u16::try_from(info.hsync_end).map_err(|_| geometry_err("hsync_end"))?,
        htotal: u16::try_from(info.htotal).map_err(|_| geometry_err("htotal"))?,
        vdisplay: u16::try_from(info.height).map_err(|_| geometry_err("height"))?,
        vsync_start: u16::try_from(info.vsync_start).map_err(|_| geometry_err("vsync_start"))?,
        vsync_end: u16::try_from(info.vsync_end).map_err(|_| geometry_err("vsync_end"))?,
        vtotal: u16::try_from(info.vtotal).map_err(|_| geometry_err("vtotal"))?,
        type_: drm_ffi::DRM_MODE_TYPE_USERDEF,
        ..Default::default()
    };
    raw.flags = if info.hsync_positive {
        drm_ffi::DRM_MODE_FLAG_PHSYNC
    } else {
        drm_ffi::DRM_MODE_FLAG_NHSYNC
    } | if info.vsync_positive {
        drm_ffi::DRM_MODE_FLAG_PVSYNC
    } else {
        drm_ffi::DRM_MODE_FLAG_NVSYNC
    };
    // The cosmetic integer refresh (the kernel recomputes from the timings).
    let denom = u64::from(info.htotal).saturating_mul(u64::from(info.vtotal));
    raw.vrefresh = u64::from(info.clock_khz)
        .saturating_mul(1000)
        .checked_div(denom)
        .and_then(|hz| u32::try_from(hz).ok())
        .unwrap_or(0);
    Ok(Mode::from(raw))
}
