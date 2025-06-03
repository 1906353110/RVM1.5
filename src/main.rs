#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![cfg_attr(test, allow(dead_code))]
#![feature(asm_sym)]
#![feature(asm_const)]
#![feature(lang_items)]
#![feature(concat_idents)]
#![feature(naked_functions)]
#![allow(unaligned_references)]
/// 项目级配置，声明no_std、no_main等属性，适配内核环境。
/// 【是否需要深究】一般不需深究，通用写法，迁移直接照抄即可。
/// 【迁移关注点】如目标平台有特殊要求，需检查feature支持性。

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate log;
#[macro_use]
extern crate lazy_static;

#[macro_use]
mod logging;
#[macro_use]
mod error;

mod cell;
mod config;
mod consts;
mod header;
mod hypercall;
mod memory;
mod percpu;
mod stats;
/// 引入第三方依赖和自定义模块，便于全局使用。
/// 【是否需要深究】模块职责需了解，但本行不需深究，结构常见。
/// 【迁移关注点】具体模块如memory、cell等可能需要后续适配/修改。

#[cfg(not(test))]
mod lang;

#[cfg(target_arch = "x86_64")]
#[path = "arch/x86_64/mod.rs"]
mod arch;
/// 针对测试环境和特定架构做条件编译，方便平台适配。
/// 【是否需要深究】不需深究，跨平台时根据实际情况实现对应模块。
/// 【迁移关注点】更换平台需注意arch模块实现。

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use config::HvSystemConfig;
use error::HvResult;
use header::HvHeader;
use percpu::PerCpu;
/// 引入常用类型和模块。
/// 【是否需要深究】只需知道用途，迁移时接口一致即可。

// 全局原子变量，用于多CPU协作与同步
static INITED_CPUS: AtomicU32 = AtomicU32::new(0);
static INIT_EARLY_OK: AtomicU32 = AtomicU32::new(0);
static INIT_LATE_OK: AtomicU32 = AtomicU32::new(0);
static ERROR_NUM: AtomicI32 = AtomicI32::new(0);
/// 用于多核/多CPU的初始化状态记录和错误标记。
/// 【是否需要深究】需了解用法，迁移到多核场景必看。
/// 判断是否有错误发生，非0则有错误

fn has_err() -> bool {
    ERROR_NUM.load(Ordering::Acquire) != 0
}
/// 【是否需要深究】常规原子用法，可直接使用。
/// 【迁移关注点】如有更复杂的错误处理机制需扩展此函数。
/// 通用自旋等待函数，等待某个条件变为false，期间检查是否有全局错误

fn wait_for(condition: impl Fn() -> bool) -> HvResult {
    while !has_err() && condition() {
        core::hint::spin_loop(); // 空转等待，降低功耗
    }
    if has_err() {
        hv_result_err!(EBUSY, "Other cpu init failed!") // 其他CPU初始化失败则报错
    } else {
        Ok(())
    }
}
/// 【是否需要深究】建议理解等待机制，涉及多核同步。
/// 【迁移关注点】如迁移到异步模型或引入硬件信号等同步原语，需要调整此机制。
/// 等待某个计数器达到最大值（通常用于等所有CPU进入某阶段）

fn wait_for_counter(counter: &AtomicU32, max_value: u32) -> HvResult {
    wait_for(|| counter.load(Ordering::Acquire) < max_value)
}
/// 【是否需要深究】可直接用，但理解含义有助于后续修改。
/// 【迁移关注点】如有更复杂的同步需求需自定义。
/// 主CPU早期初始化流程，包括日志、配置、内存管理等

fn primary_init_early() -> HvResult {
    logging::init(); // 日志系统初始化
    info!("Primary CPU init early...");

    let system_config = HvSystemConfig::get();
    // 输出配置和编译环境等信息
    println!(
        "\n\
        Initializing hypervisor...\n\
        config_signature = {:?}\n\
        config_revision = {}\n\
        build_mode = {}\n\
        log_level = {}\n\
        arch = {}\n\
        vendor = {}\n\
        stats = {}\n\
        ",
        core::str::from_utf8(&system_config.signature),
        system_config.revision,
        option_env!("MODE").unwrap_or(""),
        option_env!("LOG").unwrap_or(""),
        option_env!("ARCH").unwrap_or(""),
        option_env!("VENDOR").unwrap_or(""),
        option_env!("STATS").unwrap_or("off"),
    );

    memory::init_heap(); // 堆初始化
    system_config.check()?; // 校验配置
    info!("Hypervisor header: {:#x?}", HvHeader::get());
    debug!("System config: {:#x?}", system_config);

    memory::init_frame_allocator(); // 物理内存帧分配器初始化
    memory::init_hv_page_table()?;  // 页表初始化
    cell::init()?; // cell（安全隔离单元）初始化

    INIT_EARLY_OK.store(1, Ordering::Release); // 标记早期初始化完成
    Ok(())
}
/// 【是否需要深究】建议深入理解每个步骤，尤其memory和cell部分，这与底层实现密切相关。
/// 【迁移关注点】内存初始化、cell结构与目标平台强相关，迁移时需重点适配。
/// 主CPU后期初始化流程，目前为空，可扩展用

fn primary_init_late() {
    info!("Primary CPU init late...");
    // 目前未实现，保留扩展
    INIT_LATE_OK.store(1, Ordering::Release); // 标记后期初始化完成
}
/// 【是否需要深究】目前无内容，无需深究。
/// 【迁移关注点】如需在后期添加特殊逻辑，可在此实现。

/// 所有CPU的主入口，处理CPU间同步、阶段切换、初始化等
fn main(cpu_data: &mut PerCpu, linux_sp: usize) -> HvResult {
    let is_primary = cpu_data.id == 0;
    let online_cpus = HvHeader::get().online_cpus;
    wait_for(|| PerCpu::entered_cpus() < online_cpus)?; // 等待所有CPU进入
    println!(
        "{} CPU {} entered.",
        if is_primary { "Primary" } else { "Secondary" },
        cpu_data.id
    );

    if is_primary {
        primary_init_early()?; // 主CPU先做早期初始化
    } else {
        wait_for_counter(&INIT_EARLY_OK, 1)? // 其他CPU等待
    }

    cpu_data.init(linux_sp, cell::root_cell())?; // 各自做本地初始化
    println!("CPU {} init OK.", cpu_data.id);
    INITED_CPUS.fetch_add(1, Ordering::SeqCst); // 累加已完成CPU数
    wait_for_counter(&INITED_CPUS, online_cpus)?; // 等待全部CPU完成

    if is_primary {
        primary_init_late(); // 主CPU后期初始化
    } else {
        wait_for_counter(&INIT_LATE_OK, 1)? // 其他CPU等待
    }

    cpu_data.activate_vmm() // 进入虚拟机监控器VMM运行
}
/// 【是否需要深究】强烈建议理解本流程，是多核启动/同步核心流程，迁移必读。
/// 【迁移关注点】CPU初始化阶段、同步点、VMM激活流程需根据平台适配。


/// Hypervisor的实际入口函数，由外部调用（如bootloader），返回错误码
extern "sysv64" fn entry(cpu_data: &mut PerCpu, linux_sp: usize) -> i32 {
    if let Err(e) = main(cpu_data, linux_sp) {
        error!("{:?}", e);
        ERROR_NUM.store(e.code(), Ordering::Release);
    }
    let code = ERROR_NUM.load(Ordering::Acquire);
    println!(
        "CPU {} return back to driver with code {}.",
        cpu_data.id, code
    );
    code
}
// 【是否需要深究】需了解其调用约定及流程，涉及hypervisor与外部世界的衔接。
// 【迁移关注点】如果更换平台/调用方式（如调用约定或参数），这里要同步更改。
