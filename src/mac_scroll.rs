//! macOS trackpad gesture phases for src/scroll.rs (docs/scrolling.md).
//!
//! SDL turns each `NSEventTypeScrollWheel` event with a non-zero delta into
//! one wheel event and drops everything else: `phase` (fingers began,
//! changed, ended), `momentumPhase` (coasting after the lift) and
//! `hasPreciseScrollingDeltas` (trackpad or Magic Mouse rather than a
//! notched wheel). An NSEvent local monitor sees the same events first, on
//! the main thread inside SDL's event pump, and queues what SDL drops: a
//! [`Device`] for every event SDL will deliver, and [`Marker`]s for the
//! phases, in order. The session loop pairs them with SDL's wheel events.
//!
//! Not verifiable without a Mac: the monitor is type-checked only.
use crate::scroll::{Device, Marker};
use block2::RcBlock;
use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
use std::{
    collections::VecDeque,
    ptr::NonNull,
    sync::Mutex,
    time::{Duration, Instant},
};

/// `NSEventMaskScrollWheel` (1 << `NSEventTypeScrollWheel`).
const SCROLL_WHEEL_MASK: u64 = 1 << 22;
// NSEventPhase bits.
const BEGAN: usize = 1 << 0;
const ENDED: usize = 1 << 3;
const CANCELLED: usize = 1 << 4;
const MAY_BEGIN: usize = 1 << 5;
/// Records SDL never consumed (a dropped event) are not kept for long.
const MAX_QUEUED: usize = 256;
const STALE: Duration = Duration::from_millis(250);

enum Record {
    Wheel(Device, Instant),
    Marker(Marker),
}

static QUEUE: Mutex<VecDeque<Record>> = Mutex::new(VecDeque::new());

type Handler = RcBlock<dyn Fn(NonNull<AnyObject>) -> *mut AnyObject>;

/// The installed monitor; removed on drop.
pub struct Monitor {
    handle: Retained<AnyObject>,
    _handler: Handler,
}

impl Monitor {
    /// Installs the monitor (main thread, after SDL's video init).
    pub fn install() -> Option<Self> {
        let handler: Handler = RcBlock::new(|event: NonNull<AnyObject>| -> *mut AnyObject {
            // SAFETY: AppKit passes a valid NSEvent for the block's duration.
            record(unsafe { event.as_ref() });
            // Returned unchanged: SDL still handles every event.
            event.as_ptr()
        });
        // SAFETY: documented AppKit class method; the block outlives the
        // monitor (both are dropped together, the monitor removed first).
        let handle: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                class!(NSEvent),
                addLocalMonitorForEventsMatchingMask: SCROLL_WHEEL_MASK,
                handler: &*handler
            ]
        };
        let handle = handle?;
        tracing::info!("reading macOS trackpad scroll phases");
        Some(Self {
            handle,
            _handler: handler,
        })
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // SAFETY: removes the monitor this value installed.
        unsafe {
            let _: () = msg_send![class!(NSEvent), removeMonitor: &*self.handle];
        }
        QUEUE.lock().unwrap().clear();
    }
}

fn record(event: &AnyObject) {
    // SAFETY: NSEvent scroll-wheel accessors (macOS 10.7+).
    let (phase, momentum, precise, dx, dy) = unsafe {
        let phase: usize = msg_send![event, phase];
        let momentum: usize = msg_send![event, momentumPhase];
        let precise: bool = msg_send![event, hasPreciseScrollingDeltas];
        let dx: f64 = msg_send![event, deltaX];
        let dy: f64 = msg_send![event, deltaY];
        (phase, momentum, precise, dx, dy)
    };
    let mut queue = QUEUE.lock().unwrap();
    if queue.len() >= MAX_QUEUED {
        queue.clear();
    }
    if phase & (BEGAN | MAY_BEGIN) != 0 {
        queue.push_back(Record::Marker(Marker::Touch));
    }
    if momentum & BEGAN != 0 {
        queue.push_back(Record::Marker(Marker::MomentumStart));
    }
    // SDL sends a wheel event for exactly these (it drops zero deltas).
    if dx != 0.0 || dy != 0.0 {
        queue.push_back(Record::Wheel(
            Device {
                precise,
                momentum: momentum != 0,
            },
            Instant::now(),
        ));
    }
    if phase & ENDED != 0 {
        queue.push_back(Record::Marker(Marker::Lift));
    }
    if phase & CANCELLED != 0 {
        queue.push_back(Record::Marker(Marker::Cancel));
    }
    if momentum & (ENDED | CANCELLED) != 0 {
        queue.push_back(Record::Marker(Marker::MomentumEnd));
    }
}

/// For an SDL wheel event (`wheel`): the markers queued before it and its
/// device. At the end of an event batch (`!wheel`): the remaining markers.
pub fn take(wheel: bool) -> (Vec<Marker>, Option<Device>) {
    let mut queue = QUEUE.lock().unwrap();
    let mut markers = Vec::new();
    while let Some(record) = queue.pop_front() {
        match record {
            Record::Marker(marker) => markers.push(marker),
            // Never consumed by SDL: it would mislabel a later event.
            Record::Wheel(_, at) if at.elapsed() >= STALE => (),
            Record::Wheel(device, _) if wheel => return (markers, Some(device)),
            // The batch is over; SDL delivers this one next time.
            Record::Wheel(..) => {
                queue.push_front(record);
                return (markers, None);
            }
        }
    }
    (markers, None)
}
