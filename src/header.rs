use core::fmt::{Debug, Formatter, Result};

use crate::consts::{HV_HEADER_PTR, PER_CPU_SIZE};

/// 固定的 Header 签名，用于校验镜像有效性
const HEADER_SIGNATURE: [u8; 8] = *b"RVMIMAGE";

/// Hypervisor 镜像最前端的 Header 结构体
/// 由编译器和链接脚本在 `.header` 段中自动放置
#[repr(C)]
pub struct HvHeader {
    pub signature: [u8; 8],       // “RVMIMAGE” 标识
    pub core_size: usize,         // Hypervisor 核心（代码+数据）大小
    pub percpu_size: usize,       // 每个 CPU 专属空间的大小（由 PER_CPU_SIZE 确定）
    pub entry: usize,             // 从 Header 后跳转到 Hypervisor 入口的偏移量
    pub console_page: usize,      // Debug 控制台使用的物理页地址（暂未使用）
    pub gcov_info_head: usize,    // gcov 测试信息头地址（暂未使用）
    pub max_cpus: u32,            // Hypervisor 支持的最大 CPU 数量
    pub online_cpus: u32,         // 在运行时通告给 Hypervisor 的在线 CPU 数量
    pub debug_console_base: usize, // Debug 控制台的基础地址（暂未使用）
    pub arm_linux_hyp_vectors: u64, // ARM 架构特有，用于指向 Linux HYP 异常向量表（ARM 专用）
    pub arm_linux_hyp_abi: u32,   // ARM HYP ABI 版本（ARM 专用）
}

/// 全局只能在「物理内存映射后」才能使用
/// HV_HEADER_PTR 由 linker script 定义，指向 `.header` 段的起始
impl HvHeader {
    /// 获取 Header 的只读引用
    pub fn get<'a>() -> &'a Self {
        unsafe { &*HV_HEADER_PTR }
    }
}

//
// 以下部分用于在编译期间生成 `.header` 段的内容
// `.header` 段会被 linker 连到最终的二进制镜像最前面，
// 这样 Bootloader 或者宿主程序就能读取 Header 字段.
//

#[repr(C)]
struct HvHeaderStuff {
    signature: [u8; 8],                    // 必须先放置签名
    core_size: unsafe extern "C" fn(),     // 编译时填充的核心大小函数入口
    percpu_size: usize,                    // 每 CPU 专属区的大小
    entry: unsafe extern "C" fn(),         // 编译时填充的入口偏移函数
    console_page: usize,                   // 运行时可填充，暂置 0
    gcov_info_head: usize,                 // 运行时可填充，暂置 0
    max_cpus: u32,                         // 运行时填充在线 CPU 数
    online_cpus: u32,                      // 运行时填充的最大 CPU 数
    debug_console_base: usize,             // 运行时可填充，暂置 0
    arm_linux_hyp_vectors: u64,            // ARM 专用，暂置 0
    arm_linux_hyp_abi: u32,                // ARM 专用，暂置 0
}

// 编译期导入：这两个函数由链接器脚本生成，用于告诉 Header 核心区和入口区的大小
extern "C" {
    fn __entry_offset();
    fn __core_size();
}

/// 这一段会被放到 ELF 的 `.header` 段，
/// 最终镜像在启动时首先拷贝这段到内存，然后 Hypervisor 从此读取自身布局
#[used]
#[link_section = ".header"]
static HEADER_STUFF: HvHeaderStuff = HvHeaderStuff {
    signature: HEADER_SIGNATURE,
    core_size: __core_size,
    percpu_size: PER_CPU_SIZE, // 从 consts 中拿到 PerCpu 区的大小
    entry: __entry_offset,
    console_page: 0,
    gcov_info_head: 0,
    max_cpus: 0,
    online_cpus: 0,
    debug_console_base: 0,
    arm_linux_hyp_vectors: 0,
    arm_linux_hyp_abi: 0,
};

/// 实现 Debug 便于打印 Header 信息
impl Debug for HvHeader {
    fn fmt(&self, f: &mut Formatter) -> Result {
        f.debug_struct("HvHeader")
            .field("signature", &core::str::from_utf8(&self.signature))
            .field("core_size", &self.core_size)
            .field("percpu_size", &self.percpu_size)
            .field("entry", &self.entry)
            .field("max_cpus", &self.max_cpus)
            .field("online_cpus", &self.online_cpus)
            .finish()
    }
}
