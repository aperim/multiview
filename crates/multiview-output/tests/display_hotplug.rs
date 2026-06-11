//! Display hotplug tests (DEV-B5 / ADR-0045): the deployment-level hotplug
//! wiring — kernel netlink uevent **parsing** (the KERNEL group's wire form,
//! never the udevd-processed stream), the re-probe debounce, the
//! netlink-or-polling mode selection (rootless containers get no kernel
//! uevents → `force_probe` polling), the monitor thread over a scripted
//! event source, and the flip loop's re-probe / re-light behaviour over a
//! scripted `KmsBackend`. All CI-tested hardware-free; only the real netlink
//! socket + real connectors run on hardware.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_core::time::Rational;
use multiview_output::display::hotplug::{
    parse_uevent, select_mode, HotplugMode, HotplugMonitor, ReprobeDebounce, ReprobeFlag,
    UeventSource,
};
use multiview_output::display::{
    ConnectorDesc, ConnectorSelector, DisplayCanvas, DisplayError, DisplayModeInfo, DisplaySink,
    DisplaySinkConfig, FlipEvent, HeadSetup, KmsBackend, ModeRequest, SubmitError,
};

// ---------------------------------------------------------------------------
// Kernel uevent parsing
// ---------------------------------------------------------------------------

/// A kernel drm connector-change uevent as delivered on the KERNEL netlink
/// group: `action@devpath\0KEY=VALUE\0…`.
const DRM_CHANGE: &[u8] = b"change@/devices/pci0000:00/0000:00:01.0/drm/card1\0\
ACTION=change\0\
DEVPATH=/devices/pci0000:00/0000:00:01.0/drm/card1\0\
SUBSYSTEM=drm\0\
HOTPLUG=1\0\
CONNECTOR=85\0\
SEQNUM=4711\0";

#[test]
fn parses_a_kernel_drm_change_uevent() {
    let event = parse_uevent(DRM_CHANGE).expect("a kernel drm uevent parses");
    assert_eq!(event.action, "change");
    assert_eq!(event.subsystem.as_deref(), Some("drm"));
    assert!(event.is_display_hotplug());
}

#[test]
fn rejects_udevd_processed_messages() {
    // udevd re-broadcasts processed events with a "libudev" magic prefix on
    // its OWN multicast group; if one ever arrives it must be ignored — the
    // design listens to the KERNEL group only (ADR-0045).
    let mut datagram = b"libudev\0".to_vec();
    datagram.extend_from_slice(&[0xfe, 0xed, 0xca, 0xfe]);
    datagram.extend_from_slice(b"ACTION=change\0SUBSYSTEM=drm\0HOTPLUG=1\0");
    assert!(parse_uevent(&datagram).is_none());
}

#[test]
fn non_drm_subsystems_are_not_display_hotplug() {
    let datagram = b"add@/devices/pci0000:00/usb1/1-1\0\
ACTION=add\0\
DEVPATH=/devices/pci0000:00/usb1/1-1\0\
SUBSYSTEM=usb\0\
SEQNUM=4712\0";
    let event = parse_uevent(datagram).expect("a well-formed uevent parses");
    assert!(!event.is_display_hotplug());
}

#[test]
fn drm_events_without_the_hotplug_flag_are_not_display_hotplug() {
    // The kernel emits other drm uevents (e.g. lease changes); only a
    // connector-status change carries HOTPLUG=1.
    let datagram = b"change@/devices/pci0000:00/0000:00:01.0/drm/card1\0\
ACTION=change\0\
SUBSYSTEM=drm\0\
SEQNUM=4713\0";
    let event = parse_uevent(datagram).expect("parses");
    assert!(!event.is_display_hotplug());
}

#[test]
fn malformed_datagrams_do_not_parse() {
    assert!(parse_uevent(b"").is_none());
    assert!(parse_uevent(b"no-at-sign-header\0KEY=VALUE\0").is_none());
}

// ---------------------------------------------------------------------------
// Debounce
// ---------------------------------------------------------------------------

#[test]
fn debounce_fires_once_per_window() {
    let mut debounce = ReprobeDebounce::new(Duration::from_millis(500));
    let t0: u64 = 1_000_000_000;
    assert!(debounce.observe(t0), "the first event fires immediately");
    assert!(
        !debounce.observe(t0 + 100_000_000),
        "an event 100 ms later is inside the window: suppressed"
    );
    assert!(
        debounce.observe(t0 + 600_000_000),
        "an event 600 ms later is outside the window: fires"
    );
}

// ---------------------------------------------------------------------------
// Mode selection: kernel netlink preferred, polling fallback
// ---------------------------------------------------------------------------

/// A scripted uevent source: pops pre-loaded datagrams, then yields nothing.
struct ScriptedSource {
    datagrams: VecDeque<Vec<u8>>,
}

impl UeventSource for ScriptedSource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        if let Some(d) = self.datagrams.pop_front() {
            return Ok(Some(d));
        }
        // An idle socket: bounded wait, nothing arrived.
        std::thread::sleep(timeout.min(Duration::from_millis(5)));
        Ok(None)
    }
}

#[test]
fn select_mode_prefers_a_working_netlink_source() {
    let source = ScriptedSource {
        datagrams: VecDeque::new(),
    };
    let (mode, fallback_reason) = select_mode(Ok(source), Duration::from_secs(5));
    assert!(matches!(mode, HotplugMode::Netlink(_)));
    assert!(fallback_reason.is_none());
}

#[test]
fn select_mode_falls_back_to_polling_when_netlink_is_unavailable() {
    let (mode, fallback_reason) = select_mode::<ScriptedSource>(
        Err("binding the kernel uevent group: EPERM (rootless container)".to_owned()),
        Duration::from_secs(3),
    );
    match mode {
        HotplugMode::Polling(interval) => assert_eq!(interval, Duration::from_secs(3)),
        HotplugMode::Netlink(_) => panic!("expected the polling fallback"),
    }
    let reason = fallback_reason.expect("the fallback carries the netlink failure reason");
    assert!(reason.contains("EPERM"), "{reason}");
}

// ---------------------------------------------------------------------------
// The monitor thread
// ---------------------------------------------------------------------------

#[test]
fn monitor_debounces_a_kernel_event_burst_into_one_reprobe() {
    // A replug burst: two drm hotplug uevents (the kernel emits one per
    // affected connector/property) plus an unrelated usb event, all within
    // milliseconds — far inside the 500 ms debounce window.
    let burst = VecDeque::from(vec![
        DRM_CHANGE.to_vec(),
        b"add@/devices/pci0000:00/usb1/1-1\0ACTION=add\0SUBSYSTEM=usb\0".to_vec(),
        DRM_CHANGE.to_vec(),
    ]);
    let flag_a = ReprobeFlag::new();
    let flag_b = ReprobeFlag::new();
    let monitor = HotplugMonitor::start(
        HotplugMode::Netlink(ScriptedSource { datagrams: burst }),
        vec![flag_a.clone(), flag_b.clone()],
        Duration::from_millis(500),
    );
    wait_until(|| flag_a.is_requested() && flag_b.is_requested());
    // Let the rest of the burst drain through the monitor, then prove the
    // debounce: each flag was requested exactly once.
    std::thread::sleep(Duration::from_millis(100));
    assert!(flag_a.take(), "the first drm event requested a re-probe");
    assert!(
        !flag_a.take(),
        "the second drm event was inside the debounce window: suppressed"
    );
    assert!(flag_b.take(), "every sink's flag is requested");
    monitor.stop();
}

/// A scripted source that yields pre-loaded receive RESULTS (errors
/// included), then idles — models the kernel socket failure paths the
/// real netlink source surfaces (e.g. an ENOBUFS overrun).
struct FlakySource {
    steps: VecDeque<Result<Option<Vec<u8>>, String>>,
}

impl UeventSource for FlakySource {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        if let Some(step) = self.steps.pop_front() {
            return step;
        }
        // An idle socket: bounded wait, nothing arrived.
        std::thread::sleep(timeout.min(Duration::from_millis(5)));
        Ok(None)
    }
}

/// DEV-B5 F4: a netlink receive **error** can mean the kernel DROPPED
/// datagrams (an ENOBUFS queue overrun) — possibly the one drm hotplug
/// uevent. Event mode has no periodic re-probe, so a swallowed event would
/// be lost for the rest of the run. The monitor must treat any receive
/// error as "connector state may have changed unobserved" and request one
/// debounced recovery re-probe itself (idempotent — the flip loop that owns
/// the device just probes and finds nothing changed in the benign case).
#[test]
fn a_netlink_receive_error_requests_a_recovery_reprobe() {
    let flag_a = ReprobeFlag::new();
    let flag_b = ReprobeFlag::new();
    let monitor = HotplugMonitor::start(
        HotplugMode::Netlink(FlakySource {
            steps: VecDeque::from(vec![Err(
                "uevent recv: ENOBUFS (kernel socket overrun — datagrams dropped)".to_owned(),
            )]),
        }),
        vec![flag_a.clone(), flag_b.clone()],
        Duration::from_millis(500),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !(flag_a.is_requested() && flag_b.is_requested())
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(2));
    }
    monitor.stop();
    assert!(
        flag_a.take() && flag_b.take(),
        "a receive error (possible uevent overrun) must request a recovery \
         re-probe on every sink — a swallowed drm hotplug event is otherwise \
         lost forever in event mode"
    );
}

#[test]
fn polling_mode_requests_reprobes_periodically() {
    let flag = ReprobeFlag::new();
    let monitor = HotplugMonitor::start(
        HotplugMode::<ScriptedSource>::Polling(Duration::from_millis(20)),
        vec![flag.clone()],
        Duration::from_millis(1),
    );
    // Two successive polling fires prove periodicity (not a one-shot).
    wait_until(|| flag.take());
    wait_until(|| flag.take());
    monitor.stop();
}

// ---------------------------------------------------------------------------
// The flip loop's re-probe / re-light behaviour (scripted KmsBackend)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    Probe,
    Validate,
    Modeset,
    Submit,
}

/// A scripted backend whose connector state the test mutates between probes.
struct HotplugMockBackend {
    connectors: Arc<Mutex<Vec<ConnectorDesc>>>,
    probe_errors: Arc<Mutex<VecDeque<String>>>,
    calls: Arc<Mutex<Vec<Call>>>,
    pending_flips: u64,
    frame_counter: u64,
}

impl KmsBackend for HotplugMockBackend {
    fn probe_connectors(&mut self) -> Result<Vec<ConnectorDesc>, DisplayError> {
        self.calls.lock().unwrap().push(Call::Probe);
        if let Some(reason) = self.probe_errors.lock().unwrap().pop_front() {
            return Err(DisplayError::Device(reason));
        }
        Ok(self.connectors.lock().unwrap().clone())
    }

    fn validate_setup(&mut self, _setup: &HeadSetup) -> Result<(), DisplayError> {
        self.calls.lock().unwrap().push(Call::Validate);
        Ok(())
    }

    fn apply_modeset(&mut self, _setup: &HeadSetup) -> Result<(), DisplayError> {
        self.calls.lock().unwrap().push(Call::Modeset);
        Ok(())
    }

    fn submit_frame(&mut self, _frame: &dyn DisplayCanvas) -> Result<(), SubmitError> {
        self.calls.lock().unwrap().push(Call::Submit);
        self.pending_flips += 1;
        Ok(())
    }

    fn wait_events(&mut self, timeout: Duration) -> Result<Vec<FlipEvent>, DisplayError> {
        if self.pending_flips == 0 {
            std::thread::sleep(timeout.min(Duration::from_millis(1)));
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        while self.pending_flips > 0 {
            self.pending_flips -= 1;
            self.frame_counter += 1;
            events.push(FlipEvent {
                crtc_frame: u32::try_from(self.frame_counter).unwrap_or(u32::MAX),
                timestamp: Duration::from_millis(16 * self.frame_counter),
            });
        }
        Ok(events)
    }
}

fn hdmi_mode() -> DisplayModeInfo {
    DisplayModeInfo {
        width: 1920,
        height: 1080,
        clock_khz: 148_500,
        hsync_start: 2008,
        hsync_end: 2052,
        htotal: 2200,
        vsync_start: 1084,
        vsync_end: 1089,
        vtotal: 1125,
        hsync_positive: true,
        vsync_positive: true,
        preferred: true,
    }
}

fn hdmi_connector(connected: bool) -> ConnectorDesc {
    ConnectorDesc {
        name: "HDMI-A-1".to_owned(),
        connected,
        modes: vec![hdmi_mode()],
    }
}

struct StartedMockSink {
    connectors: Arc<Mutex<Vec<ConnectorDesc>>>,
    probe_errors: Arc<Mutex<VecDeque<String>>>,
    calls: Arc<Mutex<Vec<Call>>>,
    handle: multiview_output::display::DisplaySinkHandle,
    publisher: multiview_output::display::FramePublisher<TestCanvas>,
}

fn start_mock_sink() -> StartedMockSink {
    let connectors = Arc::new(Mutex::new(vec![hdmi_connector(true)]));
    let probe_errors = Arc::new(Mutex::new(VecDeque::new()));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let backend = HotplugMockBackend {
        connectors: Arc::clone(&connectors),
        probe_errors: Arc::clone(&probe_errors),
        calls: Arc::clone(&calls),
        pending_flips: 0,
        frame_counter: 0,
    };
    let (handle, publisher) = DisplaySink::start::<TestCanvas, _>(
        backend,
        DisplaySinkConfig {
            output_id: "out-hotplug-test".to_owned(),
            connector: ConnectorSelector::Name("HDMI-A-1".to_owned()),
            mode: ModeRequest::Auto,
            forced_mode: None,
            engine_cadence: Some(Rational::new(60, 1)),
            poll_interval: Duration::from_millis(1),
        },
    )
    .expect("startup succeeds");
    StartedMockSink {
        connectors,
        probe_errors,
        calls,
        handle,
        publisher,
    }
}

#[derive(Debug)]
struct TestCanvas;

impl DisplayCanvas for TestCanvas {
    fn width(&self) -> u32 {
        2
    }
    fn height(&self) -> u32 {
        2
    }
    fn y_plane(&self) -> &[u8] {
        &[16, 16, 16, 16]
    }
    fn uv_plane(&self) -> &[u8] {
        &[128, 128]
    }
}

fn count(calls: &Arc<Mutex<Vec<Call>>>, kind: Call) -> usize {
    calls.lock().unwrap().iter().filter(|c| **c == kind).count()
}

fn wait_until(pred: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !pred() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not reached within the test bound"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_reprobe_request_probes_without_touching_the_mode() {
    let sink = start_mock_sink();
    assert_eq!(count(&sink.calls, Call::Probe), 1, "the startup probe");

    // The connector stays connected: a re-probe must record the probe and
    // change nothing (no validate, no modeset beyond startup's one).
    sink.handle.reprobe_flag().request();
    wait_until(|| count(&sink.calls, Call::Probe) >= 2);
    assert_eq!(count(&sink.calls, Call::Modeset), 1);
    assert_eq!(count(&sink.calls, Call::Validate), 1);
    let stats = sink.handle.stats().snapshot();
    assert_eq!(stats.reprobes, 1);
    assert_eq!(stats.relights, 0);
    drop(sink.publisher);
}

#[test]
fn a_reconnect_revalidates_and_relights_the_head() {
    let sink = start_mock_sink();

    // Unplug: the next re-probe sees the connector disconnected; the sink
    // holds the last framebuffer (KMS keeps scanning it out) and does NOT
    // modeset.
    sink.connectors.lock().unwrap()[0] = hdmi_connector(false);
    sink.handle.reprobe_flag().request();
    wait_until(|| count(&sink.calls, Call::Probe) >= 2);
    assert_eq!(
        count(&sink.calls, Call::Modeset),
        1,
        "no modeset while dark"
    );

    // Replug: the next re-probe sees it connected again → TEST_ONLY validate
    // first, then the ONE re-light modeset (DP link retraining and HDMI
    // re-handshake need the modeset re-applied).
    sink.connectors.lock().unwrap()[0] = hdmi_connector(true);
    sink.handle.reprobe_flag().request();
    wait_until(|| count(&sink.calls, Call::Modeset) >= 2);
    assert_eq!(
        count(&sink.calls, Call::Validate),
        2,
        "re-light validates before committing"
    );

    let stats = sink.handle.stats().snapshot();
    assert_eq!(stats.reprobes, 2);
    assert_eq!(stats.relights, 1);
    drop(sink.publisher);
}

#[test]
fn probe_errors_never_kill_the_flip_loop() {
    let sink = start_mock_sink();
    sink.probe_errors
        .lock()
        .unwrap()
        .push_back("transient ioctl failure".to_owned());
    sink.handle.reprobe_flag().request();
    wait_until(|| count(&sink.calls, Call::Probe) >= 2);

    // The loop survived: a published frame still commits.
    let submits_before = count(&sink.calls, Call::Submit);
    sink.publisher.publish(TestCanvas);
    wait_until(|| count(&sink.calls, Call::Submit) > submits_before);
    drop(sink.publisher);
}

// ---------------------------------------------------------------------------
// End-to-end: monitor → flag → flip-loop re-probe
// ---------------------------------------------------------------------------

#[test]
fn a_kernel_uevent_drives_a_sink_reprobe_through_the_monitor() {
    let sink = start_mock_sink();
    let monitor = HotplugMonitor::start(
        HotplugMode::Netlink(ScriptedSource {
            datagrams: VecDeque::from(vec![DRM_CHANGE.to_vec()]),
        }),
        vec![sink.handle.reprobe_flag()],
        Duration::from_millis(100),
    );
    wait_until(|| count(&sink.calls, Call::Probe) >= 2);
    monitor.stop();
    drop(sink.publisher);
}

// ---------------------------------------------------------------------------
// The real kernel netlink source (feature `display-kms`)
// ---------------------------------------------------------------------------

#[cfg(feature = "display-kms")]
mod kms_netlink {
    use std::time::Duration;

    use multiview_output::display::hotplug::UeventSource as _;
    use multiview_output::display::kms::{is_initial_user_namespace, KernelUeventSocket};

    #[test]
    fn the_initial_user_namespace_map_is_recognized() {
        // The initial user namespace's identity mapping (also what a normal
        // ROOTFUL container sees — it shares the initial userns, so kernel
        // uevents reach it).
        assert!(is_initial_user_namespace(
            "         0          0 4294967295\n"
        ));
    }

    #[test]
    fn rootless_user_namespace_maps_are_not_initial() {
        // A rootless container's subuid mapping: kernel uevents are NOT
        // delivered to its netns → the polling fallback is required.
        assert!(!is_initial_user_namespace(
            "         0       1000          1\n         1     100000      65536\n"
        ));
        assert!(!is_initial_user_namespace(""));
        assert!(!is_initial_user_namespace("garbage\n"));
    }

    #[test]
    fn kernel_uevent_socket_open_never_panics() {
        // Both outcomes are legal in CI: a usable kernel uevent socket
        // (bare metal / rootful), or a clear reason (rootless / sandbox).
        // Real uevent DELIVERY is hardware/deployment-validated (t630 leg),
        // not CI-assertable.
        match KernelUeventSocket::open() {
            Ok(mut source) => {
                // A bounded idle read must not error out.
                let outcome = source.recv_timeout(Duration::from_millis(1));
                assert!(outcome.is_ok(), "idle read errored: {outcome:?}");
            }
            Err(reason) => {
                assert!(!reason.is_empty(), "the fallback reason must say why");
            }
        }
    }
}
