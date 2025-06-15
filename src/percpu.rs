use core::fmt::{Debug, Formatter, Result};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::vmm::{Vcpu, VcpuAccessGuestState};
use crate::arch::{cpu, ArchPerCpu, LinuxContext};
use crate::cell::Cell;
use crate::consts::{PER_CPU_ARRAY_PTR, PER_CPU_SIZE};
use crate::error::HvResult;
use crate::header::HvHeader;
use crate::memory::VirtAddr;

// 标记已经进入 hypervisor 的 CPU 数量（在 new() 时 +1）
static ENTERED_CPUS: AtomicU32 = AtomicU32::new(0);
// 表示当前处于“已激活”状态的 CPU 数量（vmm 正在运行）
static ACTIVATED_CPUS: AtomicU32 = AtomicU32::new(0);

// CPU 所处状态
#[derive(Debug, Eq, PartialEq)]
pub enum CpuState {
    HvDisabled, // Hypervisor 未激活
    HvEnabled,  // Hypervisor 已激活
}

// 每个 CPU 对应一个 PerCpu 结构，使用 4KB 对齐（页对齐）
#[repr(C, align(4096))]
pub struct PerCpu {
    /// 指向自己的虚拟地址（x86_64 中 thread pointer 需要）
    self_vaddr: VirtAddr,

    pub id: u32,              // 当前 CPU ID
    pub state: CpuState,      // 当前 CPU 的状态
    pub vcpu: Vcpu,           // 当前 CPU 上的虚拟 CPU 实例
    arch: ArchPerCpu,         // 架构相关的特定字段（如 GDT、TSS 等）
    linux: LinuxContext,      // Linux 下保存的上下文信息（保存恢复使用）
    // PerCpu 的栈空间会附加在结构尾部
}

impl PerCpu {
    /// 创建并初始化当前 CPU 对应的 PerCpu 实例
    pub fn new<'a>() -> HvResult<&'a mut Self> {
        // 防止超过允许的最大 CPU 数量
        if Self::entered_cpus() >= HvHeader::get().max_cpus {
            return hv_result_err!(EINVAL);
        }

        // 为当前 CPU 分配一个 ID，并获取其在 PER_CPU 数组中的虚拟地址
        let cpu_id = ENTERED_CPUS.fetch_add(1, Ordering::SeqCst);
        let vaddr = PER_CPU_ARRAY_PTR as VirtAddr + cpu_id as usize * PER_CPU_SIZE;
        let ret = unsafe { &mut *(vaddr as *mut Self) };

        // 填写初始化字段
        ret.id = cpu_id;
        ret.self_vaddr = vaddr;

        // 设置 CPU 的线程指针，用于访问当前线程上下文
        cpu::set_thread_pointer(vaddr);

        Ok(ret)
    }

    /// 获取当前 CPU 对应的 PerCpu 不可变引用
    pub fn current<'a>() -> &'a Self {
        Self::current_mut() // 安全地转换为不可变引用
    }

    /// 获取当前 CPU 对应的 PerCpu 可变引用
    pub fn current_mut<'a>() -> &'a mut Self {
        unsafe { &mut *(cpu::thread_pointer() as *mut Self) }
    }

    /// 返回当前 PerCpu 栈顶的虚拟地址（减8是为了保持栈对齐）
    pub fn stack_top(&self) -> VirtAddr {
        self as *const _ as VirtAddr + PER_CPU_SIZE - 8
    }

    /// 返回已进入 hypervisor 的 CPU 数量
    pub fn entered_cpus() -> u32 {
        ENTERED_CPUS.load(Ordering::Acquire)
    }

    /// 返回已激活 hypervisor 的 CPU 数量
    pub fn activated_cpus() -> u32 {
        ACTIVATED_CPUS.load(Ordering::Acquire)
    }

    /// 初始化当前 PerCpu 实例，包括设置 arch、vcpu、页表等
    pub fn init(&mut self, linux_sp: usize, cell: &Cell) -> HvResult {
        info!("CPU {} init...", self.id);

        // 保存当前 CPU 的 Linux 上下文（用于之后恢复）
        self.state = CpuState::HvDisabled;
        self.linux = LinuxContext::load_from(linux_sp);

        // 初始化架构相关的组件（如中断栈等）
        self.arch.init();

        // 启用 hypervisor 页表，使当前 CPU 进入 HV 地址空间
        unsafe { crate::memory::hv_page_table().read().activate() };

        // 初始化 vCPU 实例，使用 `ptr::write` 避免 Drop 问题
        unsafe {
            core::ptr::write(&mut self.vcpu, Vcpu::new(&self.linux, cell)?);
        }

        self.state = CpuState::HvEnabled;
        Ok(())
    }

    /// 激活 VMM，进入 Guest 环境
    pub fn activate_vmm(&mut self) -> HvResult {
        println!("Activating hypervisor on CPU {}...", self.id);
        ACTIVATED_CPUS.fetch_add(1, Ordering::SeqCst);

        // 进入 VMM 主循环，控制权交给 Guest OS（永远不会返回）
        self.vcpu.enter(&self.linux)?;
        unreachable!() // 理论上不应返回
    }

    /// 从 VMM 退出并恢复 Linux，返回值会传给 Linux
    pub fn deactivate_vmm(&mut self, ret_code: usize) -> HvResult {
        println!("Deactivating hypervisor on CPU {}...", self.id);
        ACTIVATED_CPUS.fetch_sub(1, Ordering::SeqCst);

        // 设置 vcpu 返回值，触发退出
        self.vcpu.set_return_val(ret_code);
        self.vcpu.exit(&mut self.linux)?;

        // 恢复 Linux 上下文
        self.linux.restore();
        self.state = CpuState::HvDisabled;

        // 切换回 Linux：这里实际不会返回（上下文直接跳转）
        self.linux.return_to_linux(self.vcpu.regs());
    }

    /// 注入一个通用 fault（比如 VMExit 异常处理出错）
    pub fn fault(&mut self) -> HvResult {
        warn!("VCPU fault: {:#x?}", self);
        self.vcpu.inject_fault()?; // 注入 fault 给 Guest
        Ok(())
    }
}

// 实现 Debug 输出，用于打印当前 CPU 状态
impl Debug for PerCpu {
    fn fmt(&self, f: &mut Formatter) -> Result {
        let mut res = f.debug_struct("PerCpu");
        res.field("id", &self.id)
            .field("self_vaddr", &self.self_vaddr)
            .field("state", &self.state);
        if self.state != CpuState::HvDisabled {
            res.field("vcpu", &self.vcpu); // 如果启用，打印 vcpu 状态
        } else {
            res.field("linux", &self.linux); // 否则打印 Linux 状态
        }
        res.finish()
    }
}
