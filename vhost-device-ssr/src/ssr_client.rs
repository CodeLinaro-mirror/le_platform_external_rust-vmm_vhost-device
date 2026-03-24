// Copyright (c) 2024 Qualcomm Innovation Center, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause-Clear
#![allow(dead_code)]
use lazy_static::lazy_static;
use log::error;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error as ThisError;
use vhost_device_ssr::ssr_clients_bindings::ssr_api::{
    cb_func_with_ctx_t, ssr_register_callback_events, ssr_unregister_callback, trigger_ssr,
    SS_ID_SHIFT,
};
use vhost_device_ssr::ssr_clients_bindings::ssr_events::*;
use vmm_sys_util::eventfd::EventFd;

#[derive(Debug, Eq, PartialEq, ThisError)]
/// Errors related to ssr client.
pub(crate) enum SsrClientError {
    #[error("Register {0} callback events failed")]
    RegisterCallbackEvents(String),
    #[error("Unregister {0} callback events failed: {1}")]
    UnregisterCallbackEvents(String, i32),
    #[error("Trigger {0} ssr failed")]
    TriggerSsr(String),
}

lazy_static! {
    ///  Supported ssr client hashmap
    ///
    ///  name -> (magic_num, ss_id)
    pub(crate) static ref Client_Map: HashMap<&'static str, (u64, u32)> = {
        let mut map = HashMap::new();
        map.insert("ADSP", (MAGIC_LPASS, SS_ID_LPASS));
        map.insert("ADSP1", (MAGIC_ADSP1, SS_ID_ADSP1));
        map.insert("ADSP2", (MAGIC_ADSP2, SS_ID_ADSP2));
        map.insert("SLPI", (MAGIC_SLPI, SS_ID_SLPI));
        map.insert("CDSP", (MAGIC_CDSP0, SS_ID_CDSP));
        map.insert("CDSP1", (MAGIC_CDSP1, SS_ID_CDSP1));
        map.insert("CDSP2", (MAGIC_CDSP2, SS_ID_CDSP2));
        map.insert("CDSP3", (MAGIC_CDSP3, SS_ID_CDSP3));
        map.insert("GPDSP0", (MAGIC_GPDSP0, SS_ID_GPDSP0));
        map.insert("GPDSP1", (MAGIC_GPDSP1, SS_ID_GPDSP1));
        map.insert("HPASSC0", (MAGIC_HPASSC0, SS_ID_HPASSC0));
        map.insert("HPASSC1", (MAGIC_HPASSC1, SS_ID_HPASSC1));
        map.insert("HPASSC2", (MAGIC_HPASSC2, SS_ID_HPASSC2));
        map
    };

    /// mapping ss_id values to their corresponding client names
    ///
    /// SSID -> qcom-ssr-name

    pub(crate) static ref Ssr_Map: HashMap<u32, &'static str> = {
        let mut ssr_map: HashMap<u32, &'static str> = HashMap::new();
        ssr_map.insert(SS_ID_LPASS, "adsp");
        ssr_map.insert(SS_ID_ADSP1, "adsp1");
        ssr_map.insert(SS_ID_ADSP2, "adsp2");
        ssr_map.insert(SS_ID_MODEM, "modem");
        ssr_map.insert(SS_ID_SLPI, "slpi");
        ssr_map.insert(SS_ID_CDSP, "cdsp");
        ssr_map.insert(SS_ID_CDSP1, "cdsp1");
        ssr_map.insert(SS_ID_CDSP2, "cdsp2");
        ssr_map.insert(SS_ID_CDSP3, "cdsp3");
        ssr_map.insert(SS_ID_GPDSP0, "gpdsp0");
        ssr_map.insert(SS_ID_GPDSP1, "gpdsp1");
        ssr_map.insert(SS_ID_HPASSC0, "adsp");
        ssr_map.insert(SS_ID_HPASSC1, "adsp1");
        ssr_map.insert(SS_ID_HPASSC2, "adsp2");
        ssr_map
    };

}

/// callback registered into SSR
pub unsafe extern "C" fn ssr_virtio_event_handler(
    ss_id: ssr_ss_id,
    ssr_event: ssr_events,
    ctx: *mut ::std::os::raw::c_void,
) -> ::std::os::raw::c_int {
    if ctx.is_null() {
        return -1;
    }
    let raw_ptr = ctx as *mut VhSsrCtx;
    log::info!(
        "Receive from callback: ssr_ss_id: {}, ssr_events:{}",
        ss_id,
        ssr_event
    );
    if let Err(e) = (*raw_ptr).write_and_notify(ss_id, ssr_event) {
        log::error!("write ctx eventfd failed: {:?}", e);
        return -1;
    }
    0
}

pub(crate) struct VhSsrCtx {
    pending: VecDeque<(ssr_ss_id, ssr_events)>,
    notify_fd: Arc<EventFd>,
}

impl VhSsrCtx {
    pub fn new(notify_fd: Arc<EventFd>) -> Self {
        VhSsrCtx {
            pending: VecDeque::new(),
            notify_fd,
        }
    }

    pub fn write_and_notify(
        &mut self,
        ss_id: ssr_ss_id,
        ssr_event: ssr_events,
    ) -> Result<(), io::Error> {
        self.pending.push_back((ss_id, ssr_event));
        if self.pending.len() == 1 {
            self.notify_fd.write(0x10)?;
        }
        Ok(())
    }

    pub fn get_response(&mut self) -> Option<(String, ssr_events)> {
        let (ss_id, ssr_event) = self.pending.pop_front()?;

        // Look up the client name by ss_id; use "unknown client" if not found
        let name = Ssr_Map.get(&ss_id).unwrap_or(&"unknown client").to_string();
        Some((name, ssr_event))
    }

    pub fn reset(&mut self) {
        if self.pending.is_empty() {
            // read notify_fd to avoid endless return in epoll_wait
            self.notify_fd.read().ok();
        }
    }

    #[cfg(test)]
    pub fn push_pending(&mut self, ss_id: ssr_ss_id, ssr_event: ssr_events) {
        self.pending.push_back((ss_id, ssr_event));
    }
}

pub trait SsrClient {
    /// Register client to SSR
    fn register(&self, prefix_name: &str) -> Result<u64, SsrClientError>;

    /// Unregister client to SSR
    fn unregister(&self) -> Result<u64, SsrClientError>;

    /// trigger event for the client from SSR
    fn trigger(&self) -> Result<u64, SsrClientError>;

    /// new default client
    fn new_default(
        prefix_name: &str,
        client_name: String,
        ctx: Arc<Mutex<VhSsrCtx>>,
        event_handler: cb_func_with_ctx_t,
    ) -> Self
    where
        Self: Sized;
}

pub(crate) struct SsrVuClient {
    ctx: Arc<Mutex<VhSsrCtx>>,
    client_magic: u64,
    client_name: String,
    event_mask: u64,
    event_handler: cb_func_with_ctx_t,
    /* *mut std::os::raw::c_void */
    priv_data: AtomicPtr<::std::os::raw::c_void>,
}

impl SsrVuClient {
    pub fn new(
        client_magic: u64,
        client_name: String,
        event_mask: u64,
        ctx: Arc<Mutex<VhSsrCtx>>,
        event_handler: cb_func_with_ctx_t,
        priv_data: AtomicPtr<::std::os::raw::c_void>,
    ) -> Self {
        SsrVuClient {
            client_magic,
            client_name,
            event_mask,
            ctx,
            event_handler,
            priv_data,
        }
    }

    pub fn get_priv_ptr(&self) -> *mut ::std::os::raw::c_void {
        self.priv_data.load(Ordering::Relaxed)
    }
}

impl SsrClient for SsrVuClient {
    fn trigger(&self) -> Result<u64, SsrClientError> {
        let priv_data = self.get_priv_ptr();
        unsafe {
            let ret = trigger_ssr(priv_data, self.client_magic);
            match ret {
                0 => Ok(0),
                _ => Err(SsrClientError::TriggerSsr(self.client_name.clone())),
            }
        }
    }

    fn register(&self, prefix_name: &str) -> Result<u64, SsrClientError> {
        let register_name = format!("{}:{}", prefix_name, self.client_name.as_str());
        let name = CString::new(register_name.as_str())
            .unwrap_or_else(|_| panic!("New client name: {} failed", register_name));
        let ctx_ptr =
            &mut *self.ctx.lock().unwrap() as *mut VhSsrCtx as *mut ::std::os::raw::c_void;
        unsafe {
            let ret = ssr_register_callback_events(
                self.client_magic,
                self.event_handler,
                self.event_mask,
                self.priv_data.as_ptr(),
                name.as_ptr(),
                ctx_ptr,
            );
            match ret {
                0 => Ok(0),
                _ => Err(SsrClientError::RegisterCallbackEvents(
                    register_name.clone(),
                )),
            }
        }
    }

    fn unregister(&self) -> Result<u64, SsrClientError> {
        let client_ctx_ptr = self.get_priv_ptr();
        unsafe {
            let ret = ssr_unregister_callback(client_ctx_ptr);
            match ret {
                0 => Ok(0),
                _ => Err(SsrClientError::UnregisterCallbackEvents(
                    self.client_name.clone(),
                    ret,
                )),
            }
        }
    }

    fn new_default(
        prefix_name: &str,
        client_name: String,
        ctx: Arc<Mutex<VhSsrCtx>>,
        event_handler: cb_func_with_ctx_t,
    ) -> Self {
        let client_magic = Client_Map.get(client_name.as_str()).unwrap().0;
        let client_id = Client_Map.get(client_name.as_str()).unwrap().1;
        let event_bits: u32 = SSR_EVENT_FAULT_NOTIFY
            | SSR_EVENT_RESTART_START
            | SSR_EVENT_RESTART_FAILED
            | SSR_EVENT_PRE_DS
            | SSR_EVENT_DUMMY
            | SSR_EVENT_RESTART_COMPLETE;

        let event_mask: u64 = ((client_id as u64) << (SS_ID_SHIFT as u64)) | (event_bits as u64);
        let client_ctx_ptr: *mut ::std::os::raw::c_void = null_mut();
        let priv_data = AtomicPtr::new(client_ctx_ptr);
        let client = Self::new(
            client_magic,
            client_name,
            event_mask,
            ctx,
            event_handler,
            priv_data,
        );
        client.register(prefix_name).unwrap();
        client
    }
}

#[cfg(test)]

mod tests {
    use super::{ssr_virtio_event_handler, Client_Map, SsrVuClient};
    use crate::ssr_client::VhSsrCtx;
    use crate::vhu_ssr::SSR_EVENT_IN_VRING_EPOLL;
    use libc::sleep;
    use std::{
        os::fd::AsRawFd,
        ptr::null_mut,
        sync::{atomic::AtomicPtr, Arc, Mutex},
        thread,
    };
    use vhost_device_ssr::ssr_clients_bindings::{
        ssr_api::{cb_func_with_ctx_t, SS_ID_SHIFT},
        ssr_events::*,
    };
    use vmm_sys_util::epoll::EventSet;
    use vmm_sys_util::{
        epoll::{ControlOperation, Epoll, EpollEvent},
        eventfd::{EventFd, EFD_NONBLOCK},
    };

    #[test]
    fn write_and_read_notify() {
        let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let ctx = Arc::new(Mutex::new(VhSsrCtx::new(Arc::clone(&notify_fd))));

        for _ in 0..10 {
            unsafe {
                let ctx_ptr =
                    &mut *ctx.lock().unwrap() as *mut VhSsrCtx as *mut ::std::os::raw::c_void;
                ssr_virtio_event_handler(SS_ID_LPASS, SSR_EVENT_FAULT_NOTIFY, ctx_ptr)
            };
        }

        let mut ctx = ctx.lock().unwrap();
        assert_eq!(ctx.notify_fd.read().unwrap(), 16);
        for _ in 0..10 {
            assert_eq!(
                ctx.get_response(),
                Some(("adsp".to_string(), SSR_EVENT_FAULT_NOTIFY))
            );
            ctx.reset();
        }
        assert_eq!(ctx.get_response(), None);
        assert_eq!(ctx.notify_fd.read().ok(), None);
    }

    #[test]
    fn write_and_read_notify_with_epoll_in_single_thread() {
        let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let raw_fd = notify_fd.as_raw_fd();
        let ctx = Arc::new(Mutex::new(VhSsrCtx::new(Arc::clone(&notify_fd))));
        let epoll_handler = Epoll::new().unwrap();
        epoll_handler
            .ctl(
                ControlOperation::Add,
                raw_fd,
                EpollEvent::new(EventSet::IN, SSR_EVENT_IN_VRING_EPOLL as u64),
            )
            .unwrap();
        for _ in 0..10 {
            let ctx_ptr = &mut *ctx.lock().unwrap() as *mut VhSsrCtx as *mut ::std::os::raw::c_void;
            unsafe { ssr_virtio_event_handler(SS_ID_CDSP, SSR_EVENT_FAULT_NOTIFY, ctx_ptr) };
        }
        let mut ready_events = vec![EpollEvent::default(); 10];
        let ev_count = epoll_handler.wait(-1, &mut ready_events[..]).unwrap();
        for i in 0..ev_count {
            if ready_events[i].data() == SSR_EVENT_IN_VRING_EPOLL as u64 {
                let mut cxt_unlocked = ctx.lock().expect("lock failed!");
                let x = cxt_unlocked.notify_fd.read().unwrap();
                let res = cxt_unlocked.get_response().unwrap();
                cxt_unlocked.reset();
                assert_eq!(x, 16);
                assert_eq!(res.0, "cdsp".to_string());
                assert_eq!(res.1, SSR_EVENT_FAULT_NOTIFY);
                for _ in 1..10 {
                    let res = cxt_unlocked.get_response().unwrap();
                    assert_eq!(res.0, "cdsp".to_string());
                    assert_eq!(res.1, SSR_EVENT_FAULT_NOTIFY);
                    cxt_unlocked.reset();
                }
            }
        }
    }

    #[test]
    fn write_and_read_notify_with_epoll_in_multiple_threads() {
        let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let raw_fd = notify_fd.as_raw_fd();
        let ctx = Arc::new(Mutex::new(VhSsrCtx::new(Arc::clone(&notify_fd))));
        let ctx_clone = Arc::clone(&ctx);
        let mut handler_list = Vec::new();
        let epoll_handler = thread::spawn(move || {
            let mut index = 1;
            let epoll_handler = Epoll::new().unwrap();
            epoll_handler
                .ctl(
                    ControlOperation::Add,
                    raw_fd,
                    EpollEvent::new(EventSet::IN, SSR_EVENT_IN_VRING_EPOLL as u64),
                )
                .unwrap();
            loop {
                let mut ready_events = vec![EpollEvent::default(); 10];
                let ev_count = epoll_handler.wait(-1, &mut ready_events[..]).unwrap();

                for i in 0..ev_count {
                    if ready_events[i].data() == SSR_EVENT_IN_VRING_EPOLL as u64 {
                        let mut cxt_unlocked = ctx_clone.lock().expect("lock failed!");
                        let x = cxt_unlocked.notify_fd.read().unwrap();
                        assert_eq!(x, 16);
                        let res = cxt_unlocked.get_response();
                        cxt_unlocked.reset();
                        match res {
                            Some(res) => {
                                assert_eq!(res.1, index);
                            }
                            None => continue,
                        }
                    }
                }
                index += 1;
                if index == 10 {
                    break;
                }
            }
        });

        handler_list.push(epoll_handler);

        for i in 1..10 {
            let ctx_clone = Arc::clone(&ctx);
            let handler = thread::spawn(move || {
                let ctx_ptr = &mut *ctx_clone.lock().expect("lock failed") as *mut VhSsrCtx
                    as *mut ::std::os::raw::c_void;
                unsafe { ssr_virtio_event_handler(i, i, ctx_ptr) };
            });
            unsafe { sleep(1) };
            handler_list.push(handler);
        }

        for handler in handler_list {
            handler.join().unwrap();
        }
    }

    #[test]
    fn verify_ssr_vu_client() {
        let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let ctx = Arc::new(Mutex::new(VhSsrCtx::new(Arc::clone(&notify_fd))));
        let ssr_test_callback: cb_func_with_ctx_t = Some(ssr_virtio_event_handler);
        let client_name = "CDSP".to_string();
        let client_magic = Client_Map.get(client_name.as_str()).unwrap().0;
        let client_id = Client_Map.get(client_name.as_str()).unwrap().1;
        let event_bits: u32 = SSR_EVENT_FAULT_NOTIFY
            | SSR_EVENT_RESTART_START
            | SSR_EVENT_RESTART_FAILED
            | SSR_EVENT_PRE_DS
            | SSR_EVENT_DUMMY
            | SSR_EVENT_RESTART_COMPLETE;

        let event_mask: u64 = ((client_id as u64) << (SS_ID_SHIFT as u64)) | (event_bits as u64);

        let client_ctx_ptr: *mut ::std::os::raw::c_void = null_mut();
        let priv_data = AtomicPtr::new(client_ctx_ptr);
        let client = SsrVuClient::new(
            client_magic,
            client_name,
            event_mask,
            ctx,
            ssr_test_callback,
            priv_data,
        );
        assert_eq!(
            client.get_priv_ptr(),
            client_ctx_ptr as *mut ::std::os::raw::c_void
        )
    }

    #[test]
    fn verify_vh_ssr_ctx() {
        let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let mut ctx = VhSsrCtx::new(Arc::clone(&notify_fd));
        assert!(ctx.pending.is_empty());
        assert_eq!(ctx.get_response(), None);

        ctx.write_and_notify(SS_ID_CDSP, 1).unwrap();
        assert_eq!(ctx.pending.len(), 1);
        assert_eq!(ctx.notify_fd.read().ok(), Some(0x10));
        assert_eq!(ctx.get_response(), Some(("cdsp".to_string(), 1)));
        ctx.reset();
        assert!(ctx.pending.is_empty());
        assert_eq!(ctx.notify_fd.read().ok(), None);

        ctx.write_and_notify(SS_ID_CDSP1, 1).unwrap();
        ctx.write_and_notify(SS_ID_GPDSP0, 2).unwrap();
        ctx.write_and_notify(SS_ID_GPDSP1, 3).unwrap();
        assert_eq!(ctx.pending.len(), 3);
        // Only one eventfd write happens (empty→non-empty transition).
        assert_eq!(ctx.notify_fd.read().ok(), Some(0x10));
        assert_eq!(ctx.get_response(), Some(("cdsp1".to_string(), 1)));
        ctx.reset(); // queue not empty yet, no eventfd read
        assert_eq!(ctx.notify_fd.read().ok(), None);
        assert_eq!(ctx.get_response(), Some(("gpdsp0".to_string(), 2)));
        ctx.reset(); // queue not empty yet, no eventfd read
        assert_eq!(ctx.notify_fd.read().ok(), None);
        assert_eq!(ctx.get_response(), Some(("gpdsp1".to_string(), 3)));

        ctx.reset(); // queue now empty, eventfd drained by reset
        assert!(ctx.pending.is_empty());
        assert_eq!(ctx.notify_fd.read().ok(), None);
    }
}
