#![no_std]
#![no_main]

use aya_ebpf::{
    bindings::xdp_action,
    macros::{map, xdp},
    maps::{Array, HashMap, PerCpuHashMap},
    programs::XdpContext,
};
use aya_log_ebpf::{debug, error, warn, info, trace};

use validator_firewall_common::RuntimeControls;

use core::mem;
use network_types::{
    eth::{EthHdr, EtherType},
    ip::{IpProto, Ipv4Hdr},
    udp::UdpHdr,
};

//These are our data structures that we use to communicate with userspace
#[map(name = "hvf_allow_list")]
static LEADER_SLOT_ALLOW_LIST: HashMap<u32, u8> = HashMap::<u32, u8>::with_max_entries(8192, 0);
#[map(name = "hvf_always_allow")]
static FULL_SCHEDULE_ALLOW_LIST: HashMap<u32, u8> = HashMap::<u32, u8>::with_max_entries(8192, 0);
#[map(name = "hvf_all_ip_stats")]
static ALL_TRAFFIC_STATS: PerCpuHashMap<u32, u64> =
    PerCpuHashMap::<u32, u64>::with_max_entries(16384, 0);
#[map(name = "hvf_blocked_ip_stats")]
static BLOCKED_TRAFFIC_STATS: PerCpuHashMap<u32, u64> =
    PerCpuHashMap::<u32, u64>::with_max_entries(16384, 0);
#[map(name = "hvf_protected_ports")]
static PROTECTED_PORTS: HashMap<u16, u8> = HashMap::<u16, u8>::with_max_entries(1024, 0);

#[map(name = "hvf_cnc")]
static CNC: Array<RuntimeControls> = Array::<RuntimeControls>::with_max_entries(1, 0);

#[xdp]
pub fn validator_firewall(ctx: XdpContext) -> u32 {
    let cnc = match CNC.get(0) {
        Some(cnc) => cnc,
        None => {
            warn!(&ctx, "No CNC data found, using defaults");
            &RuntimeControls{ global_enabled: true, close_to_leader: true }
        }
    };

    if cnc.global_enabled {
        match try_process_packet(&ctx, cnc.close_to_leader) {
            Ok(ret) => ret,
            Err(_) => {
                error!(&ctx, "Error processing packet!");
                xdp_action::XDP_PASS
            }
        }
    } else {
        xdp_action::XDP_PASS
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { core::hint::unreachable_unchecked() }
}

//Hide some unsafe blocks
#[inline(always)]
fn is_allow_listed(address: u32, close_to_leader: bool) -> bool {
    return if close_to_leader {
        unsafe { LEADER_SLOT_ALLOW_LIST.get(&address).is_some() }
    } else {
        unsafe { FULL_SCHEDULE_ALLOW_LIST.get(&address).is_some() }
    }

}

#[inline(always)]
fn is_protected_port(dest_port: u16) -> bool {
    unsafe { PROTECTED_PORTS.get(&dest_port).is_some() }
}

#[inline(always)]
fn increment_counter(address: u32, collection: &PerCpuHashMap<u32, u64>) {
    unsafe {
        if let Some(count) = collection.get_ptr_mut(&address) {
            *count += 1;
        } else {
            let _ = collection.insert(&address, &1, 0);
        }
    }
}

#[inline(always)]
fn ptr_at<T>(ctx: &XdpContext, offset: usize) -> Result<*const T, ()> {
    let start = ctx.data();
    let end = ctx.data_end();
    let len = mem::size_of::<T>();

    if start + offset + len > end {
        return Err(());
    }

    Ok((start + offset) as *const T)
}

#[inline(always)]
fn try_process_packet(ctx: &XdpContext, close_to_leader: bool) -> Result<u32, ()> {
    let eth_header: *const EthHdr = ptr_at(&ctx, 0)?;
    if let EtherType::Ipv6 = unsafe { (*eth_header).ether_type } {
        return Ok(xdp_action::XDP_PASS);
    }

    let ipv4_header: *const Ipv4Hdr = ptr_at(&ctx, EthHdr::LEN)?;
    return if let IpProto::Udp = unsafe { (*ipv4_header).proto } {
        let source_addr = u32::from_be(unsafe { (*ipv4_header).src_addr });
        let udp_header: *const UdpHdr = ptr_at(&ctx, EthHdr::LEN + Ipv4Hdr::LEN)?;
        let dest_port = u16::from_be(unsafe { (*udp_header).dest });
        if !is_protected_port(dest_port) {
            return Ok(xdp_action::XDP_PASS);
        }

        //Traffic above here is other OS traffic, not counted in our stats
        increment_counter(source_addr, &ALL_TRAFFIC_STATS);
        let action = if is_allow_listed(source_addr, close_to_leader) {
            debug!(
                ctx,
                "ALLOW SRC IP: {:i}, DEST PORT: {}",
                source_addr,
                dest_port
            );
            xdp_action::XDP_PASS
        } else {
            debug!(
                ctx,
                "DROP SRC IP: {:i}, DEST PORT: {}",
                source_addr,
                dest_port
            );
            increment_counter(source_addr, &BLOCKED_TRAFFIC_STATS);
            xdp_action::XDP_DROP
        };

        Ok(action)
    } else {
        Ok(xdp_action::XDP_PASS)
    };
}
