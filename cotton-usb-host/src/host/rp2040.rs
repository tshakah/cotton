use crate::async_pool::Pool;
use crate::debug;
use crate::host_controller::{
    DataPhase, DeviceStatus, HostController, InterruptPacket, InterruptPipe,
    MultiInterruptPipe, UsbError, UsbSpeed,
};
use crate::wire::{Direction, EndpointType, SetupPacket};
use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use futures::Stream;
use rp2040_pac as pac;
use rtic_common::waker_registration::CriticalSectionWakerRegistration;

pub struct UsbShared {
    // @TODO shouldn't be pub
    pub device_waker: CriticalSectionWakerRegistration,
    pub pipe_wakers: [CriticalSectionWakerRegistration; 16],
}

impl UsbShared {
    pub fn on_irq(&self) {
        let regs = unsafe { pac::USBCTRL_REGS::steal() };
        let ints = regs.ints().read();
        /* defmt::info!(
                    "IRQ ints={:x} inte={:x}",
                    ints.bits(),
                    regs.inte().read().bits()
                );
        */
        if ints.buff_status().bit() {
            let bs = regs.buff_status().read().bits();
            for i in 0..15 {
                if (bs & (3 << (i * 2))) != 0 {
                    defmt::info!("IRQ wakes {}", i);
                    self.pipe_wakers[i].wake();
                }
            }
            regs.buff_status().write(|w| unsafe { w.bits(0xFFFF_FFFC) });
        }
        if (ints.bits() & 1) != 0 {
            // This clears the interrupt but does NOT clear sie_status.speed!
            unsafe { regs.sie_status().modify(|_, w| w.speed().bits(3)) };
            self.device_waker.wake();
        }
        if (ints.bits() & 0x458) != 0 {
            defmt::info!("IRQ wakes 0");
            self.pipe_wakers[0].wake();
        }

        // Disable any remaining interrupts so we don't have an IRQ storm
        let bits = regs.ints().read().bits();
        unsafe {
            regs.inte().modify(|r, w| w.bits(r.bits() & !bits));
        }
        /*        defmt::info!(
            "IRQ2 ints={:x} inte={:x}",
            bits,
            regs.inte().read().bits()
        ); */
    }
}

impl UsbShared {
    // Only exists so that we can initialise the array in a const way
    #[allow(clippy::declare_interior_mutable_const)]
    const W: CriticalSectionWakerRegistration =
        CriticalSectionWakerRegistration::new();

    pub const fn new() -> Self {
        Self {
            device_waker: CriticalSectionWakerRegistration::new(),
            pipe_wakers: [Self::W; 16],
        }
    }
}

impl Default for UsbShared {
    fn default() -> Self {
        Self::new()
    }
}

pub struct UsbStatics {
    // @TODO shouldn't be pub
    pub bulk_pipes: Pool,
}

impl UsbStatics {
    pub const fn new() -> Self {
        Self {
            bulk_pipes: Pool::new(15),
        }
    }
}

impl Default for UsbStatics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Copy, Clone)]
pub struct Rp2040DeviceDetect {
    waker: &'static CriticalSectionWakerRegistration,
    status: DeviceStatus,
}

impl Rp2040DeviceDetect {
    pub fn new(waker: &'static CriticalSectionWakerRegistration) -> Self {
        Self {
            waker,
            status: DeviceStatus::Absent,
        }
    }
}

impl Stream for Rp2040DeviceDetect {
    type Item = DeviceStatus;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        defmt::trace!("DE register");
        self.waker.register(cx.waker());

        let regs = unsafe { pac::USBCTRL_REGS::steal() };
        let status = regs.sie_status().read();
        let device_status = match status.speed().bits() {
            0 => DeviceStatus::Absent,
            1 => DeviceStatus::Present(UsbSpeed::Low1_5),
            _ => DeviceStatus::Present(UsbSpeed::Full12),
        };

        if device_status != self.status {
            defmt::println!(
                "DE ready {:x} {}/{}",
                status.bits(),
                device_status,
                self.status
            );
            regs.inte().modify(|_, w| w.host_conn_dis().set_bit());
            self.status = device_status;
            Poll::Ready(Some(device_status))
        } else {
            defmt::trace!(
                "DE pending intr={:x} st={:x}",
                regs.intr().read().bits(),
                status.bits()
            );
            regs.inte().modify(|_, w| w.host_conn_dis().set_bit());
            Poll::Pending
        }
    }
}

pub struct Rp2040ControlEndpoint<'a> {
    waker: &'a CriticalSectionWakerRegistration,
}

impl<'a> Rp2040ControlEndpoint<'a> {
    pub fn new(waker: &'a CriticalSectionWakerRegistration) -> Self {
        Self { waker }
    }
}

impl Future for Rp2040ControlEndpoint<'_> {
    type Output = pac::usbctrl_regs::sie_status::R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        //defmt::trace!("CE register");
        self.waker.register(cx.waker());

        let regs = unsafe { pac::USBCTRL_REGS::steal() };
        let status = regs.sie_status().read();
        let intr = regs.intr().read();
        if (intr.bits() & 0x458) != 0 {
            defmt::info!("CE ready {:x}", status.bits());
            regs.sie_status().write(|w| unsafe { w.bits(0xFF0C_0000) });
            Poll::Ready(status)
        } else {
            regs.sie_status().write(|w| unsafe { w.bits(0xFF0C_0000) });
            defmt::trace!(
                "CE pending intr={:x} st={:x}->{:x}",
                intr.bits(),
                status.bits(),
                regs.sie_status().read().bits(),
            );
            regs.inte().modify(|_, w| {
                w.stall()
                    .set_bit()
                    .error_rx_timeout()
                    .set_bit()
                    .trans_complete()
                    .set_bit()
            });
            Poll::Pending
        }
    }
}

pub type Pipe<'a> = crate::async_pool::Pooled<'a>;

pub struct Rp2040InterruptPipe<'driver> {
    driver: &'driver Rp2040HostController,
    pipe: Pipe<'driver>,
    max_packet_size: u16,
    data_toggle: Cell<bool>,
}

impl InterruptPipe for Rp2040InterruptPipe<'_> {
    fn set_waker(&self, waker: &core::task::Waker) {
        self.driver.shared.pipe_wakers[self.pipe.n as usize].register(waker);
    }

    fn poll(&self) -> Option<InterruptPacket> {
        let dpram = unsafe { pac::USBCTRL_DPRAM::steal() };
        let bc = dpram.ep_buffer_control((self.pipe.n * 2) as usize).read();
        if bc.full_0().bit() {
            let mut result = InterruptPacket {
                size: core::cmp::min(bc.length_0().bits(), 64) as u8,
                ..Default::default()
            };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (0x5010_0200 + (self.pipe.n as u32) * 64) as *const u8,
                    &mut result.data[0] as *mut u8,
                    result.size as usize,
                )
            };
            self.data_toggle.set(!self.data_toggle.get());
            dpram.ep_buffer_control((self.pipe.n * 2) as usize).write(
                |w| unsafe {
                    w.full_0()
                        .clear_bit()
                        .pid_0()
                        .bit(self.data_toggle.get())
                        .length_0()
                        .bits(self.max_packet_size)
                        .last_0()
                        .set_bit()
                },
            );

            cortex_m::asm::delay(12);

            dpram
                .ep_buffer_control((self.pipe.n * 2) as usize)
                .modify(|_, w| w.available_0().set_bit());
            let regs = unsafe { pac::USBCTRL_REGS::steal() };
            defmt::println!(
                "IE ready inte {:x} iec {:x} ecr {:x} epbc {:x}",
                regs.inte().read().bits(),
                regs.int_ep_ctrl().read().bits(),
                dpram
                    .ep_control((self.pipe.n * 2) as usize - 2)
                    .read()
                    .bits(),
                dpram
                    .ep_buffer_control((self.pipe.n * 2) as usize)
                    .read()
                    .bits(),
            );

            Some(result)
        } else {
            let regs = unsafe { pac::USBCTRL_REGS::steal() };
            regs.inte().modify(|_, w| w.buff_status().set_bit());
            regs.int_ep_ctrl().modify(|r, w| unsafe {
                w.bits(r.bits() | (1 << self.pipe.n))
            });
            defmt::trace!(
                "IE pending inte {:x} iec {:x} ecr {:x} epbc {:x}",
                regs.inte().read().bits(),
                regs.int_ep_ctrl().read().bits(),
                dpram
                    .ep_control((self.pipe.n * 2) as usize - 2)
                    .read()
                    .bits(),
                dpram
                    .ep_buffer_control((self.pipe.n * 2) as usize)
                    .read()
                    .bits(),
            );
            regs.ep_status_stall_nak()
                .write(|w| unsafe { w.bits(3 << (self.pipe.n * 2)) });

            None
        }
    }
}

type MultiPooled<'a> = crate::async_pool::MultiPooled<'a>;

struct PipeInfo {
    address: u8,
    endpoint: u8,
    max_packet_size: u8,
    data_toggle: Cell<bool>,
}

pub struct Rp2040MultiInterruptPipe {
    shared: &'static UsbShared,
    //statics: &'static UsbStatics,
    pipes: MultiPooled<'static>,
    pipe_info: [Option<PipeInfo>; 16],
}

impl InterruptPipe for Rp2040MultiInterruptPipe {
    fn set_waker(&self, waker: &core::task::Waker) {
        for i in self.pipes.iter() {
            self.shared.pipe_wakers[(i + 1) as usize].register(waker);
        }
    }

    fn poll(&self) -> Option<InterruptPacket> {
        let dpram = unsafe { pac::USBCTRL_DPRAM::steal() };
        for i in self.pipes.iter() {
            let pipe = i + 1;
            let bc = dpram.ep_buffer_control((pipe * 2) as usize).read();
            if bc.full_0().bit() {
                let info = self.pipe_info[pipe as usize].as_ref().unwrap();
                let mut result = InterruptPacket {
                    address: info.address,
                    endpoint: info.endpoint,
                    size: core::cmp::min(bc.length_0().bits(), 64) as u8,
                    ..Default::default()
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (0x5010_0200 + (pipe as u32) * 64) as *const u8,
                        &mut result.data[0] as *mut u8,
                        result.size as usize,
                    )
                };
                info.data_toggle.set(!info.data_toggle.get());
                dpram.ep_buffer_control((pipe * 2) as usize).write(
                    |w| unsafe {
                        w.full_0()
                            .clear_bit()
                            .pid_0()
                            .bit(info.data_toggle.get())
                            .length_0()
                            .bits(info.max_packet_size as u16)
                            .last_0()
                            .set_bit()
                    },
                );

                cortex_m::asm::delay(12);

                dpram
                    .ep_buffer_control((pipe * 2) as usize)
                    .modify(|_, w| w.available_0().set_bit());
                let regs = unsafe { pac::USBCTRL_REGS::steal() };
                defmt::println!(
                    "ME ready inte {:x} iec {:x} ecr {:x} epbc {:x}",
                    regs.inte().read().bits(),
                    regs.int_ep_ctrl().read().bits(),
                    dpram.ep_control((pipe * 2) as usize - 2).read().bits(),
                    dpram.ep_buffer_control((pipe * 2) as usize).read().bits(),
                );

                return Some(result);
            }
        }

        let regs = unsafe { pac::USBCTRL_REGS::steal() };
        regs.inte().modify(|_, w| w.buff_status().set_bit());
        // shift pipes.bits left because we don't use pipe 0
        regs.int_ep_ctrl().modify(|r, w| unsafe {
            w.bits(r.bits() | (self.pipes.bits() * 2))
        });
        defmt::trace!(
            "ME pending bits {:x} inte {:x} iec {:x}",
            self.pipes.bits() * 2,
            regs.inte().read().bits(),
            regs.int_ep_ctrl().read().bits(),
        );
        let mut mask = 0;
        for i in self.pipes.iter() {
            mask |= 3 << (i * 2);
        }
        regs.ep_status_stall_nak()
            .write(|w| unsafe { w.bits(mask) });

        None
    }
}

impl MultiInterruptPipe for Rp2040MultiInterruptPipe {
    fn try_add(
        &mut self,
        address: u8,
        endpoint: u8,
        max_packet_size: u8,
        interval_ms: u8,
    ) -> Result<(), UsbError> {
        let p = self.pipes.try_alloc().ok_or(UsbError::AllPipesInUse)? + 1;
        defmt::println!("ME got pipe {}", p);
        let pi = PipeInfo {
            address,
            endpoint,
            max_packet_size,
            data_toggle: Cell::new(false),
        };

        self.pipe_info[p as usize] = Some(pi);

        let regs = unsafe { pac::USBCTRL_REGS::steal() };
        let dpram = unsafe { pac::USBCTRL_DPRAM::steal() };
        regs.host_addr_endp((p - 1) as usize).write(|w| unsafe {
            w.address()
                .bits(address)
                .endpoint()
                .bits(endpoint)
                .intep_dir()
                .clear_bit() // IN
        });

        dpram.ep_control((p * 2 - 2) as usize).write(|w| unsafe {
            w.enable()
                .set_bit()
                .interrupt_per_buff()
                .set_bit()
                .endpoint_type()
                .interrupt()
                .buffer_address()
                .bits(0x200 + (p as u16) * 64)
                .host_poll_interval()
                .bits(core::cmp::min(interval_ms as u16, 9))
        });

        dpram.ep_buffer_control((p * 2) as usize).write(|w| unsafe {
            w.full_0()
                .clear_bit()
                .length_0()
                .bits(max_packet_size as u16)
                .pid_0()
                .clear_bit()
                .last_0()
                .set_bit()
        });

        cortex_m::asm::delay(12);

        dpram
            .ep_buffer_control((p * 2) as usize)
            .modify(|_, w| w.available_0().set_bit());

        Ok(())
    }

    fn remove(&mut self, _address: u8) {
        todo!()
    }
}

trait Packetiser {
    fn prepare(&mut self, reg: &pac::usbctrl_dpram::EP_BUFFER_CONTROL);
}

struct InPacketiser {
    next_prep: u8,
    remain: u16,
    packet_size: u16,
    need_zero_size_packet: bool,
}

impl InPacketiser {
    fn new(remain: u16, packet_size: u16) -> Self {
        Self {
            next_prep: 0,
            remain,
            packet_size,
            need_zero_size_packet: (remain % packet_size) == 0,
        }
    }

    fn next_packet(&mut self) -> Option<(u16, bool)> {
        if self.remain == 0 {
            if self.need_zero_size_packet {
                self.need_zero_size_packet = false;
                return Some((0, true));
            } else {
                return None;
            }
        }
        if self.remain < self.packet_size {
            return Some((self.remain, true));
        }
        if self.remain > self.packet_size {
            return Some((self.packet_size, false));
        }
        Some((self.remain, false))
    }
}

impl Packetiser for InPacketiser {
    fn prepare(&mut self, reg: &pac::usbctrl_dpram::EP_BUFFER_CONTROL) {
        let val = reg.read();
        match self.next_prep {
            0 => {
                if !val.available_0().bit() {
                    if let Some((this_packet, is_last)) = self.next_packet() {
                        self.remain -= this_packet;
                        //defmt::info!("Prepared {}-byte space", this_packet);
                        reg.modify(|_, w| {
                            w.full_0().clear_bit();
                            w.pid_0().set_bit();
                            w.last_0().bit(is_last);
                            unsafe { w.length_0().bits(this_packet) };
                            w
                        });

                        cortex_m::asm::delay(12);

                        reg.modify(|_, w| w.available_0().set_bit());

                        self.next_prep = 1;
                    }
                }
            }

            _ => {
                if !val.available_1().bit() {
                    if let Some((this_packet, is_last)) = self.next_packet() {
                        self.remain -= this_packet;
                        //defmt::info!("Prepared {}-byte space", this_packet);
                        reg.modify(|_, w| {
                            w.full_1().clear_bit();
                            w.pid_1().clear_bit();
                            w.last_1().bit(is_last);
                            unsafe { w.length_1().bits(this_packet) };
                            w
                        });

                        cortex_m::asm::delay(12);

                        reg.modify(|_, w| w.available_1().set_bit());

                        self.next_prep = 0;
                    }
                }
            }
        }
    }
}

struct OutPacketiser<'a> {
    next_prep: u8,
    remain: usize,
    offset: usize,
    packet_size: usize,
    need_zero_size_packet: bool,
    buf: &'a [u8],
}

impl<'a> OutPacketiser<'a> {
    fn new(size: u16, packet_size: u16, buf: &'a [u8]) -> Self {
        Self {
            next_prep: 0,
            remain: size as usize,
            offset: 0,
            packet_size: packet_size as usize,
            need_zero_size_packet: (size % packet_size) == 0,
            buf,
        }
    }

    fn next_packet(&mut self) -> Option<(usize, bool)> {
        if self.remain == 0 {
            if self.need_zero_size_packet {
                self.need_zero_size_packet = false;
                return Some((0, true));
            } else {
                return None;
            }
        }
        if self.remain < self.packet_size {
            return Some((self.remain, true));
        }
        if self.remain > self.packet_size {
            return Some((self.packet_size, false));
        }
        Some((self.remain, false))
    }
}

impl Packetiser for OutPacketiser<'_> {
    fn prepare(&mut self, reg: &pac::usbctrl_dpram::EP_BUFFER_CONTROL) {
        let val = reg.read();
        match self.next_prep {
            0 => {
                if !val.available_0().bit() {
                    if let Some((this_packet, is_last)) = self.next_packet() {
                        if this_packet > 0 {
                            unsafe {
                                core::ptr::copy_nonoverlapping(
                                    &self.buf[self.offset] as *const u8,
                                    (0x5010_0000 + 0x180) as *mut u8,
                                    this_packet,
                                );
                            }
                        }
                        reg.modify(|_, w| {
                            // @todo Why is this "if" necessary?
                            if this_packet > 0 {
                                w.full_0().set_bit();
                            }
                            w.pid_0().set_bit();
                            w.last_0().bit(is_last);
                            unsafe { w.length_0().bits(this_packet as u16) };
                            w
                        });

                        cortex_m::asm::delay(12);

                        reg.modify(|_, w| w.available_0().set_bit());

                        self.remain -= this_packet;
                        self.offset += this_packet;
                        self.next_prep = 1;
                    }
                }
            }

            _ => {
                if !val.available_1().bit() {
                    if let Some((this_packet, is_last)) = self.next_packet() {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                &self.buf[self.offset] as *const u8,
                                (0x5010_0000 + 0x1C0) as *mut u8,
                                this_packet,
                            );
                        }
                        reg.modify(|_, w| {
                            w.full_1().set_bit();
                            w.pid_1().clear_bit();
                            w.last_1().bit(is_last);
                            unsafe { w.length_1().bits(this_packet as u16) };
                            w
                        });

                        cortex_m::asm::delay(12);

                        reg.modify(|_, w| w.available_1().set_bit());

                        self.remain -= this_packet;
                        self.offset += this_packet;
                        self.next_prep = 0;
                    }
                }
            }
        }
    }
}

trait Depacketiser {
    fn retire(&mut self, reg: &pac::usbctrl_dpram::EP_BUFFER_CONTROL);
}

struct InDepacketiser<'a> {
    next_retire: u8,
    remain: usize,
    offset: usize,
    buf: &'a mut [u8],
}

impl<'a> InDepacketiser<'a> {
    fn new(size: u16, buf: &'a mut [u8]) -> Self {
        Self {
            next_retire: 0,
            remain: size as usize,
            offset: 0,
            buf,
        }
    }

    fn total(&self) -> usize {
        self.offset
    }
}

impl Depacketiser for InDepacketiser<'_> {
    fn retire(&mut self, reg: &pac::usbctrl_dpram::EP_BUFFER_CONTROL) {
        let val = reg.read();
        match self.next_retire {
            0 => {
                if val.full_0().bit() {
                    defmt::trace!("Got {} bytes", val.length_0().bits());
                    let this_packet = core::cmp::min(
                        self.remain,
                        val.length_0().bits() as usize,
                    );
                    if this_packet > 0 {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (0x5010_0000 + 0x180) as *const u8,
                                &mut self.buf[self.offset] as *mut u8,
                                this_packet,
                            );
                        }
                    }

                    self.remain -= this_packet;
                    self.offset += this_packet;
                    self.next_retire = 1;
                }
            }
            _ => {
                if val.full_1().bit() {
                    defmt::trace!("Got {} bytes", val.length_1().bits());
                    let this_packet = core::cmp::min(
                        self.remain,
                        val.length_1().bits() as usize,
                    );
                    if this_packet > 0 {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (0x5010_0000 + 0x1C0) as *const u8,
                                &mut self.buf[self.offset] as *mut u8,
                                this_packet,
                            );
                        }
                    }

                    self.remain -= this_packet;
                    self.offset += this_packet;
                    self.next_retire = 0;
                }
            }
        }
    }
}

struct OutDepacketiser {
    next_retire: u8,
}

impl OutDepacketiser {
    fn new() -> Self {
        Self { next_retire: 0 }
    }
}

impl Depacketiser for OutDepacketiser {
    fn retire(&mut self, reg: &pac::usbctrl_dpram::EP_BUFFER_CONTROL) {
        let val = reg.read();
        match self.next_retire {
            0 => {
                if val.full_0().bit() {
                    self.next_retire = 1;
                }
            }
            _ => {
                if val.full_1().bit() {
                    self.next_retire = 0;
                }
            }
        }
    }
}

pub struct Rp2040HostController {
    shared: &'static UsbShared,
    statics: &'static UsbStatics,
    regs: pac::USBCTRL_REGS,
    dpram: pac::USBCTRL_DPRAM,
    control_pipes: Pool,
}

impl Rp2040HostController {
    pub fn new(
        resets: &mut pac::RESETS,
        regs: pac::USBCTRL_REGS,
        dpram: pac::USBCTRL_DPRAM,
        shared: &'static UsbShared,
        statics: &'static UsbStatics,
    ) -> Self {
        resets.reset().modify(|_, w| w.usbctrl().set_bit());
        resets.reset().modify(|_, w| w.usbctrl().clear_bit());

        regs.usb_muxing().modify(|_, w| {
            w.to_phy().set_bit();
            w.softcon().set_bit()
        });
        regs.usb_pwr().modify(|_, w| {
            w.vbus_detect().set_bit();
            w.vbus_detect_override_en().set_bit()
        });
        regs.main_ctrl().modify(|_, w| {
            w.sim_timing().clear_bit();
            w.host_ndevice().set_bit();
            w.controller_en().set_bit()
        });
        regs.sie_ctrl().write(|w| {
            w.pulldown_en().set_bit();
            w.vbus_en().set_bit();
            w.keep_alive_en().set_bit();
            w.sof_en().set_bit()
        });

        unsafe {
            pac::NVIC::unpend(pac::Interrupt::USBCTRL_IRQ);
            pac::NVIC::unmask(pac::Interrupt::USBCTRL_IRQ);
        }

        regs.inte().write(|w| w.host_conn_dis().set_bit());

        Self {
            regs,
            dpram,
            shared,
            statics,
            control_pipes: Pool::new(1),
        }
    }

    async fn alloc_pipe(&self, endpoint_type: EndpointType) -> Pipe {
        if endpoint_type == EndpointType::Control {
            self.control_pipes.alloc().await
        } else {
            let mut p = self.statics.bulk_pipes.alloc().await;
            p.n += 1;
            p
        }
    }

    async fn send_setup(
        &self,
        address: u8,
        setup: &SetupPacket,
    ) -> Result<(), UsbError> {
        self.dpram.epx_control().write(|w| {
            unsafe {
                w.buffer_address().bits(0x180);
            }
            w.interrupt_per_buff().clear_bit();
            w.enable().clear_bit()
        });

        self.dpram
            .ep_buffer_control(0)
            .write(|w| unsafe { w.bits(0) });

        // USB 2.0 s9.4.3
        self.dpram.setup_packet_low().write(|w| unsafe {
            w.bmrequesttype().bits(setup.bmRequestType);
            w.brequest().bits(setup.bRequest);
            w.wvalue().bits(setup.wValue)
        });
        self.dpram.setup_packet_high().write(|w| unsafe {
            w.wlength().bits(setup.wLength);
            w.windex().bits(setup.wIndex)
        });

        self.regs
            .sie_status()
            .write(|w| unsafe { w.bits(0xFFFF_FFFF) });

        self.regs.addr_endp().write(|w| unsafe {
            w.endpoint().bits(0);
            w.address().bits(address)
        });

        self.regs.sie_ctrl().modify(|_, w| {
            w.receive_data().clear_bit();
            w.send_data().clear_bit();
            w.send_setup().set_bit()
        });

        defmt::trace!("S ctrl->{:x}", self.regs.sie_ctrl().read().bits());

        cortex_m::asm::delay(12);

        self.regs
            .sie_ctrl()
            .modify(|_, w| w.start_trans().set_bit());

        loop {
            let f = Rp2040ControlEndpoint::new(&self.shared.pipe_wakers[0]);

            let status = f.await;

            defmt::trace!("awaited");

            if status.trans_complete().bit() {
                break;
            }

            let bcr = self.dpram.ep_buffer_control(0).read();
            let ctrl = self.regs.sie_ctrl().read();
            let bstat = self.regs.buff_status().read();
            defmt::trace!(
                "S bcr=0x{:x} sie_status=0x{:x} sie_ctrl=0x{:x} bstat={:x}",
                bcr.bits(),
                status.bits(),
                ctrl.bits(),
                bstat.bits(),
            );
            if status.data_seq_error().bit() {
                return Err(UsbError::DataSeqError);
            }
            if status.stall_rec().bit() {
                return Err(UsbError::Stall);
            }
            // if status.nak_rec().bit() {
            //     return Err(UsbError::Nak);
            // }
            if status.rx_overflow().bit() {
                return Err(UsbError::Overflow);
            }
            if status.rx_timeout().bit() {
                return Err(UsbError::Timeout);
            }
            if status.bit_stuff_error().bit() {
                return Err(UsbError::BitStuffError);
            }
            if status.crc_error().bit() {
                return Err(UsbError::CrcError);
            }
        }

        defmt::trace!("S completed");

        Ok(())
    }

    async fn control_transfer_inner(
        &self,
        address: u8,
        packet_size: u8,
        direction: Direction,
        size: usize,
        packetiser: &mut impl Packetiser,
        depacketiser: &mut impl Depacketiser,
    ) -> Result<(), UsbError> {
        let packets = size / (packet_size as usize) + 1;
        defmt::info!("we'll need {} packets", packets);

        self.dpram.epx_control().write(|w| {
            unsafe {
                w.buffer_address().bits(0x180);
            }
            if packets > 1 {
                w.double_buffered().set_bit();
                w.interrupt_per_buff().set_bit();
            }
            w.enable().set_bit()
        });

        self.dpram
            .ep_buffer_control(0)
            .write(|w| unsafe { w.bits(0) });
        packetiser.prepare(self.dpram.ep_buffer_control(0));

        self.regs
            .sie_status()
            .write(|w| unsafe { w.bits(0xFFFF_FFFF) });

        self.regs.addr_endp().write(|w| unsafe {
            w.endpoint().bits(0);
            w.address().bits(address)
        });

        let mut started = false;

        loop {
            packetiser.prepare(self.dpram.ep_buffer_control(0));
            packetiser.prepare(self.dpram.ep_buffer_control(0));

            self.regs
                .sie_status()
                .write(|w| unsafe { w.bits(0xFF00_0000) });
            self.regs.buff_status().write(|w| unsafe { w.bits(0x3) });
            self.regs.inte().modify(|_, w| {
                if packets > 2 {
                    w.buff_status().set_bit();
                }
                w.trans_complete()
                    .set_bit()
                    .error_data_seq()
                    .set_bit()
                    .stall()
                    .set_bit()
                    .error_rx_timeout()
                    .set_bit()
                    .error_rx_overflow()
                    .set_bit()
                    .error_bit_stuff()
                    .set_bit()
                    .error_crc()
                    .set_bit()
            });

            defmt::info!(
                "Initial bcr {:x}",
                self.dpram.ep_buffer_control(0).read().bits()
            );

            if !started {
                started = true;

                defmt::trace!(
                    "len{} {} ctrl{:x}",
                    size,
                    direction,
                    self.regs.sie_ctrl().read().bits()
                );
                self.regs.sie_ctrl().modify(|_, w| {
                    w.receive_data().bit(direction == Direction::In);
                    w.send_data().bit(direction == Direction::Out);
                    w.send_setup().clear_bit()
                });

                defmt::trace!(
                    "ctrl->{:x}",
                    self.regs.sie_ctrl().read().bits()
                );

                cortex_m::asm::delay(12);

                self.regs
                    .sie_ctrl()
                    .modify(|_, w| w.start_trans().set_bit());
            }

            let f = Rp2040ControlEndpoint::new(&self.shared.pipe_wakers[0]);

            let status = f.await;

            defmt::trace!("awaited");

            self.regs.buff_status().write(|w| unsafe { w.bits(0x3) });

            self.regs.inte().modify(|_, w| {
                w.trans_complete()
                    .clear_bit()
                    .error_data_seq()
                    .clear_bit()
                    .stall()
                    .clear_bit()
                    .error_rx_timeout()
                    .clear_bit()
                    .error_rx_overflow()
                    .clear_bit()
                    .error_bit_stuff()
                    .clear_bit()
                    .error_crc()
                    .clear_bit()
            });

            if status.trans_complete().bit() {
                depacketiser.retire(self.dpram.ep_buffer_control(0));
                break;
            }

            let bcr = self.dpram.ep_buffer_control(0).read();
            let ctrl = self.regs.sie_ctrl().read();
            let bstat = self.regs.buff_status().read();
            defmt::trace!(
                "bcr=0x{:x} sie_status=0x{:x} sie_ctrl=0x{:x} bstat={:x}",
                bcr.bits(),
                status.bits(),
                ctrl.bits(),
                bstat.bits(),
            );
            if status.data_seq_error().bit() {
                return Err(UsbError::DataSeqError);
            }
            if status.stall_rec().bit() {
                return Err(UsbError::Stall);
            }
            // if status.nak_rec().bit() {
            //     return Err(UsbError::Nak);
            // }
            if status.rx_overflow().bit() {
                return Err(UsbError::Overflow);
            }
            if status.rx_timeout().bit() {
                return Err(UsbError::Timeout);
            }
            if status.bit_stuff_error().bit() {
                return Err(UsbError::BitStuffError);
            }
            if status.crc_error().bit() {
                return Err(UsbError::CrcError);
            }

            depacketiser.retire(self.dpram.ep_buffer_control(0));
            depacketiser.retire(self.dpram.ep_buffer_control(0));
        }

        let bcr = self.dpram.ep_buffer_control(0).read();
        let ctrl = self.regs.sie_ctrl().read();
        defmt::trace!(
            "COMPLETE bcr=0x{:x} sie_ctrl=0x{:x}",
            bcr.bits(),
            ctrl.bits()
        );
        self.regs
            .sie_status()
            .write(|w| unsafe { w.bits(0xFF00_0000) });
        depacketiser.retire(self.dpram.ep_buffer_control(0));
        Ok(())
    }

    async fn control_transfer_in(
        &self,
        address: u8,
        packet_size: u8,
        size: usize,
        buf: &mut [u8],
    ) -> Result<usize, UsbError> {
        if buf.len() < size {
            return Err(UsbError::BufferTooSmall);
        }
        let mut packetiser =
            InPacketiser::new(size as u16, packet_size as u16);
        let mut depacketiser = InDepacketiser::new(size as u16, buf);

        self.control_transfer_inner(
            address,
            packet_size,
            Direction::In,
            size,
            &mut packetiser,
            &mut depacketiser,
        )
        .await?;

        Ok(depacketiser.total())
    }

    async fn control_transfer_out(
        &self,
        address: u8,
        packet_size: u8,
        size: usize,
        buf: &[u8],
    ) -> Result<usize, UsbError> {
        if buf.len() < size {
            return Err(UsbError::BufferTooSmall);
        }
        let mut packetiser =
            OutPacketiser::new(size as u16, packet_size as u16, buf);
        let mut depacketiser = OutDepacketiser::new();

        self.control_transfer_inner(
            address,
            packet_size,
            Direction::Out,
            size,
            &mut packetiser,
            &mut depacketiser,
        )
        .await?;

        Ok(buf.len())
    }
}

impl HostController for Rp2040HostController {
    type InterruptPipe<'driver> = Rp2040InterruptPipe<'driver> where Self: 'driver;
    type MultiInterruptPipe = Rp2040MultiInterruptPipe;
    type DeviceDetect = Rp2040DeviceDetect;

    fn device_detect(&self) -> Self::DeviceDetect {
        Rp2040DeviceDetect::new(&self.shared.device_waker)
    }

    async fn control_transfer<'a>(
        &self,
        address: u8,
        packet_size: u8,
        setup: SetupPacket,
        data_phase: DataPhase<'a>,
    ) -> Result<usize, UsbError> {
        let _pipe = self.alloc_pipe(EndpointType::Control).await;

        self.send_setup(address, &setup).await?;
        match data_phase {
            DataPhase::In(buf) => {
                self.control_transfer_in(
                    address,
                    packet_size,
                    setup.wLength as usize,
                    buf,
                )
                .await
            }
            DataPhase::Out(buf) => {
                self.control_transfer_out(
                    address,
                    packet_size,
                    setup.wLength as usize,
                    buf,
                )
                .await?;
                self.control_transfer_in(address, packet_size, 0, &mut [])
                    .await
            }
            DataPhase::None => {
                self.control_transfer_in(address, packet_size, 0, &mut [])
                    .await
            }
        }
    }

    // The trait defines this with "-> impl Future"-style syntax, but the one
    // is just sugar for the other according to Clippy.
    async fn alloc_interrupt_pipe(
        &self,
        address: u8,
        endpoint: u8,
        max_packet_size: u16,
        interval_ms: u8,
    ) -> Rp2040InterruptPipe<'_> {
        let mut pipe = self.statics.bulk_pipes.alloc().await;
        pipe.n += 1;
        debug::println!("interrupt_endpoint on pipe {}", pipe.n);

        let n = pipe.n;
        let regs = unsafe { pac::USBCTRL_REGS::steal() };
        let dpram = unsafe { pac::USBCTRL_DPRAM::steal() };
        regs.host_addr_endp((n - 1) as usize).write(|w| unsafe {
            w.address()
                .bits(address)
                .endpoint()
                .bits(endpoint)
                .intep_dir()
                .clear_bit() // IN
        });

        dpram.ep_control((n * 2 - 2) as usize).write(|w| unsafe {
            w.enable()
                .set_bit()
                .interrupt_per_buff()
                .set_bit()
                .endpoint_type()
                .interrupt()
                .buffer_address()
                .bits(0x200 + (n as u16) * 64)
                .host_poll_interval()
                .bits(core::cmp::min(interval_ms as u16, 9))
        });

        dpram.ep_buffer_control((n * 2) as usize).write(|w| unsafe {
            w.full_0()
                .clear_bit()
                .length_0()
                .bits(max_packet_size)
                .pid_0()
                .clear_bit()
                .last_0()
                .set_bit()
        });

        cortex_m::asm::delay(12);

        dpram
            .ep_buffer_control((n * 2) as usize)
            .modify(|_, w| w.available_0().set_bit());

        Self::InterruptPipe {
            driver: self,
            pipe,
            max_packet_size,
            data_toggle: Cell::new(false),
        }
    }

    fn multi_interrupt_pipe(&self) -> Rp2040MultiInterruptPipe {
        const N: Option<PipeInfo> = None;
        Self::MultiInterruptPipe {
            shared: self.shared,
            //statics: self.statics,
            pipes: MultiPooled::new(&self.statics.bulk_pipes),
            pipe_info: [N; 16],
        }
    }
}
