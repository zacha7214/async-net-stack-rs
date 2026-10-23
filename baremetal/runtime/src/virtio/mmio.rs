//! Modern MMIO lifecycle shared by typed network and entropy transports.
//! Ring and submission owners are retained through completion or confirmed reset.
use super::split::{self, Buffer, Completion, DmaRingMemory, SplitRing};
use crate::memory::dma::DmaAllocator;

pub const VERSION_1: u64 = 1 << 32;
pub const NET_MAC: u64 = 1 << 5;
pub const NET_STATUS: u64 = 1 << 16;
const REQUIRED: u64 = VERSION_1 | NET_MAC;
const SUPPORTED: u64 = REQUIRED | NET_STATUS;
const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;
const NEEDS_RESET: u32 = 64;
const FAILED: u32 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Register {
    Magic = 0x000,
    Version = 0x004,
    DeviceId = 0x008,
    DeviceFeatures = 0x010,
    DeviceFeaturesSel = 0x014,
    DriverFeatures = 0x020,
    DriverFeaturesSel = 0x024,
    QueueSel = 0x030,
    QueueNumMax = 0x034,
    QueueNum = 0x038,
    QueueReady = 0x044,
    QueueNotify = 0x050,
    InterruptStatus = 0x060,
    InterruptAck = 0x064,
    Status = 0x070,
    QueueDescLow = 0x080,
    QueueDescHigh = 0x084,
    QueueDriverLow = 0x090,
    QueueDriverHigh = 0x094,
    QueueDeviceLow = 0x0a0,
    QueueDeviceHigh = 0x0a4,
    ConfigGeneration = 0x0fc,
}

/// Ordered register access, including ordering against ordinary DMA memory.
/// # Safety
/// Accesses must have the specified width, be volatile and fully ordered against
/// preceding/following MMIO and DMA memory accesses. Methods must not panic.
/// Reset/status/queue reads must truthfully reflect the exclusively owned device.
/// Config offsets are relative to 0x100. Dropping this backend must not release
/// storage owned by outstanding DMA. Real implementations require device memory
/// attributes, or the board's explicit MMU-off contract.
pub unsafe trait Registers {
    /// # Safety
    /// Register must be readable and valid in the device's current lifecycle.
    unsafe fn read(&mut self, register: Register) -> u32;

    /// # Safety
    /// Register/value must be valid for the lifecycle; published DMA memory must
    /// be owned and retained until confirmed reset or ownership completion.
    unsafe fn write(&mut self, register: Register, value: u32);

    /// # Safety
    /// Offset must identify a negotiated byte-wide configuration field.
    unsafe fn config_u8(&mut self, offset: usize) -> u8;

    /// # Safety
    /// Offset must identify an aligned negotiated 16-bit configuration field.
    unsafe fn config_u16(&mut self, offset: usize) -> u16;
}

/// Memory-mapped AArch64 register aperture; construction does not touch registers.
#[cfg(target_arch = "aarch64")]
pub struct Mmio {
    base: usize,
}

#[cfg(target_arch = "aarch64")]
impl Mmio {
    /// # Safety
    /// Own this mapped, aligned modern Virtio MMIO aperture exclusively. It must
    /// cover control registers and net configuration, and remain accessible for
    /// this object's lifetime. Use only a coherent, no-IOMMU DMA domain
    /// (see `memory::dma`).
    pub unsafe fn new(base: usize) -> Self {
        Self { base }
    }
    fn barrier() {
        // No `nomem`: also a compiler memory barrier. Conservative full-system
        // completion barrier; optimize only with a proven platform ordering model.
        unsafe {
            core::arch::asm!("dsb sy", options(nostack, preserves_flags));
        }
    }
}

#[cfg(target_arch = "aarch64")]
unsafe impl Registers for Mmio {
    unsafe fn read(&mut self, reg: Register) -> u32 {
        Self::barrier();

        let value = unsafe { core::ptr::read_volatile((self.base + reg as usize) as *const u32) };

        Self::barrier();
        value
    }

    unsafe fn write(&mut self, reg: Register, value: u32) {
        Self::barrier();
        unsafe {
            core::ptr::write_volatile((self.base + reg as usize) as *mut u32, value);
        }

        Self::barrier();
    }

    unsafe fn config_u8(&mut self, offset: usize) -> u8 {
        Self::barrier();
        let value = unsafe { core::ptr::read_volatile((self.base + 0x100 + offset) as *const u8) };

        Self::barrier();
        value
    }

    unsafe fn config_u16(&mut self, offset: usize) -> u16 {
        Self::barrier();

        let value = unsafe { core::ptr::read_volatile((self.base + 0x100 + offset) as *const u16) };

        Self::barrier();
        u16::from_le(value)
    }
}

// Private access boundary: only the state machine below supplies register
// directions, negotiated field widths and lifetime-safe DMA addresses.
struct Ordered<R: Registers>(R);
impl<R: Registers> Ordered<R> {
    fn read(&mut self, reg: Register) -> u32 {
        unsafe { self.0.read(reg) }
    }

    fn write(&mut self, reg: Register, value: u32) {
        unsafe {
            self.0.write(reg, value);
        }
    }

    fn config_u8(&mut self, offset: usize) -> u8 {
        unsafe { self.0.config_u8(offset) }
    }

    fn config_u16(&mut self, offset: usize) -> u16 {
        unsafe { self.0.config_u16(offset) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BadMagic,
    LegacyOrUnknownVersion,
    WrongDevice,
    InvalidBudget,
    ResetTimeout,
    WrongState,
    MissingFeatures,
    FeaturesRejected,
    DeviceNeedsReset,
    DeviceFailed,
    StatusRejected,
    InvalidQueue,
    QueueUnavailable,
    QueueAlreadyReady,
    Allocation,
    QueueNotReady,
    ConfigurationUnstable,
    Ring(split::Error),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Uninitialized,
    Stopped,
    Negotiated,
    Running,
    Faulted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetConfig {
    pub mac: [u8; 6],
    pub link_up: Option<bool>,
}

/// Device-wide reset owner shared by every queue of a typed device.
/// No ring references escape; reset frees storage only after reading status zero.
pub struct Transport<R: Registers, T, const QUEUES: usize> {
    io: Ordered<R>,
    allocator: DmaAllocator,
    queues: [Option<SplitRing<DmaRingMemory, T>>; QUEUES],
    required: u64,
    supported: u64,
    state: State,
    features: u64,
    poll_budget: usize,
}

/// Two-queue network transport. Requires modern features and a supplied MAC.
pub type NetTransport<R, T = ()> = Transport<R, T, 2>;

/// One-queue entropy transport. No device-specific features or config fields.
pub type RngTransport<R, T = ()> = Transport<R, T, 1>;

impl<R: Registers, T, const QUEUES: usize> Transport<R, T, QUEUES> {
    /// # Safety
    /// `io` and `allocator` must refer to the same exclusively owned no-IOMMU DMA
    /// domain. No external submissions or queue storage may be outstanding.
    /// The device must obey Virtio reset and DMA bounds. Cloned allocator handles
    /// may allocate other regions but cannot alias this transport's allocations.
    unsafe fn new_device(
        io: R,
        allocator: DmaAllocator,
        poll_budget: usize,
        device_id: u32,
        required: u64,
        supported: u64,
    ) -> Result<Self, Error> {
        let mut io = Ordered(io);
        if poll_budget == 0 {
            return Err(Error::InvalidBudget);
        }

        if io.read(Register::Magic) != 0x7472_6976 {
            return Err(Error::BadMagic);
        }

        if io.read(Register::Version) != 2 {
            return Err(Error::LegacyOrUnknownVersion);
        }

        if io.read(Register::DeviceId) != device_id {
            return Err(Error::WrongDevice);
        }

        Ok(Self {
            io,
            allocator,
            queues: core::array::from_fn(|_| None),
            required,
            supported,
            state: State::Uninitialized,
            features: 0,
            poll_budget,
        })
    }

    pub fn negotiated_features(&self) -> u64 {
        self.features
    }

    pub fn queue_size(&self, index: usize) -> Option<u16> {
        self.queues
            .get(index)
            .and_then(|q| q.as_ref().map(|q| q.size()))
    }

    fn status(&mut self) -> Result<u32, Error> {
        let status = self.io.read(Register::Status);
        if status & NEEDS_RESET != 0 {
            self.state = State::Faulted;
            return Err(Error::DeviceNeedsReset);
        }

        if status & FAILED != 0 {
            self.state = State::Faulted;
            return Err(Error::DeviceFailed);
        }

        Ok(status)
    }

    fn fail(&mut self, error: Error) -> Error {
        self.state = State::Faulted;
        let status = self.io.read(Register::Status);
        self.io.write(Register::Status, status | FAILED);

        error
    }

    fn set_status(&mut self, value: u32) -> Result<(), Error> {
        self.io.write(Register::Status, value);
        let observed = self.status()?;
        if observed & value != value {
            return Err(self.fail(Error::StatusRejected));
        }

        Ok(())
    }

    fn require_live(&mut self) -> Result<(), Error> {
        if !matches!(self.state, State::Negotiated | State::Running) {
            return Err(Error::WrongState);
        }

        let expected = ACKNOWLEDGE
            | DRIVER
            | FEATURES_OK
            | if self.state == State::Running {
                DRIVER_OK
            } else {
                0
            };

        if self.status()? & expected != expected {
            return Err(self.fail(Error::StatusRejected));
        }

        Ok(())
    }

    /// Sticky successful stop; bounded in register reads, not wall time.
    /// Failure retains all device queues and prevents further setup or notification.
    pub fn reset(&mut self) -> Result<(), Error> {
        if self.state == State::Stopped {
            return Ok(());
        }

        self.state = State::Faulted;
        self.io.write(Register::Status, 0);
        for _ in 0..self.poll_budget {
            if self.io.read(Register::Status) == 0 {
                for queue in self.queues.iter_mut().flatten() {
                    // Ordered status zero proves ring AND payload DMA ended.
                    unsafe {
                        queue.quiesce();
                    }
                }

                self.queues = core::array::from_fn(|_| None);
                self.features = 0;
                self.state = State::Stopped;
                return Ok(());
            }

            core::hint::spin_loop();
        }

        Err(Error::ResetTimeout)
    }

    /// Negotiate this device type's fixed allowlist; unsupported features stay off.
    pub fn initialize(&mut self) -> Result<u64, Error> {
        self.reset()?;
        self.state = State::Uninitialized;
        self.set_status(ACKNOWLEDGE)?;
        self.set_status(ACKNOWLEDGE | DRIVER)?;
        self.io.write(Register::DeviceFeaturesSel, 0);
        let low = self.io.read(Register::DeviceFeatures) as u64;
        self.io.write(Register::DeviceFeaturesSel, 1);
        let offered = low | ((self.io.read(Register::DeviceFeatures) as u64) << 32);
        if offered & self.required != self.required {
            return Err(self.fail(Error::MissingFeatures));
        }

        let selected = offered & self.supported;
        self.io.write(Register::DriverFeaturesSel, 0);

        self.io.write(Register::DriverFeatures, selected as u32);

        self.io.write(Register::DriverFeaturesSel, 1);

        self.io
            .write(Register::DriverFeatures, (selected >> 32) as u32);

        self.io
            .write(Register::Status, ACKNOWLEDGE | DRIVER | FEATURES_OK);

        let status = self.status()?;
        if status & FEATURES_OK == 0 {
            return Err(self.fail(Error::FeaturesRejected));
        }

        if status & (ACKNOWLEDGE | DRIVER) != ACKNOWLEDGE | DRIVER {
            return Err(self.fail(Error::StatusRejected));
        }

        self.features = selected;
        self.state = State::Negotiated;

        Ok(selected)
    }

    /// Configure an empty split ring, choosing a power of two for both preferred and max sizes.
    /// Region ownership is installed before any address becomes device visible.
    pub fn configure_queue(&mut self, index: usize, preferred: u16) -> Result<u16, Error> {
        if self.state != State::Negotiated {
            return Err(Error::WrongState);
        }

        self.require_live()?;
        if index >= QUEUES || preferred == 0 {
            return Err(Error::InvalidQueue);
        }

        if self.queues[index].is_some() {
            return Err(Error::QueueAlreadyReady);
        }

        self.io.write(Register::QueueSel, index as u32);
        if self.io.read(Register::QueueReady) != 0 {
            return Err(self.fail(Error::QueueAlreadyReady));
        }

        let maximum = self
            .io
            .read(Register::QueueNumMax)
            .min(preferred as u32)
            .min(32768);

        if maximum == 0 {
            return Err(Error::QueueUnavailable);
        }

        let size = (1u32 << (31 - maximum.leading_zeros())) as u16;
        let layout = split::Layout::new(size).map_err(Error::Ring)?;
        let region = self
            .allocator
            .allocate(layout.bytes, 4096)
            .map_err(|_| Error::Allocation)?;

        let mut ring = SplitRing::new(DmaRingMemory::new(region), size).map_err(Error::Ring)?;
        let (descriptor, available, used) = ring.addresses();

        // Activate quarantine before exposing addresses or QueueReady.
        unsafe {
            ring.activate();
        }

        self.queues[index] = Some(ring);
        self.io.write(Register::QueueNum, size as u32);

        for (low, high, address) in [
            (Register::QueueDescLow, Register::QueueDescHigh, descriptor),
            (
                Register::QueueDriverLow,
                Register::QueueDriverHigh,
                available,
            ),
            (Register::QueueDeviceLow, Register::QueueDeviceHigh, used),
        ] {
            self.io.write(low, address as u32);
            self.io.write(high, (address >> 32) as u32);
        }

        self.io.write(Register::QueueReady, 1);
        if self.io.read(Register::QueueReady) != 1 {
            return Err(self.fail(Error::QueueNotReady));
        }

        self.require_live()?;
        Ok(size)
    }

    pub fn start(&mut self) -> Result<(), Error> {
        if self.state != State::Negotiated {
            return Err(Error::WrongState);
        }

        self.require_live()?;
        if self.queues.iter().any(Option::is_none) {
            return Err(Error::QueueNotReady);
        }

        self.set_status(ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK)?;
        self.state = State::Running;

        Ok(())
    }

    /// Explicit notification is also valid for empty queues (spurious kicks).
    /// The register backend orders prior DMA writes before the notification.
    pub fn notify(&mut self, index: usize) -> Result<(), Error> {
        if self.state != State::Running {
            return Err(Error::WrongState);
        }

        self.require_live()?;
        if index >= QUEUES {
            return Err(Error::InvalidQueue);
        }

        self.io.write(Register::QueueNotify, index as u32);
        Ok(())
    }

    /// Publish a direct chain and notify if needed. Errors consume no owner;
    /// after publication no fallible status read can turn acceptance into Err.
    /// # Safety
    /// Same payload/domain/ownership contract as `SplitRing::submit`. T must retain
    /// all backing allocations/mappings with no competing CPU access until returned
    /// by completion or released on confirmed reset. Device-specific payload semantics belong to the adapter.
    pub unsafe fn submit(
        &mut self,
        index: usize,
        token: u64,
        buffers: &[Buffer],
        owner: T,
    ) -> Result<(), (Error, T)> {
        if self.state != State::Running {
            return Err((Error::WrongState, owner));
        }

        if let Err(e) = self.require_live() {
            return Err((e, owner));
        }

        let Some(ring) = self.queues.get_mut(index).and_then(Option::as_mut) else {
            return Err((Error::InvalidQueue, owner));
        };

        let published = unsafe { ring.submit(token, buffers, owner) }
            .map_err(|(e, owner)| (Error::Ring(e), owner))?;

        if published.notify {
            self.io.write(Register::QueueNotify, index as u32);
        }

        Ok(())
    }

    /// Bounded to one completion. Detectable corruption stops all device queues until
    /// reset; no suspicious entry can return a payload owner to the caller.
    pub fn completion(&mut self, index: usize) -> Result<Option<Completion<T>>, Error> {
        if self.state != State::Running {
            return Err(Error::WrongState);
        }

        self.require_live()?;
        let ring = self
            .queues
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(Error::InvalidQueue)?;

        match ring.poll() {
            Ok(completion) => Ok(completion),
            Err(e) => Err(self.fail(Error::Ring(e))),
        }
    }

    pub fn free_descriptors(&self, index: usize) -> Option<u16> {
        self.queues
            .get(index)
            .and_then(|q| q.as_ref().map(|q| q.free_descriptors()))
    }

    pub fn pending_interrupts(&mut self) -> Result<u32, Error> {
        self.require_live()?;

        Ok(self.io.read(Register::InterruptStatus) & 3)
    }

    /// Call only after handling these observed events undefined bits masked.
    pub fn acknowledge_interrupts(&mut self, handled: u32) -> Result<(), Error> {
        self.require_live()?;
        self.io.write(Register::InterruptAck, handled & 3);

        Ok(())
    }
}

impl<R: Registers, T> NetTransport<R, T> {
    /// # Safety
    /// Registers and allocator must share a domain with the following properties:
    /// exclusive ownership, coherent DMA (see `memory::dma`), and no IOMMU.
    /// The device must obey DMA bounds and reset.
    /// No external submissions or queue storage remaining.
    pub unsafe fn new(io: R, allocator: DmaAllocator, poll_budget: usize) -> Result<Self, Error> {
        unsafe { Self::new_device(io, allocator, poll_budget, 1, REQUIRED, SUPPORTED) }
    }

    pub fn read_config(&mut self) -> Result<NetConfig, Error> {
        self.require_live()?;
        for _ in 0..self.poll_budget {
            let before = self.io.read(Register::ConfigGeneration);
            let mut mac = [0; 6];
            for (i, byte) in mac.iter_mut().enumerate() {
                *byte = self.io.config_u8(i);
            }

            let link_up = if self.features & NET_STATUS != 0 {
                Some(self.io.config_u16(6) & 1 != 0)
            } else {
                None
            };

            let after = self.io.read(Register::ConfigGeneration);
            self.require_live()?;

            if before == after {
                return Ok(NetConfig { mac, link_up });
            }
        }

        Err(Error::ConfigurationUnstable)
    }
}

impl<R: Registers, T> RngTransport<R, T> {
    /// # Safety
    /// Same exclusive DMA domain and reset/completion requirements as
    /// NetTransport::new, for modern entropy device (ID 4).
    pub unsafe fn new(io: R, allocator: DmaAllocator, poll_budget: usize) -> Result<Self, Error> {
        unsafe { Self::new_device(io, allocator, poll_budget, 4, VERSION_1, VERSION_1) }
    }
}

impl<R: Registers, T, const QUEUES: usize> Drop for Transport<R, T, QUEUES> {
    fn drop(&mut self) {
        if self.reset().is_err() {
            for queue in &mut self.queues {
                if let Some(memory) = queue.take() {
                    core::mem::forget(memory);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

