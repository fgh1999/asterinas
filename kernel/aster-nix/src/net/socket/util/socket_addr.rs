// SPDX-License-Identifier: MPL-2.0

use crate::{
    net::socket::{
        ip::{Ipv4Address, PortNum},
        vsock::addr::VsockSocketAddr,
    },
    prelude::*,
};

#[derive(Debug, PartialEq, Eq)]
pub enum SocketAddr {
    Unix,
    IPv4(Ipv4Address, PortNum),
    Vsock(VsockSocketAddr),
}
