// Copyright (c) 2024 Qualcomm Innovation Center, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause-Clear

// Copyright 2023 Linaro Ltd. All Rights Reserved.
// Leo Yan <leo.yan@linaro.org>
//
// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause
mod ssr_client;
mod vhu_ssr;

use clap::Parser;
use log::error;
use ssr_client::{ssr_virtio_event_handler, Client_Map, SsrClient, SsrVuClient, VhSsrCtx};
use vhost::vhost_user::Listener;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::exit;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use thiserror::Error as ThisError;
use vhost_device_ssr::ssr_clients_bindings::ssr_api::cb_func_with_ctx_t;
use vhost_user_backend::VhostUserDaemon;
use vhu_ssr::{VuSsrBackend, SSR_EVENT_IN_VRING_EPOLL};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

#[derive(Debug, ThisError)]
/// Errors related to low level ssr helpers
pub(crate) enum Error {
    #[error("Could not create backend: {0}")]
    CouldNotCreateBackend(std::io::Error),
    #[error("Could not create daemon: {0}")]
    CouldNotCreateDaemon(vhost_user_backend::Error),
    #[error("Could not register client client into vring epoll")]
    CouldNotRegisterNotifyEvent,
    #[error("Fatal error: {0}")]
    ServeFailed(vhost_user_backend::Error),
    #[error("Thread `{0}` panicked")]
    ThreadPanic(String, Box<dyn Any + Send>),
}
type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, Parser, Debug, PartialEq)]
#[clap(author, version, about, long_about = None)]
struct SsrArgs {
    /// Location of vhost-user Unix domain socket.
    #[clap(short, long, value_name = "SOCKET")]
    socket_path: PathBuf,

    /// names for ssr client,
    /// support CDSP CDSP1 CDSP2 CDSP3 ADSP ADSP1 ADSP2 SLPI GPDSP0 GPDSP1
    /// ADSP1,ADSP2, CDSP2 and CDSP3 are only applicable on SA8797.
    #[clap(
        short = 'c',
        long,
        use_value_delimiter = true,
        value_delimiter = ';',
        required = true
    )]
    clients_groups: Vec<String>,

    /// Enable sd_notify
    #[clap(
        short,
        long,
        default_value_t = false
    )]
    enable_sd_notify: bool,
}

impl SsrArgs {
    pub fn generate_socket_paths(&self) -> Vec<PathBuf> {
        let socket_file_name = self
            .socket_path
            .file_name()
            .expect("socket_path has no filename.");
        let socket_file_parent = self
            .socket_path
            .parent()
            .expect("socket_path has no parent directory.");

        let make_socket_path = |i: usize| -> PathBuf {
            let mut file_name = socket_file_name.to_os_string();
            file_name.push(std::ffi::OsStr::new(&i.to_string()));
            socket_file_parent.join(&file_name)
        };

        (0..self.clients_groups.len())
            .map(make_socket_path)
            .collect()
    }
}

// This is the public API through which an external program starts the
/// vhost-device-ssr backend server.

pub(crate) fn start_backend_server<D: 'static + SsrClient + Send + Sync>(
    socket: PathBuf,
    clients_list: Vec<String>,
    enable_sd_notify: bool,
) -> Result<()> {
    let notify_fd = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
    let ssr_test_callback: cb_func_with_ctx_t = Some(ssr_virtio_event_handler);
    let ctx = Arc::new(Mutex::new(VhSsrCtx::new(Arc::clone(&notify_fd))));

    let ssr_vu_clients = Arc::new(
        clients_list
            .iter()
            .map(|c| D::new_default(c.to_string(), Arc::clone(&ctx), ssr_test_callback))
            .collect(),
    );

    loop {
        let vu_ssr_backend = Arc::new(RwLock::new(
            VuSsrBackend::new(Arc::clone(&ssr_vu_clients), Arc::clone(&ctx))
                .map_err(Error::CouldNotCreateBackend)?,
        ));
        let mut daemon = VhostUserDaemon::new(
            String::from("vhost-device-ssr-backend"),
            Arc::clone(&vu_ssr_backend),
            GuestMemoryAtomic::new(GuestMemoryMmap::new()),
        )
        .map_err(Error::CouldNotCreateDaemon)?;

        let handlers = daemon.get_epoll_handlers();
        handlers[0]
            .register_listener(notify_fd.as_raw_fd(), EventSet::IN, SSR_EVENT_IN_VRING_EPOLL as u64)
            .map_err(|_| Error::CouldNotRegisterNotifyEvent)?;

        let listener = Listener::new(&socket, true).map_err(vhost_user_backend::Error::CreateBackendListener).unwrap();

        // Notify to systemd once unix_sock is ready to listen
        if enable_sd_notify {
            sd_notify::notify(true, &[sd_notify::NotifyState::Ready]).expect("Failed to send ready notification");
        }

        daemon.start(listener).unwrap();
        let result = daemon.wait();

        // Regardless of the result, we want to signal worker threads to exit
        handlers[0].send_exit_event();

        // For this convenience function we are not treating certain "expected"
        // outcomes as error. Disconnects and partial messages can be usual
        // behaviour seen from quitting guests.
        let err = match &result {
            Err(e) => match e {
                vhost_user_backend::Error::HandleRequest(vhost::vhost_user::Error::Disconnected) => Ok(()),
                vhost_user_backend::Error::HandleRequest(vhost::vhost_user::Error::PartialMessage) =>  Ok(()),
                _ => return result.map_err(Error::ServeFailed),
            },
            _ => return result.map_err(Error::ServeFailed),
        };

        if let Err(e) = err {
            log::error!("Error serving daemon: {}", e);
            vu_ssr_backend.read().unwrap().unregister_clients();
            return Err(e);
        }
    }
}

pub(crate) fn start_backend<D: 'static + SsrClient + Send + Sync>(args: SsrArgs) -> Result<()> {
    let mut handles = HashMap::new();
    let enable_sd_notify = args.enable_sd_notify;
    let (senders, receiver) = std::sync::mpsc::channel();
    for (thread_id, (socket, clients_group)) in args
        .generate_socket_paths()
        .into_iter()
        .zip(args.clients_groups.iter().cloned())
        .enumerate()
    {
        let name = format!("vhu-vsock-ssr-{:?}", clients_group);
        let clients_list: Vec<String> = clients_group
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|name| Client_Map.contains_key(name.as_str()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        let sender = senders.clone();
        let handle = thread::Builder::new()
            .name(name.clone())
            .spawn(move || {
                let result = std::panic::catch_unwind(move || {
                    start_backend_server::<D>(socket, clients_list, enable_sd_notify)
                });

                // Notify the main thread that we are done.
                sender.send(thread_id).unwrap();

                result.map_err(|e| Error::ThreadPanic(name, e))?
            })
            .unwrap();
        handles.insert(thread_id, handle);
    }

    while !handles.is_empty() {
        let thread_id = receiver.recv().unwrap();
        handles
            .remove(&thread_id)
            .unwrap()
            .join()
            .map_err(std::panic::resume_unwind)
            .unwrap()?;
    }
    Ok(())
}

fn main() {
    env_logger::init();

    if let Err(e) = start_backend::<SsrVuClient>(SsrArgs::parse()) {
        error!("{e}");
        exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn verify_generate_socket_paths() {
        let args = SsrArgs {
            socket_path: PathBuf::from("/some/socket_path"),
            clients_groups: vec![String::from("CDSP,CDSP1"), String::from("GPDSP0,GPDSP1")],
            enable_sd_notify: false
        };
        let paths = args.generate_socket_paths();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/some/socket_path0"),
                PathBuf::from("/some/socket_path1"),
            ]
        );
    }

    #[test]
    fn verify_cmd_line_arguments() {
        // All parameters have default values, except for the socket path.  White spaces are
        // introduced on purpose to make sure Strings are trimmed properly.
        let default_args: SsrArgs = Parser::parse_from([
            "",
            "--socket-path=/some/socket_path",
            "--clients-groups=CDSP;GPDSP0,GPDSP1",
        ]);

        // A valid configuration that should be equal to the above default configuration.
        let args = SsrArgs {
            socket_path: PathBuf::from("/some/socket_path"),
            clients_groups: vec![String::from("CDSP"), String::from("GPDSP0,GPDSP1")],
            enable_sd_notify: false
        };

        // All configuration elements should be what we expect them to be.  Using
        // VuSsrConfig::try_from() ensures that strings have been properly trimmed.
        assert_eq!(default_args, args);

        // Test short arguments
        let default_args: SsrArgs =
            Parser::parse_from(["", "-s=/some/socket_path", "-c=CDSP;GPDSP0,GPDSP1"]);

        assert_eq!(default_args, args);
    }

    #[test]
    fn verify_support_client_arguments() {
        let clients_group = String::from("CDSP,CDSP1,ADSP,GPDSP0,GPDSP1,A,B,C");
        let clients_list: Vec<String> = clients_group
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|name| Client_Map.contains_key(name.as_str()))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let expected_val = vec![
            String::from("CDSP"),
            String::from("CDSP1"),
            String::from("ADSP"),
            String::from("GPDSP0"),
            String::from("GPDSP1"),
        ];
        assert_eq!(
            clients_list.into_iter().collect::<HashSet<_>>(),
            expected_val.into_iter().collect::<HashSet<_>>()
        );
    }
}
