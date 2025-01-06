// Copyright (c) 2024 Qualcomm Innovation Center, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause-Clear
#![allow(dead_code)]
use lazy_static::lazy_static;
use log::error;
use std::collections::HashMap;
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
    #[error("Unregister {0} callback events failed")]
    UnregisterCallbackEvents(String),
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
        map.insert("SLPI", (MAGIC_SLPI, SS_ID_SLPI));
        map.insert("CDSP", (MAGIC_CDSP0, SS_ID_CDSP));
        map.insert("CDSP1", (MAGIC_CDSP1, SS_ID_CDSP1));
        map.insert("GPDSP0", (MAGIC_GPDSP0, SS_ID_GPDSP0));
        map.insert("GPDSP1", (MAGIC_GPDSP1, SS_ID_GPDSP1));
        map
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
    ss_id: ssr_ss_id,
    ssr_event: ssr_events,
    notify_fd: Arc<EventFd>,
}

impl VhSsrCtx {
    pub fn new(notify_fd: Arc<EventFd>) -> Self {
        VhSsrCtx {
            ss_id: 0,
            ssr_event: 0,
            notify_fd,
        }
    }

    pub fn write_and_notify(
        &mut self,
        ss_id: ssr_ss_id,
        ssr_event: ssr_events,
    ) -> Result<(), io::Error> {
        self.set_ctx(ss_id, ssr_event);
        self.notify_fd.write(0x10)
    }

    pub fn get_response(&self) -> Option<(String, ssr_events)> {
        if self.ss_id == 0 && self.ssr_event == 0 {
            log::warn!("self ss_id is 0, but get called!");
            return None;
        }
        let name = match self.ss_id {
            SS_ID_LPASS => "adsp",
            SS_ID_MODEM => "modem",
            SS_ID_SLPI => "slpi",
            SS_ID_CDSP => "cdsp",
            SS_ID_CDSP1 => "cdsp1",
            SS_ID_GPDSP0 => "gpdsp0",
            SS_ID_GPDSP1 => "gpdsp1",
            _ => "unknown client",
        }
        .to_string();
        Some((name, self.ssr_event))
    }

    pub fn reset(&mut self) {
        self.set_ctx(0, 0);
        // read notify_fd to avoid endless return in epoll_wait
        self.notify_fd.read().ok();
    }

    pub fn set_ctx(&mut self, id: u32, event: u32) {
        self.ss_id = id;
        self.ssr_event = event;
    }
}

pub trait SsrClient {
    /// Register client to SSR
    fn register(&self) -> Result<u64, SsrClientError>;

    /// Unregister client to SSR
    fn unregister(&self) -> Result<u64, SsrClientError>;

    /// trigger event for the client from SSR
    fn trigger(&self) -> Result<u64, SsrClientError>;

    /// new default client
    fn new_default(
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
    event_mask: u32,
    event_handler: cb_func_with_ctx_t,
    /* *mut *mut std::os::raw::c_void */
    priv_data: AtomicPtr<*mut ::std::os::raw::c_void>,
}

impl SsrVuClient {
    pub fn new(
        client_magic: u64,
        client_name: String,
        event_mask: u32,
        ctx: Arc<Mutex<VhSsrCtx>>,
        event_handler: cb_func_with_ctx_t,
        priv_data: AtomicPtr<*mut ::std::os::raw::c_void>,
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

    pub fn get_priv_ptr(&self) -> *mut *mut ::std::os::raw::c_void {
        self.priv_data.load(Ordering::Relaxed)
    }
}

impl SsrClient for SsrVuClient {
    fn trigger(&self) -> Result<u64, SsrClientError> {
        let priv_data = self.get_priv_ptr();
        unsafe {
            let ret = trigger_ssr(*priv_data, self.client_magic);
            match ret {
                0 => Ok(0),
                _ => Err(SsrClientError::TriggerSsr(self.client_name.clone())),
            }
        }
    }

    fn register(&self) -> Result<u64, SsrClientError> {
        let register_name = String::from("vhost-device-ssr:") + self.client_name.as_str();
        let name = CString::new(register_name.as_str())
            .unwrap_or_else(|_| panic!("New client name: {} failed", register_name));
        let priv_data = self.get_priv_ptr();
        let ctx_ptr =
            &mut *self.ctx.lock().unwrap() as *mut VhSsrCtx as *mut ::std::os::raw::c_void;
        unsafe {
            let ret = ssr_register_callback_events(
                self.client_magic,
                self.event_handler,
                self.event_mask,
                priv_data,
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
        let priv_data = self.get_priv_ptr();
        unsafe {
            let ret = ssr_unregister_callback(*priv_data);
            match ret {
                0 => Ok(0),
                _ => Err(SsrClientError::UnregisterCallbackEvents(
                    self.client_name.clone(),
                )),
            }
        }
    }

    fn new_default(
        client_name: String,
        ctx: Arc<Mutex<VhSsrCtx>>,
        event_handler: cb_func_with_ctx_t,
    ) -> Self {
        let client_magic = Client_Map.get(client_name.as_str()).unwrap().0;
        let client_id = Client_Map.get(client_name.as_str()).unwrap().1;
        let event_mask = (client_id << SS_ID_SHIFT)
            | (SSR_EVENT_FAULT_NOTIFY
                | SSR_EVENT_RESTART_START
                | SSR_EVENT_RESTART_FAILED
                | SSR_EVENT_PRE_DS
                | SSR_EVENT_DUMMY
                | SSR_EVENT_RESTART_COMPLETE);
        let mut ssr_handle: *mut ::std::os::raw::c_void = null_mut();
        let priv_data = AtomicPtr::new(&mut ssr_handle);
        let client = Self::new(
            client_magic,
            client_name,
            event_mask,
            ctx,
            event_handler,
            priv_data,
        );
        client.register().unwrap();
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
            assert_eq!(ctx.lock().unwrap().notify_fd.read().unwrap(), 16);
        }
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
                let cxt_unlocked = ctx.lock().expect("lock failed!");
                let x = cxt_unlocked.notify_fd.read().unwrap();
                let res = cxt_unlocked.get_response().unwrap();
                assert_eq!(x, 16 * 10);
                assert_eq!(res.0, "cdsp".to_string());
                assert_eq!(res.1, SSR_EVENT_FAULT_NOTIFY);
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
                        let cxt_unlocked = ctx_clone.lock().expect("lock failed!");
                        let x = cxt_unlocked.notify_fd.read().unwrap();
                        assert_eq!(x, 16);
                        let res = cxt_unlocked.get_response();
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
        let event_mask = (client_id << SS_ID_SHIFT)
            | (SSR_EVENT_FAULT_NOTIFY
                | SSR_EVENT_RESTART_START
                | SSR_EVENT_RESTART_FAILED
                | SSR_EVENT_PRE_DS
                | SSR_EVENT_DUMMY
                | SSR_EVENT_RESTART_COMPLETE);
        let mut ssr_handle: *mut ::std::os::raw::c_void = null_mut();
        let priv_data = AtomicPtr::new(&mut ssr_handle);
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
            &mut ssr_handle as *mut *mut ::std::os::raw::c_void
        )
    }

    #[test]
    fn verify_vh_ssr_ctx() {
        let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let mut ctx = VhSsrCtx::new(Arc::clone(&notify_fd));
        assert_eq!(ctx.ss_id, 0);
        assert_eq!(ctx.ssr_event, 0);
        assert_eq!(ctx.get_response(), None);

        ctx.set_ctx(SS_ID_CDSP, 1);
        assert_eq!(ctx.ss_id, SS_ID_CDSP);
        assert_eq!(ctx.ssr_event, 1);
        assert_eq!(ctx.get_response(), Some(("cdsp".to_string(), 1)));

        ctx.set_ctx(SS_ID_CDSP1, 1);
        assert_eq!(ctx.ss_id, SS_ID_CDSP1);
        assert_eq!(ctx.ssr_event, 1);
        assert_eq!(ctx.get_response(), Some(("cdsp1".to_string(), 1)));

        ctx.set_ctx(SS_ID_GPDSP0, 1);
        assert_eq!(ctx.ss_id, SS_ID_GPDSP0);
        assert_eq!(ctx.ssr_event, 1);
        assert_eq!(ctx.get_response(), Some(("gpdsp0".to_string(), 1)));

        ctx.set_ctx(SS_ID_GPDSP1, 1);
        assert_eq!(ctx.ss_id, SS_ID_GPDSP1);
        assert_eq!(ctx.ssr_event, 1);
        assert_eq!(ctx.get_response(), Some(("gpdsp1".to_string(), 1)));

        ctx.write_and_notify(2, 2).unwrap();
        assert_eq!(ctx.ss_id, 2);
        assert_eq!(ctx.ssr_event, 2);
        assert_eq!(ctx.notify_fd.read().ok(), Some(0x10));

        ctx.reset();
        // After reset, id and event are set to zero and value of notify_fd is 0, which return None
        assert_eq!(ctx.ss_id, 0);
        assert_eq!(ctx.ssr_event, 0);
        assert_eq!(ctx.notify_fd.read().ok(), None);
    }
}
