use libvmm::msr::Msr;
use x86::{segmentation, segmentation::SegmentSelector};

use super::tables::{GdtStruct, TssStruct, IDT};

/// 每个 CPU 特定的架构初始化项：包含 TSS（任务状态段）和 GDT（全局描述符表）
/// 在 Hypervisor 切入时，该结构为当前 CPU 配置新的一套段级别上下文和中断描述符
pub struct ArchPerCpu {
    tss: TssStruct,
    gdt: GdtStruct,
}

impl ArchPerCpu {
    /// 在 CPU 进入 Hypervisor 早期阶段时调用
    /// 负责为本 CPU 分配并加载 GDT、TSS 以及初始化 IDT，还设置 PAT（内存类型）
    pub fn init(&mut self) {
        // 1. 为当前 CPU 动态分配并初始化一个 TSS（任务状态段）
        //    TSS 用来在发生特权转换或中断时快速切换到一个专用的堆栈
        self.tss = TssStruct::alloc();

        // 2. 为当前 CPU 分配并初始化一个 GDT（全局描述符表）
        //    GDT 中会包含 __NULL 段描述符、代码段、数据段、TSS 段描述符等
        self.gdt = GdtStruct::alloc();
        self.gdt.init(&self.tss);

        // 3. 把新 GDT 加载到 GDTR 寄存器，然后更新 CS、DS、SS、ES 等段寄存器
        //    这样后续在 Hypervisor 下执行时，CPU 就使用新的内存段映射和特权级设置
        self.gdt.load();
        unsafe {
            // 清空 ES
            segmentation::load_es(SegmentSelector::from_raw(0));
            // 加载内核代码段选择子（KCODE_SELECTOR）
            segmentation::load_cs(GdtStruct::KCODE_SELECTOR);
            // 清空 SS、DS
            segmentation::load_ss(SegmentSelector::from_raw(0));
            segmentation::load_ds(SegmentSelector::from_raw(0));
        }

        // 4. 加载 IDT（中断描述符表），确保 Hypervisor 可以响应异常和中断
        //    IDT 是一个全局锁保护的单例，.lock() 后调用 load() 把它写入 IDTR
        IDT.lock().load();

        // 5. 加载当前 CPU 的 TSS 段：把 TSS 描述符放到 TR（任务寄存器）中
        //    这样一旦发生中断或特权转换，CPU 可以自动切换到 TSS 中定义的堆栈
        self.gdt.load_tss(GdtStruct::TSS_SELECTOR);

        // 6. 最后设置 IA32_PAT MSR（Page Attribute Table），将 PAT0 设置为 WB、PAT1 为 WC、PAT2 为 UC
        //    这是内存缓存类型映射，WB = Write-Back，WC = Write-Combining，UC = Uncacheable
        unsafe { Msr::IA32_PAT.write(0x070106) };
    }
}
