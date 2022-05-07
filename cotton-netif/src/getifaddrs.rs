use super::*;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use nix::ifaddrs;
use nix::net::if_::InterfaceFlags;

use std::{
    net::{IpAddr, Ipv4Addr},
};

/** Obtain the current list of network interfaces

The supplied function will be called with a sequence of [NetworkEvent]
objects, each describing a network interface (as
[NetworkEvent::NewLink]) or an address on that interface (as
[NetworkEvent::NewAddr]). An interface may have several addresses,
both IPv4 and IPv6. In all cases, the [NetworkEvent::NewLink] event
describing an interface, will be generated before that interface's
[NetworkEvent::NewAddr] event or events.

As the list is a snapshot of the current state, no [NetworkEvent::DelLink]
or [NetworkEvent::DelAddr] events will be generated.

For a simple listing of the returned information, just use println:

```rust
# use cotton_netif::*;
get_interfaces(|e| println!("{:?}", e))?;
# Ok::<(), std::io::Error>(())
```

As another example, here is how to list all available
multicast-capable interfaces:

```rust
# use cotton_netif::*;
get_interfaces(|e| match e {
    NetworkEvent::NewLink(_i, name, flags) => {
        if flags.contains(Flags::RUNNING | Flags::UP | Flags::MULTICAST) {
            println!("New multicast-capable interface: {}", name);
        }
    },
    _ => {},
})?;
# Ok::<(), std::io::Error>(())
```

 */
pub fn get_interfaces<FN>(mut callback: FN) -> Result<(), std::io::Error>
    where FN: FnMut(NetworkEvent)
{
    let addrs = ifaddrs::getifaddrs()?;
    let mut next_index = 1u32;
    let mut index_map = HashMap::new();
    for ifaddr in addrs {
        /* Undo Linux aliasing: "eth0:1" is "eth0" really.
         */
        let name = match ifaddr.interface_name.split_once(":") {
            None => ifaddr.interface_name,
            Some((prefix, _alias)) => prefix.to_string(),
        };

        let index = match index_map.entry(name) {
            Entry::Occupied(e) => *e.get(),
            Entry::Vacant(e) => {
                let flags = ifaddr.flags;
                let mut newflags = Default::default();
                for (iff, newf) in [
                    (InterfaceFlags::IFF_UP, Flags::UP),
                    (InterfaceFlags::IFF_RUNNING, Flags::RUNNING),
                    (InterfaceFlags::IFF_LOOPBACK, Flags::LOOPBACK),
                    (InterfaceFlags::IFF_POINTOPOINT, Flags::POINTTOPOINT),
                    (InterfaceFlags::IFF_BROADCAST, Flags::BROADCAST),
                    (InterfaceFlags::IFF_MULTICAST, Flags::MULTICAST),
                ] {
                    if flags.contains(iff) {
                        newflags |= newf;
                    }
                }

                let index = next_index;
                next_index += 1;
                callback(
                    NetworkEvent::NewLink(InterfaceIndex(index),
                                          e.key().clone(),
                                          newflags)
                );
                e.insert(index);
                index
            }
        };

        if let (Some(addr), Some(mask)) = (ifaddr.address, ifaddr.netmask) {
            if let Some(ipv4) = addr.as_sockaddr_in() {
                let ip = IpAddr::from(Ipv4Addr::from(ipv4.ip()));
                if let Some(netmask) = mask.as_sockaddr_in() {
                    callback(NetworkEvent::NewAddr(
                        InterfaceIndex(index),
                        ip,
                        netmask.ip().leading_ones() as u8)
                    );
                }
            } else if let Some(ipv6) = addr.as_sockaddr_in6() {
                if let Some(netmask) = mask.as_sockaddr_in6() {
                    callback(NetworkEvent::NewAddr(
                        InterfaceIndex(index),
                        IpAddr::from(ipv6.ip()),
                        u128::from_be_bytes(
                            netmask.as_ref().sin6_addr.s6_addr)
                            .leading_ones() as u8
                    ));
                }
            }
        }
    }
    Ok(())
}
