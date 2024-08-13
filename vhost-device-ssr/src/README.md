// Copyright (c) 2024 Qualcomm Innovation Center, Inc. All rights reserved.
// SPDX-License-Identifier: BSD-3-Clause-Clear
# vhost-device-ssr

This program is a vhost-user backend that emulates a Virtio SSR Proxy.

The program will register the client through ssr-client api, and then the event
can be transported into FE through virtio.


## Synopsis
vhost-device-ssr --socket-path <SOCKET> --clients-groups <CLIENTS_GROUPS>

## Options
```text
Options:
  -s, --socket-path <SOCKET>
          Location of vhost-user Unix domain socket
  -c, --clients-groups <CLIENTS_GROUPS>
          names for ssr client, only support CDSP CDSP0 CDSP1 LPASS SLPI GPDSP0 GPDSP1
  -h, --help
          Print help
  -V, --version
          Print version
```

## Examples

The daemon should be started first:

```shell
host# vhost-device-ssr --socket-path /some/path/ssr.sock    \
      --clients-groups CDSP0,CDSP1;GPDSP0,GPDSP1
```

Note that from the above command the socket path "/some/path/ssr.sock0" and
"/some/path/ssr.sock1" will be created for  CDSP0,CDSP1 and  GPDSP0 GPDSP1
respectively. Use `;` as delimiter.

Now only support CDSP CDSP0 CDSP1 LPASS SLPI GPDSP0 GPDSP1

## License

This project is licensed under
- [BSD-3-Clause License](https://opensource.org/licenses/BSD-3-Clause)
