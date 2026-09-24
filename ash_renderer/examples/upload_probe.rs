//! 3.2 上传链探针:转换 → 合批上传 → 驻留去重 → 维护扩容 → 跨族依赖,五层全链证据。
//!
//! 与 memory_probe 同纪律:各组是"观察 + 诊断",验证层 + 同步验证常开,VUID 收账:
//! - 组 A(转换观察组,纯 CPU):交错 32B 布局逐字节验证(pos@0/normal@12/uv@24、
//!   小端、stride);U16 索引统一加宽 U32;三种拒绝(拓扑/空/缺 POSITION)按定案
//!   策略各验一遍。
//! - 组 B(上传闭环观察组):两个 mesh 身份走真实交付路径(MeshPool + Uploader 本体,
//!   非探针复制品)——一次合批一个 staging 范围 + 一次 transfer 提交,timeline 票据
//!   完成 CPU 等待后,经 pool→readback 拷贝逐字节比对池内容;重复快照查驻留缓存
//!   零新提交(票据不增,判定线"重复快照不重复上传")。
//! - 组 C(维护扩容观察组):小容量池(首个批次触发 1MiB 初建)先收小资产,再收
//!   1.2MiB 资产触发显式维护——等在飞票据 → 整段迁移 → 等迁移票据 → 销毁旧池,
//!   停顿入账;迁移后新旧两资产内容逐字节复核(迁移没搬坏数据)。
//! - 组 D(跨族依赖观察组,3.4 接线预演):transfer 提交批末 release(仅当本机有
//!   专用 transfer 族),graphics 提交侧等 timeline 票据(GPU 侧等待,CPU 不阻塞)
//!   + acquire + 真实读(pool→readback)+ fence;宿主比对。EXCLUSIVE 成对
//!     release/acquire 在同步验证下全链跑通;同族回退机(无专用 transfer 族)无
//!     ownership 转移,形状由 memory_probe 同族闭环覆盖,本组注明跳过。
//!
//! 自含 Entry → Instance → PhysicalDevice → Device 最小链(不碰窗口);与主工程
//! 差异:无 surface/swapchain;timelineSemaphore 显式启用(上传票据真实使用,
//! 支持与启用分开——1.3 支持 mandatory,启用仍须声明)。接口定案:旧 SubmitInfo
//! + TimelineSemaphoreSubmitInfo + 旧 pipeline barrier(不启用 synchronization2,
//!   不混用 submit2/barrier2)。销毁纪律:各组先等票到 + 宿主读完,再拆——无在途
//!   引用可证明。

use std::sync::Mutex;

use ash::{ext::debug_utils, vk, Device, Entry};
use ash_renderer::vulkan::{
    convert_mesh, BufferRole, CopyRegion, GpuBuffer, MemoryContract, MeshConvertError, MeshPool,
    PoolRange, StagingCopy, UploadBatch, Uploader, VERTEX_STRIDE,
};
use bevy::asset::{uuid::Uuid, AssetId, RenderAssetUsages};
use bevy::mesh::{Indices, Mesh, PrimitiveTopology, VertexAttributeValues};

const VALIDATION_LAYER: &std::ffi::CStr = c"VK_LAYER_KHRONOS_validation";

/// 探针专用的稳定资产身份:Uuid 变体(bevy_asset 显式注册路径),与 App 路径的
/// Index 身份同构可哈希——去重判定逻辑一致,只是身份来源不同。
fn probe_id(n: u64) -> AssetId<Mesh> {
    AssetId::Uuid {
        uuid: Uuid::from_u128(u128::from(n)),
    }
}

/// 验证层消息收账:WARNING/ERROR 全记,组间分账、探针结束统一打印。
static VU_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn main() {
    let entry = unsafe { Entry::load() }.expect("加载 Vulkan loader");

    // 验证层三件事分开报:装没装、请求没请求、收没收到 VUID(结尾分账)。
    let validation_installed = unsafe { entry.enumerate_instance_layer_properties() }
        .unwrap_or_default()
        .iter()
        .any(|props| unsafe {
            std::ffi::CStr::from_ptr(props.layer_name.as_ptr()) == VALIDATION_LAYER
        });
    println!(
        "验证层 {VALIDATION_LAYER:?}: {}",
        if validation_installed {
            "已安装,本探针请求启用 + 同步验证(VUID 收账见结尾)"
        } else {
            "未安装(本机缺 Vulkan SDK),本轮裸奔——结果不构成验证证据"
        }
    );

    let app_info = vk::ApplicationInfo::default()
        .application_name(c"upload_probe")
        .api_version(vk::API_VERSION_1_3);
    let ext_names: Vec<*const std::ffi::c_char> = if validation_installed {
        vec![debug_utils::NAME.as_ptr()]
    } else {
        Vec::new()
    };
    let layer_names: Vec<*const std::ffi::c_char> = if validation_installed {
        vec![VALIDATION_LAYER.as_ptr()]
    } else {
        Vec::new()
    };
    // 同步验证随主工程同款(VkValidationFeaturesEXT);enables 须活到 create_instance 返回
    let sync_enables = if validation_installed {
        Some([vk::ValidationFeatureEnableEXT::SYNCHRONIZATION_VALIDATION])
    } else {
        None
    };
    let mut sync_validation = vk::ValidationFeaturesEXT::default();
    let mut instance_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&ext_names)
        .enabled_layer_names(&layer_names);
    if let Some(enables) = &sync_enables {
        sync_validation = sync_validation.enabled_validation_features(enables);
        instance_info = instance_info.push_next(&mut sync_validation);
    }
    let instance =
        unsafe { entry.create_instance(&instance_info, None) }.expect("vkCreateInstance");

    let _debug = validation_installed.then(|| {
        let loader = debug_utils::Instance::new(&entry, &instance);
        let messenger_info = vk::DebugUtilsMessengerCreateInfoEXT::default()
            .message_severity(
                vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                    | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
            )
            .message_type(
                vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                    | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                    | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
            )
            .pfn_user_callback(Some(probe_callback));
        unsafe { loader.create_debug_utils_messenger(&messenger_info, None) }
            .expect("创建验证 messenger")
    });

    // 与主工程同门槛:只选报 1.3 的设备
    let pds = unsafe { instance.enumerate_physical_devices() }.expect("枚举物理设备");
    let pd = *pds
        .iter()
        .find(|pd| unsafe { instance.get_physical_device_properties(**pd) }.api_version
            >= vk::API_VERSION_1_3)
        .expect("没有报支持 Vulkan 1.3 的物理设备");
    let props = unsafe { instance.get_physical_device_properties(pd) };
    let dev_name =
        unsafe { std::ffi::CStr::from_ptr(props.device_name.as_ptr()) }.to_string_lossy();
    println!(
        "设备 {dev_name}: API v{}.{}.{}(过 1.3 基线)",
        vk::api_version_major(props.api_version),
        vk::api_version_minor(props.api_version),
        vk::api_version_patch(props.api_version),
    );

    group_a_conversion();

    let (device, gfx_family, gfx_queue, transfer_family, transfer_queue) =
        create_device(&instance, pd);
    let contract = unsafe { MemoryContract::new(&instance, pd) };
    let cross_family = transfer_family != gfx_family;
    println!(
        "上传链参数:graphics 族 {gfx_family},transfer 族 {transfer_family}({})",
        if cross_family {
            "专用 transfer 族,跨族分支可实测"
        } else {
            "同族回退(无专用 transfer 族),跨族组按回退口径注明"
        }
    );

    group_b_upload_closure(&device, &contract, transfer_family, transfer_queue);
    group_c_maintenance(&device, &contract, transfer_family, transfer_queue);
    if cross_family {
        group_d_cross_family(
            &device,
            &contract,
            gfx_family,
            gfx_queue,
            transfer_family,
            transfer_queue,
        );
    } else {
        println!("\n== 组 D(跨族依赖观察):本机无专用 transfer 族,EXCLUSIVE release/acquire 路径无硬件载体——");
        println!(
            "   回退路径未实测(诚实注明);同族形状(等票据 + TRANSFER→消费屏障,无 ownership 转移)"
        );
        println!("   已由 memory_probe 组 A 同族闭环在验证层下零 VUID 覆盖。");
    }

    unsafe { device.destroy_device(None) };

    // messenger 先于 instance 销毁——ash 0.38 无自动 Drop,漏了会被 VUID-00629 收账
    if let Some(messenger) = _debug {
        let loader = debug_utils::Instance::new(&entry, &instance);
        unsafe { loader.destroy_debug_utils_messenger(messenger, None) };
    }
    unsafe { instance.destroy_instance(None) };

    println!("\n== 验证层消息收账(全程)==");
    let vu = VU_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if vu.is_empty() {
        println!("  (零条——上传全链在验证层 + 同步验证下清净)");
    } else {
        for m in vu.iter() {
            println!("  {m}");
        }
    }
    println!("\n结论口径:组 A=转换逐字节;组 B=上传闭环 + 票据 + 去重;组 C=维护迁移内容保持;");
    println!(
        "组 D=跨族 release/acquire 全链(本机具备专用 transfer 族时)。规范依据见 3.2 施工记录。"
    );
}

// ============ 组 A:转换观察(纯 CPU)============

fn group_a_conversion() {
    println!("\n== 组 A(转换观察组:32B 交错布局 + 索引 + 拒绝策略)==");

    // 全字段 + U16 索引:逐字节验布局
    let quad = demo_mesh(0, 4, false);
    let c = convert_mesh(&quad).expect("quad 可转换");
    assert_eq!(c.vertex_count, 4);
    assert_eq!(c.index_count, 6);
    assert_eq!(
        c.vertices.len(),
        4 * VERTEX_STRIDE as usize,
        "stride 32B × 4"
    );
    assert_eq!(c.indices.len(), 6 * 4, "U16 输入 → U32 输出 × 6");
    let f32le = |bytes: &[u8]| f32::from_le_bytes(bytes.try_into().expect("4 字节"));
    // 顶点 1(pos 1,0,0 / normal 0,0,1 / uv 1,0):offset 32,字段 @0/@12/@24
    assert_eq!(f32le(&c.vertices[32..36]), 1.0, "顶点1 pos.x");
    assert_eq!(f32le(&c.vertices[36..40]), 2.0, "顶点1 pos.y");
    assert_eq!(f32le(&c.vertices[32 + 12..32 + 16]), 0.0, "顶点1 normal.x");
    assert_eq!(f32le(&c.vertices[32 + 20..32 + 24]), 1.0, "顶点1 normal.z");
    assert_eq!(f32le(&c.vertices[32 + 24..32 + 28]), 1.0, "顶点1 uv.u");
    assert_eq!(f32le(&c.vertices[32 + 28..32 + 32]), 0.0, "顶点1 uv.v");
    // 顶点 2:seed=0 → pos = (2,4,6)
    assert_eq!(f32le(&c.vertices[64..68]), 2.0, "顶点2 pos.x");
    assert_eq!(f32le(&c.vertices[64 + 4..64 + 8]), 4.0, "顶点2 pos.y");
    println!(
        "[布局] 顶点1 pos@32 {:?} normal@44 {:?} uv@56 {:?};顶点2 pos@64 = (2,4,6)——offset 0/12/24、小端全过",
        &c.vertices[32..36],
        &c.vertices[32 + 12..32 + 16],
        &c.vertices[32 + 24..32 + 28],
    );

    // U16 → U32 加宽保序(含重复索引 2,1,考 firstIndex 语义)
    for (i, expect) in [0u32, 1, 2, 2, 1, 3].iter().enumerate() {
        let b = &c.indices[i * 4..i * 4 + 4];
        assert_eq!(
            u32::from_le_bytes(b.try_into().unwrap()),
            *expect,
            "索引 {i}"
        );
    }
    println!("[索引] U16 [0,1,2,2,1,3] → U32 加宽保序(重复索引保序,firstIndex 语义可考)");

    // 缺 NORMAL/UV(U32 索引):0 填充
    let bare = demo_mesh(7, 4, true);
    let c2 = convert_mesh(&bare).expect("bare 可转换");
    for v in 0..4 {
        let base = v * VERTEX_STRIDE as usize;
        assert!(
            c2.vertices[base + 12..base + 24].iter().all(|&b| b == 0),
            "顶点{v} NORMAL 区全 0"
        );
        assert!(
            c2.vertices[base + 24..base + 32].iter().all(|&b| b == 0),
            "顶点{v} UV 区全 0"
        );
    }
    println!("[缺字段] NORMAL/UV_0 缺失 → 全 0 填充(3.4 调试着色可容忍,信息缺口日志化)");

    // 无索引网格:顺序索引生成
    let mut non_indexed = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD,
    );
    non_indexed.insert_attribute(
        Mesh::ATTRIBUTE_POSITION,
        VertexAttributeValues::Float32x3(vec![[0.0; 3]; 3]),
    );
    let c3 = convert_mesh(&non_indexed).expect("无索引可转换");
    assert_eq!(c3.index_count, 3, "无索引 → 顺序索引 0..n");
    for i in 0..3u32 {
        assert_eq!(
            u32::from_le_bytes(
                c3.indices[i as usize * 4..i as usize * 4 + 4]
                    .try_into()
                    .unwrap()
            ),
            i
        );
    }
    println!("[无索引] 顺序索引 0..n 显式生成(不跳过)");

    // 拒绝三连:拓扑/空/缺 POSITION(错误值精确断言,不是"随便报错")
    let strip = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::MAIN_WORLD);
    assert!(matches!(
        convert_mesh(&strip),
        Err(MeshConvertError::UnsupportedTopology(
            PrimitiveTopology::LineList
        ))
    ));
    let empty = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD,
    );
    assert!(matches!(
        convert_mesh(&empty),
        Err(MeshConvertError::EmptyMesh)
    ));
    let mut no_pos = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD,
    );
    no_pos.insert_attribute(
        Mesh::ATTRIBUTE_NORMAL,
        VertexAttributeValues::Float32x3(vec![[0.0, 0.0, 1.0]; 4]),
    );
    assert!(matches!(
        convert_mesh(&no_pos),
        Err(MeshConvertError::MissingPosition)
    ));
    println!(
        "[拒绝] 拓扑 LineList / 空网格 / 缺 POSITION 三种拒收精确匹配错误类型(不 panic、不静默)"
    );
}

/// demo 网格:`seed` 决定数值,`verts` 顶点数;`bare` = 只有 POSITION + U32 重复索引,
/// 否则三字段全带 + U16 索引(索引 [0,1,2,2,1,3],含重复考 firstIndex 语义)。
fn demo_mesh(seed: usize, verts: usize, bare: bool) -> Mesh {
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD,
    );
    let positions: Vec<[f32; 3]> = (0..verts)
        .map(|i| {
            [
                (i + seed) as f32,
                ((i + seed) * 2) as f32,
                ((i + seed) * 3) as f32,
            ]
        })
        .collect();
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_POSITION,
        VertexAttributeValues::Float32x3(positions),
    );
    if !bare {
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_NORMAL,
            VertexAttributeValues::Float32x3(vec![[0.0, 0.0, 1.0]; verts]),
        );
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_UV_0,
            VertexAttributeValues::Float32x2(
                (0..verts).map(|i| [i as f32, 1.0 - i as f32]).collect(),
            ),
        );
        let last = 3.min(verts - 1) as u16;
        mesh.insert_indices(Indices::U16(vec![0, 1, 2, 2, 1, last]));
    } else {
        let last = 3.min(verts - 1) as u32;
        mesh.insert_indices(Indices::U32(vec![0, 1, 2, 2, 1, last]));
    }
    mesh
}

// ============ 上传与校验助手(组 B/C/D 共用,调交付类型本体)============

/// 单资产走完整交付路径:去重判定 → 容量保证 → bump → 排批 → 提交 → 驻留登记。
/// 返回票据;重复身份直接短路返回 0(零提交)。
fn upload_asset(
    pool: &mut MeshPool,
    uploader: &mut Uploader,
    id: AssetId<Mesh>,
    converted: ash_renderer::vulkan::ConvertedMesh,
) -> u64 {
    if pool.resident(id).is_some() {
        return 0; // 去重:重复快照零提交(与 App 编排同判定)
    }
    let vertex_size = converted.vertices.len() as u64;
    let index_size = converted.indices.len() as u64;
    pool.ensure_capacity(uploader, vertex_size, index_size)
        .expect("容量保证(含维护路径)");
    let vertex_buffer = pool.vertex_buffer().expect("容量保证后顶点池必在");
    let index_buffer = pool.index_buffer().expect("容量保证后索引池必在");
    let (vertex, index) = pool.alloc(vertex_size, index_size);
    let mut staging = Vec::with_capacity((vertex_size + index_size) as usize);
    staging.extend_from_slice(&converted.vertices);
    staging.extend_from_slice(&converted.indices);
    let index_src = vertex_size;
    let ticket = uploader
        .submit_batch(UploadBatch {
            staging,
            uploads: vec![
                StagingCopy {
                    dst: vertex_buffer,
                    src_offset: 0,
                    dst_offset: vertex.offset,
                    size: vertex.size,
                },
                StagingCopy {
                    dst: index_buffer,
                    src_offset: index_src,
                    dst_offset: index.offset,
                    size: index.size,
                },
            ],
            ..Default::default()
        })
        .expect("transfer 提交")
        .expect("staging 非空必有票据");
    pool.commit(
        id,
        vertex,
        index,
        converted.vertex_count,
        converted.index_count,
        ticket,
    )
    .expect("登记(探针内身份唯一)");
    ticket
}

/// 读回校验:一段 pool 区间经一次 batch(device_copies)拷到 readback 同偏移,
/// 等票据完成后宿主逐字节比对期望内容。创建的 readback buffer 返回(组 D 复用)。
#[expect(
    clippy::too_many_arguments,
    reason = "读回校验面需要设备/契约/上传器/两池/区间/期望六组事实,拆结构体是探针内过度设计"
)]
fn verify_ranges(
    device: &Device,
    contract: &MemoryContract,
    uploader: &mut Uploader,
    vertex_pool: vk::Buffer,
    index_pool: vk::Buffer,
    ranges: &[(bool, PoolRange)], // true = 索引池
    expected: &[&[u8]],
    label: &str,
) -> (GpuBuffer, GpuBuffer) {
    // 按池种类各建一份 readback:不同种的拷贝若落同一 buffer 会重叠写,
    // 同提交内重叠写未定义——顶点/索引分开落位,同种内 bump 偏移天然不重叠
    let ends = |want_index: bool| {
        ranges
            .iter()
            .filter(|&&(is_index, _)| is_index == want_index)
            .map(|&(_, r)| r.offset + r.size)
            .max()
            .unwrap_or(1)
    };
    let mut readback_v = GpuBuffer::create(device, contract, ends(false), BufferRole::Readback)
        .expect("readback_v 创建");
    let mut readback_i = GpuBuffer::create(device, contract, ends(true), BufferRole::Readback)
        .expect("readback_i 创建");
    let copies: Vec<CopyRegion> = ranges
        .iter()
        .map(|&(is_index, r)| CopyRegion {
            src: if is_index { index_pool } else { vertex_pool },
            dst: if is_index {
                readback_i.buffer()
            } else {
                readback_v.buffer()
            },
            src_offset: r.offset,
            dst_offset: r.offset,
            size: r.size,
        })
        .collect();
    let ticket = uploader
        .submit_batch(UploadBatch {
            device_copies: copies,
            ..Default::default()
        })
        .expect("回读提交")
        .expect("回读批必有票据");
    uploader.wait_until(ticket).expect("等回读票据");
    for (&(is_index, r), want) in ranges.iter().zip(expected.iter()) {
        let mut back = vec![0u8; r.size as usize];
        let target = if is_index {
            &mut readback_i
        } else {
            &mut readback_v
        };
        target.read(r.offset, &mut back).expect("readback 宿主读");
        assert_eq!(
            &back[..],
            *want,
            "{label} {} 池 @{} 读回不符",
            if is_index { "索引" } else { "顶点" },
            r.offset
        );
    }
    (readback_v, readback_i)
}

// ============ 组 B:上传闭环 + 票据 + 去重 ============

fn group_b_upload_closure(
    device: &Device,
    contract: &MemoryContract,
    transfer_family: u32,
    transfer_queue: vk::Queue,
) {
    let mut pool = MeshPool::new(device, contract);
    let mut uploader = Uploader::new(
        device,
        contract,
        transfer_family,
        transfer_queue,
        256 * 1024,
        2,
    )
    .expect("组 B 创建上传器");
    println!(
        "[编排] MeshPool 懒建 + Uploader(staging 环 2 槽 × 256KiB 起步,transfer 族 {transfer_family})"
    );

    let id_small = probe_id(1);
    let id_quad = probe_id(2);
    let small_converted = convert_mesh(&demo_mesh(0, 4, false)).expect("small 转换");
    let quad_converted = convert_mesh(&demo_mesh(100, 4, true)).expect("quad 转换");
    // 快照里带池后拷贝一份期望值(readback 校验用)
    let small_expect_v = small_converted.vertices.clone();
    let small_expect_i = small_converted.indices.clone();
    let quad_expect_v = quad_converted.vertices.clone();
    let quad_expect_i = quad_converted.indices.clone();

    let t1 = upload_asset(&mut pool, &mut uploader, id_small, small_converted);
    let t2 = upload_asset(&mut pool, &mut uploader, id_quad, quad_converted);
    println!("[提交] 两资产分两批:票据 #{t1}、#{t2}(单调 +1,一次合批一次 transfer 提交)");
    assert_eq!(t1, 1);
    assert_eq!(t2, 2);
    assert_eq!(pool.resident_count(), 2, "两身份均驻留");
    let ((uv, cv), (ui, ci)) = pool.usage();
    println!("[池] 顶点 {uv}/{cv}B 索引 {ui}/{ci}B(bump 游标;初建容量 = 需量加余量)");

    // 重复快照:同身份再来一遍 → 去重短路,票据不再增
    let t3 = upload_asset(
        &mut pool,
        &mut uploader,
        id_small,
        convert_mesh(&demo_mesh(0, 4, false)).expect("重复转换"),
    );
    assert_eq!(t3, 0, "重复身份零提交");
    assert_eq!(uploader.last_issued_ticket(), 2, "重复快照不发票据");
    println!("[去重] 重复快照查驻留缓存命中 → 零提交、票据停在 #2(判定线:重复快照不重复上传)");

    // 池内容逐字节复核:两资产 × 顶点/索引四段
    let slot_small = *pool.resident(id_small).expect("已驻留");
    let slot_quad = *pool.resident(id_quad).expect("已驻留");
    println!(
        "[引脚] small: vertex_base {} first_index {}; quad: vertex_base {} first_index {}(3.4 draw 的 firstIndex/vertexOffset 由账本行携带)",
        slot_small.vertex_base(), slot_small.first_index(), slot_quad.vertex_base(), slot_quad.first_index()
    );
    let (vertex_pool, index_pool) = (
        pool.vertex_buffer().expect("顶点池"),
        pool.index_buffer().expect("索引池"),
    );
    let (_readback_v, _readback_i) = verify_ranges(
        device,
        contract,
        &mut uploader,
        vertex_pool,
        index_pool,
        &[
            (false, slot_small.vertex),
            (true, slot_small.index),
            (false, slot_quad.vertex),
            (true, slot_quad.index),
        ],
        &[
            &small_expect_v,
            &small_expect_i,
            &quad_expect_v,
            &quad_expect_i,
        ],
        "组 B",
    );
    println!("[内容] 四段读回逐字节一致(host 写 → staging flush → 设备拷 → 宿主读全链)");
    // 清场:等全部票据完成再拆(销毁纪律可证明)
    uploader.wait_all_uploads().expect("等全部在飞票据");
    drop(_readback_v);
    drop(_readback_i);
    drop(uploader);
    drop(pool);
}

// ============ 组 C:维护扩容(迁移内容保持 + 停顿分账)============

fn group_c_maintenance(
    device: &Device,
    contract: &MemoryContract,
    transfer_family: u32,
    transfer_queue: vk::Queue,
) {
    println!("\n== 组 C(维护扩容观察:容量不足 → 显式维护迁移)==");
    let mut pool = MeshPool::new(device, contract);
    let mut uploader = Uploader::new(
        device,
        contract,
        transfer_family,
        transfer_queue,
        64 * 1024,
        2,
    )
    .expect("组 C 创建上传器");

    // 小资产先收:触发初建(1MiB 起步,按需量加余量)
    let id_small = probe_id(11);
    let small = convert_mesh(&demo_mesh(0, 4, false)).expect("C small 转换");
    let small_expect_v = small.vertices.clone();
    let small_expect_i = small.indices.clone();
    let t1 = upload_asset(&mut pool, &mut uploader, id_small, small);
    let ((uv, cv), _) = pool.usage();
    println!("[初建] 小资产票据 #{t1},顶点 {uv}/{cv}B(1MiB 起步——需量加余量,不预付猜测容量)");

    // 1.2MiB 顶点的大资产:逼出显式维护(等待在飞 + 迁移 + 等迁移票据 + 销毁旧池)
    let big_verts = 40_000usize; // 1.22MiB 顶点 + 240KB 索引
    let id_big = probe_id(12);
    let mut big = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::MAIN_WORLD,
    );
    big.insert_attribute(
        Mesh::ATTRIBUTE_POSITION,
        VertexAttributeValues::Float32x3(
            (0..big_verts)
                .map(|i| [i as f32, (i * 2) as f32, (i * 3) as f32])
                .collect(),
        ),
    );
    big.insert_indices(Indices::U32((0..60_000u32).collect()));
    let big_converted = convert_mesh(&big).expect("C big 转换");
    let big_expect_v = big_converted.vertices.clone();
    let big_expect_i = big_converted.indices.clone();
    let t2 = upload_asset(&mut pool, &mut uploader, id_big, big_converted);
    let ((uv2, cv2), (ui2, ci2)) = pool.usage();
    let ledger = pool.maintenance_ledger();
    println!(
        "[维护] 大资产票据 #{t2}:顶点 {uv2}/{cv2}B 索引 {ui2}/{ci2}B;扩容 {grow} 次,累计停顿 {pause}ms(等在飞 + 迁移 + 等迁移票据,与正常上传分账)",
        grow = ledger.grow_count,
        pause = ledger.pause_ms_total,
    );
    assert_eq!(ledger.grow_count, 1, "恰好一次显式扩容");
    assert_eq!(
        t2, 3,
        "迁移批耗掉 #2,大资产批拿 #3——维护与上传共用单调票据序列"
    );

    // 迁移后内容复核:小资产 + 大资产都逐字节一致(迁移没搬坏数据)
    let slot_small = *pool.resident(id_small).expect("small 驻留");
    let slot_big = *pool.resident(id_big).expect("big 驻留");
    let (vertex_pool, index_pool) = (
        pool.vertex_buffer().expect("顶点池"),
        pool.index_buffer().expect("索引池"),
    );
    let (_readback_v, _readback_i) = verify_ranges(
        device,
        contract,
        &mut uploader,
        vertex_pool,
        index_pool,
        &[
            (false, slot_small.vertex),
            (true, slot_small.index),
            (false, slot_big.vertex),
            (true, slot_big.index),
        ],
        &[
            &small_expect_v,
            &small_expect_i,
            &big_expect_v,
            &big_expect_i,
        ],
        "组 C",
    );
    println!("[复核] 迁移后新旧两资产四段逐字节一致([0, used) 整段迁移假设成立)");
    uploader.wait_all_uploads().expect("等全部在飞票据");
    drop(_readback_v);
    drop(_readback_i);
    drop(uploader);
    drop(pool);
}

// ============ 组 D:跨族 release/acquire(3.4 接线预演)============

fn group_d_cross_family(
    device: &Device,
    contract: &MemoryContract,
    gfx_family: u32,
    gfx_queue: vk::Queue,
    transfer_family: u32,
    transfer_queue: vk::Queue,
) {
    println!("\n== 组 D(跨族依赖观察:release → graphics 等票据 + acquire + 真实读)==");
    let mut pool = MeshPool::new(device, contract);
    let mut uploader = Uploader::new(
        device,
        contract,
        transfer_family,
        transfer_queue,
        64 * 1024,
        2,
    )
    .expect("组 D 创建上传器");

    let converted = convert_mesh(&demo_mesh(0, 4, false)).expect("D 转换");
    let expect_v = converted.vertices.clone();
    let expect_i = converted.indices.clone();
    // 上传批:末尾把顶点池(图形要读的那个)release 给 graphics 族。
    // EXCLUSIVE 成对语义:release 在 transfer 提交内;配对 acquire 在 graphics 提交内。
    // 本组手工排批(不走 upload_asset),为的是在提交里带上 release。
    pool.ensure_capacity(
        &mut uploader,
        converted.vertices.len() as u64,
        converted.indices.len() as u64,
    )
    .expect("D 容量");
    let vertex_pool = pool.vertex_buffer().expect("顶点池");
    let (vertex, index) = pool.alloc(
        converted.vertices.len() as u64,
        converted.indices.len() as u64,
    );
    let mut staging = converted.vertices.clone();
    staging.extend_from_slice(&converted.indices);
    let ticket = uploader
        .submit_batch(UploadBatch {
            staging,
            uploads: vec![
                StagingCopy {
                    dst: vertex_pool,
                    src_offset: 0,
                    dst_offset: vertex.offset,
                    size: vertex.size,
                },
                StagingCopy {
                    dst: pool.index_buffer().expect("索引池"),
                    src_offset: vertex.size,
                    dst_offset: index.offset,
                    size: index.size,
                },
            ],
            releases: vec![ash_renderer::vulkan::Release {
                buffer: vertex_pool,
                to_family: gfx_family,
            }],
            ..Default::default()
        })
        .expect("D transfer 提交")
        .expect("D 批必有票据");
    println!(
        "[release] 上传批 #{ticket} 末尾把顶点池所有权让渡给 graphics 族(transfer 队列提交内)"
    );

    // graphics 侧提交:等 timeline 票据(GPU 侧等待,CPU 不阻塞)+ acquire + 真实读
    let readback = GpuBuffer::create(device, contract, vertex.size, BufferRole::Readback)
        .expect("D readback 创建");
    let gfx_command_pool = unsafe {
        device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                .queue_family_index(gfx_family),
            None,
        )
    }
    .expect("D graphics 命令池");
    let gfx_command_buffer = unsafe {
        device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(gfx_command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )
    }
    .expect("D graphics 命令缓冲")[0];
    unsafe {
        device.begin_command_buffer(
            gfx_command_buffer,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )
    }
    .expect("D begin");
    // acquire:与 release 的族号成对;src 阶段按规范用 ALL_COMMANDS 等 release 完成
    // (sync.adoc:acquire 第一作用域为空,happens-before 由 ALL_COMMANDS 引入);
    // srcAccessMask 被规范声明忽略,置空。dst 是本提交里的真实访问:TRANSFER 读。
    let acquire = vk::BufferMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
        .src_queue_family_index(transfer_family)
        .dst_queue_family_index(gfx_family)
        .buffer(vertex_pool)
        .offset(0)
        .size(vk::WHOLE_SIZE);
    unsafe {
        device.cmd_pipeline_barrier(
            gfx_command_buffer,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[acquire],
            &[],
        );
        device.cmd_copy_buffer(
            gfx_command_buffer,
            vertex_pool,
            readback.buffer(),
            &[vk::BufferCopy {
                src_offset: vertex.offset,
                dst_offset: vertex.offset,
                size: vertex.size,
            }],
        );
        // 设备写 → 宿主读收口(readback 读侧 invalidate 兜底非 coherent)
        let to_host = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(readback.buffer())
            .offset(0)
            .size(vk::WHOLE_SIZE);
        device.cmd_pipeline_barrier(
            gfx_command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[to_host],
            &[],
        );
        device
            .end_command_buffer(gfx_command_buffer)
            .expect("D end");

        // 等待点语义:图形提交在 ALL_COMMANDS 起等上传票据——"等待发生在 GPU 执行
        // 依赖处,不要求 CPU 先阻塞等上传完成再提交"(施工计划 §2)
        let wait_values = [ticket];
        let mut timeline_submit =
            vk::TimelineSemaphoreSubmitInfo::default().wait_semaphore_values(&wait_values);
        let wait_sem = [uploader.ticket_semaphore()];
        let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
        let cbs = [gfx_command_buffer];
        let fence = device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .expect("D fence");
        device
            .queue_submit(
                gfx_queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&wait_sem)
                    .wait_dst_stage_mask(&wait_stages)
                    .command_buffers(&cbs)
                    .push_next(&mut timeline_submit)],
                fence,
            )
            .expect("D graphics 提交");
        device
            .wait_for_fences(&[fence], true, 5_000_000_000)
            .expect("等 graphics 消费完成");
        device.destroy_fence(fence, None);
    }
    println!("[acquire] graphics 提交:等票据 #{ticket}(GPU 侧)+ acquire + pool→readback 拷贝(图形队列上的真实读)");

    let mut back = vec![0u8; vertex.size as usize];
    let mut readback_mut = readback;
    readback_mut
        .read(vertex.offset, &mut back)
        .expect("D 宿主读 readback");
    assert_eq!(&back[..], &expect_v[..], "组 D 顶点池跨族读回不符");
    println!("[内容] 跨族读回逐字节一致——EXCLUSIVE release/acquire 成对语义全链跑通");

    // 清场:等全部票据完成再拆
    uploader.wait_all_uploads().expect("等全部在飞票据");
    drop(readback_mut);
    unsafe { device.destroy_command_pool(gfx_command_pool, None) };
    drop(uploader);
    drop(pool);
    let _ = expect_i; // 索引池未参与跨族读(未 release),留证据变量名防误删
}

/// graphics + 专用 transfer(如有)双族设备:返回 (device, gfx_family, gfx_queue,
/// transfer_family, transfer_queue)。显式启用 timelineSemaphore(票据真实使用,
/// 支持与启用分开)。
fn create_device(
    instance: &ash::Instance,
    pd: vk::PhysicalDevice,
) -> (Device, u32, vk::Queue, u32, vk::Queue) {
    let families = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    let gfx = families
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("无 graphics 族") as u32;
    let transfer = families
        .iter()
        .position(|f| {
            f.queue_flags.contains(vk::QueueFlags::TRANSFER)
                && !f.queue_flags.contains(vk::QueueFlags::GRAPHICS)
        })
        .map(|i| i as u32)
        .unwrap_or(gfx);
    let priority = [1.0f32];
    let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
        .queue_family_index(gfx)
        .queue_priorities(&priority)];
    if transfer != gfx {
        queue_infos.push(
            vk::DeviceQueueCreateInfo::default()
                .queue_family_index(transfer)
                .queue_priorities(&priority),
        );
    }
    let mut vulkan12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
    let device = unsafe {
        instance.create_device(
            pd,
            &vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .push_next(&mut vulkan12),
            None,
        )
    }
    .expect("vkCreateDevice");
    let gfx_queue = unsafe { device.get_device_queue(gfx, 0) };
    let transfer_queue = if transfer == gfx {
        gfx_queue
    } else {
        unsafe { device.get_device_queue(transfer, 0) }
    };
    (device, gfx, gfx_queue, transfer, transfer_queue)
}

/// 验证层回调:消息原文入 VU_LOG(返回 FALSE = 不被截获)。
///
/// # Safety
/// 本函数不被本项目调用——由 Vulkan 实现按回调契约调用,`p_callback_data`
/// 依约定为合法指针或空;`_user_data` 未使用不触碰。
unsafe extern "system" fn probe_callback(
    severity: vk::DebugUtilsMessageSeverityFlagsEXT,
    _types: vk::DebugUtilsMessageTypeFlagsEXT,
    p_callback_data: *const vk::DebugUtilsMessengerCallbackDataEXT,
    _user_data: *mut std::ffi::c_void,
) -> vk::Bool32 {
    let msg = if p_callback_data.is_null() {
        "(no data)".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr((*p_callback_data).p_message) }
            .to_string_lossy()
            .into_owned()
    };
    VU_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(format!("[{:?}] {msg}", severity));
    vk::FALSE
}
