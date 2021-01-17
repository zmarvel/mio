use crate::{Interest, Token};

use std::fmt;
use libc::{POLLOUT, POLLWRNORM, POLLWRBAND, POLLIN, POLLRDNORM, POLLRDBAND, POLLPRI};
use std::os::unix::io::{AsRawFd, RawFd};
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use std::io;

/// Unique id for use as `SelectorId`.
#[cfg(debug_assertions)]
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

pub struct Selector {
    #[cfg(debug_assertions)]
    id: usize,
    #[cfg(debug_assertions)]
    has_waker: AtomicBool,
    fds: Events,
}

impl Selector {
    pub fn new() -> io::Result<Selector> {
        Ok(Selector {
            #[cfg(debug_assertions)]
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            #[cfg(debug_assertions)]
            has_waker: AtomicBool::new(false),
            fds: Vec::new(),
        })
    }

    pub fn try_clone(&self) -> io::Result<Selector> {
        Ok(Selector {
            // It's the same selector, so we use the same id.
            #[cfg(debug_assertions)]
            id: self.id,
            #[cfg(debug_assertions)]
            has_waker: AtomicBool::new(self.has_waker.load(Ordering::Acquire)),
            fds: self.fds.clone()
        })
    }

    /// Wait for `timeout` on the registered fds.
    pub fn select(&mut self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        let timeout = timeout
            .map(|to| to.as_millis() as libc::c_int)
            .unwrap_or(-1);

        events.clear();
        syscall!(poll(
                self.fds.as_mut_ptr(),
                self.fds.len() as libc::nfds_t,
                timeout
                ))
            .map(|_n_events| {
                for &event in self.fds.iter()
                    .filter(|&&event| { event::is_readable(&event) || event::is_writable(&event) || event::is_error(&event) }) {
                        events.push(event);
                    }

                debug_assert!(events.len() == _n_events as usize)
            })
    }

    pub fn register(&mut self, fd: RawFd, _token: Token, interests: Interest) -> io::Result<()> {
        // If the fd already exists in our list, return an error
        match self.fds.iter_mut()
            .find(|&&mut pollfd| { pollfd.fd == fd }) {
            Some(_) => Err(io::Error::new(io::ErrorKind::AlreadyExists, fmt::format(format_args!("{:?}", fd)))),
            None => {
                self.fds.push(libc::pollfd {
                    fd: fd, 
                    events: interests_to_poll(interests),
                    revents: 0 as libc::c_short
                });
                Ok(())
            },
        }
    }

    pub fn reregister(&mut self, fd: RawFd, _token: Token, interests: Interest) -> io::Result<()> {
        match self.fds.iter_mut()
            .find(|&&mut pollfd| { pollfd.fd == fd }) {
            Some(pollfd) => {
                pollfd.events = interests_to_poll(interests);
                Ok(())
            },
            _ => Err(io::Error::new(io::ErrorKind::NotFound, fmt::format(format_args!("{:?}", fd)))),
        }
    }

    pub fn deregister(&mut self, fd: RawFd) -> io::Result<()> {
        self.fds.iter()
            .position(|&pollfd| { pollfd.fd == fd })
            .map_or(
                Err(io::Error::new(io::ErrorKind::NotFound, fmt::format(format_args!("{:?}", fd)))),
                |idx| {
                    self.fds.remove(idx);
                    Ok(())
                })
    }

    #[cfg(debug_assertions)]
    pub fn register_waker(&self) -> bool {
        self.has_waker.swap(true, Ordering::AcqRel)
    }
}

cfg_io_source! {
    impl Selector {
        #[cfg(debug_assertions)]
        pub fn id(&self) -> usize {
            self.id
        }
    }
}

// TODO: This doesn't make much sense for poll--is it needed?
impl AsRawFd for Selector {
    fn as_raw_fd(&self) -> RawFd {
        1
    }
}

// impl Drop for Selector {
//     fn drop(&mut self) {
//         // Nothing to do
//     }
// }

impl fmt::Debug for Selector {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = fmt.debug_struct("Selector");
        d.field("id", &self.id)
            .field("has_waker", &self.has_waker.load(Ordering::Acquire))
            .field("fds", &"...")
            .finish()
    }
}

fn interests_to_poll (interests: Interest) -> libc::c_short {
    (if interests.is_writable() {
        POLLOUT | POLLWRNORM | POLLWRBAND
    } else {
        0
    }
    | if interests.is_readable() {
        POLLIN | POLLRDNORM | POLLRDBAND | POLLPRI
    } else {
        0
    }) as libc::c_short
}

pub type Event = libc::pollfd;
pub type Events = Vec<Event>;

pub mod event {
    use std::fmt;

    use crate::sys::Event;
    use crate::Token;

    use libc::{POLLOUT, POLLWRNORM, POLLWRBAND, POLLIN, POLLRDNORM, POLLRDBAND, POLLPRI, POLLHUP, POLLERR};

    pub fn token(event: &Event) -> Token {
        Token(event.fd as usize)
    }

    pub fn is_readable(event: &Event) -> bool {
        event.revents as libc::c_short & (POLLIN | POLLRDNORM | POLLRDBAND | POLLPRI) != 0
    }

    pub fn is_writable(event: &Event) -> bool {
        event.revents as libc::c_short & (POLLOUT | POLLWRNORM | POLLWRBAND) != 0
    }

    pub fn is_error(event: &Event) -> bool {
        event.revents as libc::c_short & (POLLHUP | POLLERR) != 0
    }

    pub fn is_read_closed(event: &Event) -> bool {
        event.revents as libc::c_short & POLLHUP != 0
    }

    pub fn is_write_closed(event: &Event) -> bool {
        let revents = event.revents as libc::c_short;
        (revents & POLLHUP != 0)
            || (revents & POLLOUT != 0 && revents & POLLERR != 0)
            || (revents & POLLERR != 0)
    }

    pub fn is_priority(event: &Event) -> bool {
        event.revents as libc::c_short & (POLLRDBAND | POLLWRBAND | POLLPRI) != 0
    }

    pub fn is_aio(_: &Event) -> bool {
        // Not supported in the kernel, only in libc.
        false
    }

    pub fn is_lio(_: &Event) -> bool {
        // Not supported.
        false
    }

    pub fn debug_details(f: &mut fmt::Formatter<'_>, event: &Event) -> fmt::Result {
        #[allow(clippy::trivially_copy_pass_by_ref)]
        fn check_events(got: &libc::c_short, want: &libc::c_short) -> bool {
            (*got & want) != 0
        }
        debug_detail!(
            EventsDetails(libc::c_short),
            check_events,
            libc::POLLOUT,
            libc::POLLWRNORM,
            libc::POLLWRBAND,
            libc::POLLIN,
            libc::POLLRDNORM,
            libc::POLLRDBAND,
            libc::POLLPRI,
            libc::POLLHUP,
            libc::POLLERR,
        );

        // Can't reference fields in packed structures.
        f.debug_struct("poll_event")
            .field("events", &EventsDetails(event.revents))
            .field("fd", &event.fd)
            .finish()
    }
}
