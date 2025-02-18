// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

pub mod memory;
mod network;

//==============================================================================
// Imports
//==============================================================================

use self::memory::{
    consts::DEFAULT_MAX_BODY_SIZE,
    MemoryManager,
};
use crate::runtime::{
    libdpdk::{
        rte_delay_us_block,
        rte_eal_init,
        rte_eth_conf,
        rte_eth_dev_configure,
        rte_eth_dev_count_avail,
        rte_eth_dev_get_mtu,
        rte_eth_dev_info_get,
        rte_eth_dev_is_valid_port,
        rte_eth_dev_set_mtu,
        rte_eth_dev_start,
        rte_eth_find_next_owned_by,
        rte_eth_link,
        rte_eth_link_get_nowait,
        rte_eth_macaddr_get,
        rte_eth_promiscuous_enable,
        rte_eth_rx_mq_mode_ETH_MQ_RX_RSS as ETH_MQ_RX_RSS,
        rte_eth_rx_queue_setup,
        rte_eth_rxconf,
        rte_eth_tx_mq_mode_ETH_MQ_TX_NONE as ETH_MQ_TX_NONE,
        rte_eth_tx_queue_setup,
        rte_eth_txconf,
        rte_ether_addr,
        DEV_RX_OFFLOAD_JUMBO_FRAME,
        DEV_RX_OFFLOAD_TCP_CKSUM,
        DEV_RX_OFFLOAD_UDP_CKSUM,
        DEV_TX_OFFLOAD_MULTI_SEGS,
        DEV_TX_OFFLOAD_TCP_CKSUM,
        DEV_TX_OFFLOAD_UDP_CKSUM,
        ETH_LINK_FULL_DUPLEX,
        ETH_LINK_UP,
        ETH_RSS_IP,
        RTE_ETHER_MAX_JUMBO_FRAME_LEN,
        RTE_ETHER_MAX_LEN,
        RTE_ETH_DEV_NO_OWNER,
        RTE_PKTMBUF_HEADROOM,
    },
    network::{
        config::{
            ArpConfig,
            TcpConfig,
            UdpConfig,
        },
        types::MacAddress,
    },
    Runtime,
};
use ::anyhow::{
    bail,
    format_err,
    Error,
};
use ::std::{
    collections::HashMap,
    ffi::CString,
    mem::MaybeUninit,
    net::Ipv4Addr,
    time::Duration,
};

//==============================================================================
// Macros
//==============================================================================

macro_rules! expect_zero {
    ($name:ident ( $($arg: expr),* $(,)* )) => {{
        let ret = $name($($arg),*);
        if ret == 0 {
            Ok(0)
        } else {
            Err(format_err!("{} failed with {:?}", stringify!($name), ret))
        }
    }};
}

//==============================================================================
// Structures
//==============================================================================

/// DPDK Runtime
#[derive(Clone)]
pub struct DPDKRuntime {
    mm: MemoryManager,
    port_id: u16,
    pub link_addr: MacAddress,
    pub ipv4_addr: Ipv4Addr,
    pub arp_options: ArpConfig,
    pub tcp_options: TcpConfig,
    pub udp_options: UdpConfig,
}

//==============================================================================
// Associate Functions
//==============================================================================

/// Associate Functions for DPDK Runtime
impl DPDKRuntime {
    pub fn new(
        ipv4_addr: Ipv4Addr,
        eal_init_args: &[CString],
        arp_table: HashMap<Ipv4Addr, MacAddress>,
        disable_arp: bool,
        use_jumbo_frames: bool,
        mtu: u16,
        mss: usize,
        tcp_checksum_offload: bool,
        udp_checksum_offload: bool,
    ) -> DPDKRuntime {
        let (mm, port_id, link_addr) = Self::initialize_dpdk(
            eal_init_args,
            use_jumbo_frames,
            mtu,
            tcp_checksum_offload,
            udp_checksum_offload,
        )
        .unwrap();

        let arp_options = ArpConfig::new(
            Some(Duration::from_secs(15)),
            Some(Duration::from_secs(20)),
            Some(5),
            Some(arp_table),
            Some(disable_arp),
        );

        let tcp_options = TcpConfig::new(
            Some(mss),
            None,
            None,
            Some(0xffff),
            Some(0),
            None,
            Some(tcp_checksum_offload),
            Some(tcp_checksum_offload),
        );

        let udp_options = UdpConfig::new(Some(udp_checksum_offload), Some(udp_checksum_offload));

        Self {
            mm,
            port_id,
            link_addr,
            ipv4_addr,
            arp_options,
            tcp_options,
            udp_options,
        }
    }

    /// Initializes DPDK.
    fn initialize_dpdk(
        eal_init_args: &[CString],
        use_jumbo_frames: bool,
        mtu: u16,
        tcp_checksum_offload: bool,
        udp_checksum_offload: bool,
    ) -> Result<(MemoryManager, u16, MacAddress), Error> {
        std::env::set_var("MLX5_SHUT_UP_BF", "1");
        std::env::set_var("MLX5_SINGLE_THREADED", "1");
        std::env::set_var("MLX4_SINGLE_THREADED", "1");
        let eal_init_refs = eal_init_args.iter().map(|s| s.as_ptr() as *mut u8).collect::<Vec<_>>();
        let ret: libc::c_int = unsafe { rte_eal_init(eal_init_refs.len() as i32, eal_init_refs.as_ptr() as *mut _) };
        if ret < 0 {
            let rte_errno: libc::c_int = unsafe { dpdk_rs::rte_errno() };
            bail!("EAL initialization failed (rte_errno={:?})", rte_errno);
        }
        let nb_ports: u16 = unsafe { rte_eth_dev_count_avail() };
        if nb_ports == 0 {
            bail!("No ethernet ports available");
        }
        eprintln!("DPDK reports that {} ports (interfaces) are available.", nb_ports);

        let max_body_size: usize = if use_jumbo_frames {
            (RTE_ETHER_MAX_JUMBO_FRAME_LEN + RTE_PKTMBUF_HEADROOM) as usize
        } else {
            DEFAULT_MAX_BODY_SIZE
        };

        let memory_manager = MemoryManager::new(max_body_size)?;

        let owner: u64 = RTE_ETH_DEV_NO_OWNER as u64;
        let port_id: u16 = unsafe { rte_eth_find_next_owned_by(0, owner) as u16 };
        Self::initialize_dpdk_port(
            port_id,
            &memory_manager,
            use_jumbo_frames,
            mtu,
            tcp_checksum_offload,
            udp_checksum_offload,
        )?;

        // TODO: Where is this function?
        // if unsafe { rte_lcore_count() } > 1 {
        //     eprintln!("WARNING: Too many lcores enabled. Only 1 used.");
        // }

        let local_link_addr: MacAddress = unsafe {
            let mut m: MaybeUninit<rte_ether_addr> = MaybeUninit::zeroed();
            // TODO: Why does bindgen say this function doesn't return an int?
            rte_eth_macaddr_get(port_id, m.as_mut_ptr());
            MacAddress::new(m.assume_init().addr_bytes)
        };
        if local_link_addr.is_nil() || !local_link_addr.is_unicast() {
            Err(format_err!("Invalid mac address"))?;
        }

        Ok((memory_manager, port_id, local_link_addr))
    }

    /// Initializes a DPDK port.
    fn initialize_dpdk_port(
        port_id: u16,
        memory_manager: &MemoryManager,
        use_jumbo_frames: bool,
        mtu: u16,
        tcp_checksum_offload: bool,
        udp_checksum_offload: bool,
    ) -> Result<(), Error> {
        let rx_rings: u16 = 1;
        let tx_rings: u16 = 1;
        let rx_ring_size: u16 = 2048;
        let tx_ring_size: u16 = 2048;
        let nb_rxd: u16 = rx_ring_size;
        let nb_txd: u16 = tx_ring_size;

        let rx_pthresh: u8 = 8;
        let rx_hthresh: u8 = 8;
        let rx_wthresh: u8 = 0;

        let tx_pthresh: u8 = 0;
        let tx_hthresh: u8 = 0;
        let tx_wthresh: u8 = 0;

        let dev_info: dpdk_rs::rte_eth_dev_info = unsafe {
            let mut d: MaybeUninit<dpdk_rs::rte_eth_dev_info> = MaybeUninit::zeroed();
            rte_eth_dev_info_get(port_id, d.as_mut_ptr());
            d.assume_init()
        };

        println!("dev_info: {:?}", dev_info);
        let mut port_conf: rte_eth_conf = unsafe { MaybeUninit::zeroed().assume_init() };
        port_conf.rxmode.max_rx_pkt_len = if use_jumbo_frames {
            RTE_ETHER_MAX_JUMBO_FRAME_LEN
        } else {
            RTE_ETHER_MAX_LEN
        };
        if tcp_checksum_offload {
            port_conf.rxmode.offloads |= DEV_RX_OFFLOAD_TCP_CKSUM as u64;
        }
        if udp_checksum_offload {
            port_conf.rxmode.offloads |= DEV_RX_OFFLOAD_UDP_CKSUM as u64;
        }
        if use_jumbo_frames {
            port_conf.rxmode.offloads |= DEV_RX_OFFLOAD_JUMBO_FRAME as u64;
        }
        port_conf.rxmode.mq_mode = ETH_MQ_RX_RSS;
        port_conf.rx_adv_conf.rss_conf.rss_hf = ETH_RSS_IP as u64 | dev_info.flow_type_rss_offloads;

        port_conf.txmode.mq_mode = ETH_MQ_TX_NONE;
        if tcp_checksum_offload {
            port_conf.txmode.offloads |= DEV_TX_OFFLOAD_TCP_CKSUM as u64;
        }
        if udp_checksum_offload {
            port_conf.txmode.offloads |= DEV_TX_OFFLOAD_UDP_CKSUM as u64;
        }
        port_conf.txmode.offloads |= DEV_TX_OFFLOAD_MULTI_SEGS as u64;

        let mut rx_conf: rte_eth_rxconf = unsafe { MaybeUninit::zeroed().assume_init() };
        rx_conf.rx_thresh.pthresh = rx_pthresh;
        rx_conf.rx_thresh.hthresh = rx_hthresh;
        rx_conf.rx_thresh.wthresh = rx_wthresh;
        rx_conf.rx_free_thresh = 32;

        let mut tx_conf: rte_eth_txconf = unsafe { MaybeUninit::zeroed().assume_init() };
        tx_conf.tx_thresh.pthresh = tx_pthresh;
        tx_conf.tx_thresh.hthresh = tx_hthresh;
        tx_conf.tx_thresh.wthresh = tx_wthresh;
        tx_conf.tx_free_thresh = 32;

        unsafe {
            expect_zero!(rte_eth_dev_configure(
                port_id,
                rx_rings,
                tx_rings,
                &port_conf as *const _,
            ))?;
        }

        unsafe {
            expect_zero!(rte_eth_dev_set_mtu(port_id, mtu))?;
            let mut dpdk_mtu: u16 = 0u16;
            expect_zero!(rte_eth_dev_get_mtu(port_id, &mut dpdk_mtu as *mut _))?;
            if dpdk_mtu != mtu {
                bail!("Failed to set MTU to {}, got back {}", mtu, dpdk_mtu);
            }
        }

        let socket_id: u32 = 0;

        unsafe {
            for i in 0..rx_rings {
                expect_zero!(rte_eth_rx_queue_setup(
                    port_id,
                    i,
                    nb_rxd,
                    socket_id,
                    &rx_conf as *const _,
                    memory_manager.body_pool(),
                ))?;
            }
            for i in 0..tx_rings {
                expect_zero!(rte_eth_tx_queue_setup(
                    port_id,
                    i,
                    nb_txd,
                    socket_id,
                    &tx_conf as *const _
                ))?;
            }
            expect_zero!(rte_eth_dev_start(port_id))?;
            rte_eth_promiscuous_enable(port_id);
        }

        if unsafe { rte_eth_dev_is_valid_port(port_id) } == 0 {
            bail!("Invalid port");
        }

        let sleep_duration: Duration = Duration::from_millis(100);
        let mut retry_count: i32 = 90;

        loop {
            unsafe {
                let mut link: MaybeUninit<rte_eth_link> = MaybeUninit::zeroed();
                rte_eth_link_get_nowait(port_id, link.as_mut_ptr());
                let link: rte_eth_link = link.assume_init();
                if link.link_status() as u32 == ETH_LINK_UP {
                    let duplex = if link.link_duplex() as u32 == ETH_LINK_FULL_DUPLEX {
                        "full"
                    } else {
                        "half"
                    };
                    eprintln!(
                        "Port {} Link Up - speed {} Mbps - {} duplex",
                        port_id, link.link_speed, duplex
                    );
                    break;
                }
                rte_delay_us_block(sleep_duration.as_micros() as u32);
            }
            if retry_count == 0 {
                bail!("Link never came up");
            }
            retry_count -= 1;
        }

        Ok(())
    }
}

//==============================================================================
// Trait Implementations
//==============================================================================

impl Runtime for DPDKRuntime {}
