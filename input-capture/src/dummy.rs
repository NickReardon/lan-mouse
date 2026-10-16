use std::f64::consts::PI;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use input_event::PointerEvent;
use tokio::time::{self, Instant, Interval};

use super::{Capture, CaptureError, CaptureEvent, Position};

/// The simulated cursor always leaves the screen through this edge.
const EDGE: Position = Position::Left;

/// Pause after a release before the simulated cursor enters again,
/// so an unreachable peer is retried without flooding the log.
const REENTRY_DELAY: Duration = Duration::from_millis(500);

pub struct DummyInputCapture {
    /// whether a capture exists at [`EDGE`]
    edge_created: bool,
    /// start of the current capture session, `None` until `Begin` is sent
    start: Option<Instant>,
    /// earliest time the next capture session may begin
    resume_at: Instant,
    interval: Interval,
    offset: (i32, i32),
}

impl DummyInputCapture {
    pub fn new() -> Self {
        Self {
            edge_created: false,
            start: None,
            resume_at: Instant::now(),
            interval: time::interval(Duration::from_millis(1)),
            offset: (0, 0),
        }
    }
}

impl Default for DummyInputCapture {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Capture for DummyInputCapture {
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError> {
        if pos == EDGE {
            self.edge_created = true;
        }
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        if pos == EDGE {
            self.edge_created = false;
            self.start = None;
        }
        Ok(())
    }

    async fn release_at(&mut self, _pos: Position, _fraction: u16) -> Result<(), CaptureError> {
        self.release().await
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        // Like a real cursor crossing the edge again, the next session
        // starts with a new `Begin`, which the service needs to activate a
        // client after a release (e.g. a failed send before connecting).
        self.start = None;
        self.resume_at = Instant::now() + REENTRY_DELAY;
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

const FREQUENCY_HZ: f64 = 1.0;
const RADIUS: f64 = 100.0;

impl Stream for DummyInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Only emit while a capture exists at the edge: events without a
        // capture are dropped, which could lose `Begin`.
        let current = loop {
            let current = ready!(self.interval.poll_tick(cx));
            if self.edge_created && current >= self.resume_at {
                break current;
            }
        };
        let event = match self.start {
            None => {
                self.start.replace(current);
                self.offset = (0, 0);
                CaptureEvent::Begin
            }
            Some(start) => {
                let elapsed = start.elapsed();
                let elapsed_sec_f64 = elapsed.as_secs_f64();
                let second_fraction = elapsed_sec_f64 - elapsed_sec_f64 as u64 as f64;
                let radians = second_fraction * 2. * PI * FREQUENCY_HZ;
                let offset = (radians.cos() * RADIUS * 2., (radians * 2.).sin() * RADIUS);
                let offset = (offset.0 as i32, offset.1 as i32);
                let relative_motion = (offset.0 - self.offset.0, offset.1 - self.offset.1);
                self.offset = offset;
                let (dx, dy) = (relative_motion.0 as f64, relative_motion.1 as f64);
                CaptureEvent::Input(input_event::Event::Pointer(PointerEvent::Motion {
                    time: 0,
                    dx,
                    dy,
                }))
            }
        };
        Poll::Ready(Some(Ok((EDGE, event))))
    }
}
