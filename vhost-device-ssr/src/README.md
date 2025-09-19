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
          names for ssr client, support CDSP CDSP1 CDSP2 CDSP3 ADSP ADSP1 ADSP2 SLPI GPDSP0 GPDSP1 HPASSC0 HPASSC1 HPASSC2
          ADSP1, ADSP2, CDSP2, CDSP3, HPASSC0, HPASSC1 and HPASSC2 are only applicable on SA8797.
  -h, --help
          Print help
  -V, --version
          Print version
```

## Examples

The daemon should be started first:

```shell
host# vhost-device-ssr --socket-path /some/path/ssr.sock    \
      --clients-groups CDSP,CDSP1;GPDSP0,GPDSP1
```

Note that from the above command the socket path "/some/path/ssr.sock0" and
"/some/path/ssr.sock1" will be created for  CDSP,CDSP1 and  GPDSP0 GPDSP1
respectively. Use `;` as delimiter.

Support CDSP, CDSP1/2/3, ADSP, ADSP1/2 SLPI, GPDSP0/1, HPASSC0/1/2.
Note: ADSP1/2, CDSP2/3 and HPASSC0/1/2 are only applicable on SA8797.

## License

This project is licensed under
- [BSD-3-Clause License](https://opensource.org/licenses/BSD-3-Clause)
