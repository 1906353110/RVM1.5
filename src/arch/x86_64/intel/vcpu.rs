// 本文件实现了基于 Intel VMX（虚拟机扩展）的虚拟 CPU（VCPU）管理。
// 设计思路：
// 1. 每个 Vcpu 结构体实例代表一个虚拟机中的虚拟 CPU，负责保存和切换访客（Guest）与宿主（Host）的运行状态。
// 2. 利用 VMXON/VMCS 区域实现对 CPU 虚拟化的底层支持，确保虚拟机的隔离性和安全性。
// 3. 通过 MSR bitmap、段寄存器、控制寄存器等机制，实现对访客 CPU 行为的精细控制。
// 4. 设计上充分利用 Rust 的类型安全和抽象能力，减少裸指针和不安全操作的范围。

use core::arch::asm;
use core::fmt::{Debug, Formatter, Result};

use libvmm::msr::Msr;
use libvmm::vmx::{
    self,
    flags::{FeatureControl, FeatureControlFlags, VmxBasic},
    vmcs::{VmcsField16Guest, VmcsField32Guest, VmcsField64Guest},
    vmcs::{VmcsField16Host, VmcsField32Host, VmcsField64Host},
    vmcs::{VmcsField32Control, VmcsField64Control},
    Vmcs, VmxExitReason,
};
use x86::segmentation::SegmentSelector;
use x86_64::addr::VirtAddr;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr4, Cr4Flags};
use x86_64::registers::rflags::RFlags;

use super::structs::{MsrBitmap, VmxRegion};
use crate::arch::cpuid::CpuFeatures;
use crate::arch::segmentation::{Segment, SegmentAccessRights};
use crate::arch::tables::{GdtStruct, IDT};
use crate::arch::vmm::VcpuAccessGuestState;
use crate::arch::{GeneralRegisters, GuestPageTableImmut, LinuxContext};
use crate::cell::Cell;
use crate::error::HvResult;
use crate::percpu::PerCpu;

/// 虚拟 CPU（VCPU）结构体，用于保存当前虚拟机的运行状态。
/// 每个 VCPU 实例表示一个逻辑上的访客 CPU。
/// 设计说明：
/// - guest_regs 用于保存访客通用寄存器，保证 VMEXIT 时能正确恢复访客上下文。
/// - host_stack_top 保存宿主栈顶指针，便于 VMEXIT 时切换回宿主环境。
/// - vmxon_region/ vmcs_region 分别对应 VMXON/VMCS 区域，分别用于开启 VMX 和管理虚拟机状态。
#[repr(C)]
pub struct Vcpu {
    /// 保存访客通用寄存器，在 VMEXIT 时被保存、恢复。
    guest_regs: GeneralRegisters,
    /// 用于 VMEXIT 返回宿主时的栈顶指针（RSP）。
    host_stack_top: u64,
    /// VMXON 区域，用于执行 VMXON 指令开启 VMX 操作模式。
    vmxon_region: VmxRegion,
    /// VMCS 区域，管理这个 VCPU 的虚拟化配置。
    vmcs_region: VmxRegion,
}

/// 全局共享的 MSR bitmap，用于告诉 VMX 哪些 MSR 访问需要触发 VMEXIT。
/// 设计说明：
/// - MSR bitmap 能够极大提升性能，避免不必要的 VMEXIT。
lazy_static! {
    static ref MSR_BITMAP: MsrBitmap = MsrBitmap::default();
}

/// 一个宏，用于批量设置 VMCS 中的访客段寄存器字段。
/// 包括 selector（选择子）、base（段基址）、limit（段限长）和 AR bytes（访问权限）。
/// 设计说明：
/// - 利用宏减少重复代码，保证段寄存器设置的一致性和正确性。
macro_rules! set_guest_segment {
    ($seg: expr, $reg: ident) => {{
        use VmcsField16Guest::*;
        use VmcsField32Guest::*;
        use VmcsField64Guest::*;
        // 设置段选择子
        concat_idents!($reg, _SELECTOR).write($seg.selector.bits())?;
        // 设置段基址
        concat_idents!($reg, _BASE).write($seg.base)?;
        // 设置段限长
        concat_idents!($reg, _LIMIT).write($seg.limit)?;
        // 设置访问权限
        concat_idents!($reg, _AR_BYTES).write($seg.access_rights.bits())?;
    }};
}


impl Vcpu {
    /// 创建新的 VCPU 实例，并初始化 VMX 环境。
    /// 设计说明：
    /// - 检查 CPU 是否支持虚拟化扩展（VMX）。
    /// - 检查并设置 CR0/CR4 控制寄存器，确保进入 VMX 操作模式的前置条件。
    /// - 初始化 VMXON/VMCS 区域，开启 VMX。
    /// - 配置 VMCS，准备访客和宿主环境切换。
    pub fn new(linux: &LinuxContext, cell: &Cell) -> HvResult<Self> {
        super::check_hypervisor_feature()?;

        // 确保性能计数器关闭，避免对虚拟化产生干扰。
        if CpuFeatures::new().perf_monitor_version_id() > 0 {
            unsafe { Msr::IA32_PERF_GLOBAL_CTRL.write(0) };
        }

        // 检查控制寄存器，防止重复开启 VMX。
        let _cr0 = linux.cr0;
        let cr4 = linux.cr4;
        // TODO: check reserved bits
        if cr4.contains(Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS) {
            return hv_result_err!(EBUSY, "VMX is already turned on!");
        }

        // 检查并设置 VMX feature control MSR，确保 BIOS 允许 VMX。
        let ctrl = FeatureControl::read();
        let locked = ctrl.contains(FeatureControlFlags::LOCKED);
        let vmxon_outside = ctrl.contains(FeatureControlFlags::VMXON_ENABLED_OUTSIDE_SMX);
        if !locked {
            FeatureControl::write(
                ctrl | FeatureControlFlags::LOCKED | FeatureControlFlags::VMXON_ENABLED_OUTSIDE_SMX,
            );
        } else if !vmxon_outside {
            return hv_result_err!(ENODEV, "VMX disabled by BIOS!");
        }

        // 初始化 VMXON/VMCS 区域，revision_id 必须与硬件一致。
        let vmx_basic = VmxBasic::read();
        let vmxon_region = VmxRegion::new(vmx_basic.revision_id, false)?;
        let vmcs_region = VmxRegion::new(vmx_basic.revision_id, false)?;

        // 设置 CR0/CR4，保证进入 VMX 前的寄存器状态符合要求。
        let mut cr4 = super::super::HOST_CR4 | Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS;
        if CpuFeatures::new().has_xsave() {
            cr4 |= Cr4Flags::OSXSAVE;
        }
        unsafe {
            Cr0::write(super::super::HOST_CR0);
            Cr4::write(cr4);
        }

        // 执行 VMXON 指令，正式进入 VMX 操作模式。
        unsafe { vmx::vmxon(vmxon_region.paddr() as _)? };
        info!("successed to turn on VMX.");

        // 创建 VCPU 实例，并初始化 VMCS。
        let mut ret = Self {
            guest_regs: Default::default(),
            host_stack_top: PerCpu::current().stack_top() as _,
            vmxon_region,
            vmcs_region,
        };
        ret.vmcs_setup(linux, cell)?;

        Ok(ret)
    }

    /// 进入虚拟机，启动访客代码执行。
    /// 设计说明：
    /// - 设置访客通用寄存器，准备好访客上下文。
    /// - 执行 vmlaunch 指令，进入访客环境。
    /// - 若失败则输出错误信息。
    pub fn enter(&mut self, linux: &LinuxContext) -> HvResult {
        let regs = self.regs_mut();
        regs.rax = 0;
        regs.rbx = linux.rbx;
        regs.rbp = linux.rbp;
        regs.r12 = linux.r12;
        regs.r13 = linux.r13;
        regs.r14 = linux.r14;
        regs.r15 = linux.r15;
        unsafe {
            asm!(
                "mov rsp, {0}",
                restore_regs_from_stack!(),
                "vmlaunch",
                in(reg) regs as * const _ as usize,
            );
        }
        // 如果 vmlaunch 成功，不会返回；否则输出错误。
        error!(
            "Activate hypervisor failed: {:?}",
            Vmcs::instruction_error()
        );
        hv_result_err!(EIO)
    }

    /// 退出虚拟机，恢复宿主环境。
    /// 设计说明：
    /// - 保存访客状态到 LinuxContext。
    /// - 清除 VMCS 并关闭 VMX。
    pub fn exit(&self, linux: &mut LinuxContext) -> HvResult {
        self.load_vmcs_guest(linux)?;
        Vmcs::clear(self.vmcs_region.paddr())?;
        unsafe { vmx::vmxoff()? };
        info!("successed to turn off VMX.");
        Ok(())
    }

    /// 向访客注入通用保护异常（#GP）。
    /// 设计说明：
    /// - 用于模拟访客异常，提升虚拟化兼容性。
    pub fn inject_fault(&mut self) -> HvResult {
        Vmcs::inject_interrupt(crate::arch::ExceptionType::GeneralProtectionFault, Some(0))?;
        Ok(())
    }

    /// 推进访客 RIP 指针，跳过当前指令。
    /// 设计说明：
    /// - 用于处理 VMEXIT 后需要跳过的指令（如 hypercall）。
    pub fn advance_rip(&mut self, instr_len: u8) -> HvResult {
        VmcsField64Guest::RIP.write(VmcsField64Guest::RIP.read()? + instr_len as u64)?;
        Ok(())
    }

    /// 判断访客当前是否处于特权级（CPL=0）。
    /// 设计说明：
    /// - 用于安全检查和特权指令模拟。
    pub fn guest_is_privileged(&self) -> bool {
        SegmentAccessRights::from_bits_truncate(VmcsField32Guest::CS_AR_BYTES.read().unwrap()).dpl()
            == 0
    }

    /// 判断当前是否处于 hypercall（VMCALL）处理流程。
    pub fn in_hypercall(&self) -> bool {
        matches!(Vmcs::exit_reason(), Ok(VmxExitReason::VMCALL))
    }

    /// 获取访客页表根指针，便于后续内存访问模拟。
    pub fn guest_page_table(&self) -> GuestPageTableImmut {
        use crate::memory::{addr::align_down, GenericPageTableImmut};
        unsafe { GuestPageTableImmut::from_root(align_down(self.cr(3) as _)) }
    }
}

impl Vcpu {
    /// 初始化 VMCS，配置宿主/访客/控制域。
    /// 设计说明：
    /// - 分为 host/guest/control 三部分，分别对应宿主、访客和控制设置。
    fn vmcs_setup(&mut self, linux: &LinuxContext, cell: &Cell) -> HvResult {
        let paddr = self.vmcs_region.paddr();
        Vmcs::clear(paddr)?;
        Vmcs::load(paddr)?;
        self.setup_vmcs_host()?;
        self.setup_vmcs_guest(linux)?;
        self.setup_vmcs_control(cell)?;
        Ok(())
    }

    /// 配置 VMCS 的宿主域（Host State）。
    /// 设计说明：
    /// - 保证 VMEXIT 时能正确切换回宿主环境。
    fn setup_vmcs_host(&mut self) -> HvResult {
        VmcsField64Host::IA32_PAT.write(Msr::IA32_PAT.read())?;
        VmcsField64Host::IA32_EFER.write(Msr::IA32_EFER.read())?;

        VmcsField64Host::CR0.write(Cr0::read_raw())?;
        VmcsField64Host::CR3.write(Cr3::read().0.start_address().as_u64())?;
        VmcsField64Host::CR4.write(Cr4::read_raw())?;

        VmcsField16Host::ES_SELECTOR.write(0)?;
        VmcsField16Host::CS_SELECTOR.write(GdtStruct::KCODE_SELECTOR.bits())?;
        VmcsField16Host::SS_SELECTOR.write(0)?;
        VmcsField16Host::DS_SELECTOR.write(0)?;
        VmcsField16Host::FS_SELECTOR.write(0)?;
        VmcsField16Host::GS_SELECTOR.write(0)?;
        VmcsField16Host::TR_SELECTOR.write(GdtStruct::TSS_SELECTOR.bits())?;
        VmcsField64Host::FS_BASE.write(0)?;
        VmcsField64Host::GS_BASE.write(Msr::IA32_GS_BASE.read())?;
        VmcsField64Host::TR_BASE.write(0)?;

        VmcsField64Host::GDTR_BASE.write(GdtStruct::sgdt().base.as_u64())?;
        VmcsField64Host::IDTR_BASE.write(IDT.lock().pointer().base.as_u64())?;

        VmcsField64Host::IA32_SYSENTER_ESP.write(0)?;
        VmcsField64Host::IA32_SYSENTER_EIP.write(0)?;
        VmcsField32Host::IA32_SYSENTER_CS.write(0)?;

        let rsp = &PerCpu::current().vcpu.host_stack_top as *const _ as u64;
        VmcsField64Host::RSP.write(rsp)?; // used for saving guest registers
        VmcsField64Host::RIP.write(vmx_exit as usize as _)?;
        Ok(())
    }

    /// 配置 VMCS 的访客域（Guest State）。
    /// 设计说明：
    /// - 保证 VMENTRY 时访客环境与 LinuxContext 一致。
    fn setup_vmcs_guest(&mut self, linux: &LinuxContext) -> HvResult {
        VmcsField64Guest::IA32_PAT.write(linux.pat)?;
        VmcsField64Guest::IA32_EFER.write(linux.efer)?;

        self.set_cr(0, linux.cr0.bits());
        self.set_cr(4, linux.cr4.bits());
        self.set_cr(3, linux.cr3);

        set_guest_segment!(linux.es, ES);
        set_guest_segment!(linux.cs, CS);
        set_guest_segment!(linux.ss, SS);
        set_guest_segment!(linux.ds, DS);
        set_guest_segment!(linux.fs, FS);
        set_guest_segment!(linux.gs, GS);
        set_guest_segment!(linux.tss, TR);
        set_guest_segment!(Segment::invalid(), LDTR);

        VmcsField64Guest::GDTR_BASE.write(linux.gdt.base.as_u64())?;
        VmcsField32Guest::GDTR_LIMIT.write(linux.gdt.limit as _)?;
        VmcsField64Guest::IDTR_BASE.write(linux.idt.base.as_u64())?;
        VmcsField32Guest::IDTR_LIMIT.write(linux.idt.limit as _)?;

        VmcsField64Guest::RSP.write(linux.rsp)?;
        VmcsField64Guest::RIP.write(linux.rip)?;
        VmcsField64Guest::RFLAGS.write(0x2)?;

        VmcsField32Guest::SYSENTER_CS.write(Msr::IA32_SYSENTER_CS.read() as _)?;
        VmcsField64Guest::SYSENTER_ESP.write(Msr::IA32_SYSENTER_ESP.read())?;
        VmcsField64Guest::SYSENTER_EIP.write(Msr::IA32_SYSENTER_EIP.read())?;

        VmcsField64Guest::DR7.write(0x400)?;
        VmcsField64Guest::IA32_DEBUGCTL.write(0)?;

        VmcsField32Guest::ACTIVITY_STATE.write(0)?;
        VmcsField32Guest::INTERRUPTIBILITY_INFO.write(0)?;
        VmcsField64Guest::PENDING_DBG_EXCEPTIONS.write(0)?;

        VmcsField64Guest::VMCS_LINK_POINTER.write(core::u64::MAX)?;
        VmcsField32Guest::VMX_PREEMPTION_TIMER_VALUE.write(0)?;
        Ok(())
    }

    /// 从 VMCS 读取访客状态，保存到 LinuxContext。
    /// 设计说明：
    /// - 用于 VMEXIT 时同步访客上下文。
    fn load_vmcs_guest(&self, linux: &mut LinuxContext) -> HvResult {
        linux.rip = VmcsField64Guest::RIP.read()?;
        linux.rsp = VmcsField64Guest::RSP.read()?;
        linux.cr0 = Cr0Flags::from_bits_truncate(VmcsField64Guest::CR0.read()?);
        linux.cr3 = VmcsField64Guest::CR3.read()?;
        linux.cr4 = Cr4Flags::from_bits_truncate(VmcsField64Guest::CR4.read()?)
            - Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS;

        linux.es.selector = SegmentSelector::from_raw(VmcsField16Guest::ES_SELECTOR.read()?);
        linux.cs.selector = SegmentSelector::from_raw(VmcsField16Guest::CS_SELECTOR.read()?);
        linux.ss.selector = SegmentSelector::from_raw(VmcsField16Guest::SS_SELECTOR.read()?);
        linux.ds.selector = SegmentSelector::from_raw(VmcsField16Guest::DS_SELECTOR.read()?);
        linux.fs.selector = SegmentSelector::from_raw(VmcsField16Guest::FS_SELECTOR.read()?);
        linux.fs.base = VmcsField64Guest::FS_BASE.read()?;
        linux.gs.selector = SegmentSelector::from_raw(VmcsField16Guest::GS_SELECTOR.read()?);
        linux.gs.base = VmcsField64Guest::GS_BASE.read()?;
        linux.tss.selector = SegmentSelector::from_raw(VmcsField16Guest::TR_SELECTOR.read()?);

        linux.gdt.base = VirtAddr::new(VmcsField64Guest::GDTR_BASE.read()?);
        linux.gdt.limit = VmcsField32Guest::GDTR_LIMIT.read()? as _;
        linux.idt.base = VirtAddr::new(VmcsField64Guest::IDTR_BASE.read()?);
        linux.idt.limit = VmcsField32Guest::IDTR_LIMIT.read()? as _;

        unsafe {
            Msr::IA32_SYSENTER_CS.write(VmcsField32Guest::SYSENTER_CS.read()? as _);
            Msr::IA32_SYSENTER_ESP.write(VmcsField64Guest::SYSENTER_ESP.read()?);
            Msr::IA32_SYSENTER_EIP.write(VmcsField64Guest::SYSENTER_EIP.read()?);
        }

        Ok(())
    }

    /// 配置 VMCS 的控制域（Control Fields）。
    /// 设计说明：
    /// - 控制虚拟机行为，如中断、MSR、EPT、异常等。
    /// - 通过 MSR bitmap、EPT、异常 bitmap 等机制提升性能和安全性。
    fn setup_vmcs_control(&mut self, cell: &Cell) -> HvResult {
        use vmx::flags::PinVmExecControls as PinCtrl;
        Vmcs::set_control(
            VmcsField32Control::PIN_BASED_VM_EXEC_CONTROL,
            Msr::IA32_VMX_PINBASED_CTLS.read(),
            // NO INTR_EXITING to pass-through interrupts
            PinCtrl::NMI_EXITING.bits(),
            0,
        )?;

        use vmx::flags::PrimaryVmExecControls as CpuCtrl;
        Vmcs::set_control(
            VmcsField32Control::PROC_BASED_VM_EXEC_CONTROL,
            Msr::IA32_VMX_PROCBASED_CTLS.read(),
            // NO UNCOND_IO_EXITING to pass-through PIO
            (CpuCtrl::USE_MSR_BITMAPS | CpuCtrl::SEC_CONTROLS).bits(),
            (CpuCtrl::CR3_LOAD_EXITING | CpuCtrl::CR3_STORE_EXITING).bits(),
        )?;

        use vmx::flags::SecondaryVmExecControls as CpuCtrl2;
        let mut val = CpuCtrl2::EPT | CpuCtrl2::UNRESTRICTED_GUEST;
        let features = CpuFeatures::new();
        if features.has_rdtscp() {
            val |= CpuCtrl2::RDTSCP;
        }
        if features.has_invpcid() {
            val |= CpuCtrl2::INVPCID;
        }
        if features.has_xsaves_xrstors() {
            val |= CpuCtrl2::XSAVES;
        }
        Vmcs::set_control(
            VmcsField32Control::SECONDARY_VM_EXEC_CONTROL,
            Msr::IA32_VMX_PROCBASED_CTLS2.read(),
            val.bits(),
            0,
        )?;

        use vmx::flags::VmExitControls as ExitCtrl;
        Vmcs::set_control(
            VmcsField32Control::VM_EXIT_CONTROLS,
            Msr::IA32_VMX_EXIT_CTLS.read(),
            (ExitCtrl::HOST_ADDR_SPACE_SIZE
                | ExitCtrl::SAVE_IA32_PAT
                | ExitCtrl::LOAD_IA32_PAT
                | ExitCtrl::SAVE_IA32_EFER
                | ExitCtrl::LOAD_IA32_EFER)
                .bits(),
            0,
        )?;

        use vmx::flags::VmEntryControls as EntryCtrl;
        Vmcs::set_control(
            VmcsField32Control::VM_ENTRY_CONTROLS,
            Msr::IA32_VMX_ENTRY_CTLS.read(),
            (EntryCtrl::IA32E_MODE | EntryCtrl::LOAD_IA32_PAT | EntryCtrl::LOAD_IA32_EFER).bits(),
            0,
        )?;

        VmcsField32Control::VM_EXIT_MSR_STORE_COUNT.write(0)?;
        VmcsField32Control::VM_EXIT_MSR_LOAD_COUNT.write(0)?;
        VmcsField32Control::VM_ENTRY_MSR_LOAD_COUNT.write(0)?;

        VmcsField64Control::CR4_GUEST_HOST_MASK.write(0)?;
        VmcsField32Control::CR3_TARGET_COUNT.write(0)?;

        unsafe { cell.gpm.activate() }; // Set EPT_POINTER

        VmcsField64Control::MSR_BITMAP.write(MSR_BITMAP.paddr() as _)?;
        VmcsField32Control::EXCEPTION_BITMAP.write(0)?;

        Ok(())
    }
}

impl VcpuAccessGuestState for Vcpu {
    /// 获取访客通用寄存器只读引用。
    fn regs(&self) -> &GeneralRegisters {
        &self.guest_regs
    }

    /// 获取访客通用寄存器可写引用。
    fn regs_mut(&mut self) -> &mut GeneralRegisters {
        &mut self.guest_regs
    }

    /// 获取访客指令指针（RIP）。
    fn instr_pointer(&self) -> u64 {
        VmcsField64Guest::RIP.read().unwrap()
    }

    /// 获取访客栈指针（RSP）。
    fn stack_pointer(&self) -> u64 {
        VmcsField64Guest::RSP.read().unwrap()
    }

    /// 设置访客栈指针（RSP）。
    fn set_stack_pointer(&mut self, sp: u64) {
        VmcsField64Guest::RSP.write(sp).unwrap()
    }

    /// 获取访客 RFLAGS。
    fn rflags(&self) -> u64 {
        VmcsField64Guest::RFLAGS.read().unwrap()
    }

    /// 获取访客 FS 段基址。
    fn fs_base(&self) -> u64 {
        VmcsField64Guest::FS_BASE.read().unwrap()
    }

    /// 获取访客 GS 段基址。
    fn gs_base(&self) -> u64 {
        VmcsField64Guest::GS_BASE.read().unwrap()
    }

    /// 获取访客控制寄存器（CR0/CR3/CR4）。
    /// 设计说明：
    /// - CR4 需要特殊处理，结合 GUEST_HOST_MASK 和 READ_SHADOW。
    fn cr(&self, cr_idx: usize) -> u64 {
        (|| -> HvResult<u64> {
            Ok(match cr_idx {
                0 => VmcsField64Guest::CR0.read()?,
                3 => VmcsField64Guest::CR3.read()?,
                4 => {
                    let host_mask = VmcsField64Control::CR4_GUEST_HOST_MASK.read()?;
                    (VmcsField64Control::CR4_READ_SHADOW.read()? & host_mask)
                        | (VmcsField64Guest::CR4.read()? & !host_mask)
                }
                _ => unreachable!(),
            })
        })()
        .expect("Failed to read guest control register")
    }

    /// 设置访客控制寄存器（CR0/CR3/CR4）。
    /// 设计说明：
    /// - 遵循 Intel SDM 对 CR0/CR4 的固定位要求，保证虚拟化安全。
    /// - CR0/CR4 的某些位必须强制为 0 或 1，防止访客破坏宿主环境。
    fn set_cr(&mut self, cr_idx: usize, val: u64) {
        (|| -> HvResult {
            match cr_idx {
                0 => {
                    // Retrieve/validate restrictions on CR0
                    //
                    // In addition to what the VMX MSRs tell us, make sure that
                    // - NW and CD are kept off as they are not updated on VM exit and we
                    //   don't want them enabled for performance reasons while in root mode
                    // - PE and PG can be freely chosen (by the guest) because we demand
                    //   unrestricted guest mode support anyway
                    // - ET is ignored
                    let must0 = Msr::IA32_VMX_CR0_FIXED1.read()
                        & !(Cr0Flags::NOT_WRITE_THROUGH | Cr0Flags::CACHE_DISABLE).bits();
                    let must1 = Msr::IA32_VMX_CR0_FIXED0.read()
                        & !(Cr0Flags::PAGING | Cr0Flags::PROTECTED_MODE_ENABLE).bits();
                    VmcsField64Guest::CR0.write((val & must0) | must1)?;
                    VmcsField64Control::CR0_READ_SHADOW.write(val)?;
                    VmcsField64Control::CR0_GUEST_HOST_MASK.write(must1 | !must0)?;
                }
                3 => VmcsField64Guest::CR3.write(val)?,
                4 => {
                    // Retrieve/validate restrictions on CR4
                    let must0 = Msr::IA32_VMX_CR4_FIXED1.read();
                    let must1 = Msr::IA32_VMX_CR4_FIXED0.read();
                    let val = val | Cr4Flags::VIRTUAL_MACHINE_EXTENSIONS.bits();
                    VmcsField64Guest::CR4.write((val & must0) | must1)?;
                    VmcsField64Control::CR4_READ_SHADOW.write(val)?;
                    VmcsField64Control::CR4_GUEST_HOST_MASK.write(must1 | !must0)?;
                }
                _ => unreachable!(),
            };
            Ok(())
        })()
        .expect("Failed to write guest control register")
    }
}

impl Debug for Vcpu {
    /// 实现 Debug trait，便于调试输出 VCPU 状态。
    fn fmt(&self, f: &mut Formatter) -> Result {
        (|| -> HvResult<Result> {
            Ok(f.debug_struct("Vcpu")
                .field("guest_regs", &self.guest_regs)
                .field("rip", &self.instr_pointer())
                .field("rsp", &self.stack_pointer())
                .field("rflags", unsafe {
                    &RFlags::from_bits_unchecked(self.rflags())
                })
                .field("cr0", unsafe { &Cr0Flags::from_bits_unchecked(self.cr(0)) })
                .field("cr3", &self.cr(3))
                .field("cr4", unsafe { &Cr4Flags::from_bits_unchecked(self.cr(4)) })
                .field("cs", &VmcsField16Guest::CS_SELECTOR.read()?)
                .field("fs_base", &VmcsField64Guest::FS_BASE.read()?)
                .field("gs_base", &VmcsField64Guest::GS_BASE.read()?)
                .field("tss", &VmcsField16Guest::TR_SELECTOR.read()?)
                .finish())
        })()
        .unwrap()
    }
}

/// VMEXIT 处理函数，裸函数（naked），直接用汇编实现上下文切换。
/// 设计说明：
/// - 保存访客寄存器，切换到宿主栈，调用 vmexit_handler 处理 VMEXIT。
/// - 处理完毕后恢复访客寄存器，执行 vmresume 返回访客环境。
#[naked]
unsafe extern "sysv64" fn vmx_exit() -> ! {
    asm!(
        save_regs_to_stack!(),
        "mov r15, rsp",         // 保存临时 RSP 到 r15
        "mov rsp, [rsp + {0}]", // 切换到宿主栈顶
        "call {1}",             // 调用 VMEXIT 处理函数
        "mov rsp, r15",         // 恢复临时 RSP
        restore_regs_from_stack!(),
        "vmresume",
        "jmp {2}",
        const core::mem::size_of::<GeneralRegisters>(),
        sym crate::arch::vmm::vmexit_handler,
        sym vmresume_failed,
        options(noreturn),
    );
}

/// VMRESUME 失败处理函数，直接 panic 并输出错误信息。
fn vmresume_failed() -> ! {
    panic!("VM resume failed: {:?}", Vmcs::instruction_error());
}

