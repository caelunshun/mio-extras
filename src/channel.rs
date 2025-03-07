//! Thread safe communication channel implementing `Evented`
use crossbeam::channel as ch;
use lazycell::AtomicLazyCell;
use mio::{Evented, Poll, PollOpt, Ready, Registration, SetReadiness, Token};
use std::any::Any;
use std::error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::{fmt, io};

/// Creates a new asynchronous channel, where the `Receiver` can be registered
/// with `Poll`.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx_ctl, rx_ctl) = ctl_pair();
    let (tx, rx) = ch::unbounded();

    let tx = Sender { tx, ctl: tx_ctl };

    let rx = Receiver { rx, ctl: rx_ctl };

    (tx, rx)
}

fn ctl_pair() -> (SenderCtl, ReceiverCtl) {
    let inner = Arc::new(Inner {
        pending: AtomicUsize::new(0),
        senders: AtomicUsize::new(1),
        set_readiness: AtomicLazyCell::new(),
    });

    let tx = SenderCtl {
        inner: Arc::clone(&inner),
    };

    let rx = ReceiverCtl {
        registration: AtomicLazyCell::new(),
        inner,
    };

    (tx, rx)
}

/// Tracks messages sent on a channel in order to update readiness.
struct SenderCtl {
    inner: Arc<Inner>,
}

/// Tracks messages received on a channel in order to track readiness.
struct ReceiverCtl {
    registration: AtomicLazyCell<Registration>,
    inner: Arc<Inner>,
}

/// The sending half of a channel.
pub struct Sender<T> {
    tx: ch::Sender<T>,
    ctl: SenderCtl,
}

/// The receiving half of a channel.
pub struct Receiver<T> {
    rx: ch::Receiver<T>,
    ctl: ReceiverCtl,
}

/// An error returned from the `Sender::send` or `SyncSender::send` function.
pub enum SendError<T> {
    /// An IO error.
    Io(io::Error),

    /// The receiving half of the channel has disconnected.
    Disconnected(T),
}

/// An error returned from the `SyncSender::try_send` function.
pub enum TrySendError<T> {
    /// An IO error.
    Io(io::Error),

    /// Data could not be sent because it would require the callee to block.
    Full(T),

    /// The receiving half of the channel has disconnected.
    Disconnected(T),
}

struct Inner {
    // The number of outstanding messages for the receiver to read
    pending: AtomicUsize,
    // The number of sender handles
    senders: AtomicUsize,
    // The set readiness handle
    set_readiness: AtomicLazyCell<SetReadiness>,
}

impl<T> Sender<T> {
    /// Attempts to send a value on this channel, returning it back if it could not be sent.
    pub fn send(&self, t: T) -> Result<(), SendError<T>> {
        self.tx.send(t).map_err(SendError::from).and_then(|_| {
            self.ctl.inc()?;
            Ok(())
        })
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        Sender {
            tx: self.tx.clone(),
            ctl: self.ctl.clone(),
        }
    }
}

impl<T> Receiver<T> {
    /// Attempts to return a pending value on this receiver without blocking.
    pub fn try_recv(&self) -> Result<T, ch::TryRecvError> {
        self.rx.try_recv().and_then(|res| {
            let _ = self.ctl.dec();
            Ok(res)
        })
    }
}

impl<T> Evented for Receiver<T> {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.ctl.register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        self.ctl.reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        self.ctl.deregister(poll)
    }
}

/*
 *
 * ===== SenderCtl / ReceiverCtl =====
 *
 */

impl SenderCtl {
    /// Call to track that a message has been sent
    fn inc(&self) -> io::Result<()> {
        let cnt = self.inner.pending.fetch_add(1, Ordering::Acquire);

        if 0 == cnt {
            // Toggle readiness to readable
            if let Some(set_readiness) = self.inner.set_readiness.borrow() {
                set_readiness.set_readiness(Ready::readable())?;
            }
        }

        Ok(())
    }
}

impl Clone for SenderCtl {
    fn clone(&self) -> SenderCtl {
        self.inner.senders.fetch_add(1, Ordering::Relaxed);
        SenderCtl {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for SenderCtl {
    fn drop(&mut self) {
        if self.inner.senders.fetch_sub(1, Ordering::Release) == 1 {
            let _ = self.inc();
        }
    }
}

impl ReceiverCtl {
    fn dec(&self) -> io::Result<()> {
        let first = self.inner.pending.load(Ordering::Acquire);

        if first == 1 {
            // Unset readiness
            if let Some(set_readiness) = self.inner.set_readiness.borrow() {
                set_readiness.set_readiness(Ready::empty())?;
            }
        }

        // Decrement
        let second = self.inner.pending.fetch_sub(1, Ordering::AcqRel);

        if first == 1 && second > 1 {
            // There are still pending messages. Since readiness was
            // previously unset, it must be reset here
            if let Some(set_readiness) = self.inner.set_readiness.borrow() {
                set_readiness.set_readiness(Ready::readable())?;
            }
        }

        Ok(())
    }
}

impl Evented for ReceiverCtl {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        if self.registration.borrow().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "receiver already registered",
            ));
        }

        let (registration, set_readiness) = Registration::new2();
        poll.register(&registration, token, interest, opts)?;

        if self.inner.pending.load(Ordering::Relaxed) > 0 {
            // TODO: Don't drop readiness
            let _ = set_readiness.set_readiness(Ready::readable());
        }

        self.registration
            .fill(registration)
            .expect("unexpected state encountered");
        self.inner
            .set_readiness
            .fill(set_readiness)
            .expect("unexpected state encountered");

        Ok(())
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        match self.registration.borrow() {
            Some(registration) => poll.reregister(registration, token, interest, opts),
            None => Err(io::Error::new(
                io::ErrorKind::Other,
                "receiver not registered",
            )),
        }
    }

    fn deregister(&self, poll: &Poll) -> io::Result<()> {
        match self.registration.borrow() {
            Some(registration) => poll.deregister(registration),
            None => Err(io::Error::new(
                io::ErrorKind::Other,
                "receiver not registered",
            )),
        }
    }
}

/*
 *
 * ===== Error conversions =====
 *
 */

impl<T> From<ch::SendError<T>> for SendError<T> {
    fn from(src: ch::SendError<T>) -> SendError<T> {
        SendError::Disconnected(src.0)
    }
}

impl<T> From<io::Error> for SendError<T> {
    fn from(src: io::Error) -> SendError<T> {
        SendError::Io(src)
    }
}

impl<T> From<ch::TrySendError<T>> for TrySendError<T> {
    fn from(src: ch::TrySendError<T>) -> TrySendError<T> {
        match src {
            ch::TrySendError::Full(v) => TrySendError::Full(v),
            ch::TrySendError::Disconnected(v) => TrySendError::Disconnected(v),
        }
    }
}

impl<T> From<ch::SendError<T>> for TrySendError<T> {
    fn from(src: ch::SendError<T>) -> TrySendError<T> {
        TrySendError::Disconnected(src.0)
    }
}

impl<T> From<io::Error> for TrySendError<T> {
    fn from(src: io::Error) -> TrySendError<T> {
        TrySendError::Io(src)
    }
}

/*
 *
 * ===== Implement Error, Debug and Display for Errors =====
 *
 */

impl<T: Any> error::Error for SendError<T> {
    fn description(&self) -> &str {
        match *self {
            SendError::Io(ref io_err) => io_err.description(),
            SendError::Disconnected(..) => "Disconnected",
        }
    }
}

impl<T: Any> error::Error for TrySendError<T> {
    fn description(&self) -> &str {
        match *self {
            TrySendError::Io(ref io_err) => io_err.description(),
            TrySendError::Full(..) => "Full",
            TrySendError::Disconnected(..) => "Disconnected",
        }
    }
}

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        format_send_error(self, f)
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        format_send_error(self, f)
    }
}

impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        format_try_send_error(self, f)
    }
}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        format_try_send_error(self, f)
    }
}

#[inline]
fn format_send_error<T>(e: &SendError<T>, f: &mut fmt::Formatter) -> fmt::Result {
    match *e {
        SendError::Io(ref io_err) => write!(f, "{}", io_err),
        SendError::Disconnected(..) => write!(f, "Disconnected"),
    }
}

#[inline]
fn format_try_send_error<T>(e: &TrySendError<T>, f: &mut fmt::Formatter) -> fmt::Result {
    match *e {
        TrySendError::Io(ref io_err) => write!(f, "{}", io_err),
        TrySendError::Full(..) => write!(f, "Full"),
        TrySendError::Disconnected(..) => write!(f, "Disconnected"),
    }
}
