// Copyright (c) 2024 Qualcomm Innovation Center, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause-Clear

#![allow(dead_code)]
use std::{
    io::{self, Result as IoResult},
    sync::{Arc, Mutex},
};

use thiserror::Error as ThisError;
use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_device_ssr::ssr_clients_bindings::ssr_events::{
    SSR_EVENT_FAULT_NOTIFY, SSR_EVENT_RESTART_COMPLETE, SSR_EVENT_RESTART_START,
};
use vhost_user_backend::{VhostUserBackendMut, VringRwLock, VringT};
use virtio_bindings::bindings::virtio_config::{VIRTIO_F_NOTIFY_ON_EMPTY, VIRTIO_F_VERSION_1};
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_INDIRECT_DESC;
use virtio_queue::QueueT;
use vm_memory::{ByteValued, Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap, Le16};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

use crate::ssr_client::{SsrClient, VhSsrCtx};
pub(crate) const SSR_EVENT_IN_VRING_EPOLL: u16 = 0x100;
const QUEUE_SIZE: usize = 128;
const NUM_QUEUES: usize = 1;

/// SSR definitions from Virtio Spec
const VIRTIO_SSR_F_HOST_TO_GUEST: u16 = 0;
const SUBSYSTEM_NAME_SIZE: usize = 16;

/// QCOM Linux SSR Event definition
pub type QcomSsrNotifyType = u16;
const QCOM_SSR_BEFORE_POWERUP: QcomSsrNotifyType = 0;
const QCOM_SSR_AFTER_POWERUP: QcomSsrNotifyType = 1;
const QCOM_SSR_BEFORE_SHUTDOWN: QcomSsrNotifyType = 2;
const QCOM_SSR_AFTER_SHUTDOWN: QcomSsrNotifyType = 3;

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub(crate) struct VirtioSsrOutHdr {
    name: [u8; SUBSYSTEM_NAME_SIZE],
    event_type: Le16,
}
unsafe impl ByteValued for VirtioSsrOutHdr {}

impl VirtioSsrOutHdr {
    pub(crate) fn new(name: String, event_type: u16) -> Self {
        let mut buffer: [u8; SUBSYSTEM_NAME_SIZE] = [b'\0'; SUBSYSTEM_NAME_SIZE];
        for (i, c) in name.chars().take(buffer.len() - 1).enumerate() {
            buffer[i] = c as u8;
        }

        VirtioSsrOutHdr {
            name: buffer,
            event_type: event_type.into(),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ThisError)]
/// Errors related to vhost-device-foo daemon.
pub(crate) enum VuSsrError {
    #[error("Notification send failed")]
    SendNotificationFailed,
    #[error("Can't create eventFd")]
    EventFdError,
    #[error("Failed to handle event")]
    HandleEventNotEpollIn,
    #[error("Too many descriptors: {0}")]
    UnexpectedDescriptorCount(usize),
    #[error("Failed to write descriptor to vring")]
    UnexpectedWriteDescriptorError,
    #[error("Failed to write event to vring")]
    UnexpectedWriteVringError,
    #[error("Failed to register client")]
    FailedRegisterClient,
}

type Result<T> = std::result::Result<T, VuSsrError>;

impl From<VuSsrError> for io::Error {
    fn from(e: VuSsrError) -> Self {
        Self::new(io::ErrorKind::Other, e)
    }
}

pub(crate) struct VuSsrBackend<T: SsrClient> {
    ssr_clients: Vec<T>,
    ctx: Arc<Mutex<VhSsrCtx>>,
    event_idx: bool,
    pub exit_event: EventFd,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
}

impl<T: SsrClient> VuSsrBackend<T> {
    pub fn new(
        ssr_clients: Vec<T>,
        ctx: Arc<Mutex<VhSsrCtx>>,
    ) -> std::result::Result<Self, std::io::Error> {
        Ok(VuSsrBackend {
            ssr_clients,
            ctx,
            event_idx: false,
            exit_event: EventFd::new(EFD_NONBLOCK).map_err(|_| VuSsrError::EventFdError)?,
            mem: None,
        })
    }

    pub fn unregister_clients(&self) {
        for client in &self.ssr_clients {
            if let Err(e) = client.unregister() {
                log::error!("Error unregistering {:?}", e);
            }
        }
    }

    /// Process the event once ssr_callback called and dispatch it to guest
    fn process_event(&mut self, vring: &VringRwLock, out: VirtioSsrOutHdr) -> Result<bool> {
        let mem = self.mem.as_ref().unwrap().memory();

        let desc_chain = vring
            .get_mut()
            .get_queue_mut()
            .pop_descriptor_chain(mem.clone());
        match desc_chain {
            Some(desc_chain) => {
                let descriptors: Vec<_> = desc_chain.clone().collect();
                if descriptors.len() != 1 {
                    return Err(VuSsrError::UnexpectedDescriptorCount(descriptors.len()));
                }
                let descriptor = descriptors[0];
                desc_chain
                    .memory()
                    .write_obj(out, descriptor.addr())
                    .map_err(|_| VuSsrError::UnexpectedWriteDescriptorError)?;
                if vring
                    .add_used(desc_chain.head_index(), out.as_slice().len() as u32)
                    .is_err()
                {
                    log::error!("Couldn't write out data to the ring");
                    return Err(VuSsrError::UnexpectedWriteVringError);
                }
            }
            None => {
                log::warn!("no available descriptors in ring!");
                // Now cannot get available descriptor, which means the host cannot process
                // event data in time and overrun happens in the backend. In this case,
                // we simply drop the incoming ssr event and notify guest for handling
                // event. At the end, it returns Ok(false) so can avoid exiting the thread loop.
                vring
                    .signal_used_queue()
                    .map_err(|_| VuSsrError::SendNotificationFailed)?;

                return Ok(false);
            }
        }
        // notify guest for handling events. At the end, it returns Ok(false) so can avoid exiting the thread loop.
        vring
            .signal_used_queue()
            .map_err(|_| VuSsrError::SendNotificationFailed)?;

        Ok(true)
    }

    /// Process the event from ssr and dispatch replies
    fn process_queue(&mut self, vring: &VringRwLock) -> Result<bool> {
        let response = {
            let mut ctx_unlocked = self
                .ctx
                .lock()
                .expect("Unable to get lock in process_event");
            let response = ctx_unlocked.get_response();
            ctx_unlocked.reset();
            response
        };
        let (name, ssr_event) = match response {
            Some(response) => (response.0, response.1),
            None => return Ok(false),
        };

        // generated Before and After statuses just from one ssr event
        let msgs: Vec<VirtioSsrOutHdr> = match ssr_event {
            SSR_EVENT_FAULT_NOTIFY => vec![QCOM_SSR_BEFORE_SHUTDOWN, QCOM_SSR_AFTER_SHUTDOWN],
            SSR_EVENT_RESTART_COMPLETE => vec![QCOM_SSR_AFTER_POWERUP],
            SSR_EVENT_RESTART_START => vec![QCOM_SSR_BEFORE_POWERUP],
            _ => return Ok(false),
        }
        .into_iter()
        .map(|x| VirtioSsrOutHdr::new(name.clone(), x))
        .collect();
        for msg in msgs {
            self.process_event(vring, msg)?;
        }
        Ok(true)
    }
}

/// VhostUserBackendMut trait methods
impl<T: 'static + SsrClient + Sync + Send> VhostUserBackendMut for VuSsrBackend<T> {
    type Vring = VringRwLock;
    type Bitmap = ();

    fn num_queues(&self) -> usize {
        NUM_QUEUES
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE
    }

    fn features(&self) -> u64 {
        // this matches the current libvhost defaults except VHOST_F_LOG_ALL
        1 << VIRTIO_F_VERSION_1
            | 1 << VIRTIO_F_NOTIFY_ON_EMPTY
            | 1 << VIRTIO_RING_F_INDIRECT_DESC
            | 1 << VIRTIO_SSR_F_HOST_TO_GUEST

            // Protocol features are optional and must not be defined unless required and must be
            // accompanied by the supporting PROTOCOL_FEATURES bits in features.
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> IoResult<()> {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        evset: EventSet,
        vrings: &[VringRwLock],
        _thread_id: usize,
    ) -> IoResult<()> {
        if evset != EventSet::IN {
            return Err(VuSsrError::HandleEventNotEpollIn.into());
        }

        if device_event == SSR_EVENT_IN_VRING_EPOLL {
            let vring = &vrings[0];
            // we won't enable EVENT_IDX
            // event should be sent to FE right now, a single call is enough
            self.process_queue(vring)?;
        }
        Ok(())
    }

    fn exit_event(&self, _thread_index: usize) -> Option<EventFd> {
        self.exit_event.try_clone().ok()
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use std::result::Result;
    use vhost_device_ssr::ssr_clients_bindings::ssr_api::cb_func_with_ctx_t;
    use virtio_queue::Descriptor;
    use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryAtomic, GuestMemoryMmap};

    use crate::ssr_client::{ssr_virtio_event_handler, SsrClientError};
    use crate::vhu_ssr::SSR_EVENT_IN_VRING_EPOLL;

    use super::*;

    struct MockSsrClient {
        ctx: Arc<Mutex<VhSsrCtx>>,
    }
    impl SsrClient for MockSsrClient {
        fn register(&self) -> Result<u64, SsrClientError> {
            Ok(0)
        }

        fn unregister(&self) -> Result<u64, SsrClientError> {
            Ok(0)
        }

        fn trigger(&self) -> Result<u64, SsrClientError> {
            Ok(0)
        }

        fn new_default(
            _client_name: String,
            ctx: Arc<Mutex<VhSsrCtx>>,
            _event_handler: vhost_device_ssr::ssr_clients_bindings::ssr_api::cb_func_with_ctx_t,
        ) -> Self
        where
            Self: Sized,
        {
            MockSsrClient { ctx }
        }
    }
    impl MockSsrClient {
        fn set_ctx(&self, id: u16, event: u16) {
            self.ctx.lock().unwrap().set_ctx(id.into(), event.into());
        }
    }

    fn new_mockbackend<D: 'static + SsrClient + Send + Sync>() -> VuSsrBackend<MockSsrClient> {
        let ssr_test_callback: cb_func_with_ctx_t = Some(ssr_virtio_event_handler);
        let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let ctx = Arc::new(Mutex::new(VhSsrCtx::new(Arc::clone(&notify_fd))));
        let ssr_client0 =
            MockSsrClient::new_default("test0".to_string(), ctx.clone(), ssr_test_callback);
        let ssr_client1 =
            MockSsrClient::new_default("test1".to_string(), ctx.clone(), ssr_test_callback);
        VuSsrBackend::new(vec![ssr_client0, ssr_client1], ctx).unwrap()
    }

    #[test]
    fn verify_handle_event() {
        let mut backend = new_mockbackend::<MockSsrClient>();

        // Artificial memory
        let mem = GuestMemoryAtomic::new(
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap(),
        );

        // Update memory
        backend.update_memory(mem.clone()).unwrap();

        // Artificial Vring
        let vring = VringRwLock::new(mem, 0x1000).unwrap();
        vring.set_queue_info(0x100, 0x400, 0x800).unwrap();
        vring.set_queue_size(16);
        vring.set_queue_ready(true);

        backend.set_event_idx(false);

        // Currently handles SSR_EVENT_IN_VRING_EPOLL IN only,
        assert_eq!(
            backend
                .handle_event(SSR_EVENT_IN_VRING_EPOLL, EventSet::IN, &[vring.clone()], 0)
                .ok(),
            Some(())
        );

        assert_eq!(
            backend
                .handle_event(SSR_EVENT_IN_VRING_EPOLL, EventSet::OUT, &[vring.clone()], 0)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Other
        );
    }

    #[test]
    fn verify_process_queue() {
        let mut backend = new_mockbackend::<MockSsrClient>();
        let mem_map =
            &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0x100), 0x1000)]).unwrap();

        // Artificial memory
        let mem = GuestMemoryAtomic::new(mem_map.clone());

        // Update memory
        backend.update_memory(mem.clone()).unwrap();

        // Artificial Vring
        let vring = VringRwLock::new(mem, 0x100).unwrap();
        vring.set_queue_info(0x100, 0x200, 0x300).unwrap();
        vring.set_queue_ready(false);

        //Unavailable ssr event, return Ok(false)
        assert_eq!(backend.ctx.lock().unwrap().get_response(), None);
        assert_eq!(backend.process_queue(&vring), Ok(false));
        {
            backend
                .ctx
                .lock()
                .unwrap()
                .set_ctx(8, SSR_EVENT_FAULT_NOTIFY);
        }
        // Create a descriptor chain with two descriptors.
        let desc = Descriptor::new(0x400_u64, 0x100, 0, 0);
        mem_map.write_obj(desc, GuestAddress(0x100)).unwrap();

        let desc = Descriptor::new(0x500_u64, 0x100, 0, 0);
        mem_map.write_obj(desc, GuestAddress(0x100 + 16)).unwrap();

        // Put the descriptor index 0 in the first available ring position.
        mem_map
            .write_obj(0u16, GuestAddress(0x200).unchecked_add(4))
            .unwrap();

        // Set `avail_idx` to 2.
        mem_map
            .write_obj(2u16, GuestAddress(0x200).unchecked_add(2))
            .unwrap();
        vring.set_queue_ready(true);

        let used_idx = vring.queue_used_idx().unwrap();
        assert_eq!(used_idx, 0);

        // The mock device returns Ok(true).
        assert_eq!(backend.process_queue(&vring), Ok(true));
        let used_idx = vring.queue_used_idx().unwrap();
        assert_eq!(used_idx, 2);
    }

    #[test]
    fn verify_process_event() {
        let mut backend = new_mockbackend::<MockSsrClient>();
        let mem_map =
            &GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0x100), 0x1000)]).unwrap();

        // Artificial memory
        let mem = GuestMemoryAtomic::new(mem_map.clone());

        // Update memory
        backend.update_memory(mem.clone()).unwrap();

        // Artificial Vring
        let vring = VringRwLock::new(mem, 0x100).unwrap();
        vring.set_queue_info(0x100, 0x200, 0x300).unwrap();
        vring.set_queue_ready(false);

        //Unavailable ssr event, return Ok(false)
        assert_eq!(backend.ctx.lock().unwrap().get_response(), None);
        assert_eq!(
            backend.process_event(&vring, VirtioSsrOutHdr::new("name".to_string(), 10)),
            Ok(false)
        );
        {
            backend
                .ctx
                .lock()
                .unwrap()
                .set_ctx(8, SSR_EVENT_FAULT_NOTIFY);
        }
        // Create a descriptor chain with one descriptor.
        let desc = Descriptor::new(0x400_u64, 0x100, 0, 0);
        mem_map.write_obj(desc, GuestAddress(0x100)).unwrap();

        // Put the descriptor index 0 in the first available ring position.
        mem_map
            .write_obj(0u16, GuestAddress(0x200).unchecked_add(4))
            .unwrap();

        // Set `avail_idx` to 1.
        mem_map
            .write_obj(1u16, GuestAddress(0x200).unchecked_add(2))
            .unwrap();
        vring.set_queue_ready(true);

        let used_idx = vring.queue_used_idx().unwrap();
        assert_eq!(used_idx, 0);

        // The mock device returns Ok(true).
        assert_eq!(
            backend.process_event(&vring, VirtioSsrOutHdr::new("name".to_string(), 10)),
            Ok(true)
        );
        let used_idx = vring.queue_used_idx().unwrap();
        assert_eq!(used_idx, 1);
    }

    #[test]
    fn verify_backend() {
        let mut backend = new_mockbackend::<MockSsrClient>();
        {
            backend
                .ctx
                .lock()
                .unwrap()
                .set_ctx(8, SSR_EVENT_FAULT_NOTIFY);
        }

        // Artificial memory
        let mem = GuestMemoryAtomic::new(
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap(),
        );

        assert_eq!(backend.num_queues(), NUM_QUEUES);
        assert_eq!(backend.max_queue_size(), QUEUE_SIZE);
        assert_eq!(
            backend.features(),
            1 << VIRTIO_F_VERSION_1
                | 1 << VIRTIO_F_NOTIFY_ON_EMPTY
                | 1 << VIRTIO_RING_F_INDIRECT_DESC
                | 1 << VIRTIO_SSR_F_HOST_TO_GUEST
                | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
        );
        assert_eq!(backend.protocol_features(), VhostUserProtocolFeatures::MQ);

        assert_eq!(backend.queues_per_thread(), vec![0xffff_ffff]);
        assert!(backend.update_memory(mem.clone()).is_ok());

        backend.set_event_idx(false);
        assert!(!backend.event_idx);

        let fd = backend.exit_event(0);
        assert_matches!(fd, Some(_))
    }

    #[test]
    fn verify_virtio_ssr_outhdr() {
        let outhdr = VirtioSsrOutHdr::new("CDSP".to_string(), 10);
        assert_eq!(outhdr.name.len(), SUBSYSTEM_NAME_SIZE);
        let mut buffer: [u8; SUBSYSTEM_NAME_SIZE] = [b'\0'; SUBSYSTEM_NAME_SIZE];
        buffer[0] = b'C';
        buffer[1] = b'D';
        buffer[2] = b'S';
        buffer[3] = b'P';
        assert_eq!(outhdr.name, buffer);
        assert_eq!(outhdr.event_type, 10);

        //oversize name
        let outhdr = VirtioSsrOutHdr::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), 10);
        let mut buffer: [u8; SUBSYSTEM_NAME_SIZE] = [b'a'; SUBSYSTEM_NAME_SIZE];
        buffer[SUBSYSTEM_NAME_SIZE - 1] = b'\0';
        assert_eq!(outhdr.name, buffer);
        assert_eq!(outhdr.event_type, 10);
    }

    #[test]
    fn verify_mock_client() {
        let backend = new_mockbackend::<MockSsrClient>();
        let client = backend.ssr_clients.get(0).unwrap();
        assert_eq!(client.register(), Ok(0));
        assert_eq!(client.unregister(), Ok(0));
        assert_eq!(client.trigger(), Ok(0));
        client.set_ctx(16, SSR_EVENT_FAULT_NOTIFY as u16);
        let ctx = client.ctx.lock().unwrap().get_response().unwrap();
        assert_eq!(ctx.0, "cdsp".to_string());
        assert_eq!(ctx.1, SSR_EVENT_FAULT_NOTIFY);
    }
}
