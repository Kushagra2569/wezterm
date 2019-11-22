#[cfg(windows)]
use crate::os::windows::event::EventHandle;
#[cfg(target_os = "macos")]
use core_foundation::runloop::*;
use failure::Fallible;
use promise::{BasicExecutor, SpawnFunc};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
#[cfg(all(unix, not(target_os = "macos")))]
use {
    filedescriptor::{FileDescriptor, Pipe},
    mio::unix::EventedFd,
    mio::{Evented, Poll, PollOpt, Ready, Token},
    std::os::unix::io::AsRawFd,
};

lazy_static::lazy_static! {
    pub(crate) static ref SPAWN_QUEUE: Arc<SpawnQueue> = Arc::new(SpawnQueue::new().expect("failed to create SpawnQueue"));
}

pub(crate) struct SpawnQueue {
    spawned_funcs: Mutex<VecDeque<SpawnFunc>>,

    #[cfg(windows)]
    pub event_handle: EventHandle,

    #[cfg(all(unix, not(target_os = "macos")))]
    write: Mutex<FileDescriptor>,
    #[cfg(all(unix, not(target_os = "macos")))]
    read: Mutex<FileDescriptor>,
}

impl SpawnQueue {
    pub fn new() -> Fallible<Self> {
        Self::new_impl()
    }

    pub fn spawn(&self, f: SpawnFunc) {
        self.spawn_impl(f)
    }

    pub fn run(&self) -> bool {
        self.run_impl()
    }

    // This needs to be a separate function from the loop in `run`
    // in order for the lock to be released before we call the
    // returned function
    fn pop_func(&self) -> Option<SpawnFunc> {
        self.spawned_funcs.lock().unwrap().pop_front()
    }
}

#[cfg(windows)]
impl SpawnQueue {
    fn new_impl() -> Fallible<Self> {
        let spawned_funcs = Mutex::new(VecDeque::new());
        let event_handle = EventHandle::new_manual_reset().expect("EventHandle creation failed");
        Ok(Self {
            spawned_funcs,
            event_handle,
        })
    }

    fn spawn_impl(&self, f: SpawnFunc) {
        self.spawned_funcs.lock().unwrap().push_back(f);
        self.event_handle.set_event();
    }

    fn run_impl(&self) -> bool {
        self.event_handle.reset_event();
        let mut did_any = false;
        while let Some(func) = self.pop_func() {
            func();
            did_any = true;
        }
        did_any
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl SpawnQueue {
    fn new_impl() -> Fallible<Self> {
        // On linux we have a slightly sloppy wakeup mechanism;
        // we have a non-blocking pipe that we can use to get
        // woken up after some number of enqueues.  We don't
        // guarantee a 1:1 enqueue to wakeup with this mechanism
        // but in practical terms it does guarantee a wakeup
        // if the main thread is asleep and we enqueue some
        // number of items.
        // We can't affort to use a blocking pipe for the wakeup
        // because the write needs to hold a mutex and that
        // can block reads as well as other writers.
        let pipe = Pipe::new()?;
        let on = 1;
        unsafe {
            libc::ioctl(pipe.write.as_raw_fd(), libc::FIONBIO, &on);
            libc::ioctl(pipe.read.as_raw_fd(), libc::FIONBIO, &on);
        }
        Ok(Self {
            spawned_funcs: Mutex::new(VecDeque::new()),
            write: Mutex::new(pipe.write),
            read: Mutex::new(pipe.read),
        })
    }

    fn spawn_impl(&self, f: SpawnFunc) {
        use std::io::Write;

        self.spawned_funcs.lock().unwrap().push_back(f);
        self.write.lock().unwrap().write(b"x").ok();
    }

    fn run_impl(&self) -> bool {
        // On linux we only ever process one at at time, so that
        // we can return to the main loop and process messages
        // from the X server
        use std::io::Read;
        if let Some(func) = self.pop_func() {
            func();

            let mut byte = [0u8];
            self.read.lock().unwrap().read(&mut byte).ok();
            true
        } else {
            false
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Evented for SpawnQueue {
    fn register(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> std::io::Result<()> {
        EventedFd(&self.read.lock().unwrap().as_raw_fd()).register(poll, token, interest, opts)
    }

    fn reregister(
        &self,
        poll: &Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> std::io::Result<()> {
        EventedFd(&self.read.lock().unwrap().as_raw_fd()).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &Poll) -> std::io::Result<()> {
        EventedFd(&self.read.lock().unwrap().as_raw_fd()).deregister(poll)
    }
}

#[cfg(target_os = "macos")]
impl SpawnQueue {
    fn new_impl() -> Fallible<Self> {
        let spawned_funcs = Mutex::new(VecDeque::new());

        let observer = unsafe {
            CFRunLoopObserverCreate(
                std::ptr::null(),
                kCFRunLoopAllActivities,
                1,
                0,
                SpawnQueue::trigger,
                std::ptr::null_mut(),
            )
        };
        unsafe {
            CFRunLoopAddObserver(CFRunLoopGetMain(), observer, kCFRunLoopCommonModes);
        }

        Ok(Self { spawned_funcs })
    }

    extern "C" fn trigger(
        _observer: *mut __CFRunLoopObserver,
        _: CFRunLoopActivity,
        _: *mut std::ffi::c_void,
    ) {
        if SPAWN_QUEUE.run() {
            self.queue_wakeup();
        }
    }

    fn queue_wakeup(&self) {
        unsafe {
            CFRunLoopWakeUp(CFRunLoopGetMain());
        }
    }

    fn spawn_impl(&self, f: SpawnFunc) {
        self.spawned_funcs.lock().unwrap().push_back(f);
        self.queue_wakeup();
    }

    fn run_impl(&self) -> bool {
        if let Some(func) = self.pop_func() {
            func();
            true
        } else {
            false
        }
    }
}

pub struct SpawnQueueExecutor;
impl BasicExecutor for SpawnQueueExecutor {
    fn execute(&self, f: SpawnFunc) {
        SPAWN_QUEUE.spawn(f)
    }
}
