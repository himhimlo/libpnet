// Copyright (c) 2014-2016 Robert Clipsham <robert@octarineparrot.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Support for sending and receiving data link layer packets using the WinPcap library.

use crate::bindings::winpcap::{inet_ntop, AF_UNSPEC, GAA_FLAG_INCLUDE_PREFIX, SOCKET_ADDRESS};

use super::bindings::{bpf, winpcap};
use super::{DataLinkReceiver, DataLinkSender, MacAddr, NetworkInterface};

use ipnetwork::IpNetwork;
use pnet_sys::{AF_INET, AF_INET6};
use winapi::shared::winerror::NO_ERROR;
use winapi::shared::ws2def::SOCKADDR_IN;
use winapi::shared::ws2ipdef::SOCKADDR_IN6;
use winapi::um::winsock2;

use std::cmp;
use std::collections::VecDeque;
use std::ffi::{c_void, CStr, CString, OsString};
use std::io;
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::slice;
use std::str::from_utf8_unchecked;
use std::sync::Arc;
use winapi::ctypes::c_char;

use winapi::ctypes;

struct WinPcapAdapter {
    adapter: winpcap::LPADAPTER,
}

impl Drop for WinPcapAdapter {
    fn drop(&mut self) {
        unsafe {
            winpcap::PacketCloseAdapter(self.adapter);
        }
    }
}

struct WinPcapPacket {
    packet: winpcap::LPPACKET,
}

impl Drop for WinPcapPacket {
    fn drop(&mut self) {
        unsafe {
            winpcap::PacketFreePacket(self.packet);
        }
    }
}

/// The WinPcap's specific configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Config {
    /// The size of buffer to use when writing packets. Defaults to 4096.
    pub write_buffer_size: usize,

    /// The size of buffer to use when reading packets. Defaults to 4096.
    pub read_buffer_size: usize,
}

impl<'a> From<&'a super::Config> for Config {
    fn from(config: &super::Config) -> Config {
        Config {
            write_buffer_size: config.write_buffer_size,
            read_buffer_size: config.read_buffer_size,
        }
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            write_buffer_size: 4096,
            read_buffer_size: 4096,
        }
    }
}

/// Create a datalink channel using the WinPcap library.
#[inline]
pub fn channel(network_interface: &NetworkInterface, config: Config) -> io::Result<super::Channel> {
    let mut read_buffer = Vec::new();
    read_buffer.resize(config.read_buffer_size, 0u8);

    let mut write_buffer = Vec::new();
    write_buffer.resize(config.write_buffer_size, 0u8);

    let adapter = unsafe {
        let net_if_str = CString::new(network_interface.name.as_bytes()).unwrap();
        winpcap::PacketOpenAdapter(net_if_str.as_ptr() as *mut ctypes::c_char)
    };
    if adapter.is_null() {
        return Err(io::Error::last_os_error());
    }

    let ret = unsafe { winpcap::PacketSetHwFilter(adapter, winpcap::NDIS_PACKET_TYPE_PROMISCUOUS) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    // Set kernel buffer size
    let ret = unsafe { winpcap::PacketSetBuff(adapter, config.read_buffer_size as ctypes::c_int) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    // Immediate mode
    let ret = unsafe { winpcap::PacketSetMinToCopy(adapter, 1) };
    if ret == 0 {
        return Err(io::Error::last_os_error());
    }

    let read_packet = unsafe { winpcap::PacketAllocatePacket() };
    if read_packet.is_null() {
        unsafe {
            winpcap::PacketCloseAdapter(adapter);
        }
        return Err(io::Error::last_os_error());
    }

    unsafe {
        winpcap::PacketInitPacket(
            read_packet,
            read_buffer.as_mut_ptr() as winpcap::PVOID,
            config.read_buffer_size as winpcap::UINT,
        )
    }

    let write_packet = unsafe { winpcap::PacketAllocatePacket() };
    if write_packet.is_null() {
        unsafe {
            winpcap::PacketFreePacket(read_packet);
            winpcap::PacketCloseAdapter(adapter);
        }
        return Err(io::Error::last_os_error());
    }

    unsafe {
        winpcap::PacketInitPacket(
            write_packet,
            write_buffer.as_mut_ptr() as winpcap::PVOID,
            config.write_buffer_size as winpcap::UINT,
        )
    }

    let adapter = Arc::new(WinPcapAdapter { adapter: adapter });
    let sender = Box::new(DataLinkSenderImpl {
        adapter: adapter.clone(),
        _write_buffer: write_buffer,
        packet: WinPcapPacket {
            packet: write_packet,
        },
    });
    let receiver = Box::new(DataLinkReceiverImpl {
        adapter: adapter,
        _read_buffer: read_buffer,
        packet: WinPcapPacket {
            packet: read_packet,
        },
        // Enough room for minimally sized packets without reallocating
        packets: VecDeque::with_capacity(unsafe { (*read_packet).Length } as usize / 64),
    });
    Ok(super::Channel::Ethernet(sender, receiver))
}

struct DataLinkSenderImpl {
    adapter: Arc<WinPcapAdapter>,
    _write_buffer: Vec<u8>,
    packet: WinPcapPacket,
}

impl DataLinkSender for DataLinkSenderImpl {
    #[inline]
    fn build_and_send(
        &mut self,
        num_packets: usize,
        packet_size: usize,
        func: &mut dyn FnMut(&mut [u8]),
    ) -> Option<io::Result<()>> {
        let len = num_packets * packet_size;
        if len >= unsafe { (*self.packet.packet).Length } as usize {
            None
        } else {
            let min = unsafe { cmp::min((*self.packet.packet).Length as usize, len) };
            let slice: &mut [u8] =
                unsafe { slice::from_raw_parts_mut((*self.packet.packet).Buffer as *mut u8, min) };
            for chunk in slice.chunks_mut(packet_size) {
                func(chunk);

                // Make sure the right length of packet is sent
                let old_len = unsafe { (*self.packet.packet).Length };
                unsafe {
                    (*self.packet.packet).Length = packet_size as u32;
                }

                let ret = unsafe {
                    winpcap::PacketSendPacket(self.adapter.adapter, self.packet.packet, 0)
                };

                unsafe {
                    (*self.packet.packet).Length = old_len;
                }

                if ret == 0 {
                    return Some(Err(io::Error::last_os_error()));
                }
            }
            Some(Ok(()))
        }
    }

    #[inline]
    fn send_to(&mut self, packet: &[u8], _dst: Option<NetworkInterface>) -> Option<io::Result<()>> {
        self.build_and_send(1, packet.len(), &mut |eh: &mut [u8]| {
            eh.copy_from_slice(packet);
        })
    }
}

unsafe impl Send for DataLinkSenderImpl {}
unsafe impl Sync for DataLinkSenderImpl {}

struct DataLinkReceiverImpl {
    adapter: Arc<WinPcapAdapter>,
    _read_buffer: Vec<u8>,
    packet: WinPcapPacket,
    packets: VecDeque<(usize, usize)>,
}

unsafe impl Send for DataLinkReceiverImpl {}
unsafe impl Sync for DataLinkReceiverImpl {}

impl DataLinkReceiver for DataLinkReceiverImpl {
    fn next(&mut self) -> io::Result<&[u8]> {
        // NOTE Most of the logic here is identical to FreeBSD/OS X
        while self.packets.is_empty() {
            let ret = unsafe {
                winpcap::PacketReceivePacket(self.adapter.adapter, self.packet.packet, 0)
            };
            let buflen = match ret {
                0 => return Err(io::Error::last_os_error()),
                _ => unsafe { (*self.packet.packet).ulBytesReceived as isize },
            };
            let mut ptr = unsafe { (*self.packet.packet).Buffer  as *mut c_char};
            let end = unsafe { ((*self.packet.packet).Buffer as *mut c_char).offset(buflen) };
            while ptr < end {
                unsafe {
                    let packet: *const bpf::bpf_hdr = mem::transmute(ptr);
                    let start = ptr as isize + (*packet).bh_hdrlen as isize
                        - (*self.packet.packet).Buffer as isize;
                    self.packets
                        .push_back((start as usize, (*packet).bh_caplen as usize));
                    let offset = (*packet).bh_hdrlen as isize + (*packet).bh_caplen as isize;
                    ptr = ptr.offset(bpf::BPF_WORDALIGN(offset));
                }
            }
        }
        let (start, len) = self.packets.pop_front().unwrap();
        let slice = unsafe {
            let data = (*self.packet.packet).Buffer as usize + start;
            slice::from_raw_parts(data as *const u8, len)
        };
        Ok(slice)
    }
}

/// Get a list of available network interfaces for the current machine.
pub fn interfaces() -> Vec<NetworkInterface> {
    // use super::bindings::winpcap;

    let family = AF_UNSPEC as u32;
    let flags = GAA_FLAG_INCLUDE_PREFIX;
    let mut adapters_size: u32 = 0;

    // Call once with adapters_size = 0 to get the actual adapters_size first
    unsafe {
        winpcap::GetAdaptersAddresses(
            family,
            flags, 
            std::ptr::null_mut(), 
            std::ptr::null_mut(), 
            &mut adapters_size);
    }

    let mut vec_size = adapters_size / mem::size_of::<winpcap::IP_ADAPTER_ADDRESSES>() as u32;
    if adapters_size % mem::size_of::<winpcap::IP_ADAPTER_ADDRESSES>() as u32 != 0 {
        vec_size += 1;
    }
    let mut adapters = Vec::with_capacity(vec_size as usize);

    let dw_ret_val = unsafe {
        winpcap::GetAdaptersAddresses(
            family,
            flags, 
            std::ptr::null_mut(), 
            adapters.as_mut_ptr(), 
            &mut adapters_size)
    };

    if dw_ret_val != NO_ERROR {
        panic!("Unable to call GetAdaptersAddresses (dw_ret_val = {dw_ret_val})");
    }

    unsafe { adapters.set_len(vec_size as usize); }

    // Create a complete list of NetworkInterfaces for the machine
    let mut cursor = adapters.as_mut_ptr();
    let mut all_ifaces = Vec::with_capacity(vec_size as usize);
    while !cursor.is_null() {
        let mac = unsafe {
            MacAddr(
                (*cursor).PhysicalAddress[0],
                (*cursor).PhysicalAddress[1],
                (*cursor).PhysicalAddress[2],
                (*cursor).PhysicalAddress[3],
                (*cursor).PhysicalAddress[4],
                (*cursor).PhysicalAddress[5],
            )
        };
        let mut ip_cursor = unsafe { (*cursor).FirstUnicastAddress as winpcap::PIP_ADAPTER_UNICAST_ADDRESS };
        let mut ips = Vec::new();
        while !ip_cursor.is_null() {
            if let Ok(ip_network) = parse_ip_network(unsafe { &(*ip_cursor).Address }, unsafe { (*ip_cursor).OnLinkPrefixLength }) {
                ips.push(ip_network);
            }
            ip_cursor = unsafe { (*ip_cursor).Next };
        }

        unsafe {
            let name_str_ptr = (*cursor).AdapterName as *const i8;

            let bytes = CStr::from_ptr(name_str_ptr).to_bytes();
            let name_str = from_utf8_unchecked(bytes).to_owned();
            
            let description_str_ptr = (*cursor).Description;
            let len = libc::wcslen(description_str_ptr);
            let bytes = &*std::ptr::slice_from_raw_parts(description_str_ptr, len);
            let description_str = OsString::from_wide(bytes).into_string().unwrap();

            all_ifaces.push(NetworkInterface {
                name: name_str,
                description: description_str,
                index: (*cursor).IfIndex,
                mac: Some(mac),
                ips: ips,
                flags: (*cursor).Flags,
            });

            cursor = (*cursor).Next;
        }
    }

    let mut buf = vec![0u8; 4096];
    let mut buflen = buf.len() as u32;

    if unsafe { winpcap::PacketGetAdapterNames(buf.as_mut_ptr() as *mut i8, &mut buflen) } == 0 {
        buf.resize(buflen as usize, 0);

        // Second call should now work with the correct buffer size. If not, this may be
        // due to some privilege or other unforeseen issue.
        if unsafe { winpcap::PacketGetAdapterNames(buf.as_mut_ptr() as *mut i8, &mut buflen) } == 0
        {
            panic!("Unable to get interface list despite increasing buffer size");
        }
    }

    let buf_str = unsafe { from_utf8_unchecked(&buf) };
    let iface_names = buf_str.split("\0\0").next();
    let mut vec = Vec::new();

    // Return only supported adapters
    match iface_names {
        Some(iface_names) => {
            for iface in iface_names.split('\0') {
                let name = iface.to_owned();
                let next = all_ifaces
                    .iter()
                    .filter(|x| name[..].ends_with(&x.name[..]))
                    .next();
                if next.is_some() {
                    let mut iface = next.unwrap().clone();
                    iface.name = name;
                    vec.push(iface);
                }
            }
        }
        None => (),
    };

    vec
}

fn parse_ip_network(address: *const SOCKET_ADDRESS, prefix: u8) -> Result<IpNetwork, ()> {
    let socket_address = unsafe { (*address).lpSockaddr };
    match  unsafe { (*socket_address).sa_family } as i32 {
        AF_INET => {
            let ip_str_ptr = unsafe { winsock2::inet_ntoa((*(socket_address as *const SOCKADDR_IN)).sin_addr) }; 
            let ip_bytes = unsafe { CStr::from_ptr(ip_str_ptr).to_bytes() };
            let ip_str = unsafe { from_utf8_unchecked(ip_bytes).to_owned() };
            let ip = ip_str.parse().map_err(|_| ())?;
            
            IpNetwork::new(ip, prefix).map_err(|_| ())
        },
        AF_INET6 => {
            let mut ip_buf = [0; 46];
            let sa = &unsafe { *(socket_address as *const SOCKADDR_IN6) }.sin6_addr;
            let sa = sa as *const _;
            unsafe { inet_ntop(AF_INET6, sa as *const c_void, ip_buf.as_mut_ptr(), 46) }; 
            let ip_str = unsafe { CStr::from_ptr(ip_buf.as_ptr()) };
            let ip_str = ip_str.to_str().map_err(|_| ())?;
            let ip = ip_str.parse().map_err(|_| ())?;

            IpNetwork::new(ip, prefix).map_err(|_| ())
        },
        _ => unreachable!(),
    }
}
