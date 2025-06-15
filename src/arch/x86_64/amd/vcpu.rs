use core::arch::asm;
use core::fmt::{Debug, Formatter, Result};

use libvmm::msr::Msr;
use libvmm::svm::flags::{InterruptType, VmcbCleanBits, VmcbIntInfo, VmcbTlbControl};
use libvmm::svm::{vmcb::VmcbSegment, SvmExitCode, SvmIntercept, Vmcb};
use x86::{segmentation, segmentation::SegmentSelector, task};
use x86_64::addr::VirtAddr;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
use x86_64::registers::model_specific::{Efer, EferFlags};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::DescriptorTablePointer;

use crate::arch::segmentation::Segment;
use crate::arch::vmm::VcpuAccessGuestState;
use crate::arch::{GeneralRegisters, GuestPageTableImmut, LinuxContext};
use crate::cell::Cell;
use crate::error::HvResult;
use crate::memory::{addr::virt_to_phys, Frame, GenericPageTableImmut};
use crate::percpu::PerCpu;

/// SVM（AMD 虚拟化）版的虚拟 CPU 结构
#[repr(C)]
pub struct Vcpu {
    /// VMEXIT 时保存访客通用寄存器
    guest_regs: GeneralRegisters,
    /// VMEXIT 时加载的 GS_BASE（线程指针）
    host_tp: u64,
    /// VMEXIT 时加载的 RSP（栈顶）
    host_stack_top: u64,
    /// 保存宿主状态的物理页（SVM SAVE AREA）
    host_save_area: Frame,
    /// 虚拟机控制块（VMCB），包含访客和控制状态
    pub(super) vmcb: Vmcb,
}

impl Vcpu {
    /// 创建并初始化一个 SVM VCPU：
    /// 1. 关闭所有性能计数器
    /// 2. 打开 SVM（Efer.SVML）、设置 HSAVE_PA
    /// 3. 恢复 CR0/CR4 到预定义 Host 值
    /// 4. 分配 host_save_area，构建默认 VMCB
    pub fn new(linux: &LinuxContext, cell: &Cell) -> HvResult<Self> {
        super::check_hypervisor_feature()?;

        // 1) 关闭所有性能计数器，避免 guest 干扰
        unsafe {
            const PERF_EVT_SEL_EN: u64 = 1 << 22;
            for i in 0..6 {
                let sel = match i {
                    0 => Msr::PERF_EVT_SEL0,
                    1 => Msr::PERF_EVT_SEL1,
                    2 => Msr::PERF_EVT_SEL2,
                    3 => Msr::PERF_EVT_SEL3,
                    4 => Msr::PERF_EVT_SEL4,
                    _ => Msr::PERF_EVT_SEL5,
                };
                sel.write(sel.read() & !PERF_EVT_SEL_EN);
            }
        }

        // 2) 检查是否已开启 SVM
        let efer = Efer::read();
        if efer.contains(EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE) {
            return hv_result_err!(EBUSY, "SVM is already turned on!");
        }
        // 分配保存区 & 打开 SVM
        let host_save_area = Frame::new()?;
        unsafe {
            Efer::write(efer | EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE);
            Msr::VM_HSAVE_PA.write(host_save_area.start_paddr() as _);
        }
        info!("successed to turn on SVM.");

        // 3) 复位 CR0/CR4 到预先定义的 Host 值
        unsafe {
            Cr0::write(super::super::HOST_CR0);
            Cr4::write(super::super::HOST_CR4);
        }

        // 4) 构造 VCPU 实例
        let cpu_data = PerCpu::current();
        let mut ret = Self {
            guest_regs: Default::default(),
            host_tp: cpu_data as *const _ as _,
            host_stack_top: cpu_data.stack_top() as _,
            host_save_area,
            vmcb: Default::default(),
        };
        // 设置 VMCB 的访客 & 控制状态
        ret.vmcb_setup(linux, cell);

        Ok(ret)
    }

    /// 进入 guest：
    /// 1. 将 VMCB 物理地址装入 RAX
    /// 2. 恢复访客寄存器
    /// 3. 执行 `clgi`（清中断），`vmload` + 跳转到 SVM 运行入口
    pub fn enter(&mut self, linux: &LinuxContext) -> HvResult {
        let vmcb_paddr = virt_to_phys(&self.vmcb as *const _ as usize);
        let regs = self.regs_mut();
        // 在调用 VMRUN 之前恢复调用者保存寄存器
        regs.rax = vmcb_paddr as _;
        regs.rbx = linux.rbx;
        regs.rbp = linux.rbp;
        regs.r12 = linux.r12;
        regs.r13 = linux.r13;
        regs.r14 = linux.r14;
        regs.r15 = linux.r15;
        unsafe {
            asm!(
                "clgi",                                // 清除中断，确保 SVM 运行不中断
                "mov rsp, {0}",                        // 切换到 VCPU 的栈
                restore_regs_from_stack!(),            // 恢复通用寄存器
                "vmload rax",                          // 加载 VMCB，rax=VMCB PA
                "jmp {1}",                             // 跳到 SVM 运行循环（svm_run）
                in(reg) regs as *const _ as usize,
                sym svm_run,
                options(noreturn),
            );
        }
    }

    /// 退出 guest：
    /// 1. 从 VMCB 保存区读取访客状态到 LinuxContext
    /// 2. 执行 `stgi`（设置中断），关闭 SVM
    pub fn exit(&self, linux: &mut LinuxContext) -> HvResult {
        self.load_vmcb_guest(linux);
        unsafe {
            asm!("stgi");                          // 恢复中断
            Efer::write(Efer::read() & !EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE);
            Msr::VM_HSAVE_PA.write(0);
        }
        info!("successed to turn off SVM.");
        Ok(())
    }

    /// 向 guest 注入一个一般保护异常（#GP）
    pub fn inject_fault(&mut self) -> HvResult {
        self.vmcb.inject_event(
            VmcbIntInfo::from(InterruptType::Exception, crate::arch::ExceptionType::GeneralProtectionFault),
            0,
        );
        Ok(())
    }

    /// 访客指令执行后需前进 RIP
    pub fn advance_rip(&mut self, instr_len: u8) -> HvResult {
        self.vmcb.save.rip += instr_len as u64;
        Ok(())
    }

    /// 判断访客当前是否特权（CPL == 0）
    pub fn guest_is_privileged(&self) -> bool {
        self.vmcb.save.cpl == 0
    }

    /// 判断退出原因是否为 VMMCALL
    pub fn in_hypercall(&self) -> bool {
        matches!(self.vmcb.control.exit_code.try_into(), Ok(SvmExitCode::VMMCALL))
    }

    /// 获取访客页表，用于构建嵌套页表（NPT）
    pub fn guest_page_table(&self) -> GuestPageTableImmut {
        unsafe { GuestPageTableImmut::from_root((self.vmcb.save.cr3 as usize & !0xfff) as _) }
    }
}

// 以下两方法分别用于将 DTR/段寄存器写入 VMCB save area：

impl Vcpu {
    /// 设置 VMCB 中的 GDTR/IDTR
    fn set_vmcb_dtr(vmcb_seg: &mut VmcbSegment, dtr: &DescriptorTablePointer) {
        vmcb_seg.limit = dtr.limit as u32 & 0xffff;
        vmcb_seg.base = dtr.base.as_u64();
    }

    /// 设置 VMCB 中的段寄存器（selector/base/limit/attr）
    fn set_vmcb_segment(vmcb_seg: &mut VmcbSegment, seg: &Segment) {
        vmcb_seg.selector = seg.selector.bits();
        vmcb_seg.attr = seg.access_rights.as_svm_segment_attributes();
        vmcb_seg.limit = seg.limit;
        vmcb_seg.base = seg.base;
    }

    /// 完整初始化 VMCB，包括 save area（访客状态）和 control area（运行控制）
    fn vmcb_setup(&mut self, linux: &LinuxContext, cell: &Cell) {
        // 1) 控制寄存器
        self.set_cr(0, linux.cr0.bits());
        self.set_cr(4, linux.cr4.bits());
        self.set_cr(3, linux.cr3);

        // 2) Save area: 访客的段寄存器 / DTR / RIP / RSP / MSR 等
        let vmcb = &mut self.vmcb.save;
        Self::set_vmcb_segment(&mut vmcb.es, &linux.es);
        Self::set_vmcb_segment(&mut vmcb.cs, &linux.cs);
        Self::set_vmcb_segment(&mut vmcb.ss, &linux.ss);
        Self::set_vmcb_segment(&mut vmcb.ds, &linux.ds);
        Self::set_vmcb_segment(&mut vmcb.fs, &linux.fs);
        Self::set_vmcb_segment(&mut vmcb.gs, &linux.gs);
        Self::set_vmcb_segment(&mut vmcb.tr, &linux.tss);
        Self::set_vmcb_segment(&mut vmcb.ldtr, &Segment::invalid());
        Self::set_vmcb_dtr(&mut vmcb.idtr, &linux.idt);
        Self::set_vmcb_dtr(&mut vmcb.gdtr, &linux.gdt);
        vmcb.cpl            = 0;                       // Linux 进入 hypervisor 前为特权级 0
        vmcb.rflags         = 0x2;                     // 保证 reserved bit 1 always set
        vmcb.rip            = linux.rip;               // 访客下一条指令
        vmcb.rsp            = linux.rsp;               // 访客栈顶
        vmcb.rax            = 0;                       // hypervisor return 值，默认 0
        vmcb.sysenter_cs    = Msr::IA32_SYSENTER_CS.read();
        vmcb.sysenter_eip   = Msr::IA32_SYSENTER_EIP.read();
        vmcb.sysenter_esp   = Msr::IA32_SYSENTER_ESP.read();
        vmcb.star           = linux.star;
        vmcb.lstar          = linux.lstar;
        vmcb.cstar          = linux.cstar;
        vmcb.sfmask         = linux.fmask;
        vmcb.kernel_gs_base = Msr::IA32_KERNEL_GSBASE.read();
        vmcb.efer           = linux.efer | EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE.bits(); // SVM 标志
        vmcb.g_pat          = linux.pat;
        vmcb.dr7            = 0x400;                   // 单步调试屏蔽
        vmcb.dr6            = 0xffff_0ff0;             // 断点状态

        // 3) Control area: 定义哪些事件引起退出 & NPT
        let vmcb = &mut self.vmcb.control;
        vmcb.intercept_exceptions = 0;
        vmcb.np_enable           = 1;                  // 启用 NPT（Nested Paging）
        vmcb.guest_asid          = 1;                  // 唯一 guest ASID
        vmcb.clean_bits          = VmcbCleanBits::empty(); // 全部 state 标记为“新”
        vmcb.nest_cr3            = cell.gpm.page_table().root_paddr() as _;
        vmcb.tlb_control         = VmcbTlbControl::FlushAsid as _;

        // 4) 设置要拦截的指令/事件
        for &intc in &[
            SvmIntercept::NMI, SvmIntercept::CPUID, SvmIntercept::SHUTDOWN,
            SvmIntercept::VMRUN, SvmIntercept::VMMCALL, SvmIntercept::VMLOAD,
            SvmIntercept::VMSAVE, SvmIntercept::STGI, SvmIntercept::CLGI,
            SvmIntercept::SKINIT,
        ] {
            self.vmcb.set_intercept(intc);
        }
    }

    /// 在 VMCB save area 读取回访客寄存器，恢复 LinuxContext
    fn load_vmcb_guest(&self, linux: &mut LinuxContext) {
        let vmcb = &self.vmcb.save;
        linux.rip  = vmcb.rip;
        linux.rsp  = vmcb.rsp;
        linux.cr0  = Cr0Flags::from_bits_truncate(vmcb.cr0);
        linux.cr3  = vmcb.cr3;
        linux.cr4  = Cr4Flags::from_bits_truncate(vmcb.cr4);
        linux.efer = vmcb.efer & !EferFlags::SECURE_VIRTUAL_MACHINE_ENABLE.bits();

        // 恢复段寄存器选择子
        linux.es.selector  = SegmentSelector::from_raw(vmcb.es.selector);
        linux.cs.selector  = SegmentSelector::from_raw(vmcb.cs.selector);
        linux.ss.selector  = SegmentSelector::from_raw(vmcb.ss.selector);
        linux.ds.selector  = SegmentSelector::from_raw(vmcb.ds.selector);

        // 恢复 GDT/IDT 基址和限长
        linux.gdt.base = VirtAddr::new(vmcb.gdtr.base);
        linux.gdt.limit= vmcb.gdtr.limit as _;
        linux.idt.base = VirtAddr::new(vmcb.idtr.base);
        linux.idt.limit= vmcb.idtr.limit as _;

        // FS/GS/TSS 等需手动恢复
        linux.fs.selector  = segmentation::fs();
        linux.gs.selector  = segmentation::gs();
        linux.tss.selector = unsafe { task::tr() };
        linux.fs.base      = Msr::IA32_FS_BASE.read();
        linux.gs.base      = vmcb.gs.base;
    }
}

/// 实现 VcpuAccessGuestState trait，方便外部统一访问 guest 寄存器
impl VcpuAccessGuestState for Vcpu {
    fn regs(&self) -> &GeneralRegisters { &self.guest_regs }
    fn regs_mut(&mut self) -> &mut GeneralRegisters { &mut self.guest_regs }
    fn instr_pointer(&self) -> u64 { self.vmcb.save.rip }
    fn stack_pointer(&self) -> u64 { self.vmcb.save.rsp }
    fn set_stack_pointer(&mut self, sp: u64) { self.vmcb.save.rsp = sp }
    fn rflags(&self) -> u64 { self.vmcb.save.rflags }
    fn fs_base(&self) -> u64 { Msr::IA32_FS_BASE.read() }
    fn gs_base(&self) -> u64 { self.vmcb.save.gs.base }
    fn cr(&self, cr_idx: usize) -> u64 { match cr_idx {
        0 => self.vmcb.save.cr0,
        3 => self.vmcb.save.cr3,
        4 => self.vmcb.save.cr4,
        _ => unreachable!(),
    }}
    fn set_cr(&mut self, cr_idx: usize, val: u64) { match cr_idx {
        0 => self.vmcb.save.cr0 = val & !Cr0Flags::NOT_WRITE_THROUGH.bits(),
        3 => self.vmcb.save.cr3 = val,
        4 => self.vmcb.save.cr4 = val,
        _ => unreachable!(),
    }}
}

impl Debug for Vcpu {
    fn fmt(&self, f: &mut Formatter) -> Result {
        // 只打印核心字段，便于调试
        f.debug_struct("Vcpu")
            .field("guest_regs", &self.guest_regs)
            .field("rip", &self.instr_pointer())
            .field("rsp", &self.stack_pointer())
            .field("rflags", unsafe { &RFlags::from_bits_unchecked(self.rflags()) })
            .field("cr0", unsafe { &Cr0Flags::from_bits_unchecked(self.cr(0)) })
            .field("cr3", &self.cr(3))
            .field("cr4", unsafe { &Cr4::from_bits_unchecked(self.cr(4)) })
            .finish()
    }
}

/// SVM 运行循环的汇编入口，naked 函数实现：
/// 1. 执行 VMRUN
/// 2. 保存宿主寄存器到栈
/// 3. 切换到 host_tp/host_stack_top
/// 4. 调用 vmexit_handler_wrapper
/// 5. 恢复栈和寄存器，重新 VMRUN
#[naked]
unsafe extern "sysv64" fn svm_run() -> ! {
    asm!(
        "vmrun rax",
        save_regs_to_stack!(),
        "mov r14, rax",             // 保存 VMRUN 返回值
        "mov r15, rsp",             // 暂存 RSP
        "mov rdi, [rsp + {0}]",     // 第一个参数：host_tp
        "mov rsp, [rsp + {0} + 8]", // RSP = host_stack_top
        "call {1}",                 // vmexit_handler_wrapper
        "lea rsp, [r15 + 8]",       // 恢复 RSP，跳过 saved RAX
        "push r14",                 // 恢复 RAX
        restore_regs_from_stack!(),
        "jmp {2}",                  // 再次 VMRUN
        const core::mem::size_of::<GeneralRegisters>(),
        sym vmexit_handler_wrapper,
        sym svm_run,
        options(noreturn),
    );
}

/// VMEXIT 处理器 wrapper：
/// 保存/恢复 GS_BASE，然后调用 Rust 处理函数
extern "sysv64" fn vmexit_handler_wrapper(cpu_data: &mut PerCpu) {
    // VMRUN 不自动保存/恢复 GS_BASE，需要手动处理
    let guest_tp = Msr::IA32_GS_BASE.read();
    cpu_data.vcpu.vmcb.save.gs.base = guest_tp;
    unsafe { Msr::IA32_GS_BASE.write(cpu_data as *const _ as u64) };
    crate::arch::vmm::vmexit_handler();
    unsafe { Msr::IA32_GS_BASE.write(guest_tp) };
}
