# 纹理格式与Gamma：从png字节到采样值的色彩编码链

> 2026-09-28。3.3.1 贴图链路落地后的知识沉淀，回答五个问题：纹理当前存的是 png 吗？用什么工具解码？解码后以什么格式存成 image？不同格式差在哪？Linear 与 gamma 怎么区分？[显存机制篇](显存机制：Buffer与Image之别、swizzle不透明与访问路径特化.md)管"字节怎么排"（tiling/swizzle），本篇管"字节怎么读"（格式与色彩编码）——两篇合起来才是 texel 的完整解释链。

## 0. 判定线

**解码从不改变颜色值：png 里存的整数就是像素值，解码只是拆包（解 deflate、还原行滤波，顶多补一个 alpha 通道）；"这串字节是什么意思"由格式在采样时决定——UNORM 按线性 u/255 读，SRGB 读完再由硬件过一遍 sRGB 解码曲线。同一份 png 字节，挂两种格式，采出两种颜色。**

三条随之定案的事实（3.3 已按此实现）：

1. **格式决策点在 glTF loader，文件不参与**：bevy 按纹理语义（用途）定 sRGB/线性，落进 `Image.texture_descriptor.format`；png 自带的 gAMA/iCCP 色彩块不被应用。渲染器只剩"映射 + 校验"。
2. **sRGB↔线性转换是 TMU 固定功能，零着色器成本**（显存机制篇 §4.2 访问路径特化的格式侧实例）。
3. **同布局才能零重排上传**：R8G8B8A8 的 texel 是 4 字节按 R,G,B,A 地址序排列，与解码输出逐字节相同——copy 引擎只做 tiling 重排，不换通道、不变数值。

## 1. 资产形态：15 张 png，8-bit，14 张 512²

宿主实跑的 15 张贴图全部是 PNG 文件（`assets/models/FlightHelmet/`），5 材质 × (BaseColor + Normal + OcclusionRoughMetal)。`file` 报告：14 张 512×512、1 张 32×32（LensesMat_Normal）；色彩类型 14 张 8-bit RGB、1 张 8-bit RGBA（LensesMat_BaseColor——玻璃材质要 alpha）。

png 容器的两层包装都要拆：

- **行滤波 + deflate 压缩**（IDAT）：解码 = 逐行还原滤波器再解压，只还原整数，颜色值不动。
- **元数据块**（IHDR 位深/色彩类型、gAMA、iCCP…）：bevy 只读前者当形状；后者（色彩管理）一律忽略。

文件在磁盘上多大 ≠ 在显存里多大：磁盘大小是压缩后字节数，显存里是 w×h×4（RGBA8）的未压缩体量——15 张共 14,684,160 B（14.7MB），与 3.3.1 宿主日志逐字节对上。

## 2. 解码：`image` crate 在 CPU 上产出 RGBA8

解码器是 Rust **`image` crate 0.25.2**（bevy_image 的依赖，格式按 feature 启用；我们 `CompressedImageFormats::NONE` 只关 basis/ktx2 压缩容器，png/jpeg 照常）。驱动方是 bevy 的 `ImageLoader`——宿主 host.rs 补位注册的那个（TexturePlugin::finish 被禁）。解码在资产流水线的异步任务里做，不占 GPU、不占帧时间。

调用链（crates/bevy_image）：

```
ImageLoader → reader.decode() → DynamicImage::ImageRgb8         （image crate：拆包装）
           → into_rgba8()                                       （3 通道补 alpha=255）
           → Image::from_dynamic（image_texture_conversion.rs:36-47）
           → Image { data: Vec<u8>, texture_descriptor.format } （format 由 is_srgb 定，见 §3）
```

要点：

- **解码产物 = 源像素值 + 补 alpha**：RGB8 → RGBA8（A=255）；源若是 16-bit png 则产 R16Unorm 等整型，Luma8 则 R8Unorm——`from_dynamic` 按源色彩类型逐分支映射。FlightHelmet 全部落 RGBA8。
- CPU 侧 `Image.data` 是**紧凑行主序**的线性字节数组（w×4 字节/行），正是 3.2/3.3.1 staging 段直接搬进 VkBuffer 的东西。

## 3. 格式决策点：文件不决定格式，用途决定

bevy_gltf 的 `get_linear_textures`（crates/bevy_gltf/src/loader/gltf_ext/mod.rs:46）把 **normal / occlusion / metallicRoughness**（及各扩展数据贴图）收进线性名单；`is_srgb = !linear_textures.contains(idx)`（loader/mod.rs:1210）——**不在名单即 sRGB**（baseColor、emissive）。

于是同一张 png 的两种命运：

| 用途 | 语义 | bevy 格式 | 我们的 VkFormat |
|---|---|---|---|
| baseColor、emissive | 颜色（给光照） | Rgba8UnormSrgb | R8G8B8A8_SRGB |
| normal、occlusionRoughMetal | 数据（给数学） | Rgba8Unorm | R8G8B8A8_UNORM |

FlightHelmet 实测：sRGB 角色 5（全部 BaseColor）/ 线性 10（Normal + OcclusionRoughMetal）。为什么 baseColor 编码、法线不编码，见 §5。

## 4. VkFormat 解剖：名字即"布局 + 解释"契约

格式名读作：分量 × 位宽 × 数值解释。以 R8G8B8A8_UNORM 为例：

- **布局**：每 texel 4 字节，R 在地址 0、G 1、B 2、A 3——与 §2 解码输出逐字节相同，所以上传零重排。
- **解释**：8bit 无符号整数 → 浮点 u/255（UNORM = unsigned normalized，[0,1]）。

数值解释家族（Vulkan 数百个格式的共同语法）：

| 后缀 | 字节含义 | 着色器读到的 |
|---|---|---|
| UNORM / SNORM | 无/有符号整数 | 线性归一到 [0,1] / [-1,1] |
| SRGB | sRGB 编码字节 | 硬件解码后的**线性**值（仅 RGB，A 恒线性） |
| UINT / SINT | 整数 | 不归一的原始整数 |
| SFLOAT / UFLOAT | 浮点 | 原样浮点 |

三组常被混淆的"不同"：

1. **SRGB vs UNORM：同布局不同解释**。R8G8B8A8_SRGB 与 R8G8B8A8_UNORM 在显存里字节完全一样（事后无法区分），差异只在 TMU 采样路径上多不过一条固定功能曲线——"存储与解释分离"的格式版（显存机制篇判定线）。也因此格式必须提前定好（§3）：**解释参数挂在 image 上、由 image view 携带，采样器与此无关**——`VkSampler` 根本没有格式参数，只管滤波/寻址/LOD；同一个采样器既可采 SRGB 也可采 UNORM 贴图。这正是采样器能跨贴图去重复用的原因：3.3.4 的 sampler 表只按滤波参数分槽，不按色彩角色分槽。数值方向：编码 0.5 解码后 ≈0.214（线性值变小）——但这是"感知编码 → 物理强度"的翻译而非把图调暗，输出端还要编码回去（§6）。
2. **位宽 = 精度档位**：8bit 整数（256 档）→ 16bit（65536 档）→ float（指数 + 尾数，HDR）。精度需求决定选型，与编码无关。
3. **分量序 = 通道怎么排**：R8G8B8A8 与 B8G8R8A8 字节序相反（BGRA 来自显示设备惯例）。我们的 swapchain 是 B8G8R8A8_UNORM——手写清屏色就要按 BGRA 序给。

压缩格式（BC/ASTC/ETC2）是另一维：texel 按块压缩，TMU 就地解压后走同一条采样路径（显存机制篇 §4.2 的 BCn 通路）；`CompressedImageFormats::NONE` 表示我们不收此类容器。无压缩格式建图前还要过 format features 检查（usage 位 → FormatFeatureFlags，3.3.1 `GpuImage::create` 必查 optimal_tiling_features——注意是 SAMPLED_IMAGE 不叫 SAMPLED）。

## 5. Linear 与 gamma：为什么有两种"颜色格式"

机制先说清三件事：

1. **显示端吃 sRGB 信号**。CRT 荧光粉响应曲线的遗留 + 人眼对暗部差异更敏感，使"亮度感知"近似幂律；sRGB 就是给显示信号定的分段编码曲线（短线性段 + gamma 2.4 段，整体 ≈2.2）。美术在编码空间作画，png 里存的就是编码值——这就是 §1 那些整数的身份。
2. **光照数学只在物理（线性）空间成立**。乘法、加法、平方衰减都是线性运算；拿编码值直接算等于先吃了一遍未知 gamma 的扭曲。
3. **8bit 预算下，编码是省档位的手段**。sRGB 编码把 256 档在暗部摊得更密；线性 8bit 的暗部会出明显色带。

所以 GPU 的固定分工：**存储端 sRGB 编码（省 bit），采样端硬件解码进线性（TMU 固定功能，零成本），计算全程线性，输出端再编码回 sRGB**。SRGB 格式就是"存储端身份"的声明——采样时自动解码，着色器一行不用写。

两条纪律由此而来：

- **数据贴图绝不编码**：法线 (0.5,0.5,1.0)、遮蔽、粗糙度是数学输入不是颜色，过一遍解码曲线 = 法线歪、粗糙度错——§3 线性名单存在的全部理由。
- **不许双重转换**：SRGB 格式 + 着色器手动 pow = 解码两次，结果一致地错。

Unity 类比：`TextureFormat.RGBA32`（存储布局）与 `GraphicsFormat.R8G8B8A8_SRGB`（解释方式）在 2020+ 拆成两套枚举；颜色贴图勾 sRGB、法线不勾，就是这个机制在编辑器里的开关形态——引擎侧同样把编码下放给硬件采样。

## 6. 链路两端：入口已实证，出口是 3.4 的待决点

- **入口（已实证）**：15 张 png → image crate 解码补 alpha → Rgba8Unorm(Srgb) → 逐字节进 staging → R8G8B8A8_SRGB/UNORM 的 VkImage（OPTIMAL tiling，重排归 copy 引擎）。整链颜色值未变，唯一的色彩语义变换将在未来采样时由 TMU 做。
- **出口（待决）**：我们的 swapchain 选 B8G8R8A8_UNORM（[swapchain.rs:67](../../../../ash_renderer/src/vulkan/swapchain.rs)），线性值直出上屏。官方 bevy_render 优先挑 SRGB surface，非 SRGB 也会给 swapchain 套 `add_srgb_suffix()` 的 sRGB view 渲染（crates/bevy_render/src/view/window/mod.rs:399-415）——线性值在 ROP 写出时自动编码。清屏时代两者无感；**3.4 画出受光几何后，缺输出编码会在屏幕上显形（中间调偏暗）**。届时二选一：swapchain 换 SRGB 格式（对齐官方），或着色器端手动编码。动不动由用户拍板，此处只立案。
