use {
    crate::ring::{Ring, RingCtx, RingOp},
    io_uring::{opcode, squeue, types, IoUring},
    slab::Slab,
    std::{
        collections::VecDeque,
        ffi::{CStr, CString, OsString},
        fs, io,
        os::{
            fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd},
            unix::ffi::OsStringExt as _,
        },
        path::{Path, PathBuf},
        time::Duration,
    },
};

/// A fast hard linker using io_uring
pub struct RingHardLinker {
    ring: Ring<State, Op>,
}

impl RingHardLinker {
    /// Creates a new hard linker
    pub fn new() -> io::Result<Self> {
        const SUBMISSION_QUEUE_POLL_IDLE: Duration = Duration::from_secs(1);
        let ring = IoUring::builder()
            .setup_sqpoll(SUBMISSION_QUEUE_POLL_IDLE.as_millis() as u32)
            // .setup_coop_taskrun() <-- bprumo TODO: try this too
            .build(1024)?; // bprumo TODO: bench/doc 1024

        // bprumo TODO: bench/doc 12
        ring.submitter().register_iowq_max_workers(&mut [12, 0])?;
        Ok(Self::with_ring(ring))
    }

    /// Creates a new hard linker, using the provided io_uring instance
    pub fn with_ring(ring: IoUring) -> Self {
        Self {
            ring: Ring::new(ring, State),
        }
    }

    /// bprumo TODO: doc
    pub fn drain(&mut self) -> io::Result<()> {
        self.ring.drain()
    }

    /// bprumo TODO: doc
    pub fn submit(&mut self, original: impl AsRef<Path>, link: impl AsRef<Path>) -> io::Result<()> {
        let op = Op::HardLink(HardLinkOp::new(original, link)?);
        // bprumo TODO: SAFETY: doc
        unsafe { self.ring.push(op)? };
        Ok(())
    }
}

/// bprumo TODO: doc
#[derive(Debug)]
pub struct State;

/// bprumo TODO: doc
#[derive(Debug)]
struct HardLinkOp {
    old_dir_fd: RawFd,
    old_path: CString,
    new_dir_fd: RawFd,
    new_path: CString,
}

impl HardLinkOp {
    /// bprumo TODO: doc
    fn new(old: impl AsRef<Path>, new: impl AsRef<Path>) -> io::Result<Self> {
        let old = old.as_ref();
        let new = new.as_ref();
        if !old.is_absolute() || !new.is_absolute() {
            return Err(io::Error::other(format!(
                "hard link paths must be absolute, old: '{}' new: '{}'",
                old.display(),
                new.display(),
            )));
        }

        Ok(HardLinkOp {
            old_dir_fd: -1,
            old_path: CString::new(old.as_os_str().as_encoded_bytes())?,
            new_dir_fd: -1,
            new_path: CString::new(new.as_os_str().as_encoded_bytes())?,
        })
    }

    /// bprumo TODO: doc
    fn entry(&mut self) -> squeue::Entry {
        opcode::LinkAt::new(
            types::Fd(self.old_dir_fd),
            self.old_path.as_ptr() as _,
            types::Fd(self.new_dir_fd),
            self.new_path.as_ptr() as _,
        )
        .build()
    }

    /// bprumo TODO: doc
    fn complete(self, res: io::Result<i32>, ring: &mut RingCtx<State, Op>) -> io::Result<()> {
        _ = res?;
        Ok(())
    }
}

/// bprumo TODO: doc
#[derive(Debug)]
enum Op {
    HardLink(HardLinkOp),
}

impl RingOp<State> for Op {
    /// bprumo TODO: doc
    fn entry(&mut self) -> squeue::Entry {
        match self {
            Op::HardLink(op) => op.entry(),
        }
    }

    /// bprumo TODO: doc
    fn complete(self, res: io::Result<i32>, ring: &mut RingCtx<State, Self>) -> io::Result<()>
    where
        Self: Sized,
    {
        match self {
            Op::HardLink(op) => op.complete(res, ring),
        }
    }
}
