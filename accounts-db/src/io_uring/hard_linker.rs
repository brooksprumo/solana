use {
    agave_io_uring::{Completion, Ring, RingOp},
    io_uring::{opcode, squeue, types, IoUring},
    std::{
        ffi::CString,
        io,
        os::fd::{AsRawFd, RawFd},
        path::Path,
    },
};

/// A fast hard linker using io_uring
pub struct RingHardLinker {
    ring: Ring<State, Op>,
}

impl RingHardLinker {
    /// Creates a new hard linker
    pub fn new() -> io::Result<Self> {
        // config values copied from RingDirRemover
        let ring = IoUring::builder().setup_sqpoll(1000).build(1024)?;
        ring.submitter().register_iowq_max_workers(&mut [12, 0])?;
        Ok(Self::with_ring(ring))
    }

    /// Creates a new hard linker the provided io_uring instance
    fn with_ring(ring: IoUring) -> Self {
        Self {
            ring: Ring::new(ring, State),
        }
    }

    /// bprumo TODO: doc
    pub fn hard_link_rel(
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
        self.submit(op)
    }

    // brooks TODO: doc
    pub fn hard_link_abs(
        &mut self,
        original: impl AsRef<Path>,
        link: impl AsRef<Path>,
    ) -> io::Result<()> {
        let op = Op::HardLink(HardLinkOp::with_absolute_paths(original, link)?);
        self.submit(op)
    }

    // brooks TODO: doc
    fn submit(&mut self, op: Op) -> io::Result<()> {
        self.ring.push(op)
    }

    /// bprumo TODO: doc
    pub fn drain(&mut self) -> io::Result<()> {
        self.ring.drain()
    }
}

// bprumo TODO: doc
// should I create/hold the new/old dir fds here in State?
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

        // with absolute paths both `old_dir_fd` and `new_dir_fd` are ignored
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
    fn complete(
        &mut self,
        _comp: &mut Completion<State, Op>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        _ = res?;
        Ok(())
    }
}

#[derive(Debug)]
enum Op {
    HardLink(HardLinkOp),
}

impl RingOp<State> for Op {
    fn entry(&mut self) -> squeue::Entry {
        match self {
            Op::HardLink(op) => op.entry(),
        }
    }

    fn complete(
        &mut self,
        comp: &mut Completion<State, Op>,
        res: io::Result<i32>,
    ) -> io::Result<()> {
        match self {
            Op::HardLink(op) => op.complete(comp, res),
        }
    }
}
