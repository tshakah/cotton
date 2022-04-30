use super::*;

use std::{
    io::Error,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use async_stream::stream;
use futures_util::stream;
use futures_util::Stream;

use neli::{
    consts::{
        nl::{NlmF, NlmFFlags},
        rtnl::{
            Arphrd, Ifa, IfaFFlags, Iff, IffFlags, Ifla, RtAddrFamily, Rtm,
        },
        socket::NlFamily,
    },
    err::DeError,
    err::SerError,
    err::WrappedError,
    nl::{NlPayload, Nlmsghdr},
    rtnl::Ifaddrmsg,
    rtnl::Ifinfomsg,
    socket::tokio::NlSocket,
    socket::NlSocketHandle,
    types::NlBuffer,
    types::RtBuffer,
};

fn ip(ip_bytes: &[u8]) -> Option<IpAddr> {
    match ip_bytes.len() {
        4 => Some(IpAddr::from(Ipv4Addr::from(
            u32::from_ne_bytes(ip_bytes.try_into().unwrap()).to_be(),
        ))),

        16 => Some(IpAddr::from(Ipv6Addr::from(
            u128::from_ne_bytes(ip_bytes.try_into().unwrap()).to_be(),
        ))),

        _ => {
            println!(
                "Unrecognized address length of {} found",
                ip_bytes.len()
            );
            None
        }
    }
}

fn map_rx_error(err: DeError) -> Error {
    if let DeError::Wrapped(WrappedError::IOError(io_error)) = err {
        io_error
    } else {
        Error::from(ErrorKind::Other)
    }
}

fn map_tx_error(err: SerError) -> Error {
    if let SerError::Wrapped(WrappedError::IOError(io_error)) = err {
        io_error
    } else {
        Error::from(ErrorKind::Other)
    }
}

fn translate_link_message(
    msg: &Nlmsghdr<Rtm, Ifinfomsg>,
) -> Option<NetworkEvent> {
    if let NlPayload::Payload(p) = &msg.nl_payload {
        let handle = p.rtattrs.get_attr_handle();
        let name = handle
            .get_attr_payload_as_with_len::<String>(Ifla::Ifname)
            .ok();
        if let Some(name) = name {
            let flags = &p.ifi_flags;
            let mut newflags = Flags::NONE;
            for (iff, newf) in [
                (&Iff::Up, Flags::UP),
                (&Iff::Running, Flags::RUNNING),
                (&Iff::Loopback, Flags::LOOPBACK),
                (&Iff::Pointopoint, Flags::POINTTOPOINT),
                (&Iff::Broadcast, Flags::BROADCAST),
                (&Iff::Multicast, Flags::MULTICAST),
            ] {
                if flags.contains(iff) {
                    newflags |= newf;
                }
            }
            match msg.nl_type {
                Rtm::Newlink => {
                    return Some(NetworkEvent::NewLink(
                        InterfaceIndex(p.ifi_index as u32),
                        name,
                        newflags,
                    ))
                }
                Rtm::Dellink => {
                    return Some(NetworkEvent::DelLink(InterfaceIndex(
                        p.ifi_index as u32,
                    )))
                }
                _ => (),
            }
        }
    }
    None
}

fn translate_addr_message(
    msg: &Nlmsghdr<Rtm, Ifaddrmsg>,
) -> Option<NetworkEvent> {
    if let NlPayload::Payload(p) = &msg.nl_payload {
        let handle = p.rtattrs.get_attr_handle();
        if let Some(addr) = handle
            .get_attr_payload_as_with_len::<&[u8]>(Ifa::Address)
            .ok()
            .and_then(ip)
        {
            match msg.nl_type {
                Rtm::Newaddr => {
                    return Some(NetworkEvent::NewAddr(
                        InterfaceIndex(p.ifa_index as u32),
                        addr,
                        p.ifa_prefixlen,
                    ))
                }
                Rtm::Deladdr => {
                    return Some(NetworkEvent::DelAddr(InterfaceIndex(
                        p.ifa_index as u32,
                    )))
                }
                _ => (),
            }
        }
    }
    None
}

fn get_links(
    mut ss: NlSocket,
) -> impl Stream<Item = Result<NetworkEvent, Error>> {
    let mut buffer = Vec::new();
    stream! {
        loop {
            let res: Result<NlBuffer<Rtm, Ifinfomsg>, DeError> =
                ss.recv(&mut buffer).await;
            match res {
                Ok(msgs) =>
                    for msg in msgs {
                        if let Some(event) = translate_link_message(&msg) {
                            yield Ok(event);
                        }
                    },
                Err(e) => yield Err(map_rx_error(e))
            }
        }
    }
}

fn get_addrs(
    mut ss: NlSocket,
) -> impl Stream<Item = Result<NetworkEvent, Error>> {
    let mut buffer = Vec::new();
    stream! {
        loop {
            let res: Result<NlBuffer<Rtm, Ifaddrmsg>, DeError> =
                ss.recv(&mut buffer).await;
            match res {
                Ok(msgs) =>
                    for msg in msgs {
                        if let Some(event) = translate_addr_message(&msg) {
                            yield Ok(event);
                        }
                    },
                Err(e) => yield Err(map_rx_error(e))
            }
        }
    }
}

pub async fn network_interfaces_dynamic(
) -> Result<impl Stream<Item = Result<NetworkEvent, Error>>, Error> {
    /* Group constants from <linux/rtnetlink.h> not wrapped by neli 0.6.1:
     *  1 = RTNLGRP_LINK (link events)
     *  5 = RTNLGRP_IPV4_IFADDR (ipv4 events)
     *  9 = RTNLGRP_IPV6_IFADDR (ipv6 events) <-- needs a 3rd socket?
     */
    let link_handle = NlSocketHandle::connect(NlFamily::Route, None, &[1])?;
    let mut link_socket = NlSocket::new(link_handle)?;
    let addr_handle = NlSocketHandle::connect(NlFamily::Route, None, &[5])?;
    let mut addr_socket = NlSocket::new(addr_handle)?;

    let ifinfomsg = Ifinfomsg::new(
        RtAddrFamily::Unspecified,
        Arphrd::Ether,
        0,
        IffFlags::empty(),
        IffFlags::empty(),
        RtBuffer::new(),
    );
    let nl_link_header = Nlmsghdr::new(
        None,
        Rtm::Getlink,
        NlmFFlags::new(&[NlmF::Request, NlmF::Match]),
        None,
        None,
        NlPayload::Payload(ifinfomsg),
    );

    link_socket
        .send(&nl_link_header)
        .await
        .map_err(map_tx_error)?;

    let ifaddrmsg = Ifaddrmsg {
        ifa_family: RtAddrFamily::Inet,
        ifa_prefixlen: 0,
        ifa_flags: IfaFFlags::empty(),
        ifa_scope: 0,
        ifa_index: 0,
        rtattrs: RtBuffer::new(),
    };
    let nl_addr_header = Nlmsghdr::new(
        None,
        Rtm::Getaddr,
        NlmFFlags::new(&[NlmF::Request, NlmF::Root]),
        None,
        None,
        NlPayload::Payload(ifaddrmsg),
    );

    addr_socket
        .send(&nl_addr_header)
        .await
        .map_err(map_tx_error)?;

    Ok(stream::select(
        Box::pin(get_links(link_socket)),
        Box::pin(get_addrs(addr_socket)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_4byte_addr() {
        let input = [192u8, 168u8, 0u8, 200u8];

        let result = ip(&input);

        assert_eq!(result, "192.168.0.200".parse().ok());
    }

    #[test]
    fn parse_16byte_addr() {
        let input = [0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

        let result = ip(&input);

        assert_eq!(result, "::1".parse().ok());
    }

    #[test]
    fn no_parse_5byte_addr() {
        let input = [2u8, 3, 4, 5, 6];

        let result = ip(&input);

        assert_eq!(result, None);
    }
}
