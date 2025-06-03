use crate::arch::NestedPageTable;
use crate::config::{CellConfig, HvSystemConfig};
use crate::error::HvResult;
use crate::memory::addr::{GuestPhysAddr, HostPhysAddr};
use crate::memory::{MemFlags, MemoryRegion, MemorySet};
/// 引入各模块的主要类型和函数
/// 【基础知识】
/// - 虚拟化常用Guest/Host物理地址，MemorySet类似于一组内存区间的管理器。
/// - Jailhouse驱动理念下，每个Cell就像“隔离沙箱/虚拟机”。

#[derive(Debug)]
pub struct Cell<'a> {
    /// Cell 配置，描述当前 cell（类似 VM）的资源分配、权限等
    pub config: CellConfig<'a>,
    /// Cell 拥有的 Guest 物理内存视图（即嵌套页表控制的访存范围）
    pub gpm: MemorySet<NestedPageTable>,
}
/// 【基础知识】
/// - Cell可以理解为“虚拟机容器”或“隔离域”，它有自己配置和受保护的内存区。
/// - config：定义这个Cell能干啥、有什么资源
/// - gpm：用来描述它的虚拟物理地址空间

impl Cell<'_> {
    /// 创建root cell（宿主cell，具有最高权限）
    fn new_root() -> HvResult<Self> {
        let sys_config = HvSystemConfig::get(); // 获取全局系统配置
        let cell_config = sys_config.root_cell.config(); // 拿到root cell的配置
        let hv_phys_start = sys_config.hypervisor_memory.phys_start as usize; // hypervisor自身的物理起始地址
        let hv_phys_size = sys_config.hypervisor_memory.size as usize;         // hypervisor自身的内存大小

        let mut gpm = MemorySet::new(); // 创建空的物理内存映射集

        // 1. 先把hypervisor自己的物理内存映射进去（只读，不使用大页）
        gpm.insert(MemoryRegion::new_with_empty_mapper(
            hv_phys_start,
            hv_phys_size,
            MemFlags::READ | MemFlags::NO_HUGEPAGES,
        ))?;
        // 2. 遍历root cell配置的所有物理内存区域，逐个插入gpm（即构造访存映射）
        for region in cell_config.mem_regions() {
            gpm.insert(MemoryRegion::new_with_offset_mapper(
                region.virt_start as GuestPhysAddr,
                region.phys_start as HostPhysAddr,
                region.size as usize,
                region.flags,
            ))?;
        }
        trace!("Guest phyiscal memory set: {:#x?}", gpm);

        Ok(Self {
            config: cell_config,
            gpm,
        })
    }
}
/// 【是否需要深究】建议深入理解！
/// - 这是 cell 初始化（尤其是 root cell，超级管理员cell）的关键流程，关系到内存映射、资源分配、安全隔离等，
/// - 迁移到其它平台或引入多cell、多tenant支持时都要参考这里的思路。
/// 【迁移关注点】
/// - MemoryRegion、MemorySet、嵌套页表（NestedPageTable）都与底层硬件/体系结构相关，适配时要重点调整。

// 用 Once 保证只初始化一次
static ROOT_CELL: spin::Once<Cell> = spin::Once::new();

/// 获取 root cell 的只读引用（全局唯一）
pub fn root_cell<'a>() -> &'a Cell<'a> {
    ROOT_CELL.get().expect("Uninitialized root cell!")
}
/// 【是否需要深究】此处是全局只读单例常用写法，可直接用
/// 【基础知识】Once模式常用于实现安全的全局初始化

/// Cell 管理子系统初始化（只初始化root cell）
pub fn init() -> HvResult {
    crate::arch::vmm::check_hypervisor_feature()?; // 检查底层vmm相关特性（如CPU虚拟化扩展是否支持）

    let root_cell = Cell::new_root()?; // 创建root cell实例
    info!("Root cell init end.");
    debug!("{:#x?}", root_cell);

    ROOT_CELL.call_once(|| root_cell); // 全局只初始化一次
    Ok(())
}
// 【是否需要深究】需要理解init整体流程和新建root cell的操作，迁移到多cell或支持动态cell分配时需调整。
// 【迁移关注点】
// - check_hypervisor_feature和Cell::new_root依赖于架构和硬件，迁移时要结合目标平台能力进行改造。

