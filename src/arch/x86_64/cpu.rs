use libvmm::msr::Msr; // 读写 MSR（Model-Specific Register）寄存器
use super::cpuid::CpuId;

/// 获取 CPU 频率（单位：MHz）
/// 使用 CPUID 指令尝试读取频率，如果读取失败则默认返回 4000MHz。
pub fn frequency() -> u16 {
    static CPU_FREQUENCY: spin::Once<u16> = spin::Once::new(); // 保证只初始化一次
    *CPU_FREQUENCY.call_once(|| {
        const DEFAULT: u16 = 4000;
        CpuId::new()
            .get_processor_frequency_info()                 // 尝试通过 CPUID 获取频率信息
            .map(|info| info.processor_base_frequency())    // 提取 base frequency
            .unwrap_or(DEFAULT)                             // 失败就用默认值
            .max(DEFAULT)                                   // 防止过小，向上取最大
    })
}

/// 获取当前 CPU 的时间戳计数（TSC），单位是 clock cycle。
/// 使用 `rdtscp` 指令（带序列保证的 TSC）。
pub fn current_cycle() -> u64 {
    let mut aux = 0;
    unsafe { core::arch::x86_64::__rdtscp(&mut aux) } // 返回值为 TSC
}

/// 返回当前时间（单位：纳秒）
/// 简单地将 cycle 转为纳秒：cycle * 1000 / MHz
pub fn current_time_nanos() -> u64 {
    current_cycle() * 1000 / frequency() as u64
}

/// 获取当前 CPU 的线程指针（TLS 基址）
/// 相当于访问 `gs:0`，返回 `PerCpu::self_vaddr`
pub fn thread_pointer() -> usize {
    let ret;
    unsafe {
        core::arch::asm!(
        "mov {0}, gs:0",       // 从 GS 寄存器段寄存器偏移 0 处取出值
        out(reg) ret,
        options(nostack)
        )
    };
    ret
}

/// 设置当前线程的 `gs` 基地址（用于访问 TLS）
/// 让 `gs:0` 指向当前 CPU 的 `PerCpu` 结构体
pub fn set_thread_pointer(tp: usize) {
    unsafe {
        Msr::IA32_GS_BASE.write(tp as u64); // 写 MSR 0xC0000101，即 IA32_GS_BASE
    }
}
