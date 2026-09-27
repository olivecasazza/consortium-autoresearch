//! I/O engine abstraction.
//!
//! Rust implementation of `ClusterShell.Engine`.
//!
//! The Python version has EPoll, Poll, and Select backends.
//! In Rust we will likely use a single backend (mio or polling crate).

use slotmap::{new_key_type, SlotMap};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io;
use std::os::unix::io::RawFd;
use thiserror::Error;

// ============================================================================
// Constants
// ============================================================================

/// Read event flag
pub const E_READ: u32 = 0x1;

/// Write event flag
pub const E_WRITE: u32 = 0x2;

/// Time epsilon for floating point comparisons
pub const EPSILON: f64 = 1.0e-3;

/// Unlimited fanout value
pub const FANOUT_UNLIMITED: i32 = -1;

// ============================================================================
// Error Types
// ============================================================================

/// Error types for the Engine module
#[derive(Error, Debug)]
pub enum EngineError {
    /// Operation aborted, optionally with kill flag
    #[error("Operation aborted (kill={kill})")]
    Abort { kill: bool },

    /// Operation timed out
    #[error("Operation timed out")]
    Timeout,

    /// Illegal operation was attempted
    #[error("Illegal operation: {0}")]
    IllegalOperation(String),

    /// Engine is already running
    #[error("Engine is already running")]
    AlreadyRunning,

    /// Operation not supported
    #[error("Not supported: {0}")]
    NotSupported(String),

    /// I/O error
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

// ============================================================================
// Timer ID
// ============================================================================

new_key_type! { pub struct TimerId; }

// ============================================================================
// EngineBaseTimer
// ============================================================================

/// Base timer structure with fire delay, interval, and autoclose settings.
///
/// This is the core timer struct that Engine uses. It can be armed/disarmed
/// and supports repeating timers via interval.
#[derive(Debug, Clone)]
pub struct EngineBaseTimer {
    fire_delay: f64,
    interval: f64,
    autoclose: bool,
}

impl EngineBaseTimer {
    /// Create a new EngineBaseTimer
    ///
    /// # Arguments
    /// * `fire_delay` - Seconds until timer fires (-1.0 means not armed)
    /// * `interval` - Seconds between repeats (-1.0 means no repeat)
    /// * `autoclose` - Whether to autoclose the client when timer fires
    pub fn new(fire_delay: f64, interval: f64, autoclose: bool) -> Self {
        Self {
            fire_delay,
            interval,
            autoclose,
        }
    }

    /// Invalidate this timer (disarm it)
    pub fn invalidate(&mut self) {
        self.fire_delay = -1.0;
    }

    /// Check if this timer is valid (armed)
    pub fn is_valid(&self) -> bool {
        self.fire_delay >= 0.0
    }

    /// Set the next fire delay and interval
    ///
    /// # Arguments
    /// * `fire_delay` - Seconds until next fire (-1.0 means disarm)
    /// * `interval` - Seconds between repeats (-1.0 means no repeat)
    pub fn set_nextfire(&mut self, fire_delay: f64, interval: f64) {
        self.fire_delay = fire_delay;
        self.interval = interval;
    }

    /// Get the fire delay
    pub fn fire_delay(&self) -> f64 {
        self.fire_delay
    }

    /// Get the interval
    pub fn interval(&self) -> f64 {
        self.interval
    }

    /// Get the autoclose flag
    pub fn autoclose(&self) -> bool {
        self.autoclose
    }
}

// ============================================================================
// TimerQueue (Internal)
// ============================================================================

/// Internal timer entry used by TimerQueue
#[derive(Debug, Clone, PartialEq)]
struct TimerEntry {
    fire_date: f64,
    timer_id: TimerId,
    armed: bool,
}

impl Eq for TimerEntry {}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse ordering: lower fire_date comes first (min-heap behavior)
        self.fire_date
            .partial_cmp(&other.fire_date)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| self.timer_id.cmp(&other.timer_id))
    }
}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Timer queue managing a set of timers using a BinaryHeap.
///
/// Timers are ordered by their fire_date for efficient expiration checking.
/// Repeating timers are automatically rearmed after firing.
#[derive(Debug)]
pub struct TimerQueue {
    timers: BinaryHeap<Reverse<TimerEntry>>,
    timer_data: SlotMap<TimerId, EngineBaseTimer>,
    armed_count: usize,
    current_time: f64,
}

impl TimerQueue {
    /// Create a new empty TimerQueue
    pub fn new() -> Self {
        Self {
            timers: BinaryHeap::new(),
            timer_data: SlotMap::with_capacity_and_key(16),
            armed_count: 0,
            current_time: 0.0,
        }
    }

    /// Schedule a timer for future firing
    ///
    /// # Arguments
    /// * `timer_id` - The ID of the timer to schedule
    pub fn schedule(&mut self, timer_id: TimerId) {
        if let Some(timer) = self.timer_data.get(timer_id) {
            if timer.fire_delay >= 0.0 {
                let fire_date = self.current_time + timer.fire_delay;
                let entry = TimerEntry {
                    fire_date,
                    timer_id,
                    armed: true,
                };
                self.timers.push(Reverse(entry));
                self.armed_count += 1;
            }
        }
    }

    /// Reschedule an existing timer
    ///
    /// # Arguments
    /// * `timer_id` - The ID of the timer to reschedule
    pub fn reschedule(&mut self, timer_id: TimerId) {
        // Remove old entry if it exists
        self.invalidate(timer_id);
        self.schedule(timer_id);
    }

    /// Invalidate (disarm) a timer
    ///
    /// # Arguments
    /// * `timer_id` - The ID of the timer to invalidate
    pub fn invalidate(&mut self, timer_id: TimerId) {
        // Find and remove the timer entry
        let mut found = false;
        let mut new_heap = BinaryHeap::new();
        for Reverse(entry) in self.timers.drain() {
            if entry.timer_id == timer_id {
                found = true;
                // Update the timer state
                if let Some(timer) = self.timer_data.get_mut(timer_id) {
                    timer.invalidate();
                }
            } else {
                new_heap.push(Reverse(entry));
            }
        }
        self.timers = new_heap;
        if found {
            self.armed_count = self.armed_count.saturating_sub(1);
        }
    }

    /// Fire all expired timers
    ///
    /// Returns a list of (timer_id, autoclose) for timers that fired
    pub fn fire_expired(&mut self) -> Vec<(TimerId, bool)> {
        let mut fired = Vec::new();
        let now = self.current_time;

        // Process all timers that have expired
        while let Some(Reverse(entry)) = self.timers.peek() {
            if entry.fire_date > now + EPSILON {
                break;
            }

            let entry = self.timers.pop().unwrap().0;
            let timer_id = entry.timer_id;

            if let Some(timer) = self.timer_data.get(timer_id) {
                if entry.armed && timer.fire_delay >= 0.0 {
                    fired.push((timer_id, timer.autoclose));

                    // If this is a repeating timer, reschedule it
                    if timer.interval >= 0.0 {
                        let new_fire_date = now + timer.interval;
                        let new_entry = TimerEntry {
                            fire_date: new_fire_date,
                            timer_id,
                            armed: true,
                        };
                        self.timers.push(Reverse(new_entry));
                    } else {
                        // One-shot timer: disarm it
                        if let Some(t) = self.timer_data.get_mut(timer_id) {
                            t.invalidate();
                        }
                        self.armed_count = self.armed_count.saturating_sub(1);
                    }
                }
            }
        }

        fired
    }

    /// Get the time until the next timer fires
    ///
    /// Returns None if no timers are armed
    pub fn nextfire_delay(&self) -> Option<f64> {
        if let Some(Reverse(entry)) = self.timers.peek() {
            let delay = entry.fire_date - self.current_time;
            if delay > EPSILON {
                Some(delay)
            } else {
                Some(0.0)
            }
        } else {
            None
        }
    }

    /// Clear all timers
    pub fn clear(&mut self) {
        self.timers.clear();
        self.timer_data.clear();
        self.armed_count = 0;
    }

    /// Add a timer to the queue
    ///
    /// Returns the TimerId for the new timer
    pub fn add_timer(&mut self, timer: EngineBaseTimer) -> TimerId {
        self.timer_data.insert(timer)
    }

    /// Remove a timer by ID
    pub fn remove_timer(&mut self, timer_id: TimerId) {
        self.invalidate(timer_id);
        self.timer_data.remove(timer_id);
    }

    /// Get the number of armed timers
    pub fn armed_count(&self) -> usize {
        self.armed_count
    }

    /// Update the current time (should be called before fire_expired)
    pub fn update_time(&mut self, time: f64) {
        self.current_time = time;
    }

    /// Check if a timer is valid
    pub fn is_valid(&self, timer_id: TimerId) -> bool {
        self.timer_data
            .get(timer_id)
            .map(|t| t.is_valid())
            .unwrap_or(false)
    }
}

impl Default for TimerQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// EngineTimer (concrete timer with handler)
// ============================================================================

/// Concrete timer with a callback handler.
///
/// When the timer fires, the handler function is called. This mirrors
/// Python's `EngineTimer` which has `_fire()` calling `handler.ev_timer()`.
pub struct EngineTimer {
    base: EngineBaseTimer,
    handler: Box<dyn FnMut() + Send>,
}

impl EngineTimer {
    /// Create a new EngineTimer.
    ///
    /// # Arguments
    /// * `fire_delay` - Seconds until timer fires (-1.0 means not armed)
    /// * `interval` - Seconds between repeats (-1.0 means no repeat)
    /// * `autoclose` - Whether to autoclose the associated client
    /// * `handler` - Callback invoked when the timer fires
    pub fn new(
        fire_delay: f64,
        interval: f64,
        autoclose: bool,
        handler: impl FnMut() + Send + 'static,
    ) -> Self {
        Self {
            base: EngineBaseTimer::new(fire_delay, interval, autoclose),
            handler: Box::new(handler),
        }
    }

    /// Fire the timer (invoke the handler).
    pub fn fire(&mut self) {
        (self.handler)();
    }

    /// Get a reference to the underlying EngineBaseTimer.
    pub fn base(&self) -> &EngineBaseTimer {
        &self.base
    }

    /// Get a mutable reference to the underlying EngineBaseTimer.
    pub fn base_mut(&mut self) -> &mut EngineBaseTimer {
        &mut self.base
    }
}

impl std::fmt::Debug for EngineTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineTimer")
            .field("base", &self.base)
            .field("handler", &"<fn>")
            .finish()
    }
}

// ============================================================================
// EngineClient trait
// ============================================================================

/// Trait for objects that can be managed by the Engine.
///
/// Clients are I/O objects (files, sockets, pipes) that the engine
/// monitors for readability/writability and manages through their lifecycle.
pub trait EngineClient: Send {
    /// Start the client
    fn start(&mut self) -> io::Result<()>;

    /// Close the client
    ///
    /// # Arguments
    /// * `abort` - Whether this is an abort close
    /// * `timeout` - Whether this is a timeout close
    fn close(&mut self, abort: bool, timeout: bool);

    /// Get the file descriptor, if any
    fn fd(&self) -> Option<RawFd>;

    /// Get the event interest bitmask (E_READ, E_WRITE, etc.)
    fn events(&self) -> u32;

    /// Check if this client can be delayed (batched)
    fn is_delayable(&self) -> bool;

    /// Check if this client is currently registered with the engine
    fn is_registered(&self) -> bool;

    /// Set the registered state
    fn set_registered(&mut self, val: bool);

    /// Check if this client should be autoclosed
    fn autoclose(&self) -> bool;

    /// Handle a read event on this client.
    /// Returns the number of bytes read, or 0 on EOF.
    fn handle_read(&mut self) -> io::Result<usize> {
        Ok(0)
    }

    /// Handle a write event on this client.
    /// Returns the number of bytes written.
    fn handle_write(&mut self) -> io::Result<usize> {
        Ok(0)
    }
}

// ============================================================================
// Client ID
// ============================================================================

new_key_type! { pub struct ClientId; }

// ============================================================================
// PollBackend trait
// ============================================================================

/// Trait for I/O polling backends.
///
/// This trait abstracts the underlying polling mechanism (epoll, kqueue, poll, etc.)
/// and allows for different implementations without changing the core Engine logic.
pub trait PollBackend: Send + Sync {
    /// Register a file descriptor for events
    ///
    /// # Arguments
    /// * `fd` - The file descriptor to register
    /// * `events` - Event bitmask (E_READ, E_WRITE)
    /// * `client_id` - The client ID associated with this fd
    fn register(&mut self, fd: RawFd, events: u32, client_id: ClientId) -> io::Result<()>;

    /// Modify event registration for a file descriptor
    fn modify(&mut self, fd: RawFd, events: u32, client_id: ClientId) -> io::Result<()>;

    /// Unregister a file descriptor
    fn unregister(&mut self, fd: RawFd) -> io::Result<()>;

    /// Wait for events with optional timeout (in seconds)
    ///
    /// Returns a list of (fd, events, client_id) tuples for ready events
    fn wait(&mut self, timeout: Option<f64>) -> io::Result<Vec<(RawFd, u32, ClientId)>>;

    /// Get the backend name
    fn name(&self) -> &str;
}

// ============================================================================
// Engine struct
// ============================================================================

/// The main I/O event loop engine.
///
/// The Engine manages:
/// - Timer scheduling and expiration
/// - Client registration and event monitoring
/// - The main event loop
/// - Fanout control (maximum concurrent operations)
///
/// # Example
/// ```no_run
/// use consortium::engine::{Engine, EngineBaseTimer, EngineClient, PollBackend, ClientId, FANOUT_UNLIMITED};
/// use std::os::unix::io::RawFd;
///
/// // Engine requires a concrete PollBackend implementation.
/// // See NullBackend in tests for a minimal example.
/// ```
pub struct Engine<B: PollBackend> {
    clients: SlotMap<ClientId, Box<dyn EngineClient>>,
    timerq: TimerQueue,
    reg_clifds: HashMap<RawFd, ClientId>,
    poll_backend: B,
    fanout: usize,
    running: bool,
    exited: bool,
    evlooprefcnt: i32,
}

impl<B: PollBackend> Engine<B> {
    /// Create a new Engine instance with the given backend.
    ///
    /// # Arguments
    /// * `backend` - The polling backend implementation
    /// * `fanout` - Maximum concurrent operations (0 for no limit)
    pub fn with_backend(backend: B, fanout: usize) -> Self {
        Self {
            clients: SlotMap::with_key(),
            timerq: TimerQueue::new(),
            reg_clifds: HashMap::new(),
            poll_backend: backend,
            fanout,
            running: false,
            exited: false,
            evlooprefcnt: 0,
        }
    }

    /// Add a timer and return its ID
    ///
    /// # Arguments
    /// * `timer` - The timer to add
    pub fn add_timer(&mut self, timer: EngineBaseTimer) -> TimerId {
        let timer_id = self.timerq.add_timer(timer);
        self.timerq.schedule(timer_id);
        timer_id
    }

    /// Remove a timer by ID
    ///
    /// # Arguments
    /// * `id` - The timer ID to remove
    pub fn remove_timer(&mut self, id: TimerId) {
        self.timerq.remove_timer(id);
    }

    /// Fire all expired timers
    pub fn fire_timers(&mut self) {
        let _fired = self.timerq.fire_expired();
        // Timer firing is handled internally by the timer queue.
        // Client-associated timers will be integrated when the
        // full client lifecycle is implemented.
    }

    /// Run the engine event loop
    ///
    /// # Arguments
    /// * `timeout` - Optional timeout in seconds (None for no limit)
    pub fn run(&mut self, timeout: Option<f64>) -> Result<(), EngineError> {
        if self.running {
            return Err(EngineError::AlreadyRunning);
        }

        self.running = true;
        self.exited = false;

        let start_time = self.timerq.current_time;
        let max_time = timeout.map(|t| start_time + t);

        while self.running {
            // Update current time
            self.timerq.update_time(self.timerq.current_time + 0.001);

            // Fire any expired timers
            self.fire_timers();

            // Check if we should exit
            if !self.running {
                break;
            }

            // Get next timer delay
            let next_timer_delay = self.timerq.nextfire_delay();

            // Calculate timeout for poll
            let poll_timeout = match (max_time, next_timer_delay) {
                (Some(max), Some(timer_delay)) => {
                    let remaining = max - self.timerq.current_time;
                    if remaining <= EPSILON {
                        return Err(EngineError::Timeout);
                    }
                    // Use f64::min by comparing manually since f64 doesn't implement Ord
                    Some(if remaining < timer_delay {
                        remaining
                    } else {
                        timer_delay
                    })
                }
                (Some(max), None) => {
                    let remaining = max - self.timerq.current_time;
                    if remaining <= EPSILON {
                        return Err(EngineError::Timeout);
                    }
                    Some(remaining)
                }
                (None, Some(timer_delay)) => Some(timer_delay),
                (None, None) => None,
            };

            // Wait for events
            let events = self.poll_backend.wait(poll_timeout.map(|t| t.max(0.0)))?;

            // Process events — dispatch read/write handlers
            for (_fd, events_mask, client_id) in events {
                if let Some(client) = self.clients.get_mut(client_id) {
                    if events_mask & E_READ != 0 {
                        let _ = client.handle_read();
                    }
                    if events_mask & E_WRITE != 0 {
                        let _ = client.handle_write();
                    }
                }
            }

            // Check timeout
            if let Some(max) = max_time {
                if self.timerq.current_time >= max {
                    return Err(EngineError::Timeout);
                }
            }
        }

        self.exited = true;
        self.running = false;
        Ok(())
    }

    /// Abort the engine
    ///
    /// # Arguments
    /// * `kill` - If true, force kill all clients
    pub fn abort(&mut self, kill: bool) {
        if kill {
            // Force kill all clients
            for (_, client) in self.clients.iter_mut() {
                client.close(true, false);
            }
        }
        self.running = false;
    }

    /// Check if the engine is running
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Check if the engine has exited
    pub fn exited(&self) -> bool {
        self.exited
    }

    /// Add a client to the engine
    ///
    /// # Arguments
    /// * `client` - The client to add
    pub fn add_client(&mut self, client: Box<dyn EngineClient>) -> Result<ClientId, EngineError> {
        let client_id = self.clients.insert(client);

        if let Some(client) = self.clients.get(client_id) {
            if let Some(fd) = client.fd() {
                if fd >= 0 {
                    let events = client.events();
                    self.poll_backend.register(fd, events, client_id)?;
                    self.reg_clifds.insert(fd, client_id);
                }
            }
        }

        Ok(client_id)
    }

    /// Remove a client by ID
    ///
    /// # Arguments
    /// * `id` - The client ID to remove
    pub fn remove_client(&mut self, id: ClientId) {
        if let Some(client) = self.clients.get(id) {
            if let Some(fd) = client.fd() {
                if fd >= 0 {
                    let _ = self.poll_backend.unregister(fd);
                    self.reg_clifds.remove(&fd);
                }
            }
        }
        self.clients.remove(id);
    }

    /// Get the fanout value
    pub fn fanout(&self) -> usize {
        self.fanout
    }

    /// Update client event interests
    pub fn update_client_events(&mut self, client_id: ClientId) -> Result<(), EngineError> {
        if let Some(client) = self.clients.get(client_id) {
            if let Some(fd) = client.fd() {
                if fd >= 0 {
                    let events = client.events();
                    if self.reg_clifds.contains_key(&fd) {
                        self.poll_backend.modify(fd, events, client_id)?;
                    } else {
                        self.poll_backend.register(fd, events, client_id)?;
                        self.reg_clifds.insert(fd, client_id);
                    }
                }
            }
        }
        Ok(())
    }

    /// Stop the engine event loop.
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Increment the event loop reference count.
    /// While refcnt > 0, the engine loop stays alive.
    pub fn evloop_acquire(&mut self) {
        self.evlooprefcnt += 1;
    }

    /// Decrement the event loop reference count.
    /// When refcnt reaches 0, the engine stops.
    pub fn evloop_release(&mut self) {
        self.evlooprefcnt -= 1;
        if self.evlooprefcnt <= 0 {
            self.running = false;
        }
    }

    /// Get the number of registered clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    /// Get the timer queue (immutable).
    pub fn timerq(&self) -> &TimerQueue {
        &self.timerq
    }

    /// Get the timer queue (mutable).
    pub fn timerq_mut(&mut self) -> &mut TimerQueue {
        &mut self.timerq
    }

    /// Start all registered clients.
    pub fn start_all_clients(&mut self) -> io::Result<()> {
        let ids: Vec<ClientId> = self.clients.keys().collect();
        for id in ids {
            if let Some(client) = self.clients.get_mut(id) {
                client.start()?;
            }
        }
        Ok(())
    }
}

// ============================================================================
// NullBackend — no-op backend for testing
// ============================================================================

/// A no-op polling backend that never returns events.
///
/// Useful for testing timer logic and client management without actual I/O.
#[derive(Debug, Default)]
pub struct NullBackend;

impl PollBackend for NullBackend {
    fn register(&mut self, _fd: RawFd, _events: u32, _client_id: ClientId) -> io::Result<()> {
        Ok(())
    }

    fn modify(&mut self, _fd: RawFd, _events: u32, _client_id: ClientId) -> io::Result<()> {
        Ok(())
    }

    fn unregister(&mut self, _fd: RawFd) -> io::Result<()> {
        Ok(())
    }

    fn wait(&mut self, _timeout: Option<f64>) -> io::Result<Vec<(RawFd, u32, ClientId)>> {
        Ok(Vec::new())
    }

    fn name(&self) -> &str {
        "null"
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_engine_constants() {
        assert_eq!(E_READ, 0x1);
        assert_eq!(E_WRITE, 0x2);
        assert!((EPSILON - 1.0e-3).abs() < EPSILON);
        assert_eq!(FANOUT_UNLIMITED, -1);
    }

    #[test]
    fn test_engine_base_timer() {
        let mut timer = EngineBaseTimer::new(5.0, -1.0, false);
        assert!(timer.is_valid());
        assert_eq!(timer.fire_delay(), 5.0);
        assert_eq!(timer.interval(), -1.0);

        timer.invalidate();
        assert!(!timer.is_valid());
        assert_eq!(timer.fire_delay(), -1.0);

        timer.set_nextfire(10.0, 2.0);
        assert!(timer.is_valid());
        assert_eq!(timer.fire_delay(), 10.0);
        assert_eq!(timer.interval(), 2.0);
    }

    #[test]
    fn test_timer_queue_scheduling() {
        let mut timerq = TimerQueue::new();

        // Add some timers
        let timer1 = timerq.add_timer(EngineBaseTimer::new(1.0, -1.0, false));
        let timer2 = timerq.add_timer(EngineBaseTimer::new(2.0, -1.0, false));
        let timer3 = timerq.add_timer(EngineBaseTimer::new(0.5, -1.0, false));

        // Schedule them
        timerq.schedule(timer1);
        timerq.schedule(timer2);
        timerq.schedule(timer3);

        // Check armed count
        assert_eq!(timerq.armed_count(), 3);

        // Check next fire delay (should be timer3 at 0.5s)
        let delay = timerq.nextfire_delay();
        assert!(delay.is_some());
        assert!((delay.unwrap() - 0.5).abs() < EPSILON);
    }

    #[test]
    fn test_timer_queue_firing() {
        let mut timerq = TimerQueue::new();

        // Add timers with different delays
        let timer1 = timerq.add_timer(EngineBaseTimer::new(0.1, -1.0, false));
        let timer2 = timerq.add_timer(EngineBaseTimer::new(0.2, -1.0, false));

        timerq.schedule(timer1);
        timerq.schedule(timer2);

        // Advance time past both timers
        timerq.update_time(0.3);

        // Fire expired timers
        let fired = timerq.fire_expired();

        // Should have fired both timers
        assert_eq!(fired.len(), 2);

        // Both should be invalid now (one-shot timers)
        assert!(!timerq.is_valid(timer1));
        assert!(!timerq.is_valid(timer2));
    }

    #[test]
    fn test_timer_queue_invalidation() {
        let mut timerq = TimerQueue::new();

        let timer1 = timerq.add_timer(EngineBaseTimer::new(1.0, -1.0, false));
        let timer2 = timerq.add_timer(EngineBaseTimer::new(2.0, -1.0, false));

        timerq.schedule(timer1);
        timerq.schedule(timer2);

        // Invalidate timer1
        timerq.invalidate(timer1);

        // Should only have one armed timer now
        assert_eq!(timerq.armed_count(), 1);

        // Check next fire delay (should be timer2 at 2.0s)
        let delay = timerq.nextfire_delay();
        assert!(delay.is_some());
        assert!((delay.unwrap() - 2.0).abs() < EPSILON);
    }

    #[test]
    fn test_timer_queue_repeat() {
        let mut timerq = TimerQueue::new();

        // Add a repeating timer
        let timer = timerq.add_timer(EngineBaseTimer::new(0.1, 0.1, false));
        timerq.schedule(timer);

        // Advance time past first fire
        timerq.update_time(0.15);
        let fired = timerq.fire_expired();
        assert_eq!(fired.len(), 1);

        // Timer should still be valid (repeating)
        assert!(timerq.is_valid(timer));

        // Advance time past second fire
        timerq.update_time(0.25);
        let fired = timerq.fire_expired();
        assert_eq!(fired.len(), 1);

        // Timer should still be valid
        assert!(timerq.is_valid(timer));
    }

    #[test]
    fn test_engine_error_types() {
        let err = EngineError::Abort { kill: true };
        assert!(matches!(err, EngineError::Abort { kill: true }));

        let err = EngineError::Timeout;
        assert!(matches!(err, EngineError::Timeout));

        let err = EngineError::IllegalOperation("test".to_string());
        assert!(matches!(err, EngineError::IllegalOperation(_)));

        let err = EngineError::AlreadyRunning;
        assert!(matches!(err, EngineError::AlreadyRunning));

        let err = EngineError::NotSupported("test".to_string());
        assert!(matches!(err, EngineError::NotSupported(_)));

        let io_err = io::Error::new(io::ErrorKind::NotFound, "not found");
        let err = EngineError::Io(io_err);
        assert!(matches!(err, EngineError::Io(_)));
    }

    // -- NullBackend tests ---------------------------------------------------

    #[test]
    fn test_null_backend_name() {
        let nb = NullBackend;
        assert_eq!(nb.name(), "null");
    }

    #[test]
    fn test_null_backend_wait_returns_empty() {
        let mut nb = NullBackend;
        let events = nb.wait(Some(0.0)).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_null_backend_register_modify_unregister() {
        let mut nb = NullBackend;
        // SlotMap needs a ClientId but NullBackend ignores it — use a
        // stand-in from an ephemeral slotmap.
        let mut sm: SlotMap<ClientId, ()> = SlotMap::with_key();
        let cid = sm.insert(());
        nb.register(0, E_READ, cid).unwrap();
        nb.modify(0, E_WRITE, cid).unwrap();
        nb.unregister(0).unwrap();
    }

    // -- Engine with NullBackend tests --------------------------------------

    struct DummyClient {
        registered: bool,
        started: bool,
    }

    impl DummyClient {
        fn new() -> Self {
            Self {
                registered: false,
                started: false,
            }
        }
    }

    impl EngineClient for DummyClient {
        fn start(&mut self) -> io::Result<()> {
            self.started = true;
            Ok(())
        }

        fn close(&mut self, _abort: bool, _timeout: bool) {}

        fn fd(&self) -> Option<RawFd> {
            None // no real fd
        }

        fn events(&self) -> u32 {
            E_READ
        }

        fn is_delayable(&self) -> bool {
            false
        }

        fn is_registered(&self) -> bool {
            self.registered
        }

        fn set_registered(&mut self, val: bool) {
            self.registered = val;
        }

        fn autoclose(&self) -> bool {
            false
        }
    }

    #[test]
    fn test_engine_creation() {
        let engine = Engine::with_backend(NullBackend, 0);
        assert!(!engine.is_running());
        assert!(!engine.exited());
        assert_eq!(engine.fanout(), 0);
        assert_eq!(engine.client_count(), 0);
    }

    #[test]
    fn test_engine_add_remove_client() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        let cid = engine.add_client(Box::new(DummyClient::new())).unwrap();
        assert_eq!(engine.client_count(), 1);
        engine.remove_client(cid);
        assert_eq!(engine.client_count(), 0);
    }

    #[test]
    fn test_engine_add_timer() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        let tid = engine.add_timer(EngineBaseTimer::new(1.0, -1.0, false));
        assert!(engine.timerq().is_valid(tid));
        engine.remove_timer(tid);
        assert!(!engine.timerq().is_valid(tid));
    }

    #[test]
    fn test_engine_fire_timers() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        let tid = engine.add_timer(EngineBaseTimer::new(0.1, -1.0, false));
        assert!(engine.timerq().is_valid(tid));
        engine.timerq_mut().update_time(1.0);
        engine.fire_timers();
        assert!(!engine.timerq().is_valid(tid));
    }

    #[test]
    fn test_engine_abort() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        engine.abort(false);
        assert!(!engine.is_running());
    }

    #[test]
    fn test_engine_evloop_refcnt() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        engine.evloop_acquire();
        engine.evloop_acquire();
        engine.evloop_release();
        // refcnt is 1, engine should still be "runnable"
        // evloop_release doesn't start the engine, just would stop it if it were running
        engine.evloop_release();
        // refcnt is 0, running should be false
        assert!(!engine.is_running());
    }

    #[test]
    fn test_engine_stop() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        // Simulate running state
        engine.stop();
        assert!(!engine.is_running());
    }

    #[test]
    fn test_engine_start_all_clients() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        engine.add_client(Box::new(DummyClient::new())).unwrap();
        engine.add_client(Box::new(DummyClient::new())).unwrap();
        engine.start_all_clients().unwrap();
        assert_eq!(engine.client_count(), 2);
    }

    #[test]
    fn test_engine_update_client_events() {
        let mut engine = Engine::with_backend(NullBackend, 0);
        let cid = engine.add_client(Box::new(DummyClient::new())).unwrap();
        // DummyClient has no fd, so this is a no-op but shouldn't error
        engine.update_client_events(cid).unwrap();
    }

    // -- EngineTimer tests --------------------------------------------------

    #[test]
    fn test_engine_timer_creation() {
        let timer = EngineTimer::new(1.0, -1.0, false, || {});
        assert!(timer.base().is_valid());
        assert_eq!(timer.base().fire_delay(), 1.0);
        assert_eq!(timer.base().interval(), -1.0);
        assert!(!timer.base().autoclose());
    }

    #[test]
    fn test_engine_timer_fire_calls_handler() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let mut timer = EngineTimer::new(0.5, -1.0, false, move || {
            counter_clone.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(counter.load(Ordering::SeqCst), 0);
        timer.fire();
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        timer.fire();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_engine_timer_invalidate() {
        let mut timer = EngineTimer::new(1.0, -1.0, false, || {});
        assert!(timer.base().is_valid());
        timer.base_mut().invalidate();
        assert!(!timer.base().is_valid());
    }

    #[test]
    fn test_engine_timer_debug() {
        let timer = EngineTimer::new(1.0, -1.0, false, || {});
        let debug_str = format!("{:?}", timer);
        assert!(debug_str.contains("EngineTimer"));
        assert!(debug_str.contains("<fn>"));
    }

    #[test]
    fn test_engine_timer_with_autoclose() {
        let timer = EngineTimer::new(2.0, 0.5, true, || {});
        assert!(timer.base().autoclose());
        assert_eq!(timer.base().interval(), 0.5);
    }
}
