//!A configurable, multi-threaded signals and slots implementation for Rust.
//!Uses mpsc channels and a configurable event-loop-like system internally to allow for fully
//!multi-threaded message handling without wasting cpu time.
//!
//! # Example
//!
//! ```
//! use apex::{Signal, Slot};
//! use std::sync::{ Arc, atomic::{
//!     AtomicBool, AtomicI32, Ordering }
//! };
//! use std::time::Duration;
//!
//! //create a slot that will poll continuously for 10 milliseconds in 10 millisecond intervals
//! let mut slot = Slot::<i32>::new(Duration::from_millis(10), Duration::from_millis(10));
//! let mut sig = Signal::<i32>::new();
//!
//! let multiply = Arc::new(AtomicBool::new(true));
//! let multiply_cloned = multiply.clone();
//! let num = Arc::new(AtomicI32::new(5));
//! let num_cloned = num.clone();
//!
//! slot.connect(&mut sig);
//! let callback = move |t: i32| {
//!     if multiply_cloned.load(Ordering::Relaxed) {
//!         let mut temp = num_cloned.load(Ordering::Relaxed);
//!         temp *= t;
//!         num_cloned.store(temp, Ordering::Release);
//!     }
//!     else {
//!         let mut temp = num_cloned.load(Ordering::Relaxed);
//!         temp /= t;
//!         num_cloned.store(temp, Ordering::Relaxed);
//!     }
//! };
//! slot.open(callback);
//!
//! sig.emit(5);
//! //in practice this would be using some sort of timing synchronization mechanism,
//! //but this is good enough for this simple test/example to make sure the 'emit' has finished
//! //before 'assert' gets called
//! std::thread::sleep(std::time::Duration::from_millis(10));
//!
//! let multiplied = num.load(Ordering::Relaxed);
//! assert_eq!(multiplied, 25);
//! multiply.store(false, Ordering::Relaxed);
//! //ditto above
//! std::thread::sleep(std::time::Duration::from_millis(10));
//!
//! sig.emit(25);
//! //ditto above
//! std::thread::sleep(std::time::Duration::from_millis(10));
//!
//! let divided = num.load(Ordering::Relaxed);
//! assert_eq!(divided, 1);
//! slot.close();
//!
//! ```
//!
extern crate crossbeam_channel;
extern crate crossbeam_utils;
use crossbeam_channel::{Sender, Receiver, unbounded, SendError};
use crossbeam_utils::sync::ShardedLock as Lock;

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use std::time::{ Duration, Instant };

///
///Error type for Slot-related actions
///
#[derive(Debug)]
pub enum SlotError {
    ///Slot was Already Open
    AlreadyOpen,
    ///Slot has never been Opened
    NotOpened,
    ///Slot was previously Closed
    AlreadyClosed,
    ///Joining of 'event loop' thread error
    ThreadJoin(Box<dyn std::any::Any + Send>),
}

impl fmt::Display for SlotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            SlotError::AlreadyOpen => write!(f, "AlreadyOpen Error: Slot Already Open"),
            SlotError::NotOpened => write!(f, "NotOpened Error: Slot Not Opened"),
            SlotError::AlreadyClosed => write!(f, "AlreadyClosed Error: Slot Already Closed"),
            SlotError::ThreadJoin(x) =>
                write!(f, "ThreadJoin Error: Slot failed to join signal handling thread: {:?}", x),
        }
    }
}

impl Error for SlotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        //this could just as easily just be swapped out for None, but will
        //leave the match incase we need to expand in future
        match &self {
            SlotError::AlreadyOpen => None,
            SlotError::NotOpened => None,
            SlotError::AlreadyClosed => None,
            SlotError::ThreadJoin(_) => None,
        }
    }
}

///
///Slot component of the Signals and Slots implementation.
///This uses mpsc channels provided by 'crossbeam' internally for communication between this and connected 'Signal's
///
pub struct Slot<T: Send + Sync + Copy + 'static> {
    ///the channel sender, given to Signals to communicate to this Slot
    sender: Sender<T>,
    ///the channel receiver
    receiver: Arc<Lock<Receiver<T>>>,
    ///the length of time to poll for signals
    poll_time: Duration,
    ///the length of time to sleep between polling for signals
    update_interval: Duration,
    ///'event loop' thread join handle
    receiver_handle: Option<JoinHandle<()>>,
    ///lifeline to the 'event loop' thread so it can check if we have been dropped, and abort
    ///accordingly
    thread_lifeline_receiver: Option<Receiver<()>>,
    ///connection to the 'event loop' so we can tell it when to close
    thread_abort_sender: Option<Sender<bool>>
}

impl<T: Send + Sync + Copy + 'static> Slot<T> {
    ///
    ///Creates a new Slot of the type T, with the given polling time and update_interval.
    ///
    /// # Arguments
    /// * 'poll-time' - the length of time the slot should poll for signals every update interval
    /// * 'update-interval' - the length of time the slot should sleep between polling for signals
    ///
    /// # Returns
    /// * a new Slot<T>
    ///
    pub fn new(poll_time: Duration, update_interval: Duration) -> Slot<T> {
        let (sender, receiver) = unbounded::<T>();
        Slot {
            sender,
            receiver: Arc::new(Lock::new(receiver)),
            poll_time,
            update_interval,
            receiver_handle: None,
            thread_lifeline_receiver: None,
            thread_abort_sender: None,
        }
    }

    ///
    ///Connects the given Signal to this Slot
    ///
    /// # Arguments
    /// * 'signal' - the signal to connect
    ///
    pub fn connect(&self, signal: &mut Signal<T>) {
        signal.add_sender(self.sender.clone());
    }

    ///
    ///Opens this slot to receiving signals
    ///
    /// This spawns a new 'event loop' thread for the slot to poll for signals and call the
    /// 'callback' closure upon receiving them. If the Slot is already open, nothing is done and
    /// SlotError::AlradyOpen is returned.
    ///
    /// # Arguments
    /// * 'callback' - the closure to call upon receiving a signal. Because this is called on a
    ///                separate thread, objects captured by the closer must be threadsafe (Send +
    ///                Sync)
    ///
    /// # Returns
    /// * success - Ok(())
    /// * failure - Err(SlotError::AlreadyOpen)
    ///
    pub fn open(&mut self, mut callback: impl FnMut(T) + 'static + Send + Sync) -> Result<(), SlotError> {
        if self.thread_abort_sender.is_none() {
            self.receiver_handle = None;
        }
        let rec = self.receiver.clone();
        match &self.receiver_handle {
            None => {
                let (thread_lifeline_sender, thread_lifeline_receiver) = unbounded::<()>();
                let (thread_abort_sender, thread_abort_receiver) = unbounded::<bool>();
                self.thread_lifeline_receiver = Some(thread_lifeline_receiver);
                self.thread_abort_sender = Some(thread_abort_sender);
                let update_interval = self.update_interval;
                let poll_time = self.poll_time;
                self.receiver_handle = Some(thread::spawn(move || {
                    let mut current_time = Instant::now();
                    'event_loop: loop {
                        if let Ok(x) = rec.read() {
                            if let Ok(y) = x.try_recv() {
                                (callback)(y);
                            }
                        }
                        if thread_lifeline_sender.send(()).is_err() {
                            break 'event_loop;
                        }
                        if let Ok(x) = thread_abort_receiver.try_recv() {
                            if x {
                                break 'event_loop;
                            }
                        }
                        if current_time.elapsed() > poll_time {
                            std::thread::sleep(update_interval);
                            current_time = Instant::now();
                        }
                    }
                }));
                Ok(())
            },
            Some(_) => Err(SlotError::AlreadyOpen)
        }
    }

    ///
    ///Closes the Slot, so it can no longer receive Signals
    ///
    ///This joins the internal 'event loop' thread.
    ///If the Slot wasn't open, SlotError::NotOpened is returned.
    ///If the Slot was already closed, SlotError::AlreadyClosed is returned.
    ///If there was an error joining the thread,
    ///SlotError::ThreadJoin(Box<dyn std::any::Any + Send>) is returned.
    ///Otherwise, Ok(()) is returned.
    ///
    /// # Returns
    /// * success - Ok(())
    /// * failure - SlotError
    ///
    pub fn close(mut self) -> Result<(), SlotError> {
        match self.receiver_handle {
            Some(handle) => match self.thread_abort_sender {
                Some(sender) => {
                    if sender.send(true).is_err() {
                        //just doing this to get rid of compiler warning
                        //we don't care if the send failed, we'll pick up why with the join
                        //-> the send will have only failed if the thread panicked
                    }
                    let x = handle.join();
                    if x.is_err() {
                        return Err(SlotError::ThreadJoin(x.err().unwrap()));
                    }
                    self.thread_lifeline_receiver = None;
                    self.thread_abort_sender = None;
                    Ok(())
                },
                None => Err(SlotError::AlreadyClosed)
            },
            None => Err(SlotError::NotOpened)
        }
    }
}

///
///Error Type for Signal-related actions
#[derive(Debug)]
pub enum SignalError<T> {
    ///Error sending the Signal to the Slots
    SendError(SendError<T>),
    ///Signal is not connected to any Slot, but an emit was attempted
    NotConnected
}

impl<T> fmt::Display for SignalError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self {
            SignalError::SendError(x) =>
                write!(f, "crossbeam_channel::SendError Error: {}", x),
            SignalError::NotConnected =>
                write!(f, "NotConnected Error: Signal Not Connected to a Slot"),
        }
    }
}

impl<T: fmt::Debug + Send + 'static> Error for SignalError<T> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self {
            SignalError::SendError(x) => Some(x),
            SignalError::NotConnected => None,
        }
    }
}

///
///Signal component of the Signals and Slots implementation.
///This uses mpsc channels provided by 'crossbeam' internally for communication between this and connected Slots
///
#[derive(Default)]
pub struct Signal<T: Send + Sync + Copy + 'static> {
    ///list of senders to emit signals down
    senders: Vec<Sender<T>>
}

impl<T: Send + Sync + Copy + 'static> Signal<T> {
    ///
    ///Creates a new Signal of the Type T
    ///
    /// # Returns
    /// * a new Signal<T>
    pub fn new() -> Signal<T> {
        Signal {
            senders: vec![]
        }
    }

    ///
    ///Adds the given sender to the Signal's list of senders
    ///
    /// # Arguments
    /// * 'sender' - the channel Sender to add to the list
    ///
    pub fn add_sender(&mut self, sender: Sender<T>) {
        self.senders.push(sender);
    }

    ///
    ///Emits the given signal to all of the connected Slots
    ///
    ///May return a SignalError if the Signal is not connected to any Slots
    ///or an error occured when sending to any Slot. If an error occurs when
    ///sending to a Slot, all Slots will still be sent to and only the most
    ///recent error will be returned.
    ///
    /// # Arguments
    /// * 'signal' - The signal to be emitted
    ///
    /// # Returns
    /// * success - Ok(())
    /// * failure - SignalError
    ///
    pub fn emit(&self, signal: T) -> Result<(), SignalError<T>> {
        if self.senders.is_empty() {
            return Err(SignalError::NotConnected);
        }
        let mut err: Option<SignalError<T>> = None;
        for sender in &self.senders {
            match sender.send(signal) {
                Ok(_) => (),
                Err(x) => err = Some(SignalError::SendError(x))
            }
        }
        if let Some(x) = err {
            return Err(x);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn example1() -> Result<(), Box<dyn std::error::Error + 'static>> {
        use crate::{Signal, Slot};
        use std::time::Duration;
        let mut slot = Slot::<i32>::new(Duration::from_millis(10), Duration::from_millis(10));
        let mut sig = Signal::<i32>::new();
        slot.connect(&mut sig);
        let callback = move |t: i32| {
            assert_eq!(t, 1);
        };
        slot.open(callback)?;
        sig.emit(1)?;
        slot.close()?;
        Ok(())
    }

    #[test]
    fn example2() -> Result<(), Box<dyn std::error::Error + 'static>> {
        use crate::{Signal, Slot};
        use std::time::Duration;

        //create a slot that will poll continuously for 10 milliseconds in 10 millisecond intervals
        let mut slot = Slot::<i32>::new(Duration::from_millis(10), Duration::from_millis(10));
        let mut sig = Signal::<i32>::new();

        let add = false;
        let mut test_val = 1;
        slot.connect(&mut sig);
        let callback = move |t:i32| {
            if add {
                test_val += t;
            }
            else {
                test_val -= t;
            }
            assert_eq!(test_val, 0);
        };
        slot.open(callback)?;
        sig.emit(1)?;
        slot.close()?;
        Ok(())
    }

    #[test]
    fn example3() -> Result<(), Box<dyn std::error::Error + 'static>> {
        use crate::{Signal, Slot};
        use std::sync::{ Arc, atomic::{
            AtomicBool, AtomicI32, Ordering }
        };
        use std::time::Duration;

        //create a slot that will poll continuously for 10 milliseconds in 10 millisecond intervals
        let mut slot = Slot::<i32>::new(Duration::from_millis(10), Duration::from_millis(10));
        let mut sig = Signal::<i32>::new();

        let multiply = Arc::new(AtomicBool::new(true));
        let multiply_cloned = multiply.clone();
        let num = Arc::new(AtomicI32::new(5));
        let num_cloned = num.clone();

        slot.connect(&mut sig);
        let callback = move |t: i32| {
            if multiply_cloned.load(Ordering::Relaxed) {
                let mut temp = num_cloned.load(Ordering::Relaxed);
                temp *= t;
                num_cloned.store(temp, Ordering::Release);
            }
            else {
                let mut temp = num_cloned.load(Ordering::Relaxed);
                temp /= t;
                num_cloned.store(temp, Ordering::Relaxed);
            }
        };
        slot.open(callback)?;

        sig.emit(5)?;
        //in practice this would be using some sort of timing synchronization mechanism,
        //but this is good enough for this simple test/example to make sure the 'emit' has finished
        //before 'assert' gets called
        std::thread::sleep(std::time::Duration::from_millis(10));

        let multiplied = num.load(Ordering::Relaxed);
        assert_eq!(multiplied, 25);
        multiply.store(false, Ordering::Relaxed);
        //ditto above
        std::thread::sleep(std::time::Duration::from_millis(10));

        sig.emit(25)?;
        //ditto above
        std::thread::sleep(std::time::Duration::from_millis(10));

        let divided = num.load(Ordering::Relaxed);
        assert_eq!(divided, 1);
        slot.close()?;
        Ok(())
    }
}
