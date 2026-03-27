# Aether NVMe User-Space Driver Design Document
## Version 0.1.0 -- Draft

---

## Table of Contents

1. NVMe Architecture Overview
2. Driver Initialization Sequence
3. I/O Path Design (The Hot Path)
4. Per-Core Queue Architecture
5. DMA Buffer Management
6. Error Handling
7. Integration with Async Executor
8. Key Data Structures

---

## 1. NVMe Architecture Overview

NVMe (Non-Volatile Memory Express) is a register-level interface for host
software to communicate with non-volatile memory subsystems over PCIe. This
section covers the subset of the NVMe 1.4 specification relevant to this
driver.

### 1.1 Controller Registers (BAR0)

The NVMe controller exposes its registers through PCI BAR0. All registers are
memory-mapped. The critical registers and their offsets:

| Offset | Register | Description |
|:-------|:---------|:------------|
| 0x00   | CAP      | Controller Capabilities (64-bit) |
| 0x08   | VS       | Version (32-bit) |
| 0x0C   | INTMS    | Interrupt Mask Set (32-bit) |
| 0x10   | INTMC    | Interrupt Mask Clear (32-bit) |
| 0x14   | CC       | Controller Configuration (32-bit) |
| 0x1C   | CSTS     | Controller Status (32-bit) |
| 0x20   | NSSR     | NVM Subsystem Reset (32-bit) |
| 0x24   | AQA      | Admin Queue Attributes (32-bit) |
| 0x28   | ASQ      | Admin Submission Queue Base Address (64-bit) |
| 0x30   | ACQ      | Admin Completion Queue Base Address (64-bit) |
| 0x1000 | SQ0TDBL  | Submission Queue 0 Tail Doorbell (32-bit) |
| 0x1000 + (2y * doorbell_stride) | SQyTDBL | Submission Queue y Tail Doorbell |
| 0x1000 + ((2y+1) * doorbell_stride) | CQyHDBL | Completion Queue y Head Doorbell |

The doorbell stride is determined by bits 35:32 of the CAP register:
`doorbell_stride = 4 << CAP.DSTRD`. For most controllers, DSTRD=0, so the
stride is 4 bytes and doorbells are packed contiguously starting at offset
0x1000.

```rust
/// NVMe controller register layout, mapped from BAR0.
/// All fields are volatile MMIO -- reads/writes must use volatile operations.
///
/// Reference: NVMe spec 1.4, Section 3.1
#[repr(C)]
pub struct NvmeRegisters {
    /// Controller Capabilities -- read-only.
    /// Bits 15:0   MQES: Maximum Queue Entries Supported (0-based)
    /// Bits 23:16  CQR:  Contiguous Queues Required
    /// Bits 27:24  AMS:  Arbitration Mechanism Supported
    /// Bits 31:28  TO:   Timeout (in 500ms units)
    /// Bits 35:32  DSTRD: Doorbell Stride
    /// Bit  37     CSS:  NVM Command Set Supported
    /// Bits 47:44  MPSMIN: Memory Page Size Minimum (2 ^ (12 + MPSMIN))
    /// Bits 51:48  MPSMAX: Memory Page Size Maximum
    pub cap: Volatile<u64>,        // 0x00

    /// Version -- read-only. Major.Minor.Tertiary.
    pub vs: Volatile<u32>,         // 0x08

    /// Interrupt Mask Set -- write-only for legacy interrupts.
    /// Not used with MSI-X.
    pub intms: Volatile<u32>,      // 0x0C

    /// Interrupt Mask Clear -- write-only for legacy interrupts.
    pub intmc: Volatile<u32>,      // 0x10

    /// Controller Configuration -- read/write.
    /// Bit  0     EN:    Enable
    /// Bits 6:4   CSS:   Command Set Selected
    /// Bits 10:7  MPS:   Memory Page Size (2 ^ (12 + MPS))
    /// Bits 13:11 AMS:   Arbitration Mechanism Selected
    /// Bits 19:16 IOSQES: I/O SQ Entry Size (2^n)
    /// Bits 23:20 IOCQES: I/O CQ Entry Size (2^n)
    pub cc: Volatile<u32>,         // 0x14

    _reserved0: u32,               // 0x18

    /// Controller Status -- read-only.
    /// Bit  0  RDY:  Ready
    /// Bit  1  CFS:  Controller Fatal Status
    /// Bit  4  NSSRO: NVM Subsystem Reset Occurred
    pub csts: Volatile<u32>,       // 0x1C

    /// NVM Subsystem Reset (optional).
    pub nssr: Volatile<u32>,       // 0x20

    /// Admin Queue Attributes -- read/write.
    /// Bits 11:0   ASQS: Admin SQ Size (0-based)
    /// Bits 27:16  ACQS: Admin CQ Size (0-based)
    pub aqa: Volatile<u32>,        // 0x24

    /// Admin Submission Queue Base Address -- read/write.
    /// Must be page-aligned (4096).
    pub asq: Volatile<u64>,        // 0x28

    /// Admin Completion Queue Base Address -- read/write.
    /// Must be page-aligned (4096).
    pub acq: Volatile<u64>,        // 0x30
}
```

The `Volatile<T>` wrapper enforces volatile reads and writes at the type level,
preventing the compiler from eliding or reordering MMIO accesses:

```rust
/// Wrapper that forces volatile access. No interior mutability tricks --
/// every read is a load, every write is a store, the compiler cannot optimize
/// either away.
#[repr(transparent)]
pub struct Volatile<T: Copy> {
    value: T,
}

impl<T: Copy> Volatile<T> {
    #[inline(always)]
    pub fn read(&self) -> T {
        unsafe { core::ptr::read_volatile(&self.value) }
    }

    #[inline(always)]
    pub fn write(&mut self, val: T) {
        unsafe { core::ptr::write_volatile(&mut self.value, val) }
    }
}
```

### 1.2 Submission and Completion Queues

NVMe communication is built on paired ring buffers: Submission Queues (SQ) for
host-to-controller commands, and Completion Queues (CQ) for controller-to-host
responses.

**Submission Queue Entry (SQE) -- 64 bytes:**

Every NVMe command is exactly 64 bytes. The first 16 bytes (Command Dword 0
through the NSID field) are common across all command types. The remaining 48
bytes are command-specific.

```rust
/// NVMe Submission Queue Entry -- 64 bytes, the universal command format.
///
/// Reference: NVMe spec 1.4, Figure 106
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvmeCommand {
    /// Command Dword 0:
    ///   Bits 7:0   OPC:  Opcode
    ///   Bits 9:8   FUSE: Fused operation (0 = normal)
    ///   Bits 15:10 Reserved
    ///   Bits 31:16 CID:  Command Identifier
    pub cdw0: u32,

    /// Namespace Identifier. 0xFFFFFFFF for commands not namespace-specific.
    pub nsid: u32,

    /// Reserved (command Dword 2 and 3).
    pub cdw2: u32,
    pub cdw3: u32,

    /// Metadata Pointer (MPTR). Physical address of metadata buffer.
    pub mptr: u64,

    /// Data Pointer -- PRP Entry 1. Physical address of data buffer
    /// or first PRP list entry.
    pub prp1: u64,

    /// Data Pointer -- PRP Entry 2. Second PRP entry, or physical address
    /// of a PRP list if the transfer spans more than 2 pages.
    pub prp2: u64,

    /// Command-specific Dwords 10-15.
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

const _: () = assert!(core::mem::size_of::<NvmeCommand>() == 64);
```

**Completion Queue Entry (CQE) -- 16 bytes:**

```rust
/// NVMe Completion Queue Entry -- 16 bytes.
///
/// Reference: NVMe spec 1.4, Figure 123
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NvmeCompletion {
    /// Command-specific result (Dword 0).
    pub result: u32,

    /// Reserved.
    pub rsvd: u32,

    /// SQ Head Pointer -- the controller's view of the SQ head.
    /// Bits 15:0  SQHD
    /// Bits 31:16 SQID: SQ Identifier
    pub sq_head_and_id: u32,

    /// Status and Command ID.
    /// Bits 15:0  CID:  Command Identifier (matches the SQE CID)
    /// Bit  16    P:    Phase Tag
    /// Bits 31:17 SF:   Status Field
    ///   Bits 24:17  SC:  Status Code
    ///   Bits 27:25  SCT: Status Code Type
    ///   Bit  28     CRD: Command Retry Delay
    ///   Bit  29     M:   More
    ///   Bit  30     DNR: Do Not Retry
    pub status_and_cid: u32,
}

const _: () = assert!(core::mem::size_of::<NvmeCompletion>() == 16);

impl NvmeCompletion {
    /// Extract the Command Identifier from the completion entry.
    #[inline]
    pub fn cid(&self) -> u16 {
        (self.status_and_cid & 0xFFFF) as u16
    }

    /// Extract the Phase Tag bit.
    #[inline]
    pub fn phase(&self) -> bool {
        (self.status_and_cid >> 16) & 1 == 1
    }

    /// Extract the Status Code Type (SCT) and Status Code (SC).
    /// Returns (SCT, SC). A successful completion is (0, 0).
    #[inline]
    pub fn status(&self) -> (u8, u8) {
        let sf = self.status_and_cid >> 17;
        let sc = (sf & 0xFF) as u8;
        let sct = ((sf >> 8) & 0x7) as u8;
        (sct, sc)
    }

    /// True if this completion indicates success (SCT=0, SC=0).
    #[inline]
    pub fn succeeded(&self) -> bool {
        self.status() == (0, 0)
    }

    /// Extract the SQ Head Pointer -- tells us how far the controller
    /// has consumed our submissions.
    #[inline]
    pub fn sq_head(&self) -> u16 {
        (self.sq_head_and_id & 0xFFFF) as u16
    }
}
```

### 1.3 Ring Buffer Mechanics

Both SQ and CQ operate as circular ring buffers with wrap-around semantics.

**Submission Queue (host-managed):** The host maintains the tail pointer. When
submitting a command, the host writes the SQE at `sq[tail]`, advances `tail =
(tail + 1) % depth`, then writes the new tail value to the SQ Tail Doorbell
register. The controller maintains the head pointer and reports it back in
completion entries.

**Completion Queue (controller-managed):** The controller writes CQEs at
`cq[head]` (from the controller's perspective), toggling a Phase Tag bit each
time the queue wraps. The host detects new completions by checking whether the
Phase Tag of the next CQE matches the host's expected phase. After processing
completions, the host writes the new head to the CQ Head Doorbell register.

The Phase Tag mechanism eliminates the need for the host to track the
controller's CQ tail pointer. The host simply scans forward through the CQ
until it finds an entry whose phase does not match the expected value.

### 1.4 Admin Queue vs I/O Queues

The NVMe spec defines two classes of queue pairs:

**Admin Queue Pair (QID 0):** Created during controller initialization by
writing physical addresses into ASQ/ACQ registers. Used exclusively for
management commands: Identify Controller, Identify Namespace, Create I/O
Completion Queue, Create I/O Submission Queue, Set Features, Get Features, etc.
The admin queue is limited to 4096 entries. There is exactly one admin queue
pair per controller.

**I/O Queue Pairs (QID 1+):** Created via admin commands (Create I/O CQ,
Create I/O SQ). Used for all data transfer commands: Read, Write, Flush,
Dataset Management (TRIM/Deallocate). In this driver, we create one I/O queue
pair per core to eliminate cross-core synchronization.

### 1.5 NVMe Command Opcodes

The opcodes used by this driver:

**Admin opcodes (submitted to QID 0):**

| Opcode | Command |
|:-------|:--------|
| 0x05   | Create I/O Completion Queue |
| 0x01   | Create I/O Submission Queue |
| 0x06   | Identify |
| 0x09   | Set Features |
| 0x0A   | Get Features |
| 0x08   | Abort |

**I/O opcodes (submitted to QID 1+):**

| Opcode | Command |
|:-------|:--------|
| 0x01   | Write |
| 0x02   | Read |
| 0x04   | Dataset Management (TRIM) |
| 0x00   | Flush |

```rust
/// NVMe Admin Command opcodes.
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum AdminOpcode {
    DeleteIoSq         = 0x00,
    CreateIoSq         = 0x01,
    GetLogPage         = 0x02,
    DeleteIoCq         = 0x04,
    CreateIoCq         = 0x05,
    Identify           = 0x06,
    Abort              = 0x08,
    SetFeatures        = 0x09,
    GetFeatures        = 0x0A,
    AsyncEventRequest  = 0x0C,
}

/// NVMe I/O Command opcodes (NVM Command Set).
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum IoOpcode {
    Flush              = 0x00,
    Write              = 0x01,
    Read               = 0x02,
    DatasetManagement  = 0x09,
}
```

### 1.6 Doorbell Registers

Each queue has a 32-bit doorbell register. Submission queues have a Tail
Doorbell; completion queues have a Head Doorbell. The host writes these to
notify the controller of new submissions or processed completions.

Doorbell addresses are computed from the base (BAR0 + 0x1000):

```
SQ y Tail Doorbell = 0x1000 + (2y       * (4 << CAP.DSTRD))
CQ y Head Doorbell = 0x1000 + ((2y + 1) * (4 << CAP.DSTRD))
```

Writing to a doorbell is a posted PCIe write (fire-and-forget from the host's
perspective). The write-combining flag on the doorbell page can coalesce
multiple doorbell writes if the CPU supports it, but for correctness each
doorbell write must be a single 32-bit store.

---

## 2. Driver Initialization Sequence

Initialization is the only phase that requires ASI syscalls. Once complete, all
I/O operates entirely in user-space. The sequence is designed to minimize kernel
transitions by batching ASI operations.

### 2.1 Device Discovery

```rust
/// Discover NVMe controllers on the PCIe bus.
/// NVMe devices have PCI class code 0x0108 (Mass Storage: NVM).
pub fn discover_nvme_controllers() -> Result<PciDeviceInfo, NvmeError> {
    let filter = DeviceFilter {
        vendor_id: None,
        device_class: Some(0x0108),  // NVM Express
    };
    let mut devices = [PciDeviceInfo::zeroed(); 8];
    let count = asi::device::dev_enumerate(&filter, &mut devices)
        .map_err(|_| NvmeError::NoDeviceFound)?;

    if count == 0 {
        return Err(NvmeError::NoDeviceFound);
    }

    // Select the first unclaimed NVMe controller.
    devices[..count]
        .iter()
        .find(|d| !d.claimed)
        .copied()
        .ok_or(NvmeError::AllDevicesClaimed)
}
```

### 2.2 Capability Acquisition

The driver needs three types of capabilities from the kernel:

1. **DeviceBar** for BAR0 -- gives access to the NVMe controller register set.
2. **DmaPool** -- a region of physically contiguous, IOMMU-mapped memory for
   queue buffers, PRP lists, and I/O data buffers.
3. **MsixVector** -- one per I/O completion queue, plus one for the admin CQ.

These are acquired in a single batched syscall:

```rust
/// Capabilities required by the NVMe driver.
pub struct NvmeCapabilities {
    pub bar0: CapHandle,
    pub dma_pool: CapHandle,
    pub msix_vectors: [CapHandle; MAX_IO_QUEUES + 1], // +1 for admin CQ
}

pub fn acquire_capabilities(
    dev: &PciDeviceInfo,
    num_io_queues: usize,
) -> Result<NvmeCapabilities, NvmeError> {
    // Size the DMA pool:
    //   Admin SQ + CQ:  2 * 64 * 4096 = 512 KB (generous)
    //   Per I/O queue pair: SQ(64*queue_depth) + CQ(16*queue_depth)
    //   PRP list pool: 4096 * num_io_queues
    //   Data buffer pool: sized separately (application-dependent)
    let queue_depth = 256;
    let dma_size = (2 * 64 * 4096)  // admin queues (oversized for alignment)
        + num_io_queues * (64 * queue_depth + 16 * queue_depth)  // I/O queues
        + num_io_queues * 4096  // PRP list pages
        + 16 * 1024 * 1024;    // 16 MB I/O buffer pool

    let num_vectors = num_io_queues + 1;

    // Build batch: 1 DeviceBar + 1 DmaPool + N MsixVectors
    // Total ops: 2 + num_vectors
    let mut ops: Vec<BatchOp> = Vec::new();

    ops.push(BatchOp::new(AsiOp::CapRequest, &CapRequest::DeviceBar {
        pci_addr: dev.address,
        bar_index: 0,
    }));

    ops.push(BatchOp::new(AsiOp::CapRequest, &CapRequest::DmaPool {
        size_bytes: dma_size,
        alignment: 4096,
        device: dev.address,
    }));

    for vec_idx in 0..num_vectors {
        ops.push(BatchOp::new(AsiOp::CapRequest, &CapRequest::MsixVector {
            pci_addr: dev.address,
            vector: vec_idx as u16,
        }));
    }

    asi::batch(&mut ops)?;

    // Extract handles from results.
    // (error handling omitted for clarity -- each op has its own result)
    Ok(NvmeCapabilities {
        bar0: ops[0].result.as_cap_handle()?,
        dma_pool: ops[1].result.as_cap_handle()?,
        msix_vectors: core::array::from_fn(|i| {
            ops[2 + i].result.as_cap_handle().unwrap()
        }),
    })
}
```

### 2.3 Memory Mapping

Two memory regions are mapped:

1. **BAR0 (controller registers):** Mapped with `UNCACHEABLE` because MMIO
   registers must not be cached. The CPU must observe every read/write at the
   device, not a stale cache line.

2. **DMA pool:** Mapped with `READ | WRITE` (normal caching). DMA coherency
   is maintained by the platform (x86 is cache-coherent for DMA). The IOMMU
   allows the NVMe controller to access these physical pages.

```rust
pub struct NvmeMappings {
    pub regs: MappedRegion,   // BAR0 -- controller registers
    pub dma: MappedRegion,    // DMA pool -- queues + buffers
}

pub fn map_resources(
    caps: &NvmeCapabilities,
    bar0_size: usize,
    dma_size: usize,
) -> Result<NvmeMappings, NvmeError> {
    let mut ops = [
        BatchOp::new(AsiOp::MemMap, &MemMapArgs {
            handle: caps.bar0,
            offset: 0,
            length: bar0_size,
            flags: MapFlags::READ | MapFlags::WRITE | MapFlags::UNCACHEABLE,
        }),
        BatchOp::new(AsiOp::MemMap, &MemMapArgs {
            handle: caps.dma_pool,
            offset: 0,
            length: dma_size,
            flags: MapFlags::READ | MapFlags::WRITE,
        }),
    ];

    asi::batch(&mut ops)?;

    Ok(NvmeMappings {
        regs: ops[0].result.as_mapped_region()?,
        dma: ops[1].result.as_mapped_region()?,
    })
}
```

### 2.4 Controller Reset and Initialization

After mapping BAR0, the driver performs the NVMe controller reset and
initialization sequence defined in NVMe spec Section 7.6.1.

```rust
impl NvmeController {
    /// Reset the NVMe controller and bring it to a ready state.
    ///
    /// Follows the NVMe 1.4 initialization sequence (Section 7.6.1):
    /// 1. Set CC.EN = 0 and wait for CSTS.RDY = 0
    /// 2. Configure admin queue addresses and sizes
    /// 3. Set CC.EN = 1 with desired configuration
    /// 4. Wait for CSTS.RDY = 1
    pub fn reset_and_init(
        regs: &mut NvmeRegisters,
        admin_sq_phys: PhysicalAddress,
        admin_cq_phys: PhysicalAddress,
        admin_queue_depth: u16,
    ) -> Result<(), NvmeError> {
        // Step 1: Disable the controller.
        let mut cc = regs.cc.read();
        cc &= !(1 << 0); // Clear CC.EN
        regs.cc.write(cc);

        // Wait for CSTS.RDY = 0.
        // The timeout is CAP.TO * 500ms. Read CAP.TO from bits 31:24.
        let cap = regs.cap.read();
        let timeout_ms = (((cap >> 24) & 0xFF) as u64) * 500;
        let deadline = tsc_deadline_ms(timeout_ms);

        loop {
            let csts = regs.csts.read();
            if csts & 1 == 0 {
                break; // RDY = 0, controller is disabled
            }
            if csts & (1 << 1) != 0 {
                return Err(NvmeError::ControllerFatalStatus);
            }
            if tsc_now() > deadline {
                return Err(NvmeError::ResetTimeout);
            }
            core::hint::spin_loop();
        }

        // Step 2: Configure admin queues.
        // AQA: ACQS (bits 27:16) and ASQS (bits 11:0) are 0-based.
        let aqa = ((admin_queue_depth as u32 - 1) << 16)
                | (admin_queue_depth as u32 - 1);
        regs.aqa.write(aqa);
        regs.asq.write(admin_sq_phys.0);
        regs.acq.write(admin_cq_phys.0);

        // Step 3: Configure and enable the controller.
        // CC fields:
        //   EN     = 1       (bit 0)
        //   CSS    = 0b000   (bits 6:4) -- NVM Command Set
        //   MPS    = 0       (bits 10:7) -- 4 KB pages (2^(12+0))
        //   AMS    = 0b000   (bits 13:11) -- Round Robin
        //   IOSQES = 6       (bits 19:16) -- 2^6 = 64 bytes per SQ entry
        //   IOCQES = 4       (bits 23:20) -- 2^4 = 16 bytes per CQ entry
        let cc_val: u32 = (1 << 0)        // EN
            | (0 << 4)                     // CSS = NVM
            | (0 << 7)                     // MPS = 4K
            | (0 << 11)                    // AMS = Round Robin
            | (6 << 16)                    // IOSQES = 64B
            | (4 << 20);                   // IOCQES = 16B
        regs.cc.write(cc_val);

        // Step 4: Wait for CSTS.RDY = 1.
        let deadline = tsc_deadline_ms(timeout_ms);
        loop {
            let csts = regs.csts.read();
            if csts & 1 == 1 {
                return Ok(()); // Controller is ready
            }
            if csts & (1 << 1) != 0 {
                return Err(NvmeError::ControllerFatalStatus);
            }
            if tsc_now() > deadline {
                return Err(NvmeError::InitTimeout);
            }
            core::hint::spin_loop();
        }
    }
}
```

### 2.5 Admin Queue Creation

The admin queue pair is special: it is configured via register writes (ASQ,
ACQ, AQA) rather than via commands. The memory for the admin SQ and CQ must be
allocated from the DMA pool and translated to physical addresses.

```rust
/// Allocate and configure the admin queue pair.
pub fn create_admin_queues(
    dma_alloc: &mut DmaAllocator,
    regs: &mut NvmeRegisters,
) -> Result<NvmeQueuePair, NvmeError> {
    let depth: u16 = 32; // Admin queue does not need to be deep

    // Allocate SQ memory: depth * 64 bytes, 4096-byte aligned.
    let sq_buf = dma_alloc.allocate(
        depth as usize * 64,
        4096,
    )?;

    // Allocate CQ memory: depth * 16 bytes, 4096-byte aligned.
    let cq_buf = dma_alloc.allocate(
        depth as usize * 16,
        4096,
    )?;

    // Zero the queue memory.
    unsafe {
        core::ptr::write_bytes(sq_buf.vaddr, 0, sq_buf.len);
        core::ptr::write_bytes(cq_buf.vaddr, 0, cq_buf.len);
    }

    // The physical addresses are written to ASQ/ACQ during reset_and_init.
    // See Section 2.4 above.

    Ok(NvmeQueuePair {
        qid: 0,
        sq: SubmissionQueue {
            entries: sq_buf.vaddr as *mut NvmeCommand,
            phys: sq_buf.paddr,
            depth,
            tail: 0,
            head: 0,
            doorbell: doorbell_ptr(regs, 0, true),
        },
        cq: CompletionQueue {
            entries: cq_buf.vaddr as *mut NvmeCompletion,
            phys: cq_buf.paddr,
            depth,
            head: 0,
            phase: true,
            doorbell: doorbell_ptr(regs, 0, false),
        },
        free_cids: (0..depth).rev().collect(),
        inflight: InflightMap::new(depth),
    })
}

/// Compute the doorbell register address for a given queue.
/// `is_sq` selects between SQ Tail Doorbell and CQ Head Doorbell.
fn doorbell_ptr(
    regs: &NvmeRegisters,
    qid: u16,
    is_sq: bool,
) -> *mut Volatile<u32> {
    let cap = regs.cap.read();
    let dstrd = ((cap >> 32) & 0xF) as usize;
    let stride = 4usize << dstrd;
    let base = regs as *const NvmeRegisters as *mut u8;
    let offset = 0x1000 + ((2 * qid as usize + if is_sq { 0 } else { 1 }) * stride);
    unsafe { base.add(offset) as *mut Volatile<u32> }
}
```

### 2.6 Identify Controller and Identify Namespace

After the admin queue is operational, the driver issues Identify commands to
learn about the controller and its namespaces.

**Identify Controller (CNS=1):** Returns a 4096-byte data structure describing
controller capabilities -- maximum data transfer size, number of namespaces,
firmware revision, serial number, etc.

**Identify Namespace (CNS=0, NSID=1):** Returns namespace metadata -- LBA
format, capacity, supported features. The LBA size (typically 512B or 4096B)
determines how we translate byte offsets to LBA addresses.

```rust
/// Issue an Identify Controller command via the admin queue.
pub fn identify_controller(
    admin: &mut NvmeQueuePair,
    dma_alloc: &mut DmaAllocator,
) -> Result<IdentifyController, NvmeError> {
    // Allocate a 4096-byte DMA buffer for the identify data.
    let buf = dma_alloc.allocate(4096, 4096)?;

    let cmd = NvmeCommand {
        cdw0: build_cdw0(AdminOpcode::Identify as u8, admin.alloc_cid()?),
        nsid: 0,
        cdw2: 0,
        cdw3: 0,
        mptr: 0,
        prp1: buf.paddr.0,
        prp2: 0,
        cdw10: 1, // CNS = 1 (Identify Controller)
        cdw11: 0,
        cdw12: 0,
        cdw13: 0,
        cdw14: 0,
        cdw15: 0,
    };

    admin.submit_and_poll(cmd)?;

    // Parse the identify data from the DMA buffer.
    Ok(unsafe { *(buf.vaddr as *const IdentifyController) })
}

/// Key fields from the Identify Controller data structure.
/// Full structure is 4096 bytes; we only extract what we need.
#[repr(C)]
pub struct IdentifyController {
    pub vid: u16,            // Offset 0: PCI Vendor ID
    pub ssvid: u16,          // Offset 2: Subsystem Vendor ID
    pub sn: [u8; 20],        // Offset 4: Serial Number
    pub mn: [u8; 40],        // Offset 24: Model Number
    pub fr: [u8; 8],         // Offset 64: Firmware Revision
    pub rab: u8,             // Offset 72: Recommended Arb Burst
    pub ieee: [u8; 3],       // Offset 73: IEEE OUI
    _pad0: [u8; 178],
    pub mdts: u8,            // Offset 77: Maximum Data Transfer Size
                             //   (in units of 2^(12 + CAP.MPSMIN) bytes)
                             //   0 = no limit imposed by controller
    // ... remaining fields up to 4096 bytes
    _pad1: [u8; 3818],
    pub nn: u32,             // Offset 516: Number of Namespaces
}
```

### 2.7 I/O Queue Pair Creation

I/O queues are created via admin commands. The CQ must be created before its
associated SQ, because the SQ creation command references the CQ ID.

```rust
/// Create one I/O Completion Queue via the admin queue.
fn create_io_cq(
    admin: &mut NvmeQueuePair,
    qid: u16,
    depth: u16,
    cq_phys: PhysicalAddress,
    msix_vector: u16,
) -> Result<(), NvmeError> {
    let cmd = NvmeCommand {
        cdw0: build_cdw0(AdminOpcode::CreateIoCq as u8, admin.alloc_cid()?),
        nsid: 0,
        cdw2: 0, cdw3: 0, mptr: 0,
        prp1: cq_phys.0,
        prp2: 0,
        // CDW10: QID (15:0) | Queue Size 0-based (31:16)
        cdw10: (qid as u32) | (((depth - 1) as u32) << 16),
        // CDW11: PC=1 (physically contiguous) | IEN=1 (interrupts enabled) |
        //        IV (31:16) = MSI-X vector
        cdw11: 0b11 | ((msix_vector as u32) << 16),
        cdw12: 0, cdw13: 0, cdw14: 0, cdw15: 0,
    };
    admin.submit_and_poll(cmd)
}

/// Create one I/O Submission Queue via the admin queue.
fn create_io_sq(
    admin: &mut NvmeQueuePair,
    qid: u16,
    depth: u16,
    sq_phys: PhysicalAddress,
    cqid: u16,
) -> Result<(), NvmeError> {
    let cmd = NvmeCommand {
        cdw0: build_cdw0(AdminOpcode::CreateIoSq as u8, admin.alloc_cid()?),
        nsid: 0,
        cdw2: 0, cdw3: 0, mptr: 0,
        prp1: sq_phys.0,
        prp2: 0,
        // CDW10: QID (15:0) | Queue Size 0-based (31:16)
        cdw10: (qid as u32) | (((depth - 1) as u32) << 16),
        // CDW11: PC=1 | QPRIO=0 (medium) | CQID (31:16)
        cdw11: 0b01 | ((cqid as u32) << 16),
        cdw12: 0, cdw13: 0, cdw14: 0, cdw15: 0,
    };
    admin.submit_and_poll(cmd)
}

/// Build CDW0 from opcode and command ID.
#[inline]
fn build_cdw0(opcode: u8, cid: u16) -> u32 {
    (opcode as u32) | ((cid as u32) << 16)
}
```

### 2.8 Interrupt Binding

Each completion queue gets its own MSI-X vector, bound to the core that will
poll that queue. This is done via ASI's `irq_attach`:

```rust
/// Bind MSI-X vectors to EventSlots, one per CQ, pinned to their target core.
pub fn bind_interrupts(
    msix_caps: &[CapHandle],
    event_slots: &[EventSlot],
    core_ids: &[CoreId],
) -> Result<Vec<IrqBinding>, NvmeError> {
    let mut bindings = Vec::with_capacity(msix_caps.len());

    // This can be batched into a single asi_batch call.
    let mut ops: Vec<BatchOp> = msix_caps.iter().enumerate().map(|(i, cap)| {
        BatchOp::new(AsiOp::IrqAttach, &IrqAttachArgs {
            msix_handle: *cap,
            slot: &event_slots[i],
            target_core: Some(core_ids[i]),
        })
    }).collect();

    asi::batch(&mut ops)?;

    for op in &ops {
        bindings.push(op.result.as_irq_binding()?);
    }

    Ok(bindings)
}
```

### 2.9 Complete Initialization Summary

The full initialization requires exactly 3-4 ASI syscalls:

| Syscall | Operations batched | Purpose |
|:--------|:-------------------|:--------|
| 1       | `dev_enumerate`    | Find NVMe controller |
| 2       | N `cap_request`    | Acquire BAR0, DMA pool, MSI-X vectors |
| 3       | 2 `mem_map` + N `irq_attach` | Map memory, bind interrupts |
| 4 (optional) | `mem_vtop_batch` | Translate queue addresses to physical |

After these syscalls, the driver operates entirely in user-space. The NVMe
controller reset, admin queue setup, Identify commands, and I/O queue creation
are all performed by directly writing to mapped MMIO registers and DMA memory.

---

## 3. I/O Path Design (The Hot Path)

The I/O path is the performance-critical code that runs after initialization.
It involves zero syscalls, zero kernel transitions, and (with per-core queues)
zero cross-core synchronization.

### 3.1 Request-to-Command Translation

A user-space read or write request specifies an LBA range and a DMA buffer.
The driver translates this into an NVMe Read or Write command:

```rust
impl NvmeQueuePair {
    /// Submit a read command. Returns a Future that completes when the
    /// controller finishes the transfer.
    ///
    /// `lba`: starting Logical Block Address
    /// `num_blocks`: number of blocks to read (0-based in the command,
    ///               so we subtract 1)
    /// `buf`: DMA buffer with known physical address
    pub fn submit_read(
        &mut self,
        nsid: u32,
        lba: u64,
        num_blocks: u16,
        buf: &DmaBuf,
    ) -> Result<NvmeIoFuture, NvmeError> {
        if self.is_full() {
            return Err(NvmeError::QueueFull);
        }

        let cid = self.alloc_cid()?;

        let cmd = NvmeCommand {
            cdw0: build_cdw0(IoOpcode::Read as u8, cid),
            nsid,
            cdw2: 0,
            cdw3: 0,
            mptr: 0,
            // PRP1: physical address of the data buffer.
            // For transfers <= 1 page, PRP1 is sufficient.
            // For transfers spanning 2 pages, PRP2 is the second page.
            // For transfers > 2 pages, PRP2 points to a PRP list.
            prp1: buf.paddr.0,
            prp2: buf.prp2(num_blocks),
            // CDW10-11: Starting LBA (64-bit)
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            // CDW12: NLB (15:0) = Number of Logical Blocks - 1
            cdw12: (num_blocks - 1) as u32,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };

        self.submit(cmd, cid)
    }

    /// Submit a write command.
    pub fn submit_write(
        &mut self,
        nsid: u32,
        lba: u64,
        num_blocks: u16,
        buf: &DmaBuf,
    ) -> Result<NvmeIoFuture, NvmeError> {
        if self.is_full() {
            return Err(NvmeError::QueueFull);
        }

        let cid = self.alloc_cid()?;

        let cmd = NvmeCommand {
            cdw0: build_cdw0(IoOpcode::Write as u8, cid),
            nsid,
            cdw2: 0,
            cdw3: 0,
            mptr: 0,
            prp1: buf.paddr.0,
            prp2: buf.prp2(num_blocks),
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: (num_blocks - 1) as u32,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };

        self.submit(cmd, cid)
    }
}
```

### 3.2 Submission Path

The core submission logic writes the command to the SQ ring buffer and rings
the doorbell:

```rust
impl NvmeQueuePair {
    /// Submit a command to the SQ and return a Future for its completion.
    ///
    /// This is the hot path. It performs:
    /// 1. One write to the SQ entry (64 bytes, single cache line on most CPUs)
    /// 2. One 32-bit write to the doorbell register
    /// 3. Registration in the inflight map
    ///
    /// No syscalls. No locks (this queue is core-local). No allocations.
    fn submit(
        &mut self,
        cmd: NvmeCommand,
        cid: u16,
    ) -> Result<NvmeIoFuture, NvmeError> {
        // Write the command into the SQ at the tail position.
        unsafe {
            let slot = self.sq.entries.add(self.sq.tail as usize);
            core::ptr::write_volatile(slot, cmd);
        }

        // Advance the tail with wrap-around.
        self.sq.tail = (self.sq.tail + 1) % self.sq.depth;

        // Create the Future and register it in the inflight map
        // BEFORE ringing the doorbell. The controller may complete
        // the command before we return from this function.
        let future = self.inflight.register(cid);

        // Ring the doorbell -- a single 32-bit MMIO write.
        // This is a posted PCIe write; it returns immediately.
        unsafe {
            (*self.sq.doorbell).write(self.sq.tail as u32);
        }

        Ok(future)
    }

    /// Check if the SQ is full.
    /// Full when advancing the tail would collide with the head.
    #[inline]
    fn is_full(&self) -> bool {
        (self.sq.tail + 1) % self.sq.depth == self.sq.head
    }

    /// Allocate the next free command ID from the queue-local free list.
    /// A CID is not recycled until its owning Future has either consumed
    /// the completion or been dropped.
    #[inline]
    fn alloc_cid(&mut self) -> Result<u16, NvmeError> {
        self.free_cids.pop().ok_or(NvmeError::QueueFull)
    }
}
```

### 3.3 Completion Processing

The completion path scans the CQ for new entries using the Phase Tag mechanism:

```rust
impl NvmeQueuePair {
    /// Poll the CQ for completed commands. Returns the number of
    /// completions processed.
    ///
    /// This is called by the async reactor when the EventSlot fires
    /// or during periodic polling.
    pub fn process_completions(&mut self) -> u32 {
        let mut processed = 0u32;

        loop {
            // Read the CQE at the current head position.
            let cqe = unsafe {
                core::ptr::read_volatile(
                    self.cq.entries.add(self.cq.head as usize)
                )
            };

            // Check the Phase Tag. If it does not match our expected phase,
            // there are no more new completions.
            if cqe.phase() != self.cq.phase {
                break;
            }

            // Update the SQ head from the completion -- this tells us
            // how many SQ entries the controller has consumed.
            self.sq.head = cqe.sq_head();

            // Extract the command ID and look up the corresponding Future.
            let cid = cqe.cid();
            self.inflight.complete(cid, cqe);

            // Advance the CQ head.
            self.cq.head = (self.cq.head + 1) % self.cq.depth;

            // If we wrapped around the CQ, toggle the expected phase.
            if self.cq.head == 0 {
                self.cq.phase = !self.cq.phase;
            }

            processed += 1;
        }

        // If we processed any completions, update the CQ Head Doorbell
        // to tell the controller it can reuse those CQ slots.
        if processed > 0 {
            unsafe {
                (*self.cq.doorbell).write(self.cq.head as u32);
            }
        }

        processed
    }
}
```

### 3.4 Why Zero Syscalls

The I/O path achieves zero syscalls because all interactions are through
memory-mapped registers and shared DMA memory:

- **Command submission:** Write to a DMA buffer (SQ entry), then a single MMIO
  store to the doorbell register. Both are user-space memory accesses.
- **Completion detection:** Read from a DMA buffer (CQ entry). The controller
  writes completions directly to the CQ memory via DMA.
- **Interrupt notification:** The kernel writes to the EventSlot (a single
  atomic increment). The user-space reactor reads it. No signal delivery, no
  context switch into a handler.
- **Buffer management:** Application DMA buffers are pre-allocated and their
  physical addresses are known. No per-I/O address translation is needed.

The only kernel involvement is the optional interrupt path: when the controller
fires an MSI-X vector, the kernel's ISR atomically increments the EventSlot
counter. This is a few nanoseconds of kernel time, and the driver can also
operate in a pure polling mode with no interrupts at all.

### 3.5 Zero-Copy Design

The driver supports true zero-copy I/O. Application buffers allocated from the
DMA pool have known physical addresses. These physical addresses are placed
directly into the NVMe command's PRP entries. The NVMe controller DMAs data
directly to/from the application's buffer -- there is no intermediate kernel
buffer, no memcpy, no bounce buffer.

```
Application buffer (in DMA pool)
    |
    | physical address known at allocation time
    v
NVMe command PRP1 = buf.paddr
    |
    | controller DMAs directly to/from this address
    v
NVMe SSD
```

---

## 4. Per-Core Queue Architecture

### 4.1 Design

Each CPU core gets its own dedicated NVMe I/O queue pair (SQ + CQ). This
eliminates all cross-core synchronization on the I/O path.

```
Core 0                Core 1                Core N
  |                     |                     |
  v                     v                     v
SQ/CQ pair (QID=1)   SQ/CQ pair (QID=2)   SQ/CQ pair (QID=N+1)
  |                     |                     |
  | MSI-X vec 1         | MSI-X vec 2         | MSI-X vec N
  | EventSlot[1]        | EventSlot[2]        | EventSlot[N]
  v                     v                     v
NVMe Controller (arbitrates across all SQs)
```

### 4.2 MSI-X Vector Affinity

Each I/O CQ is assigned a unique MSI-X vector. The vector is pinned to the
same core that owns the queue pair via `irq_attach` with `target_core`. This
ensures:

1. The interrupt fires on the core that will process the completions.
2. The CQ data is already in that core's L1/L2 cache (the controller's DMA
   write pulled the cache line, and the interrupt arrives on the same core).
3. No inter-processor interrupts (IPIs) are needed to wake a remote core.

```rust
/// Per-core I/O queue setup.
pub fn setup_per_core_queues(
    controller: &mut NvmeController,
    num_cores: usize,
    queue_depth: u16,
) -> Result<Vec<CoreQueueContext>, NvmeError> {
    let mut contexts = Vec::with_capacity(num_cores);

    for core_idx in 0..num_cores {
        let qid = (core_idx + 1) as u16; // QID 0 is admin

        // Allocate queue memory from DMA pool.
        let sq_buf = controller.dma_alloc.allocate(
            queue_depth as usize * 64, 4096,
        )?;
        let cq_buf = controller.dma_alloc.allocate(
            queue_depth as usize * 16, 4096,
        )?;

        // Create CQ first (SQ references the CQ).
        create_io_cq(
            &mut controller.admin,
            qid,
            queue_depth,
            cq_buf.paddr,
            qid, // MSI-X vector = QID (vector 0 is admin)
        )?;

        create_io_sq(
            &mut controller.admin,
            qid,
            queue_depth,
            sq_buf.paddr,
            qid, // CQID = QID (1:1 mapping)
        )?;

        contexts.push(CoreQueueContext {
            core_id: CoreId(core_idx as u32),
            queue_pair: NvmeQueuePair::new(qid, sq_buf, cq_buf, queue_depth,
                                            controller.regs),
            event_slot: EventSlot::new(),
        });
    }

    Ok(contexts)
}

/// Everything a core needs to do I/O independently.
pub struct CoreQueueContext {
    pub core_id: CoreId,
    pub queue_pair: NvmeQueuePair,
    pub event_slot: EventSlot,
}
```

### 4.3 No Cross-Core Synchronization

Because each core has its own SQ/CQ pair, there are no atomics, mutexes, or
CAS loops on the I/O path. The data flow is entirely core-local:

- The SQ tail pointer is written only by the owning core.
- The CQ is written by the NVMe controller via DMA and read only by the
  owning core.
- The inflight map (CID -> Waker) is accessed only by the owning core.
- The doorbell registers are written only by the owning core.

The only shared state is the NVMe controller's internal arbitration across
SQs, which is handled in hardware.

### 4.4 Queue Depth Sizing

Queue depth affects both throughput and memory usage:

- **Minimum useful depth:** 32 entries. Below this, the pipeline stalls
  because the SQ fills before completions arrive.
- **Typical depth:** 128-256 entries. This provides enough outstanding
  commands to saturate a modern NVMe SSD (which can handle 64K+ IOPS per
  queue).
- **Maximum depth:** 65535 entries (NVMe spec limit), but diminishing returns
  above 1024 for most workloads.
- **Memory cost per queue pair:** `depth * 64` (SQ) + `depth * 16` (CQ) = 80
  bytes per entry. A depth-256 queue pair costs 20 KB.

The driver defaults to 256 entries per I/O queue pair. This balances memory
usage (~20 KB per core) against the ability to keep the SSD pipeline full.

---

## 5. DMA Buffer Management

### 5.1 DmaBuf Abstraction

A `DmaBuf` represents a contiguous region of memory that has both a virtual
address (for CPU access) and a physical address (for device DMA access). The
physical address is resolved at allocation time, not at I/O time.

```rust
/// A DMA-capable buffer with known physical address.
///
/// This is the fundamental unit of data transfer between the CPU and the
/// NVMe controller. The physical address is embedded in NVMe commands
/// (PRP entries), and the virtual address is used by application code
/// to read/write the data.
#[derive(Clone, Copy)]
pub struct DmaBuf {
    /// Virtual address in the process's address space.
    pub vaddr: *mut u8,
    /// Physical address for DMA. Placed directly into NVMe PRP entries.
    pub paddr: PhysicalAddress,
    /// Size of the buffer in bytes.
    pub len: usize,
}

impl DmaBuf {
    /// Compute the PRP2 value for an NVMe command using this buffer.
    ///
    /// PRP (Physical Region Page) addressing rules:
    /// - Transfer fits in one page: PRP2 is unused (set to 0).
    /// - Transfer spans exactly 2 pages: PRP2 = physical addr of second page.
    /// - Transfer spans > 2 pages: PRP2 = physical addr of a PRP list
    ///   (an array of physical page addresses in a separate DMA buffer).
    pub fn prp2(&self, num_blocks: u16) -> u64 {
        let block_size = 512; // TODO: get from Identify Namespace
        let transfer_size = num_blocks as usize * block_size;
        let page_size = 4096usize;

        // Offset within the first page.
        let first_page_offset = self.paddr.0 as usize & (page_size - 1);
        let first_page_remaining = page_size - first_page_offset;

        if transfer_size <= first_page_remaining {
            // Fits in one page. PRP2 unused.
            0
        } else if transfer_size <= first_page_remaining + page_size {
            // Spans exactly two pages.
            // PRP2 = start of the second page (page-aligned).
            (self.paddr.0 & !(page_size as u64 - 1)) + page_size as u64
        } else {
            // Spans > 2 pages. Must use a PRP list.
            // The PRP list is stored in a separate DmaBuf (see PrpListPool).
            // This path is handled by the scatter-gather logic below.
            panic!("multi-page transfers require PRP list -- use submit_sg()");
        }
    }
}
```

### 5.2 DMA Allocator

The DMA allocator carves buffers from the DMA pool that was acquired via ASI.
It is a bump allocator for simplicity during initialization, with a free-list
for runtime reuse.

```rust
/// Bump allocator over a DMA pool region. Used during initialization
/// for queue memory. Runtime I/O buffers use the DmaBufPool free-list.
pub struct DmaAllocator {
    /// Virtual base address of the DMA pool.
    base_vaddr: *mut u8,
    /// Physical base address of the DMA pool.
    base_paddr: PhysicalAddress,
    /// Total size of the pool.
    total_size: usize,
    /// Current allocation offset (bump pointer).
    offset: usize,
    /// ASI DMA pool capability handle (for vtop if needed).
    dma_cap: CapHandle,
}

impl DmaAllocator {
    /// Allocate a DMA buffer with the given size and alignment.
    pub fn allocate(
        &mut self,
        size: usize,
        alignment: usize,
    ) -> Result<DmaBuf, NvmeError> {
        // Align the current offset.
        let aligned_offset = (self.offset + alignment - 1) & !(alignment - 1);

        if aligned_offset + size > self.total_size {
            return Err(NvmeError::OutOfDmaMemory);
        }

        let vaddr = unsafe { self.base_vaddr.add(aligned_offset) };
        let paddr = PhysicalAddress(self.base_paddr.0 + aligned_offset as u64);

        self.offset = aligned_offset + size;

        Ok(DmaBuf { vaddr, paddr, len: size })
    }
}
```

### 5.3 Scatter-Gather Lists and PRP Lists

For transfers larger than 8 KB (two 4 KB pages), NVMe requires a PRP List --
a page-aligned array of physical page addresses stored in DMA-accessible
memory. The controller reads this list via DMA to determine where to scatter
(read) or gather (write) the data.

```rust
/// A PRP list stored in DMA memory. Used for transfers spanning > 2 pages.
///
/// Each entry in the list is a 64-bit physical page address.
/// The list itself must be page-aligned (4096 bytes) and located in
/// DMA-accessible memory.
///
/// A single PRP list page can hold 512 entries (4096 / 8), supporting
/// transfers up to 512 * 4096 = 2 MB.
pub struct PrpList {
    /// The DMA buffer holding the PRP list entries.
    pub buf: DmaBuf,
    /// Number of entries written.
    pub count: usize,
}

impl PrpList {
    /// Build a PRP list for a scatter-gather transfer.
    ///
    /// `pages` is a slice of physical page addresses for the transfer.
    /// The first page is placed in PRP1 of the NVMe command (not in this list).
    /// The second page onward goes into this list.
    pub fn build(
        prp_buf: &DmaBuf,
        pages: &[PhysicalAddress],
    ) -> PrpList {
        let entries = prp_buf.vaddr as *mut u64;
        // Pages[0] goes into PRP1. Pages[1..] go into the PRP list.
        for (i, page) in pages[1..].iter().enumerate() {
            unsafe {
                core::ptr::write_volatile(entries.add(i), page.0);
            }
        }
        PrpList {
            buf: *prp_buf,
            count: pages.len() - 1,
        }
    }
}

/// Translate a range of virtual addresses to physical addresses
/// using ASI's batch vtop. Called once during buffer pool setup,
/// NOT on the hot path.
pub fn resolve_physical_addresses(
    dma_cap: CapHandle,
    vaddrs: &[*const u8],
) -> Result<Vec<PhysicalAddress>, NvmeError> {
    let mut paddrs = vec![PhysicalAddress(0); vaddrs.len()];
    asi::memory::mem_vtop_batch(dma_cap, vaddrs, &mut paddrs)
        .map_err(|_| NvmeError::VtopFailed)?;
    Ok(paddrs)
}
```

### 5.4 Buffer Pool (Hot-Path Allocation)

Pre-allocating a pool of fixed-size DMA buffers avoids per-I/O allocation
overhead. The pool is a simple free-list of `DmaBuf` objects:

```rust
/// Pre-allocated pool of DMA buffers for hot-path I/O.
///
/// All buffers are the same size (typically 4096 bytes for single-page I/O).
/// Physical addresses are resolved once at pool creation. Allocation and
/// deallocation are O(1) stack operations with no syscalls.
pub struct DmaBufPool {
    /// Free list of available buffers.
    free: Vec<DmaBuf>,
    /// Buffer size (all buffers in the pool are the same size).
    buf_size: usize,
}

impl DmaBufPool {
    /// Create a pool of `count` buffers, each `buf_size` bytes.
    pub fn new(
        dma_alloc: &mut DmaAllocator,
        buf_size: usize,
        count: usize,
    ) -> Result<Self, NvmeError> {
        let mut free = Vec::with_capacity(count);

        for _ in 0..count {
            let buf = dma_alloc.allocate(buf_size, buf_size)?;
            free.push(buf);
        }

        Ok(DmaBufPool { free, buf_size })
    }

    /// Allocate a buffer from the pool. O(1), no syscall.
    #[inline]
    pub fn alloc(&mut self) -> Option<DmaBuf> {
        self.free.pop()
    }

    /// Return a buffer to the pool. O(1), no syscall.
    #[inline]
    pub fn free(&mut self, buf: DmaBuf) {
        self.free.push(buf);
    }
}
```

### 5.5 Alignment Requirements

NVMe PRP entries have specific alignment rules:

- **PRP1 offset:** The offset within the first page determines the start of
  the transfer. Can be any byte offset within a page.
- **PRP2 (when pointing to a data page):** Must be page-aligned (4096-byte
  boundary) if the transfer crosses a page boundary.
- **PRP List pointer:** The PRP list itself must start on a page-aligned
  address (PRP2 must be page-aligned when pointing to a PRP list).
- **PRP List entries:** Each entry in a PRP list must be page-aligned
  (pointing to the start of a physical page).

The DMA allocator enforces 4096-byte alignment for all queue buffers and PRP
list buffers. Application data buffers should also be page-aligned to avoid
complex partial-page PRP calculations.

---

## 6. Error Handling

### 6.1 NVMe Controller Errors (CQ Status Codes)

Every completion entry contains a Status Field with Status Code Type (SCT) and
Status Code (SC). The driver must handle these:

```rust
/// NVMe completion status, extracted from the CQE Status Field.
#[derive(Debug, Clone, Copy)]
pub struct NvmeStatus {
    /// Status Code Type:
    ///   0 = Generic Command Status
    ///   1 = Command Specific Status
    ///   2 = Media and Data Integrity Errors
    ///   3 = Path Related Status
    pub sct: u8,
    /// Status Code (interpretation depends on SCT).
    pub sc: u8,
    /// Do Not Retry flag. If set, retrying will not succeed.
    pub dnr: bool,
}

/// Common NVMe error codes the driver must handle.
#[derive(Debug)]
pub enum NvmeError {
    // -- Initialization errors --
    NoDeviceFound,
    AllDevicesClaimed,
    ControllerFatalStatus,
    ResetTimeout,
    InitTimeout,
    OutOfDmaMemory,
    VtopFailed,

    // -- I/O path errors --
    QueueFull,
    CommandFailed(NvmeStatus),

    // -- Specific NVMe status codes (SCT=0, Generic) --
    /// SC=0x00: Successful Completion (not an error, but listed for completeness)
    Success,
    /// SC=0x01: Invalid Command Opcode
    InvalidOpcode,
    /// SC=0x02: Invalid Field in Command
    InvalidField,
    /// SC=0x04: Data Transfer Error
    DataTransferError,
    /// SC=0x05: Commands Aborted due to Power Loss Notification
    PowerLoss,
    /// SC=0x06: Internal Error
    InternalError,
    /// SC=0x0A: Namespace Not Ready
    NamespaceNotReady,

    // -- Media errors (SCT=2) --
    /// SC=0x80: Write Fault
    WriteFault,
    /// SC=0x81: Unrecovered Read Error
    UnrecoveredRead,
    /// SC=0x82: End-to-end Guard Check Error
    GuardCheckError,

    // -- ASI / capability errors --
    CapabilityRevoked,
    AsiError(u64),
}

impl NvmeStatus {
    /// Classify the status for the caller.
    pub fn classify(&self) -> ErrorClass {
        match (self.sct, self.sc) {
            (0, 0) => ErrorClass::Success,
            (0, sc) if sc <= 0x06 => {
                if self.dnr { ErrorClass::Fatal } else { ErrorClass::Retryable }
            }
            (2, _) => ErrorClass::MediaError,
            _ => ErrorClass::Fatal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorClass {
    Success,
    Retryable,
    MediaError,
    Fatal,
}
```

### 6.2 Device Removal / Hot-Unplug

If the NVMe device is physically removed or the capability is revoked by a
parent process, all MMIO reads will return `0xFFFFFFFF` (PCIe completion for a
non-existent device). The driver detects this by checking CSTS reads:

```rust
impl NvmeController {
    /// Check if the controller is still reachable.
    /// A read of all-ones from CSTS indicates device removal.
    pub fn is_alive(&self) -> bool {
        let csts = self.regs.csts.read();
        // 0xFFFFFFFF = device gone (PCIe returns all-ones for
        // reads to non-existent devices).
        if csts == 0xFFFFFFFF {
            return false;
        }
        // CFS (Controller Fatal Status) bit.
        if csts & (1 << 1) != 0 {
            return false;
        }
        true
    }
}
```

On capability revocation, ASI increments the generation counter. Any
subsequent attempt to use the capability (e.g., a hypothetical reconfiguration
call) returns `CapError::GenerationMismatch`. However, since the hot path does
not use ASI calls, the driver must detect removal via MMIO reads returning
all-ones.

When device removal is detected, all in-flight Futures are completed with
`NvmeError::CapabilityRevoked`, including Futures that have not yet polled and
therefore have not registered a waker. The driver then transitions to a failed
state that rejects new submissions.

### 6.3 DMA Errors

DMA errors are rare on modern hardware but can occur due to IOMMU faults
(accessing memory outside the authorized region) or ECC errors. These manifest
as NVMe controller errors (the controller cannot complete the DMA transfer) and
appear in CQ entries with appropriate status codes.

The IOMMU fault path is handled by the kernel, which logs the fault and may
revoke the DMA capability. The driver observes this as either a completion
error or a capability revocation.

### 6.4 Queue Full / Backpressure

When the SQ is full (tail would collide with head), `submit_read` /
`submit_write` returns `NvmeError::QueueFull`. The caller must wait for
completions to free SQ slots before resubmitting.

The async executor handles this naturally: when a submission fails with
`QueueFull`, the Future yields to the executor, which processes completions
(freeing SQ slots), then retries the submission.

```rust
/// Submit a read with automatic backpressure handling.
/// Yields to the executor if the queue is full, then retries.
pub async fn read_with_backpressure(
    qp: &mut NvmeQueuePair,
    nsid: u32,
    lba: u64,
    num_blocks: u16,
    buf: &DmaBuf,
) -> Result<(), NvmeError> {
    loop {
        match qp.submit_read(nsid, lba, num_blocks, buf) {
            Ok(future) => return future.await,
            Err(NvmeError::QueueFull) => {
                // Process completions to free SQ slots.
                qp.process_completions();
                // Yield to let other tasks run.
                core::future::poll_fn(|cx| {
                    cx.waker().wake_by_ref();
                    core::task::Poll::Pending
                }).await;
            }
            Err(e) => return Err(e),
        }
    }
}
```

---

## 7. Integration with Async Executor

### 7.1 I/O Futures

Each NVMe command submission returns a `Future` that resolves when the
controller posts a completion for that command.

```rust
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

/// Future representing a pending NVMe I/O operation.
///
/// Created by `NvmeQueuePair::submit()`. Completes when the CQ
/// processing loop finds a completion entry with this command's CID.
pub struct NvmeIoFuture {
    /// Owning inflight map. This remains core-local.
    map: NonNull<InflightMap>,
    /// CID + generation pair proving which slot instance this Future owns.
    lease: CommandLease,
    /// Prevent cross-core migration; this Future is intentionally !Send.
    _not_send: PhantomData<&'static LocalOnly>,
}

/// Per-command state shared between submission and completion.
///
/// Stored in the InflightMap, indexed by CID. Generation counters prevent
/// stale Future instances from aliasing a later command that reuses the
/// same CID.
pub struct InflightEntry {
    /// Monotonic slot generation.
    pub generation: u64,
    /// Free / Submitted / Completed.
    pub state: SlotState,
    /// Set by the completion processing loop or fail_all path.
    pub result: Option<Result<NvmeCompletion, NvmeError>>,
    /// Set by the Future's poll() when it registers interest.
    pub waker: Option<Waker>,
    /// True until the Future is dropped or resolves.
    pub future_attached: bool,
}

impl Future for NvmeIoFuture {
    type Output = Result<NvmeCompletion, NvmeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let map = unsafe { this.map.as_mut() };
        let entry = &mut map.entries[this.lease.cid as usize];

        if entry.generation != this.lease.generation {
            return Poll::Ready(Err(NvmeError::StaleSlot));
        }

        if let Some(result) = entry.result.take() {
            entry.future_attached = false;
            entry.waker = None;
            map.recycle(this.lease.cid);
            Poll::Ready(result)
        } else {
            entry.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// Note: NvmeIoFuture is intentionally !Send. Moving it across cores would
// violate the queue-local ownership model and reintroduce synchronization.
```

### 7.2 Completion Reactor

The async reactor on each core is driven by the EventSlot mechanism. The
reactor loop:

1. Processes all ready tasks (polls Futures).
2. When no tasks are ready, checks the EventSlot for new completions.
3. If completions are found, processes them (waking Futures).
4. If no completions and no ready tasks, parks the thread via
   `thread_park` on the EventSlot.

```rust
/// Per-core reactor that drives the async executor and NVMe completions.
pub struct NvmeReactor {
    /// The queue pair owned by this core.
    pub queue_pair: NvmeQueuePair,
    /// EventSlot written by the kernel on MSI-X interrupt.
    pub event_slot: EventSlot,
    /// Last observed EventSlot counter value.
    last_counter: u64,
}

impl NvmeReactor {
    /// Check for new completions. Called by the executor's poll loop.
    ///
    /// Returns true if any completions were processed (meaning Futures
    /// may have been woken and should be polled).
    pub fn check_completions(&mut self) -> bool {
        // Fast check: has the EventSlot counter changed?
        let current = self.event_slot.counter.load(
            core::sync::atomic::Ordering::Acquire
        );

        if current != self.last_counter {
            self.last_counter = current;
            self.queue_pair.process_completions() > 0
        } else {
            // Also try polling the CQ directly -- the controller may have
            // posted completions that we haven't seen an interrupt for yet
            // (interrupt coalescing, or we are in polling mode).
            self.queue_pair.process_completions() > 0
        }
    }

    /// Park the thread until the EventSlot fires or a timeout expires.
    /// This is the ONLY syscall on the I/O path, and it only happens
    /// when the core has no work to do (idle).
    pub fn park_until_interrupt(&self, timeout: Option<Duration>) {
        asi::thread::thread_park(
            &[&self.event_slot],
            timeout,
        ).ok();
    }
}
```

### 7.3 Waker Map (InflightMap)

The `InflightMap` connects command IDs to their associated Futures. It uses a
fixed-size slot array plus a queue-local free-list of reusable CIDs.

```rust
/// Maps in-flight command IDs to their pending Future state.
///
/// Fixed-size array indexed by CID. No heap allocation, no hashing.
/// All operations are O(1).
///
/// This is core-local -- no synchronization needed.
pub struct InflightMap {
    entries: Vec<InflightEntry>,
    free_cids: Vec<u16>,
}

impl InflightMap {
    pub fn new(depth: u16) -> Self {
        let entries = (0..depth)
            .map(|_| InflightEntry {
                generation: 0,
                state: SlotState::Free,
                result: None,
                waker: None,
                future_attached: false,
            })
            .collect();
        let free_cids = (0..depth).rev().collect();
        InflightMap { entries, free_cids }
    }

    /// Register a new in-flight command. The returned Future owns the CID
    /// lease until it resolves or is dropped.
    pub fn register(&mut self) -> Result<(u16, NvmeIoFuture), NvmeError> {
        let cid = self.free_cids.pop().ok_or(NvmeError::QueueFull)?;
        let entry = &mut self.entries[cid as usize];
        entry.generation = entry.generation.wrapping_add(1).max(1);
        entry.state = SlotState::Submitted;
        entry.result = None;
        entry.waker = None;
        entry.future_attached = true;

        Ok((cid, NvmeIoFuture {
            map: NonNull::from(self),
            lease: CommandLease {
                cid,
                generation: entry.generation,
            },
            _not_send: PhantomData,
        }))
    }

    /// Complete a command by writing the CQE and waking the Future.
    pub fn complete(&mut self, cid: u16, cqe: NvmeCompletion) {
        let entry = &mut self.entries[cid as usize];
        entry.state = SlotState::Completed;
        entry.result = Some(Ok(cqe));

        if entry.future_attached {
            if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        } else {
            self.recycle(cid);
        }
    }

    /// Complete all in-flight commands with an error (used on device removal).
    /// Even Futures that have not polled yet will observe the error on first
    /// poll because the result is stored unconditionally.
    pub fn fail_all(&mut self, error: NvmeError) {
        for entry in self.entries.iter_mut() {
            if entry.state != SlotState::Free {
                entry.state = SlotState::Completed;
                entry.result = Some(Err(error));
                if let Some(waker) = entry.waker.take() {
                    waker.wake();
                }
            }
        }
    }

    fn recycle(&mut self, cid: u16) {
        let entry = &mut self.entries[cid as usize];
        entry.state = SlotState::Free;
        entry.result = None;
        entry.waker = None;
        entry.future_attached = false;
        self.free_cids.push(cid);
    }
}
```

---

## 8. Key Data Structures

This section consolidates all primary data structures and their relationships.

### 8.1 NvmeController

The top-level structure representing the entire NVMe driver state:

```rust
/// Top-level NVMe controller state. Created once during initialization.
pub struct NvmeController {
    /// Pointer to the memory-mapped NVMe register set (BAR0).
    pub regs: *mut NvmeRegisters,
    /// Admin queue pair (QID 0).
    pub admin: NvmeQueuePair,
    /// Controller identity from the Identify Controller command.
    pub identity: IdentifyController,
    /// Active namespace ID (typically 1).
    pub nsid: u32,
    /// LBA size in bytes (from Identify Namespace).
    pub block_size: u32,
    /// Total number of blocks in the namespace.
    pub total_blocks: u64,
    /// Maximum data transfer size in bytes (from MDTS).
    pub max_transfer_size: usize,
    /// DMA allocator for dynamic allocations.
    pub dma_alloc: DmaAllocator,
    /// Capability handles (held for the driver's lifetime).
    pub caps: NvmeCapabilities,
}
```

### 8.2 NvmeQueuePair

A paired SQ + CQ with associated bookkeeping:

```rust
/// A matched Submission Queue / Completion Queue pair.
///
/// QID 0 = admin queue. QID 1+ = I/O queues.
/// Each queue pair is owned by exactly one core (no sharing).
pub struct NvmeQueuePair {
    /// Queue identifier (0 = admin, 1+ = I/O).
    pub qid: u16,
    /// Submission queue state.
    pub sq: SubmissionQueue,
    /// Completion queue state.
    pub cq: CompletionQueue,
    /// Queue-local free list of reusable command IDs.
    pub free_cids: Vec<u16>,
    /// Map of in-flight command IDs to pending Futures.
    pub inflight: InflightMap,
}

/// Submission Queue state (host-managed tail, controller-managed head).
pub struct SubmissionQueue {
    /// Pointer to the SQ entry array in DMA memory.
    pub entries: *mut NvmeCommand,
    /// Physical address of the SQ (for register programming).
    pub phys: PhysicalAddress,
    /// Queue depth (number of entries).
    pub depth: u16,
    /// Tail index (next slot to write). Managed by the host.
    pub tail: u16,
    /// Head index (last consumed by controller). Updated from CQEs.
    pub head: u16,
    /// Pointer to the SQ Tail Doorbell register (MMIO).
    pub doorbell: *mut Volatile<u32>,
}

/// Completion Queue state (controller-managed tail, host-managed head).
pub struct CompletionQueue {
    /// Pointer to the CQ entry array in DMA memory.
    pub entries: *mut NvmeCompletion,
    /// Physical address of the CQ (for register programming).
    pub phys: PhysicalAddress,
    /// Queue depth (number of entries).
    pub depth: u16,
    /// Head index (next slot to read). Managed by the host.
    pub head: u16,
    /// Expected Phase Tag. Toggles each time the CQ wraps.
    pub phase: bool,
    /// Pointer to the CQ Head Doorbell register (MMIO).
    pub doorbell: *mut Volatile<u32>,
}
```

### 8.3 NvmeCommand and NvmeCompletion

Defined in Section 1.2 above. Reproduced here for reference:

- `NvmeCommand`: 64-byte SQE, defined in Section 1.2.
- `NvmeCompletion`: 16-byte CQE, defined in Section 1.2.

### 8.4 DmaBuf and DmaPool

- `DmaBuf`: Virtual + physical address pair, defined in Section 5.1.
- `DmaAllocator`: Bump allocator over a DMA pool, defined in Section 5.2.
- `DmaBufPool`: Pre-allocated free-list of fixed-size DMA buffers, defined in
  Section 5.4.

### 8.5 InflightMap

Fixed-size array mapping CID to `InflightEntry`, defined in Section 7.3.

### 8.6 Relationship Diagram

```
NvmeController
 |-- regs: *mut NvmeRegisters          (BAR0 MMIO, UNCACHEABLE)
 |-- admin: NvmeQueuePair              (QID 0)
 |-- caps: NvmeCapabilities
 |     |-- bar0: CapHandle
 |     |-- dma_pool: CapHandle
 |     `-- msix_vectors: [CapHandle]
 |-- dma_alloc: DmaAllocator
 `-- identity: IdentifyController

CoreQueueContext (one per core)
 |-- core_id: CoreId
 |-- queue_pair: NvmeQueuePair          (QID = core + 1)
 |     |-- sq: SubmissionQueue
 |     |     |-- entries: *mut NvmeCommand    (DMA memory)
 |     |     `-- doorbell: *mut Volatile<u32> (BAR0 MMIO)
 |     |-- cq: CompletionQueue
 |     |     |-- entries: *mut NvmeCompletion (DMA memory)
 |     |     `-- doorbell: *mut Volatile<u32> (BAR0 MMIO)
 |     `-- inflight: InflightMap
 |           `-- entries: [InflightEntry]
 |                 |-- result: Option<NvmeCompletion>
 |                 `-- waker: Option<Waker>
 `-- event_slot: EventSlot              (shared with kernel ISR)
       |-- counter: AtomicU64
       `-- timestamp: AtomicU64
```

---

## Appendix A: Complete I/O Path Walkthrough

This traces a single 4 KB read from submission to completion.

**Step 1: Application submits a read.**
```rust
let buf = pool.alloc().unwrap();   // O(1), from pre-allocated DmaBufPool
let future = qp.submit_read(1, lba, 1, &buf)?;
```

**Step 2: Inside `submit_read`.**
- Build an `NvmeCommand` with opcode=0x02 (Read), PRP1=buf.paddr, LBA, NLB=0.
- Write the 64-byte command to `sq.entries[sq.tail]` via volatile store.
- Increment `sq.tail` mod `depth`.
- Register the CID in `inflight` (stores a pointer for the Future).
- Write `sq.tail` to the SQ Tail Doorbell (32-bit MMIO write).

**Step 3: Hardware processes the command.**
- The NVMe controller reads the SQE from DMA memory.
- It performs the flash read and DMAs the data into `buf.paddr`.
- It writes a 16-byte CQE to `cq.entries[cq.tail]` with Phase Tag set.
- It fires the MSI-X vector assigned to this CQ.

**Step 4: Kernel ISR.**
- The kernel's ISR (a few instructions) atomically increments
  `event_slot.counter`.
- If the thread is parked, it sends an IPI to wake it.

**Step 5: Reactor observes the EventSlot change.**
- The async reactor calls `check_completions()`.
- It sees `event_slot.counter` has changed.
- It calls `qp.process_completions()`.

**Step 6: CQ processing.**
- Read `cq.entries[cq.head]` via volatile load.
- Phase Tag matches expected phase -- this is a new completion.
- Extract CID from the CQE.
- Call `inflight.complete(cid, cqe)` -- stores the CQE and wakes the Waker.
- Advance `cq.head`, toggle phase if wrapped.
- Write `cq.head` to the CQ Head Doorbell.

**Step 7: Future resolves.**
- The executor polls `NvmeIoFuture`.
- `entry.result` is `Some(cqe)`.
- Returns `Poll::Ready(Ok(cqe))`.

**Step 8: Application reads the data.**
- The data is already in `buf.vaddr` (DMA'd directly by the controller).
- No memcpy needed. The application accesses it directly.

**Total kernel involvement:** One atomic increment in the ISR (about 5-10 ns).
Everything else is user-space memory accesses and MMIO writes.

---

## Appendix B: NVMe Register Bit Field Reference

### CAP -- Controller Capabilities (offset 0x00, 64-bit, read-only)

| Bits  | Field   | Description |
|:------|:--------|:------------|
| 15:0  | MQES    | Maximum Queue Entries Supported (0-based). Max value = 65535. |
| 16    | CQR     | Contiguous Queues Required. 1 = queues must be physically contiguous. |
| 18:17 | AMS     | Arbitration Mechanism Supported (bitmask). |
| 23:19 | Reserved | |
| 31:24 | TO      | Timeout. Worst-case time for CSTS.RDY transitions, in 500ms units. |
| 35:32 | DSTRD   | Doorbell Stride. Doorbell register spacing = `4 << DSTRD` bytes. |
| 36    | NSSRS   | NVM Subsystem Reset Supported. |
| 44:37 | CSS     | Command Sets Supported (bitmask). Bit 0 = NVM Command Set. |
| 45    | BPS     | Boot Partition Support. |
| 47:46 | Reserved | |
| 51:48 | MPSMIN  | Memory Page Size Minimum. Min page size = `2^(12+MPSMIN)`. |
| 55:52 | MPSMAX  | Memory Page Size Maximum. Max page size = `2^(12+MPSMAX)`. |
| 63:56 | Reserved | |

### CC -- Controller Configuration (offset 0x14, 32-bit, read/write)

| Bits  | Field   | Description |
|:------|:--------|:------------|
| 0     | EN      | Enable. 0->1 starts controller init. 1->0 triggers reset. |
| 3:1   | Reserved | |
| 6:4   | CSS     | Command Set Selected. 0b000 = NVM Command Set. |
| 10:7  | MPS     | Memory Page Size. Page size = `2^(12+MPS)`. |
| 13:11 | AMS     | Arbitration Mechanism Selected. 0b000 = Round Robin. |
| 15:14 | SHN     | Shutdown Notification. 0b00=none, 0b01=normal, 0b10=abrupt. |
| 19:16 | IOSQES  | I/O SQ Entry Size. Entry size = `2^IOSQES`. Must be 6 (64B). |
| 23:20 | IOCQES  | I/O CQ Entry Size. Entry size = `2^IOCQES`. Must be 4 (16B). |
| 31:24 | Reserved | |

### CSTS -- Controller Status (offset 0x1C, 32-bit, read-only)

| Bits  | Field   | Description |
|:------|:--------|:------------|
| 0     | RDY     | Ready. Set by controller when initialization is complete. |
| 1     | CFS     | Controller Fatal Status. Set on unrecoverable error. |
| 2     | SHST    | Shutdown Status. 0b00=normal, 0b01=in progress, 0b10=complete. |
| 3     | NSSRO   | NVM Subsystem Reset Occurred. |
| 4     | PP      | Processing Paused. |
| 31:5  | Reserved | |

---

## Appendix C: Design Decisions and Rationale

### Why per-core queues instead of a shared queue with locking?

NVMe hardware supports up to 65535 queue pairs precisely to enable this
pattern. A shared queue requires either a lock (adding latency and contention)
or a lock-free MPSC structure (complex and still requiring atomics). Per-core
queues eliminate all synchronization. The cost is additional queue memory (~20
KB per core), which is negligible.

### Why polling + interrupt hybrid instead of pure polling?

Pure polling achieves the lowest latency but wastes CPU cycles when the
workload is idle. Pure interrupt-driven completion has higher latency due to
the interrupt delivery path. The hybrid approach polls when busy (zero
overhead) and falls back to interrupts when idle (saves power, frees the
core). The EventSlot mechanism makes the transition seamless: the reactor
checks the counter (one atomic load), and if nothing changed, calls
`thread_park`.

### Why bump allocation for queues and free-list for I/O buffers?

Queue memory is allocated once during initialization and never freed. A bump
allocator is the simplest and fastest allocator for this pattern. I/O buffers
are allocated and freed on every request, so a free-list provides O(1)
allocation without fragmentation (all buffers are the same size).

### Why resolve physical addresses at allocation time?

The alternative is to call `mem_vtop` at I/O submission time, which would add
a syscall to the hot path. By resolving physical addresses once when the
buffer is allocated (or when the pool is created), the hot path remains
syscall-free. This works because DMA pool memory is pinned -- the physical
address never changes.

### Why store the Waker in the InflightMap instead of using a channel?

Channels allocate and synchronize. The InflightMap is a fixed-size array with
O(1) indexed access and no allocation. Since the queue pair is core-local,
there is no contention. The completion loop writes directly to the entry and
calls `waker.wake()`, which is the minimal overhead path for integrating with
an async executor.
