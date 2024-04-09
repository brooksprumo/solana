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
            //.setup_sqpoll(SUBMISSION_QUEUE_POLL_IDLE.as_millis() as u32) // <-- bprumo TODO: alessandro says to try removing this
            .setup_coop_taskrun()
            .build(1024)?; // bprumo TODO: bench/doc 1024

        let previous = ring.submitter().register_iowq_max_workers(&mut [12, 0])?; // bprumo TODO: bench/doc 12
        log::error!("bprumo DEBUG: original ring iowq max workers: {previous:?}");
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
    pub fn submit(
        &mut self,
        original_dir_fd: impl AsRawFd,
        original_path: impl AsRef<Path>,
        link_dir_fd: impl AsRawFd,
        link_path: impl AsRef<Path>,
    ) -> io::Result<()> {
        let op = Op::HardLink(HardLinkOp::new(
            original_dir_fd,
            original_path,
            link_dir_fd,
            link_path,
        )?);
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
    fn new(
        old_dir_fd: impl AsRawFd,
        old_path: impl AsRef<Path>,
        new_dir_fd: impl AsRawFd,
        new_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        Ok(HardLinkOp {
            old_dir_fd: old_dir_fd.as_raw_fd(),
            old_path: CString::new(old_path.as_ref().as_os_str().as_encoded_bytes())?,
            new_dir_fd: new_dir_fd.as_raw_fd(),
            new_path: CString::new(new_path.as_ref().as_os_str().as_encoded_bytes())?,
        })
    }

    /// bprumo TODO: doc
    /// bprumo TODO: remove me?
    fn with_absolute_paths(old: impl AsRef<Path>, new: impl AsRef<Path>) -> io::Result<Self> {
        let old = old.as_ref();
        let new = new.as_ref();
        if !old.is_absolute() || !new.is_absolute() {
            return Err(io::Error::other(format!(
                "hard link paths must be absolute, old: '{}' new: '{}'",
                old.display(),
                new.display(),
            )));
        }

        Self::new(-1, old, -1, new)
    }

    /// bprumo TODO: doc
    fn entry(&mut self) -> squeue::Entry {
        opcode::LinkAt::new(
            types::Fd(self.old_dir_fd),
            self.old_path.as_ptr().cast(),
            types::Fd(self.new_dir_fd),
            self.new_path.as_ptr().cast(),
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
