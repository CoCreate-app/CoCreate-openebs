use std::{
    fmt::Debug,
    ops::{Deref, DerefMut},
    ptr::NonNull,
};

use libc::c_void;
use nix::errno::Errno;

use spdk_sys::{
    spdk_bdev_io,
    spdk_bdev_readv_blocks,
    spdk_bdev_reset,
    spdk_bdev_unmap_blocks,
    spdk_bdev_write_zeroes_blocks,
    spdk_bdev_writev_blocks,
    spdk_io_channel,
};

use crate::{
    bdev::{
        nexus::{
            nexus_bdev::NEXUS_PRODUCT_ID,
            nexus_channel::{DrEvent, NexusChannel, NexusChannelInner},
        },
        nexus_lookup,
        ChildState,
        Nexus,
        NexusStatus,
        Reason,
    },
    core::{
        Bdev,
        BdevHandle,
        Bio,
        Cores,
        GenericStatusCode,
        IoStatus,
        IoType,
        Mthread,
        Reactors,
    },
    ffihelper::FfiResult,
};

#[allow(unused_macros)]
macro_rules! offset_of {
    ($container:ty, $field:ident) => {
        unsafe { &(*(0usize as *const $container)).$field as *const _ as usize }
    };
}

#[allow(unused_macros)]
macro_rules! container_of {
    ($ptr:ident, $container:ty, $field:ident) => {{
        (($ptr as usize) - offset_of!($container, $field)) as *mut $container
    }};
}

#[repr(transparent)]
#[derive(Debug, Clone)]
pub(crate) struct NexusBio(Bio);

impl DerefMut for NexusBio {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Deref for NexusBio {
    type Target = Bio;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<*mut c_void> for NexusBio {
    fn from(ptr: *mut c_void) -> Self {
        Self(Bio::from(ptr))
    }
}

impl From<*mut spdk_bdev_io> for NexusBio {
    fn from(ptr: *mut spdk_bdev_io) -> Self {
        Self(Bio::from(ptr))
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct NioCtx {
    in_flight: u8,
    num_ok: u8,
    status: IoStatus,
    channel: NonNull<spdk_io_channel>,
    core: u32,
}

#[derive(Debug, Clone)]
#[repr(C)]
enum Disposition {
    /// All IOs are completed, and the final status of the IO should be set to
    /// the enum variant
    Complete(IoStatus),
    /// IOs are still in flight status of the last IO that failed
    Flying(IoStatus),
    /// retire the current child
    Retire(IoStatus),
}

pub(crate) fn nexus_submit_io(mut io: NexusBio) {
    if let Err(e) = match io.cmd() {
        IoType::Read => io.readv(),
        // these IOs are submitted to all the underlying children
        IoType::Write | IoType::WriteZeros | IoType::Reset | IoType::Unmap => {
            io.submit_all()
        }
        IoType::Flush => {
            io.ok();
            Ok(())
        }
        IoType::NvmeAdmin => {
            io.fail();
            Err(Errno::EINVAL)
        }

        _ => {
            trace!(?io, "not supported");
            io.fail();
            Err(Errno::EOPNOTSUPP)
        }
    } {
        error!(?e, ?io, "Error during IO submission");
    }
}

impl NexusBio {
    /// helper function to wrap the raw pointers into new types. From here we
    /// should not be dealing with any raw pointers.
    pub unsafe fn nexus_bio_setup(
        channel: *mut spdk_sys::spdk_io_channel,
        io: *mut spdk_sys::spdk_bdev_io,
    ) -> Self {
        let mut bio = NexusBio::from(io);
        let ctx = bio.ctx_as_mut();
        // for verification purposes when retiring a child
        ctx.core = Cores::current();
        ctx.channel = NonNull::new(channel).unwrap();
        ctx.status = IoStatus::Pending;
        ctx.in_flight = 0;
        ctx.num_ok = 0;
        bio
    }

    /// invoked when a nexus Io completes
    unsafe extern "C" fn child_completion(
        child_io: *mut spdk_bdev_io,
        success: bool,
        nexus_io: *mut c_void,
    ) {
        let mut nexus_io = NexusBio::from(nexus_io);
        let child_io = Bio::from(child_io);
        nexus_io.complete(child_io, success);
    }

    #[inline(always)]
    /// a mutable reference to the IO context
    fn ctx_as_mut(&mut self) -> &mut NioCtx {
        self.specific_as_mut::<NioCtx>()
    }

    #[inline(always)]
    /// immutable reference to the IO context
    fn ctx(&self) -> &NioCtx {
        self.specific::<NioCtx>()
    }

    /// Determine what to do with the IO if anything. In principle, it's the
    /// same way any other IO is completed but, it wrapped by the
    /// disposition. The general approach is if we return Disposition::
    /// Retire(IoStatus::Success) it means that we retire the current child and
    /// then, return mark the IO successful.
    fn disposition(&mut self) -> Disposition {
        let ctx = self.ctx_as_mut();
        match ctx.status {
            // all child IO's completed, complete the parent IO
            IoStatus::Pending if ctx.in_flight == 0 => {
                Disposition::Complete(IoStatus::Success)
            }
            // some child IO has completed, but not all
            IoStatus::Pending if ctx.in_flight != 0 => {
                Disposition::Flying(IoStatus::Success)
            }
            // Other IO are still inflight we encountered an error, retire this
            // child
            IoStatus::Failed if ctx.in_flight != 0 => {
                Disposition::Retire(IoStatus::Pending)
            }

            // this IO failed, but we have seen successfully IO's for the parent
            // already retire it
            IoStatus::Failed if ctx.num_ok != 0 && ctx.in_flight == 0 => {
                Disposition::Retire(IoStatus::Success)
            }

            // ALL io's have failed
            IoStatus::Failed if ctx.num_ok == 0 && ctx.in_flight == 0 => {
                Disposition::Complete(IoStatus::Failed)
            }
            // all IOs that where partially submitted completed, no bubble up
            // the ENOMEM to the upper layer we do not care if the
            // IO failed or complete, the whole IO must be resubmitted
            IoStatus::NoMemory if ctx.in_flight == 0 => {
                Disposition::Complete(IoStatus::NoMemory)
            }
            _ => {
                error!("{:?}", ctx);
                Disposition::Complete(IoStatus::Failed)
            }
        }
    }

    /// returns the type of command for this IO
    #[inline(always)]
    fn cmd(&self) -> IoType {
        self.io_type()
    }

    /// completion handler for the nexus when a child IO completes
    pub fn complete(&mut self, child_io: Bio, success: bool) {
        assert_eq!(self.ctx().core, Cores::current());

        // decrement the counter of in flight IO
        self.ctx_as_mut().in_flight -= 1;

        // record the state of at least one of the IO's.
        if !success {
            self.ctx_as_mut().status = IoStatus::Failed;
        } else {
            self.ctx_as_mut().num_ok += 1;
        }

        match self.disposition() {
            // the happy path, all is good
            Disposition::Complete(IoStatus::Success) => self.ok(),
            // All of IO's have failed but all remaining in flights completed
            // now as well depending on the error we can attempt to
            // do a retry.
            Disposition::Complete(IoStatus::Failed) => self.fail(),

            // IOs were submitted before we bumped into ENOMEM. The IO has
            // now completed, so we can finally report back to the
            // callee that we encountered ENOMEM during submission
            Disposition::Complete(IoStatus::NoMemory) => self.no_mem(),

            // We can mark the IO as success but before we do we need to retire
            // this child. This typically would only match when the last IO
            // has failed i.e [ok,ok,fail]
            Disposition::Retire(IoStatus::Success) => {
                assert_eq!(success, false);
                error!(
                    ?self,
                    ?child_io,
                    "{}:{}",
                    Cores::current(),
                    "last child IO failed completion"
                );
                self.try_retire(child_io.clone());
                self.ok();
            }

            // IO still in flight (pending) fail this IO and continue by setting
            // the parent status back to pending for example [ok,
            // fail, pending]
            Disposition::Retire(IoStatus::Pending) => {
                assert_eq!(success, false);
                error!(
                    ?self,
                    ?child_io,
                    "{}:{}",
                    Cores::current(),
                    "some child IO completion failed"
                );

                self.try_retire(child_io.clone());
                // more IO is pending ensure we set the proper context state
                self.ctx_as_mut().status = IoStatus::Pending;
            }
            // Disposition::Flying(_) => {
            //     assert_eq!(self.ctx().status, IoStatus::Pending);
            //     assert_ne!(self.ctx().in_flight, 0);
            // },
            _ => {}
        }

        // always free the child IO. The status of the child IO has been set by
        // the underlying device before invocation of the callback.
        child_io.free();
    }

    /// reference to the inner channels. The inner channel contains the specific
    /// per-core data structures.
    #[allow(clippy::mut_from_ref)]
    fn inner_channel(&self) -> &mut NexusChannelInner {
        NexusChannel::inner_from_channel(self.ctx().channel.as_ptr())
    }

    //TODO make const
    fn data_ent_offset(&self) -> u64 {
        let b = self.bdev();
        assert_eq!(b.product_name(), NEXUS_PRODUCT_ID);
        unsafe { Nexus::from_raw((*b.as_ptr()).ctxt) }.data_ent_offset
    }

    /// helper routine to get a channel to read from
    fn read_channel_at_index(&self, i: usize) -> &BdevHandle {
        &self.inner_channel().readers[i]
    }

    /// submit a read operation to one of the children of this nexus
    #[inline(always)]
    fn submit_read(&self, hdl: &BdevHandle) -> Result<(), Errno> {
        let (desc, chan) = hdl.io_tuple();
        unsafe {
            spdk_bdev_readv_blocks(
                desc,
                chan,
                self.iovs(),
                self.iov_count(),
                self.offset() + self.data_ent_offset(),
                self.num_blocks(),
                Some(Self::child_completion),
                self.as_ptr().cast(),
            )
        }
        .to_result(Errno::from_i32)
    }

    /// submit read IO to some child
    fn readv(&mut self) -> Result<(), Errno> {
        if let Some(i) = self.inner_channel().child_select() {
            let hdl = self.read_channel_at_index(i);
            self.submit_read(hdl).map(|_| {
                self.ctx_as_mut().in_flight += 1;
            })
        } else {
            self.fail();
            Err(Errno::ENODEV)
        }
    }

    #[inline(always)]
    fn submit_write(&self, hdl: &BdevHandle) -> Result<(), Errno> {
        let (desc, chan) = hdl.io_tuple();
        unsafe {
            spdk_bdev_writev_blocks(
                desc,
                chan,
                self.iovs(),
                self.iov_count(),
                self.offset() + self.data_ent_offset(),
                self.num_blocks(),
                Some(Self::child_completion),
                self.as_ptr().cast(),
            )
        }
        .to_result(Errno::from_i32)
    }

    #[inline(always)]
    fn submit_unmap(&self, hdl: &BdevHandle) -> Result<(), Errno> {
        let (desc, chan) = hdl.io_tuple();
        unsafe {
            spdk_bdev_unmap_blocks(
                desc,
                chan,
                self.offset() + self.data_ent_offset(),
                self.num_blocks(),
                Some(Self::child_completion),
                self.as_ptr().cast(),
            )
        }
        .to_result(Errno::from_i32)
    }

    #[inline(always)]
    fn submit_write_zeroes(&self, hdl: &BdevHandle) -> Result<(), Errno> {
        let (desc, chan) = hdl.io_tuple();
        unsafe {
            spdk_bdev_write_zeroes_blocks(
                desc,
                chan,
                self.offset() + self.data_ent_offset(),
                self.num_blocks(),
                Some(Self::child_completion),
                self.as_ptr().cast(),
            )
        }
        .to_result(Errno::from_i32)
    }

    #[inline(always)]
    fn submit_reset(&self, hdl: &BdevHandle) -> Result<(), Errno> {
        let (desc, chan) = hdl.io_tuple();
        unsafe {
            spdk_bdev_reset(
                desc,
                chan,
                Some(Self::child_completion),
                self.as_ptr().cast(),
            )
        }
        .to_result(Errno::from_i32)
    }
    /// Submit the IO to all underlying children, failing on the first error we
    /// find. When an IO is partially submitted -- we must wait until all
    /// the child IOs have completed before we mark the whole IO failed to
    /// avoid double frees. This function handles IO for a subset that must
    /// be submitted to all the underlying children.
    fn submit_all(&mut self) -> Result<(), Errno> {
        let mut inflight = 0;
        let mut status = IoStatus::Pending;

        let result = match self.cmd() {
            IoType::Write => {
                self.inner_channel().writers.iter().try_for_each(|h| {
                    self.submit_write(h).map(|_| {
                        inflight += 1;
                    })
                })
            }
            IoType::Unmap => {
                self.inner_channel().writers.iter().try_for_each(|h| {
                    self.submit_unmap(h).map(|_| {
                        inflight += 1;
                    })
                })
            }
            IoType::WriteZeros => {
                self.inner_channel().writers.iter().try_for_each(|h| {
                    self.submit_write_zeroes(h).map(|_| {
                        inflight += 1;
                    })
                })
            }
            IoType::Reset => {
                self.inner_channel().writers.iter().try_for_each(|h| {
                    self.submit_reset(h).map(|_| {
                        inflight += 1;
                    })
                })
            }
            // we should never reach here, if we do it is a bug.
            _ => unreachable!(),
        }
        .map_err(|se| {
            match se {
                Errno::ENOMEM => status = IoStatus::NoMemory,
                _ => status = IoStatus::Failed,
            }
            debug!(
                "IO submission failed with {} already submitted IOs {}",
                se, inflight
            );
            se
        });

        if inflight != 0 {
            self.ctx_as_mut().in_flight = inflight;
            self.ctx_as_mut().status = status;
        } else {
            // if no IO was submitted at all, we can fail the IO now.
            if matches!(result, Err(Errno::ENOMEM)) {
                self.no_mem();
            } else {
                // right now this could only be EINVAL, make sure to verify this
                // during debug builds
                debug_assert_eq!(result.err(), Some(Errno::EINVAL));
                self.fail();
            }
        }
        result
    }

    fn try_retire(&mut self, child_io: Bio) {
        let nvme_status = child_io.nvme_status();
        trace!(?nvme_status);

        if nvme_status.status_code() != GenericStatusCode::InvalidOpcode {
            Reactors::master().send_future(Self::child_retire(
                self.nexus_as_ref().name.clone(),
                child_io.bdev(),
            ));
        }
    }

    /// Retire a child for this nexus.
    async fn child_retire(nexus: String, child: Bdev) {
        match nexus_lookup(&nexus) {
            Some(nexus) => {
                if let Some(child) = nexus.child_lookup(&child.name()) {
                    let current_state = child.state.compare_and_swap(
                        ChildState::Open,
                        ChildState::Faulted(Reason::IoError),
                    );

                    if current_state == ChildState::Open {
                        warn!(
                            "core {} thread {:?}, faulting child {}",
                            Cores::current(),
                            Mthread::current(),
                            child,
                        );

                        nexus.pause().await.unwrap();
                        nexus.reconfigure(DrEvent::ChildFault).await;
                        // TODO: an error can occur here if a separate task,
                        // e.g. grpc request is also deleting the child.
                        if let Err(err) = child.destroy().await {
                            error!(
                                "{}: destroying child {} failed {}",
                                nexus, child, err
                            );
                        }

                        nexus.resume().await.unwrap();
                        if nexus.status() == NexusStatus::Faulted {
                            error!(":{} has no children left... ", nexus);
                        }
                    }
                }
            }
            None => {
                debug!(
                    "{} does not belong (anymore) to nexus {}",
                    child, nexus
                );
            }
        }
    }
}
