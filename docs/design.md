# lmflow 设计方案

> 状态:**成品**。Rust 引擎、C ABI、C++ 糖层(含 OpenCV 互转)、18 个内置算子、
> Python 绑定(pybind11)、原生 SDK 发布(各平台头文件+库)全部就位;**255 个测试**
> (Rust 217 + Python 38)全绿,TSan 硬门禁 0 竞态。Rust / C++ / Python 三种宿主的
> hello_world 都输出正确;支持线程池绑核 + 实时优先级(Linux/Android),可交叉编到
> Android / iOS / 鸿蒙。
> 定位:一个数据流图计算框架 —— 把计算描述成**有向图**,节点是**算子(Kernel)**,
> 边上流动**带时间戳的数据包(Packet)**。
> 组成:**Rust 引擎 + C ABI 门面 + C++/Python 算子**。
> 核心概念:`Graph`(图)、`Node`(节点)、`Kernel`(算子)、`Edge/Port`(边/端口)、
> `Packet`(数据包)、`Contract`(端口类型契约)、`Poller/Observer`(输出的拉/推)。

---

## 0. 目标与范围

### 0.1 做什么

- 引擎(调度、线程、边队列、拓扑、YAML 解析)用 **Rust** 实现。
- 对外**只暴露一层 C ABI**(`include/lmflow/flow.h`)。这是唯一的稳定接口。
- 算子可用 **C++**(经 `flow.hpp` 糖层)或 **Python**(经 pybind11)编写,YAML 中平等引用。
- 图的拓扑与参数由 **YAML** 描述,含每节点的 `options`。

### 0.2 阶段划分

| 阶段 | 内容 |
|---|---|
| **B(已完成)** | 每条边一个 FIFO;节点就绪 = 每个输入口至少有一个包。多输入按时间戳对齐、bound 传播、`max_in_flight > 1` 并行 context 池均已落地。 |
| **A(已完成)** | `process_timestamps`:落地为 `batch` 输入策略(攒够一批一次交给算子,见 §7.10)。B/A 两阶段的引擎能力至此全部落地。 |

**批处理(`batch` 输入策略)已支持**(见 §7.10):攒够 `capacity` 个包一次交给算子,`process()` 用 `input_count` / `input_at` 读整批,关流刷余批。用于批推理 / 窗口聚合。v1 单输入口。内置 `BatchSumKernel` 为样板。

**零输入口 source 节点(生成型算子)已支持**(见 §7.4):内核自产数据、`source_done()` 自报产完;源须挂线程池执行器(否则会独占宿主主线程)。内置 `RangeSourceKernel` 为样板。

**子图(subgraph)+ 跨文件 `include` 已支持**(见 §7.11):纯**建图期**变换,把带 `subgraphs` / `node.type` 的配置展平成等价扁平图,运行时引擎 / 调度器不感知子图。

**反馈环(back-edge)已支持**(见 §7.12):把输入口标为 `back_edges` 即「最新值反馈寄存器」—— 容量 1、不参与就绪 / 终止 / 对齐;去掉 back-edge 后的图仍是 DAG。未被 back-edge 打断的拓扑环仍在 `init` 报错。

### 0.3 非目标

- 不做分布式 / 跨进程;单进程内多线程。
- 不做 GPU 内存空间(`LMFlowBuffer.device` 已预留字段,但本版本只有 CPU)。
- 不支持图跑完后重跑(`Terminated` 之后只能 `free`)。

---

## 1. 决策记录(ADR)

已锁定的决策及其**理由** —— 避免重复讨论。

| # | 决策 | 理由 |
|---|---|---|
| 1 | **只暴露 C ABI**,不提供 C++ 类接口 | 模板/`std::any` 无法过 FFI;C ABI 稳定、可被任意语言绑定 |
| 2 | **算子用 C++/Python,引擎用 Rust** | 保留既有 C++ 算子资产;引擎侧拿到 Rust 的并发安全收益 |
| 3 | **走阶段 B 先行** | 时间戳同步是最难的一块,先打通垂直切片再补 |
| 4 | **cargo 主导构建**;Python 部分破例用 CMake | 单一工具链最省事;但 pybind11 的 wheel 必须有 C++ 构建步骤 |
| 5 | **Python 绑定用 pybind11**(非 PyO3) | 场景要 OpenCV:`cv::Mat` ↔ numpy 零拷贝是 pybind11 最成熟的路径 |
| 6 | **引擎不解释 payload**,只搬引用 | 任意数据类型都能流动;张量、图像只是「又一个类型」,不该做成引擎的一等概念 |
| 7 | **内建类型只为跨语言存在**,对引擎无特权 | 跨语言时对方无法解读不透明 C++ 对象,故需一套双方都认识的内存约定 |
| 8 | **`LMFlowBuffer` 一个 N 维描述符统管**图像/张量/音频 | 避免 IMAGE/TENSOR/AUDIO 各设一种类型;语义对齐 numpy buffer protocol |
| 9 | **Python 算子只收发内建类型** | 保持 payload 语言中立。否则 Python 算子的输出接到 C++ 算子上会得到无法解读的指针,且只在运行时暴露;另有 GIL-in-drop 死锁风险 |
| 10 | **省拷贝用 CoW**(`take_input` + `make_mutable`) | 线性管线全程零拷贝;被扇出共享时才复制,从而不污染其它分支 |
| 11 | **只有图输入口限流,内部边不对生产者背压** | 内部边设硬上界会让「扇出后汇合」的合法 DAG 死锁(详见 §7.5) |
| 12 | **手写 `flow.h` 为权威 + 布局一致性测试**(不用 cbindgen) | header 是给用户看的文档,可读性优先;用 `static_assert` + Rust 测试钉死布局,拿到等价安全性 |
| 13 | **`flow.hpp` 糖层保留**,但不属于 ABI | 让 C++ 算子写法自然;模板便利全部在用户 TU 内 monomorphize,不过界 |
| 14 | **OpenCV 不进 core**,隔离到可选头 `flow_cv.hpp` | 引擎与 `flow.h`/`flow.hpp` 零图像库依赖,没装 OpenCV 也能编译全部 core |
| 15 | **`LMFlowBuffer` 预留 `flags`/`device`/`reserved`** | 一次性预留,未来加字段(最可能是 GPU 内存空间)不破 ABI |
| 16 | **节点默认跑在宿主主线程**,并发是显式 opt-in | 默认零并发、执行顺序确定、断点调试直观;副产品:Python 算子默认无 GIL 争抢 |
| 17 | **端口扁平序号 = YAML 声明顺序** | 常见做法是按 tag 字典序分组,混用「有标签/无标签」端口时 `Index(0)` 拿到的不是写的第一个 —— 真实陷阱,故分道 |
| 18 | **引入 side packet(常量输入)** | `options` 只能给标量/JSON,无法交付「一个已初始化好的模型」这类对象 |
| 19 | **输入策略做成节点级可插拔**(`sync`/`immediate`/`fixed_size`/`sync_set`) | 实时丢帧与(A 阶段的)时间戳对齐共用同一扩展点;`fixed_size` 同时是「内部边无界」的配套内存约束 |
| 20 | **全局水位兜底**,超限时转化为图输入口背压 | 内部边无界的直接后果:100 帧 × 6 条边 × 6MB ≈ 3.6GB。只在图输入口刹车不会重新引入 diamond 死锁 |
| 21 | **节点级统计 + watchdog**,但**不做抢占中断** | 卡死必须能定位到具体节点;而中断一个正在跑的算子无法安全实现(同 `cancel` 语义),故只做可观测 |
| 31 | **节点统计全用原子、不用 `Mutex`** | 每包每节点都要更新,放锁里就是在热路径加锁(改造前每包 4 次加锁:计时进/出 + 耗时 + processed)。改原子后实测端到端 **-4~5%**,且顺带满足 R1「调算子时不持任何引擎锁」。计数器用 `Relaxed`(不参与 happens-before);`started_us` 归零时**不清**,读侧按 `in_flight > 0` 判断,从而避开「清零 vs 新一次开始」的覆盖竞争 |
| 32 | **内存序分两类**:纯计数器 `Relaxed`,同步标志一律 `SeqCst` | 已逐个审过 39 处。降为 `Relaxed` 的只有 8 处 —— 全局水位(`total_queued`/`_bytes`,**每包每消费者 4 次 RMW**)与 `dropped`,它们除了跟阈值比一下不承载任何 happens-before。其余 31 处**有意保留**:终止判定的两段式检查(`input_closed[i] && queue_len(i)==0`)、`has_error`/`cancelled`/`source_done` 的发布、执行器 stop 标志、边与 poller 的 `closed` —— 降级会坏掉终止正确性。收益是 **arm64 专属**:实测 codegen,aarch64 上 SeqCst RMW 走 `__aarch64_ldadd8_acq_rel`、Relaxed 走 `__aarch64_ldadd8_relax`,而 x86_64 两者都是同一条 `lock incq`(故本机 bench 量不出),Android / iOS arm64 是发布目标 |
| 22 | **`type_id` = FNV-1a(修饰名)**,而非 `typeid().hash_code()` | 后者实现定义、不保证跨动态库一致;而本项目 C++ 算子在 core、Python 绑定在另一 `.so`,天然跨产物。事后再改需全量重编,故一开始就用稳定方案 + `LMFLOW_DECLARE_TYPE_NAME` 逃生口 |
| 23 | **时间戳单调性:图输入口强制校验,内部边仅 debug 构建校验** | 外部数据进入的唯一门校验一次即可挡住绝大多数乱序;内部边逐包校验是热路径开销,且算子产出乱序属算子 bug,用 `debug_assertions` 捕获即可 |
| 24 | **不做 stream header**,用 side packet 覆盖 | header 会引入「流上的第二种数据」及其生命周期问题;side packet 已能表达「整条流不变的属性」。少一个概念优于多一个 |
| 25 | **不做程序化构图 API**,动态图由宿主生成 YAML | 保持单一真相源;builder API 是一大片表面积,应由真实需求驱动而非预先设计 |
| 26 | **不启用 `LMFLOW_TYPE_HOST_OBJECT`** | 见 ADR #9;若将来启用,须配套「原生对象端口不得接异语言算子」的拓扑校验 |
| 27 | **子图(subgraph)= 纯建图期展开,不进 ABI** | node 的 `type:` 填子图名;`subgraphs:` 段内联定义、`include:` 引外部子图库。建图期展平成扁平图(命名空间 `parent/inner` + 边界按位置重映射),引擎不感知子图。见 §7.11 |
| 28 | **`max_in_flight > 1` 用 context 池 + 按序重排**,而非单纯放开并发 | 并行处理多个时间戳时,完成顺序 ≠ 时间戳顺序;必须按序号重排刷新,否则下游时间戳非单调。序号在认领时按「取最小就绪时间戳」的顺序分配,故序号序 = 时间戳序 |
| 29 | **`max_in_flight > 1` 强制要求配 executor** | 默认执行器是宿主主线程,单线程下并行度恒为 1;配了才有意义,没配报错而非静默 |
| 30 | **认领时即弹入输入(pop-at-claim)**,而非运行时 | 并行认领必须原子地「定时间戳 + 弹包」,否则两个并发认领会取到同一时间戳的包。副作用:空闲节点会立刻认领第一个包,故它不受 `fixed_size` 丢弃约束(在飞的包不算积压) |

---

## 2. 分层架构

```
┌──────────────────────────────────────────────────────────────────┐
│  宿主(驱动图):Rust / C++ / Python                                │
│    new → init_from_yaml → add_poller → start                      │
│    loop { input.send(pkt); poller.next() } → close → wait_done    │
├─────────────────────── C ABI (include/lmflow/flow.h) ────────────────────┤ ← unsafe 边界①
│  Rust 引擎 (lmflow crate, lib + staticlib + cdylib)               │
│    graph(索引 arena) · scheduler/executor · edge(FIFO)            │
│    packet · timestamp · config(YAML) · registry · poller          │
├─────────────────────── C ABI (回调) ──────────────────────────────┤ ← unsafe 边界②
│  算子:C++(flow.hpp 糖层)   |   Python(pybind11 蹦床)            │
│        process 内调 lmflow_ctx_* 读输入 / 发输出                     │
└──────────────────────────────────────────────────────────────────┘
```

- 边界①:宿主 → 引擎(Rust 提供 `extern "C"` 函数)。
- 边界②:引擎 → 算子(算子侧提供 vtable),算子回读 → 引擎(`lmflow_ctx_*`)。
- **两个方向都在 C ABI 上,没有 C++ 模板过界。**

---

## 3. 数据模型

### 3.1 Packet 与所有权

```rust
pub enum Payload {
    Native(Box<dyn Any + Send + Sync>),  // Rust 侧构造
    Builtin(BuiltinPayload),              // 引擎分配:BYTES / BUFFER / 标量 / STR
    Foreign(ForeignPayload),              // 外部构造:裸指针 + drop_fn
}

pub struct ForeignPayload {
    ptr: *mut c_void,
    drop_fn: Option<unsafe extern "C" fn(*mut c_void)>,
    type_id: u64,
}
impl Drop for ForeignPayload {
    fn drop(&mut self) { /* 调一次 drop_fn */ }
}
// 断言:同一 payload 任一时刻只被单线程访问;drop 可能发生在别的线程。
// 前提由「节点独占令牌」保证(§7.0 规则 R3)。这是本设计唯一的 Send/Sync unsafe 断言。
unsafe impl Send for ForeignPayload {}
unsafe impl Sync for ForeignPayload {}

#[derive(Clone)]
pub struct Packet {
    data: Option<Arc<Payload>>,   // None = 空包(仅时间戳,用于 bound / 关流)
    ts:   Timestamp,
}
```

跨界表示 `LMFlowPacket{payload, type_id, timestamp, owner, drop_fn}`(40 字节),
**三种所有权语义由 `owner` 区分**:

| 场景 | `owner` | 语义 |
|---|---|---|
| 宿主/算子新建 | NULL | 提交(`send`/`emit`)后引擎接管;不提交须 `lmflow_packet_drop` |
| 引擎借出(`lmflow_ctx_input`、observer 回调) | 非空 | **借用**,不得 drop、不得跨回调留存 |
| 引擎移交(`poller_next`、内建构造、`clone`) | 非空 | **调用方持有一份引用**,须 `emit`/`send` 或 `lmflow_packet_drop` |

多路分发(`Forward`/扇出)= `Arc::clone`,**不拷贝数据**。

### 3.2 数据类型模型:两条路

**引擎对 payload 完全不作解释**,只搬引用、只按 `type_id` 做相等性校验。因此任意类型都能流动。

1. **任意自定义类型**(推荐给纯 C++ / 纯 Rust 管线)
   调用方自备指针与 `drop_fn`,`type_id` 自取(C++ 糖层默认 `typeid` 哈希)。
   `cv::Mat`、自定义结构体、模型张量对象一视同仁,引擎零参与。
2. **内建类型**(为**跨语言**而设)
   `BYTES` / `I64` / `F64` / `BOOL` / `STR` / `BUFFER`。由引擎分配、复制、释放,
   免去在别的语言里构造 `drop_fn`;`type_id` 为约定常量,跨语言稳定。
   **对引擎没有特权**,只是一套双方都认识的内存约定。

> `type_id` 的现实限制:`typeid(T).hash_code()` 跨编译器/跨动态库/`-fno-rtti` 不保证一致。
> 算子与宿主分属不同编译产物时应改用稳定方案(对类型名做 FNV-1a)。糖层已把生成逻辑
> 收敛到单一函数 `lmflow::TypeId<T>()`,改一处即可。

### 3.3 LMFlowBuffer:一个描述符统管所有大块数值数据

```c
typedef struct {
  void*   data;                    /* 首字节 */
  int64_t shape[8];                /* 各维元素数 */
  int64_t strides[8];              /* 各维**字节**步长 */
  int32_t ndim, dtype;
  uint32_t flags; int32_t device;  /* READONLY 标记 / 内存空间 */
  int64_t reserved[2];             /* ABI 预留,置零 */
} LMFlowBuffer;                      /* 168 字节,布局由 static_assert 钉死 */
```

| 用途 | 表示 |
|---|---|
| 灰度图 | `ndim=2, [H,W]` |
| 彩色图 | `ndim=3, [H,W,C]`(`cv::Mat` / numpy HWC) |
| 推理张量 | `ndim=4, [N,C,H,W]` |
| 音频 | `ndim=2, [帧数, 声道]` |

语义与 numpy buffer protocol 一致(strides 以字节计,不要求连续),所以
numpy 与 `cv::Mat` 都能**零拷贝**包住同一块内存。

### 3.4 引用与写时复制(省拷贝)

payload 默认**不可变共享**。需要就地改写时用 CoW —— 语义等同 `Arc::make_mut`。

**关键点(极易写错)**:算子拿到的输入包是**借用**,引擎的 `Context` 自己还持一份引用。
若直接 `clone` + `make_mutable`,引用数 ≥ 2,**CoW 必然复制**,省拷贝的意图落空。
必须先把包从输入槽**取走**:

```cpp
lmflow::Packet p = cc.TakeInput(0);          // ← 关键:移出输入槽
LMFlowBuffer buf{};
if (LMFlowStatus st = p.MakeMutableBuffer(&buf)) return st;   // 独占 → 零拷贝
// ...原地写 buf.data...
cc.Emit(0, std::move(p));
```

- 线性管线(本节点是唯一消费者)→ 全程零拷贝。
- 上游是 `Split` 扇出、数据被别的分支共享 → 才复制一份,**因此不会污染对方**。
  这同时消除了「共享 payload 被就地改写」的数据竞争。

**CoW 生效的不变量(必须写进代码注释)**:
> **引擎在投递数据包之后,不得再保留任何额外引用。**
> 否则引用数恒 ≥ 2,CoW 永远退化成全量拷贝 —— 而且不报错,只是变慢。
> (例如「为推进时间戳边界而缓存最后一个包」这类实现会静默破坏它。)

限制:CoW 只支持**引擎持有的内建 payload**(`BUFFER`/`BYTES`/标量)。自定义 payload
引擎只有 `drop_fn`、无从复制,返回 `LMFLOW_ERR_INVALID_ARG`(该情形自行拷贝)。
要通用化需给 `LMFlowPacket` 增加 `clone_fn` 字段 —— 会改 ABI 布局,本版本不做。

### 3.5 Timestamp

```
UNSET < UNSTARTED < PRE_STREAM < MIN … MAX < POST_STREAM < ONE_OVER_POST_STREAM < DONE
                                 └ 普通数据区间 ┘
```
`PRE_STREAM`/`POST_STREAM` 是流首/流尾的单包位置;`DONE` 表示端口已关且不再有数据;
`UNSET` 为默认值。算术用**饱和运算**,哨兵附近不会溢出回绕。
已实现于 `core/src/timestamp.rs`(含 9 个单测)。

> 提交时 `timestamp == UNSET` 的包,引擎自动继承当前 `input_timestamp`(与 `Forward` 一致);
> 在图输入口上提交 `UNSET` 视为非法。

### 3.6 Side packet:常量输入

整个 run 期间**不变的任意对象**,由宿主在启动前注入,算子按名字读取。

| | `options` | side packet |
|---|---|---|
| 来源 | YAML | 宿主运行前注入 |
| 内容 | 标量 / JSON | **任意 payload**(含自定义类型) |
| 典型 | 阈值、尺寸、开关 | 已加载的模型句柄、标定矩阵、查找表、词表、外部资源上下文 |

**没有 side packet 就无法把「一个已经初始化好的模型」交给算子** —— `options` 做不到,
这是引入它的直接原因。

```c
LMFlowStatus lmflow_graph_set_side_packet(LMFlowGraph*, const char* name, LMFlowPacket pkt);  /* start 之前 */
LMFlowPacket lmflow_ctx_side_packet(const LMFlowContext*, const char* name);                /* 借用,勿 drop */
bool       lmflow_ctx_has_side_packet(const LMFlowContext*, const char* name);
```

- 必须在 `start` 之前设置,之后返回 `LMFLOW_ERR_STATE`;传入即移交所有权,引擎持有到 graph 释放。
- 算子侧读到的是**借用**,不得 `drop`。

---

## 4. 对外 C 接口

> **权威定义见 `include/lmflow/flow.h`**(手写,已通过 `gcc -std=c11 -Wall -Wextra` 与
> `g++ -std=c++17` 双向验证)。本节只列分组,避免文档与 header 双份漂移。

| 分组 | 主要函数 |
|---|---|
| ABI / 诊断 | `lmflow_abi_version` `lmflow_last_error` `lmflow_set_log_callback` |
| Packet 通用 | `lmflow_packet_drop` `lmflow_packet_clone` |
| 内建类型 | `lmflow_packet_from_bytes/i64/f64/bool/str` + 对应 `as_*` |
| 缓冲 | `lmflow_packet_new_buffer` `lmflow_packet_from_buffer` `lmflow_packet_as_buffer` `lmflow_dtype_size` |
| 写时复制 | `lmflow_packet_make_mutable_buffer` `lmflow_packet_make_mutable_bytes` |
| 算子注册 | `lmflow_register_kernel` + `LMFlowKernelVTable`(48 字节) |
| Contract | `lmflow_contract_num_inputs/outputs` `…_input_id(tag)` `…_set_any` `…_set_type` |
| Context 数据 | `lmflow_ctx_input` `…_input_payload` `…_take_input` `…_emit` `…_forward` `…_set_next_ts_bound` |
| Context 端口 | `lmflow_ctx_input_id(tag,idx)` `…_input_index(name)` `…_input_name` `…_num_inputs/outputs` |
| Context 参数 | `lmflow_ctx_option_i64/f64/bool/str` + `lmflow_ctx_options_json` |
| 图 | `lmflow_graph_new` `…_init_from_yaml` `…_start` `…_free` |
| 输入 | `lmflow_graph_input` → `lmflow_input_send/try_send/close`;便捷 `lmflow_graph_add_packet` |
| 输出 | `lmflow_graph_add_poller(_ex)` → `lmflow_poller_next/try_next/next_timeout`;`lmflow_graph_observe(_ex)` |
| 终止 | `lmflow_graph_cancel` `…_close_input` `…_close_all_inputs` `…_wait_done` `…_wait_done_timeout` |
| Side packet | `lmflow_graph_set_side_packet` `lmflow_ctx_side_packet` `lmflow_ctx_has_side_packet` |
| 类型名(诊断) | `lmflow_register_type_name` `lmflow_type_name` |
| 空闲 / 暂停 | `lmflow_graph_wait_until_idle` `…_timeout` `lmflow_graph_pause` `lmflow_graph_resume` |
| 算子自我信息 | `lmflow_ctx_node_name` `…_kernel_name` `lmflow_ctx_log` `lmflow_ctx_set_error` `lmflow_ctx_close_reason` `lmflow_ctx_counter_add` |
| 参数(增强) | 点号路径嵌套、`lmflow_ctx_require_option_*`(必需参数)、`lmflow_ctx_option_*_array`(数组) |
| 全局水位 | `lmflow_graph_total_queued` `…_total_queued_bytes`;YAML `max_queued_packets/bytes` |
| 统计 | `lmflow_graph_node_stats`(`LMFlowNodeStats`,**全原子无锁**采集)、`lmflow_graph_counter_value`;YAML `watchdog_ms` |
| 状态 / 拓扑 | `lmflow_graph_state` `…_num_input_ports/output_ports/num_nodes` `lmflow_registered_kernel_*` |
| 内省 | `lmflow_graph_dump` `lmflow_graph_to_dot`(Graphviz DOT) `lmflow_graph_queue_depth` `lmflow_graph_dropped_count` `lmflow_graph_last_error` `lmflow_packet_debug_string` |

**接口设计要点**

- `lmflow_ctx_forward` 专为直通存在:算子不碰引用计数,引擎内部共享同一 `Arc`。
- 输入口**句柄化**(`LMFlowInput*`),热路径免去按名字 hash+strcmp。
- 阻塞接口一律有非阻塞/超时兄弟:`try_send` / `try_next` / `next_timeout` / `wait_done_timeout`。
  生产代码建议一律用带超时版本 —— 算子逻辑有误会让图静止而非结束,无超时等待就是永久挂起。
- `lmflow_last_error()` 是**线程局部**的:算子在工作线程失败时,其文本不会出现在宿主线程。
  要拿那条信息用 `lmflow_graph_last_error(graph)`。
- `lmflow_graph_dump` 返回**线程局部**缓冲,多线程同时调用不会互相踩踏。
- `lmflow_graph_to_dot(g, with_stats)` 导出 Graphviz DOT(`dot -Tsvg` 可渲染):子图命名空间还原成嵌套 cluster,节点填色 = 所在执行器,图例列各线程池的线程数 / 绑定核(亲和力)/ 实时优先级。返回值同 dump(线程局部,不得 free)。
  `with_stats = true` 时节点标签额外标出运行统计(处理数 · 平均延迟 · 收/发包数 · 队列峰值 · 错误数),填色改为**按平均延迟的热力图**(绿=快 → 红=慢)—— 一眼定位瓶颈节点;此时执行器仅以标签里的 `@name` 标出。可在运行期间随时调用(读原子快照)。
- 日志回调:引擎保证调用时**不持有任何内部锁**,故回调里可安全抢 GIL / 加锁。

---

## 5. 算子

### 5.1 C++ 算子(`flow.hpp` 糖层)

糖层 header-only、**不属于 ABI**、零运行开销,100% 建立在 `flow.h` 上。
核心是 `KernelAdapter<T>` 把虚函数桥成 C ABI vtable,并在蹦床里 `try/catch`
**挡住 C++ 异常穿越 FFI**(异常穿 `extern "C"` 是 UB)。

```cpp
class PassThroughKernel : public lmflow::Kernel {
 public:
  static void GetContract(lmflow::Contract& c) { c.InputSetAny(0); c.OutputSetAny(0); }
  lmflow::Status Process(lmflow::Context& cc) override {
    cc.Forward(0, 0);                     // 零拷贝直通
    return lmflow::Status::Ok();
  }
};
LMFLOW_REGISTER_KERNEL(PassThroughKernel)    // 或 LMFLOW_REGISTER_KERNEL_AS(T, "别名")
```

- 静态 `GetContract` 可选,有则经 SFINAE 自动接线。
- `Packet::Get<T>()` 带 `type_id` 校验;`TryGet<T>()` 类型不符返回 `nullptr`(绝不 UB)。
- `Context` 禁拷贝/移动,防止算子把只在回调期有效的句柄留存。
- 注册:**内置算子用显式聚合注册**(`lmflow_register_builtin_kernels`),因为静态初始化
  对象在静态库中可能被链接器裁剪;用户算子可直接用宏。
- **算子与引擎解耦**:注册表(`src/kernel.rs`)是唯一一张语言无关的 `name → vtable` 表,
  C++(`flow.hpp`)、Python(pybind11)、Rust(`trait Kernel`)三条路都汇入同一个
  `kernel::register`,引擎不知道算子是什么语言写的。内置的 18 个 C++ 算子因此只是**捆绑的
  算子库**、不是引擎的一部分 —— 它们放在 crate 之外(`lmflow/cpp/`,见 §11),**不随发布的
  crate 分发**;由 `builtin-kernels` feature(**默认关**)编入,只在本仓库内可用。
- **引擎自带默认 Rust 算子**(`src/builtin.rs`,建图时 `Graph::from_config` 自动注册一次、
  零 C++、任何配置下都在)—— **刻意只有两个**,且都纯结构性、零 payload 假设:
  `PassThrough`(直通接线)与 `Sink`(只消费,让分支自行终结;计 `sink.packets`)。
  名字**不带 `Kernel` 后缀**,以免与 C++ 内置算子重名(注册表按名字唯一,重名直接报错)。
  **为什么不多放**:`Scale`/`Sum`/`Zip`/`Filter` 之类必须假设 payload 是 i64,与 ADR #6
  「引擎不解释 payload」相悖;演示引擎语义是 `cpp/kernels/` 那 18 个与 `examples/` 的职责。
  扇出也不需要算子 —— **一条边可直接挂多个消费者**是原生能力(见 §7.5),故不放 `Split`。
- 内置算子清单见 `cpp/kernels/register.cc` 表头。其中**张量前处理组**(纯数值 BUFFER):
  `Cast`(dtype 转换)、`Affine`(`x*scale+shift`)、`Clamp`、`Reduce`(→F64 标量)——
  统一走 double 做 dtype 分派,连续缓冲、暂不支持 F16。示例见
  `examples/python/preprocess/preprocess.py`(u8 图 → f32 → 归一化 → clamp)。

### 5.2 Python 算子(pybind11,已实现)

```python
@lmflow.kernel("PyDouble")
class PyDouble(lmflow.Kernel):
    @staticmethod
    def get_contract(c):
        c.input_set_any(0); c.output_set_any(0)
    def open(self, cc):    self.factor = cc.option_int("factor", 2)
    def process(self, cc): cc.emit(0, cc.input(0).as_int() * self.factor)
```

与 C++ 算子在 YAML 里**平等引用** —— 引擎不知道算子是什么语言写的。
细节见 §8 专章。约束:只收发内建类型;回调期持 GIL;异常转错误码。

### 5.2b Rust 算子(`trait Kernel`,已实现)

Rust 也能一等公民地写算子:实现 `lmflow::Kernel`(`get_contract`/`open`/`process`/`close`)+
运行期 `register_kernel::<T>("Name")`,YAML 用 `kernel: Name` 引用。安全的 `KernelCtx`/
`KernelContract` 包装内部 `Context`/`Contract`(不裸调 C ABI);panic / `Err` 都被 `catch_unwind`
兜成图错误、不穿越边界。这是**C ABI vtable 之上的糖**(和 `flow.hpp` 的 `KernelAdapter<T>` 对 C++
做的一样),**不是**引擎绕过 vtable 的原生快路(见 ADR)。见 `core/src/kernel_api.rs`。

> **外部 Rust 工程**:`cargo add lmflow` → `use lmflow::{Graph, Packet, Timestamp, Kernel, register_kernel}`。
> 引擎就是这一个 crate `lmflow`(包名 = 库名 = `lmflow` → `liblmflow.a`,C ABI/CMake/Python 共用同一库)。

### 5.3 内置算子清单(`cpp/kernels/`,一文件一算子)

既是可用算子,也是 API 覆盖用例。

> **类型约定**:捆绑算子一律用**内建类型**(`LMFLOW_TYPE_I64` 等)声明契约,
> 而不是 `InputSet<int>`(C++ 的 typeid)。原因是后者 Python/Go 侧无从产生同样的
> 标识 —— 若用它,Python 送来的整数会被类型校验拒绝。这条是「一套算子三语言可用」
> 的前提,`flow.hpp` 为此提供了 `InputSetBuiltin`。

| 算子 | 用途 | 覆盖接口 |
|---|---|---|
| `PassThrough` | 零拷贝直通 | `Forward` |
| `Scale` | 参数化数值变换 | `OptionI64`、类型声明 |
| `Sum` | 有状态累加,`Close` 输出总和 | 跨包状态、`PostStream` |
| `Split` | 1 进 2 出(扇出) | 多输出 |
| `Zip` | 2 进 1 出 | `InputId(tag)` 按标签定位端口 |
| `Filter` | 条件过滤 | 不 `Emit` + `SetNextTimestampBound` |
| `Stringify` | `int → std::string` | 异类型输入输出 |
| `Sink` | 只消费不产出 | 零输出口 |
| `Invert` | 原地改写 | `TakeInput` + CoW `MakeMutableBuffer` |
| `Normalize` | 参数用法示范 | 必需参数 / 数组参数 / 点号路径 / side packet 声明 / 日志 / 关闭原因 |

---

## 6. 图与拓扑

### 6.1 Rust 侧结构:索引 arena

核心决定:**不还原自引用裸指针对象图**,所有实体存在 `Graph` 的 `Vec` 里,
相互引用用 `usize` id。这既契合数据流本身的整数 id 语义,也避开借用检查器的自引用难题。

```rust
pub type NodeId = usize;
pub type EdgeId = usize;

pub struct Edge {
    name: String,
    producer: Option<NodeId>,           // None = 图输入口喂入
    consumers: Vec<(NodeId, usize)>,    // (下游节点, 它的第几个输入口)
    queue: Mutex<VecDeque<Packet>>,
    bounded: bool,                      // 仅图输入口为 true(§7.5)
    closed: AtomicBool,
}

pub struct Node {
    name: String,
    kernel: KernelInstance,             // C++/Python 算子黑盒
    inputs:  Vec<EdgeId>,
    outputs: Vec<EdgeId>,
    ctx: Mutex<Context>,
    sched: Mutex<NodeSched>,
}

pub struct Graph {
    nodes: Vec<Node>, edges: Vec<Edge>,
    edge_by_name: HashMap<String, EdgeId>,
    graph_inputs: HashMap<String, EdgeId>,
    graph_outputs: HashMap<String, EdgeId>,
    executors: HashMap<String, Executor>,
    pollers: Vec<Arc<Poller>>,
    state: Mutex<GraphState>,
}
```

用 `HashMap<name, EdgeId>` 直接拿 id,避免「名字排序名次」与「存储下标」混用这类错误。

### 6.2 `init_from_yaml` 的校验清单

任一不通过即返回错误,并可由 `lmflow_last_error()` 取原因:

1. 端口名引用不到上游生产者;
2. 同一端口名有多个生产者;
3. 图输入口与某节点的输出口同名;
4. **拓扑成环** —— 未被 back-edge 打断的环无法终止,故直接拒绝;把反馈输入口标为 `back_edges` 即可合法成环(§7.12);
5. 节点的 `executor` 名未在 `executors:` 中定义;
6. **零输入口节点** —— B 阶段的就绪规则对空集恒为真,会被无限调度成自旋(§7.4);
7. `max_in_flight > 1` 却没配 executor(默认执行器是主线程,单线程无并行)→ 报错,不静默降级。

> 第 7 条尤其重要:**宁可报错也不静默忽略**,否则用户以为开了并行、实际没有。

### 6.3 图的生命周期状态机

```
      new()            init_from_yaml()          start()
 ─────────────► Created ──────────────► Initialized ──────────► Running
                                             │                     │
                    add_poller / observe ────┘        close_all_inputs()
                    input()(取句柄)                  或 cancel()
                                                                   ▼
                                                                Draining
                                                                   │ 全部节点关闭
                                                                   ▼
                                                               Terminated ──► free()
```

| 状态 | 允许的操作 |
|---|---|
| `Created` | `init_from_yaml`、`free` |
| `Initialized` | `add_poller`、`observe`、`input`(取句柄)、`start`、`free` |
| `Running` | `send`/`try_send`、`poller_next*`、`close_input`、`close_all_inputs`、`cancel`、`wait_done*`、`dump`、`queue_depth` |
| `Draining` | 同 `Running`,但 `send` 返回 `LMFLOW_ERR_CLOSED` |
| `Terminated` | `wait_done*`(立即返回)、`last_error`、`dump`、`free` |

- 其它组合返回 **`LMFLOW_ERR_STATE`**(如 `start` 两次、未 `start` 就 `send`)。
- **`add_poller` / `observe` 必须在 `start` 之前**,否则可能丢失已产出的包。
- 本版本**不支持重跑**:`Terminated` 之后只能 `free`。

### 6.4 端口的命名与定位

有**两套标识符**,分工明确:

| 标识符 | 属于 | 用途 |
|---|---|---|
| **端口名 (name)** | **图** | 连接用:上游 `output_ports` 与下游 `input_ports` **同名即连成一条边**。整张图中每个名字只能有一个生产者 |
| **标签 (tag)** | **算子** | 算子表达「我哪个口是什么语义」,**不依赖 YAML 书写顺序**,改边名也不会错 |

声明语法(`input_ports` / `output_ports` 的元素):

```yaml
input_ports: ["frames",                    # 无 tag(归入空 tag "")
              "VIDEO:cam0",                # 有 tag,index 自动
              "MASK:0:m0", "MASK:1:m1"]    # 有 tag,index 显式
```

三种定位方式:

```cpp
cc.InputId("VIDEO")       // → 1    按 tag(推荐:语义稳定)
cc.InputId("MASK", 1)     // → 3
cc.InputIndex("frames")   // → 0    按边名(通用/路由类算子偶尔需要)
cc.Input(0)               //        直接序号,最省事但依赖声明顺序
```

规则:

- tag 约定大写字母 / 数字 / 下划线,不含 `:`;空 tag 表示「无标签」。
- 同一算子、同一 tag 下 index 必须从 0 连续、不得重复(否则 init 报错)。
- **扁平序号 = YAML 声明顺序**(第 0 个声明即序号 0)。

> 最后一条是**有意与常见做法分道**(ADR #17)。按 tag 字典序分组的方案下,混用
> 有标签与无标签端口时 `Index(0)` 拿到的不一定是你写的第一个 —— 这是个只在运行时
> 才暴露的陷阱。改成声明顺序后它就不存在了。

---

## 7. 执行模型(B 阶段详细规格)

### 7.0 三条硬规则(整套并发设计的地基)

> **R1 —— 调用算子期间不持任何引擎锁。**
> 回调前必须放掉所有锁,否则算子内部回调 `lmflow_ctx_*`(或算子自身阻塞)会死锁。
> 这条决定了 §7.3 的 staging 设计。
>
> **R2 —— 锁序恒为 `node.sched` → `edge.queue`,禁止反向。**
> 推包路径必须「锁边 → push → **解锁边** → 再唤醒下游节点」。全局无环,故无死锁。
>
> **R3 —— `running == true` 是节点的独占令牌。**
> 一旦某线程把节点置为 `running`,它即独占该节点的 `Context` 与「从输入边弹包」的权利,
> 之后**无需再持 `node.sched` 锁**。这是 §3.1 中 `Send/Sync` 断言的**依据**。

### 7.1 M2:同步 push(无线程,先验证 FFI 闭合)

- `send` → push 到图输入边 → 驱动其消费者。
- `drive(node)`:就绪则每个输入口弹一个包装进 `Context` → 调算子 → 把 staging 分发到下游边 → 递归驱动。
- 为防深图/环形爆栈,用**显式工作栈**(`Vec<NodeId>`)而非真递归。
- hello_world 全程跑在调用线程上,零并发即正确。
- ⚠ M2 下 `executors:` 配置**不生效**(没有线程池),文档须写明,别让人以为配了就有用。

### 7.2 M3:节点调度状态机(合并唤醒,防丢唤醒)

```rust
struct NodeSched {
    opened: bool, closed: bool,
    running: bool,   // 已有任务在跑(独占令牌,R3)
    rescan: bool,    // 运行期间又来了包 —— 跑完必须重扫,否则丢唤醒
}
```

```rust
fn try_claim(node) -> bool {
    let mut s = nodes[node].sched.lock();            // R2:先 node
    if !s.opened || s.closed { return false; }
    if s.running { s.rescan = true; return false; }  // 合并唤醒,不重复入队
    if !inputs_ready(node) { return false; }         // 内部按需短暂锁 edge
    s.running = true; true                            // 拿到独占令牌
}

fn run(node) {                        // 由 R3,以下全程不持 node 锁
    let ctx = pop_one_from_each_input(node);
    let st  = kernel.process(ctx);     // ★ 零锁调用算子(R1)
    if st != OK { record_error(st); discard_staging(node); return finish(node); }
    dispatch_staging_to_downstream(node);   // 锁边 push → 解锁 → 唤醒下游(R2)
    finish(node);
}

fn finish(node) {
    let again = { let mut s = lock(); s.running = false;
                  let a = s.rescan; s.rescan = false; a };
    if again || inputs_ready(node) { schedule_if_claimed(node); }
    maybe_close(node);                 // §7.6
}
```

**为什么必须有 `rescan`**:若上游在本节点 `running` 期间 push 了包,`try_claim` 会因
`running` 返回 false;若不记下这次唤醒,包就永远躺在队列里没人处理 —— 经典丢唤醒。
(这也是调度态需要 `Scheduling / SchedulingPending` 两态区分的原因。)

### 7.3 emit 走 staging —— 让 R1 成立

算子在 `process` 里调 `emit`/`forward` **不直接写下游边**(那要在回调期持边锁,违反 R1),
而是写进本节点 `Context` 内的暂存区;`process` 返回后引擎统一分发。

```rust
struct Context {
    inputs:  Vec<Option<Packet>>,   // None = 已被 take_input 取走
    staging: Vec<Vec<Packet>>,      // 每个输出口一个暂存队列
    input_ts: Timestamp,
}
```

- `emit(i, pkt)` → `staging[i].push(接管 pkt)`。
- `forward(in, out)` → `staging[out].push(inputs[in].clone())`(仅 `Arc::clone`)。
- `take_input(i)` → `inputs[i].take()`,所有权移交算子(CoW 的前提,§3.4)。
- 分发时对每条输出边按消费者数 `Arc::clone`。

⚠ **裸 C ABI 用户注意**:`take_input` 后若既不 `emit` 也不 `drop` 就早退,会泄漏。
C++/Python 侧有 RAII/GC 兜住,裸 C 没有。

### 7.4 就绪判定 + 零输入 source 节点

B 规则:**每个输入口都有 ≥1 包** → 取各口队首组成一次 `Process`(多输入按时间戳对齐,见 §7.2)。

**零输入节点 = source(生成型算子)**,由内核自产数据。它套不上上面的规则(空集「恒真」既会自旋、又会「空真即关」),故单独处理:
- **就绪**:未自报完成即「可产出」;`try_claim` 给它一个单调递增的时间戳(认领序号),auto-`emit` 继承 → 下游单调。并发度受 `max_in_flight` 个槽约束;`finish` 后自我重挂续产。
- **定速(v1)**:由**内核自定速** —— `process` 里自行阻塞(等帧 / 读下一条 / sleep)。故 source **必须挂线程池执行器**(跑主线程会独占、拖垮全图;`config` 校验强制)。不做引擎级 timer/rate(留后续)。非自定速的源会灌爆下游(内部边不背压,见 §7.5)。
- **完成**:内核调 `source_done()` → 引擎停止再触发本节点、关其输出边(复用关流级联,§7.6)→ 下游收流 → 图正常终止。无限源(永不 `source_done`)由 host `cancel()` 停。

内置 `RangeSourceKernel`(产 `0..count` 后 `source_done`)为样板。

### 7.5 背压策略

**只有图输入口是限流点;图内部的边不对生产者施加背压。**

| 边 | 上界 | 满了怎样 |
|---|---|---|
| 图输入口 | 有界(`max_queue_size`) | `send` 阻塞至有空位或图终止;`try_send` 返回 `LMFLOW_ERR_WOULD_BLOCK` |
| 内部边(节点→节点) | **无硬上界** | 仅在超过软水位时告警/计数,可用 `queue_depth` 观测 |

**为什么内部边不设硬上界** —— 否则「扇出后再汇合」的合法 DAG 会死锁:

```
        ┌─► B(慢) ─┐
   A ──►┤           ├──► D
        └─► C(快) ─┘
```
C 迅速填满 D 的输入队列而阻塞;D 却要等 B 那一路才能消费;B 又在等 A 推进;
而 A 已阻塞在 C 上 —— **循环等待,且不需要环形拓扑就会发生**。

生产者永不阻塞在内部边上,循环等待即无从形成。内存总量由「图输入口限流 ×
DAG 有界的扇出倍数」间接约束。

**阻塞 `send` 命中全局水位时怎么等** —— 分两种执行模型:

- 图跑在**宿主主线程**(默认执行器):send 就地 `pump_step()` 自己推进一步(跑一个节点),
  把水位压下去再继续。
- 图跑在**线程池**:主线程无任务可推,此时 send 应**等池排水**(`wait_activity_since`),
  由工作线程消费降水位后唤醒。**只有当全图彻底空转(无在飞任务)水位却仍下不去**
  (例如下游无人消费、包堆在出口)——那才是真卡死,`send` 返回 `WOULD_BLOCK` 而非永久阻塞。

  > ⚠ 曾有缺陷:早期实现里 send 只会 `pump_step()`,而它只跑主线程任务;于是**池图**上
  > 一撞水位就误报 `WOULD_BLOCK`(阻塞 send 退化成了报错)。已修正为「推不动且池仍在跑就等排水」,
  > 并有回归测试 `blocking_send_applies_backpressure_on_pool_instead_of_erroring` 守卫。

### 7.6 关流与终止

- `close_all_inputs` → 所有图输入边标 `closed`,并唤醒其消费者(触发排空)。
- **节点可关条件**:所有输入边 `closed` 且队列空 且 `!running`。
  → 调算子 `close`(零锁,R1)→ 标 `closed` → 关自己所有输出边 → 递归下游。
- 检查点:`finish(node)` 末尾 **和** 上游关流事件 —— 两处都要查,否则末尾节点可能永不关闭。
- **终止判定**:`GraphState{ open_nodes, err }` + `Condvar`;节点关闭时 `open_nodes -= 1`,
  归零则 `notify_all`。`wait_done` 等这个条件。
- `poller_next`:所在边 `closed` 且队空 → 返回 false;否则阻塞在 poller 自己的 condvar 上。
- **`cancel` 不是抢占**:停止调度新任务、丢弃在途包、唤醒所有等待者,但**已在执行中的
  算子回调不会被中断**。故 `cancel` 返回后可能仍有一个算子在跑,须 `wait_done` 确认静止。

### 7.7 错误路径

- 算子返回非 0 → `record_error` → 置 `AtomicBool has_error`(快路径)+ 存首个错误(含文本)。
- **失败时丢弃该次的 staging**,不传播半成品输出。
- 此后 `try_claim` 一律返回 false(停止调度)、`send` 返回错误、所有 poller 被唤醒返回 false、
  `wait_done` 返回该错误码,`lmflow_graph_last_error` 可取文本。

### 7.8 竞态 / 死锁自查表

| 隐患 | 挡法 |
|---|---|
| 同一节点被两线程并发 `process` | R3 独占令牌(`running` 在 node 锁下置位) |
| 丢唤醒(包躺队列没人跑) | `rescan` 标记 + `finish` 重扫 |
| 回调期持锁死锁 | R1 零锁调用 + staging 暂存 |
| 锁序成环 | R2 单向 `node → edge`;push 后先解锁再唤醒 |
| **扇出汇合背压死锁** | §7.5 内部边不背压 |
| 末尾节点永不关闭 | 关流在 `finish` 与上游事件两处检查 |
| `wait_done` 永久阻塞 | `open_nodes` 计数 + Condvar;错误也唤醒;并提供超时版本 |
| 零输入节点自旋 | init 阶段拒绝 |
| 共享 payload 被就地改写 | 只读视图 + CoW(§3.4) |
| payload 跨线程析构 | R3 保证访问串行;`Send` 断言写明前提 |
| 日志回调与引擎形成锁序环 | 回调时保证不持引擎锁 |

### 7.9 执行器与线程归属

图在 YAML 里定义**命名线程池**,节点按名字选择在哪个池上执行:

```yaml
executors:
  - name: "cpu"
    type: "ThreadPoolExecutor"
    num_threads: 4
  - name: "io"
    type: "ThreadPoolExecutor"
    num_threads: 1
nodes:
  - { name: "decode", kernel: "Decoder",  executor: "io"  }
  - { name: "detect", kernel: "Detector", executor: "cpu" }
  - { name: "draw",   kernel: "Overlay" }        # 未指定 → 宿主主线程
```

**CPU 亲和力(绑核)** —— 池可选 `affinity: [核号...]`,worker `i` 绑到 `affinity[i % len]` 号核:

```yaml
executors:
  - { name: "rt", type: "ThreadPoolExecutor", num_threads: 2, affinity: [2, 3] }
```

- 语义是 `sched_setaffinity` 的**硬绑核**:worker 线程**只能**在所列核上跑,减少迁移抖动、利于 NUMA/实时。
- `affinity` 长度 == `num_threads` 即 1:1 独占绑核;短于线程数则按 `i % len` 轮转复用。
- **仅 Linux 内核系生效**(`target_os = linux`,含 Android=`android`、OpenHarmony/鸿蒙标准系统=`linux`);
  iOS/macOS/Windows 无 `sched_setaffinity`(Apple 不允许硬绑核,由系统管核),静默降级为不绑,不影响正确性。
- 只用 libc(glibc/Bionic/musl)已链接的 `sched_setaffinity` 符号(`extern "C"` 声明),不引入 `libc` crate,守住「零外部 crate 依赖」。
- ⚠ 亲和力 ≠ **优先级**:它只限制「能在哪些核跑」,不改变线程分到多少 CPU 时间。

**实时优先级** —— 池可选 `priority: 1..=99`,把 worker 切到 `SCHED_FIFO` 实时调度:

```yaml
executors:
  - { name: "rt", type: "ThreadPoolExecutor", num_threads: 2, affinity: [2, 3], priority: 20 }
```

- `0`(默认)= 普通分时(`SCHED_OTHER`),不动;`1..=99` = 该 RT 优先级的 `SCHED_FIFO`(Linux/Android)。
- **iOS/macOS 无应用级 SCHED_FIFO** —— 映射成 Apple 的 **QoS class**:`priority>0` → `USER_INITIATED`
  (「用户在等结果」,正合推理),顶格(`>=90`)→ `USER_INTERACTIVE`。这是 Darwin 上表达线程重要性的正道。
- **尽力而为**:设实时调度需 `CAP_SYS_NICE`/root,拿不到就静默降级为普通分时,不影响正确性。
- 与绑核**配合**是刻意的:实时线程只在被绑的核上抢占,万一算子死循环也只拖垮那几个核、不拖垮整机。
  worker 空闲时阻塞在 condvar 上(让出 CPU),故 idle 的 RT 线程不会空转饿死别人。

**默认(节点未写 `executor`)= 宿主主线程,不是线程池**(ADR #16):

- 默认零并发、执行顺序确定、断点调试直观;**并发是显式 opt-in**。
- 副产品:**Python 算子默认跑在 Python 主线程上,完全没有 GIL 争抢**
  (只有显式把 Python 算子放进线程池时才需要考虑 GIL,见 §8.2)。

⚠ **主线程任务的执行时机** —— 引擎不能凭空占用宿主线程,只能在宿主**进入引擎**时借用它。
因此主线程节点的任务在宿主调用下列**阻塞接口**期间被抽取执行:

```
lmflow_graph_wait_done / _timeout
lmflow_graph_wait_until_idle / _timeout
lmflow_poller_next / _timeout
lmflow_input_send(阻塞等待空位时)
```

- 若宿主只 `send` 而从不调用上述任一接口,**主线程上的节点不会推进**。
- 反之,这些接口在等待期间一律抽取并执行主线程任务,故不会因此死锁。
- 节点引用了未定义的 executor 名字 → init 阶段报错(§6.2 第 5 条)。

> 与里程碑的关系:M2(同步 push)天然只有主线程执行器 —— 它不是临时方案,
> 而正是**默认执行模式**;M3 增加线程池,供显式选择。

### 7.10 输入策略(节点级可插拔)

「多个输入口如何凑成一次 `Process`」+「队列满了怎么办」被抽成**可插拔策略**,
而不是写死在引擎里 —— 实时丢帧与(A 阶段的)时间戳对齐因此共用同一扩展点。

```yaml
- name: "detect"
  kernel: "Detector"
  input_ports: ["frames"]
  input_policy: { type: "fixed_size", capacity: 2 }
```

| `type` | 语义 | 阶段 |
|---|---|---|
| `sync`(默认) | 所有输入口齐备才触发。B = 每口至少一个包;A = 按时间戳对齐 | B / A |
| `immediate` | 各输入口独立触发,不等其它口。适合无需对齐的旁路处理 | B |
| `fixed_size` | 有界 + **满则丢弃最旧的包**(`capacity` 默认 1) | B |
| `sync_set` | **部分对齐**:输入口划成若干组,每组内各自按时间戳对齐,任一组就绪即触发、只带该组的口 | B |
| `batch` | **批处理**:攒够 `capacity` 个包一次交给算子(`process()` 用 `input_count`/`input_at` 读整批),关流刷余批。v1 单输入口 | A |

`sync_set` 用于「A、B 该配对、C 独立」这类图 —— 只能全对齐或全不齐是不够的。配置用端口名给出分组,
须**完整划分**全部输入口(每口恰属一组;要独立就单独成组):

```yaml
input_policy: { type: sync_set, sets: [["left", "right"], ["imu"]] }
```

引擎内部把「就绪判定」的返回从单个时间戳升级为**触发计划(时间戳 + 参与口)**:认领时只对参与口
弹包、推进 bound,其余口原样不动。`sync`/`immediate`/`fixed_size` 的参与口是"全部",行为不变。

**多路选择(Mux)不做成输入策略,而是内置 `MuxKernel`**:控制口(输入 0,I64 选择器)的值选择
把哪个数据口(输入 1..)转发到输出,配默认 `sync` 用。理由:push 模型下"只要求选中那一路"的
策略会让未选中口越积越多、甚至因陈旧时间戳卡死;`sync` 全对齐 + kernel 读控制转发既不积压也不卡,
且**引擎不解释 payload**(读控制值的是 kernel,守住 ADR #6)。

`fixed_size` 是**有意的有损**策略,且**不阻塞上游** —— 因此与「内部边不背压」(§7.5)
并不冲突,而是其配套的内存约束手段:摄像头 30fps 而算子只跑 10fps 时,
无界队列会让内存无限增长,**丢旧帧才是正确取舍**。

`batch` 把多个时间戳**攒成一批**一次交给算子(与「单包/次」相反):`process()` 里 `input_count(i)`
给出本次批大小、`input_at(i, k)` 取第 k 个包 —— 单包策略下 `input_count` 恒 0/1,故同一套访问器
两种交付通用。攒够 `capacity` 即触发,**关流时不足一批也刷出**(不丢数据),批的输出时间戳继承批内
末包(下游单调)。用于批推理、窗口聚合。v1 仅单输入口(多口批对齐留后续);内置 `BatchSumKernel` 为样板。

**丢包绝不静默**:除 `lmflow_graph_dropped_count(port)` 累计计数外,首次丢弃还会打一条
WARN 日志。任何有损行为都必须可观测,否则「跑通了」和「悄悄丢了一半」无法区分。

---

### 7.11 子图(subgraph)与跨文件 include

子图把一张小图打包成「一个节点」复用;`include` 把子图库拆进独立 YAML。两者都是**纯建图期变换**(`GraphConfig → 展平的 GraphConfig`,见 `src/expand.rs`),插在 parse 与 `check_supported`/`build` 之间。**运行时引擎 / 调度器完全不感知子图** —— 连边纯按端口名字符串,展开只是多产出些节点和名字。

**定义与实例化**:

```yaml
subgraphs:
  Denoise:                 # 子图名
    nodes:
      - { name: a, kernel: BlurKernel,    input_ports: [sin], output_ports: [mid] }
      - { name: b, kernel: SharpenKernel, input_ports: [mid], output_ports: [sout] }
    input_ports: [sin]     # 边界输入口
    output_ports: [sout]   # 边界输出口
nodes:
  - { name: d, type: Denoise, input_ports: [raw], output_ports: [clean] }   # 实例:type 填子图名
  - { name: s, kernel: ScaleKernel, input_ports: [clean], output_ports: [out] }
input_ports: [raw]
output_ports: [out]
```

**展开规则**:实例节点 `d`(`type: Denoise`)被内联替换成子图内部节点:

- **命名空间**:内部节点名 → `d/a`、`d/b`;内部边 → `d/<边名>`(如 `d/mid`)。用 `/` 分隔(`:` 是端口 tag 分隔符,不能用)。同一子图实例化多次(`d`、`e`)各自命名空间,内部边不串。
- **边界按位置重映射**:子图 `input_ports[i]` ↔ 实例 `input_ports[i]`,`output_ports` 同(数目须一致,不齐报错)。上例 `sin→raw`、`sout→clean`,于是展平后 `d/a`(`raw`→`d/mid`)、`d/b`(`d/mid`→`clean`),与手写扁平图完全等价。
- **递归 + 环检测**:子图内部节点还能是子图实例(`type:`),递归展开;`A→…→A` 报错。
- **`kernel` 与 `type` 二选一**:实例节点填 `type`、算子节点填 `kernel`,两个都给或都不给都报错。

**跨文件 `include`**:

```yaml
# main.yml
include: ["lib.yml"]       # 相对本文件目录;可多个;可递归(被引文件也能 include)
nodes:
  - { name: d, type: Denoise, input_ports: [raw], output_ports: [clean] }   # Denoise 定义在 lib.yml
```

- 只并入被引文件的 `subgraphs`(它是子图库);其 `nodes` / `executors` / ports 忽略。子图内部节点按名引用**主图**的 executor。
- 子图重名(跨不同文件)报错;同一文件被引多次(菱形)去重、不报错。
- 相对路径需要基准目录,故 `include` **仅 `from_yaml_file` 支持**;`from_yaml`(文本入口)遇 `include` 明确报错。C ABI 的 `lmflow_graph_init_from_yaml_file` 自动受益。

未知子图名 / 边界数不齐 / 环 / `kernel`+`type` 冲突,都在 `init` 时报错(`LMFLOW_ERR_INVALID_ARG`),不静默。

---

### 7.12 反馈环(back-edge)—— 最新值反馈寄存器

默认图必须是 DAG(`check_acyclic` 拒环)。要让边成环(反馈控制、循环状态、流驱动迭代),把消费**反馈**的那个输入口列进节点的 `back_edges`:

```yaml
nodes:
  - name: acc
    kernel: FeedbackAddKernel
    input_ports: [in, out]      # in = 正向;out = 反馈(消费本节点自己的输出边 out)
    output_ports: [out]
    back_edges: [out]           # 把 out 口标为反馈寄存器 → 自环合法
input_ports: [in]
output_ports: [out]
```

**语义(最新值反馈寄存器)**:被标记的输入口是一个**容量 1、留最新值、消费一次**的队列,且

- **不参与就绪判定** —— 只有**正向**输入能触发节点(*核心不变式*:反馈包永不自激重跑);
- **不参与终止判定** —— 节点靠正向输入排空即可关闭,关闭级联绕环拆解;
- **不参与时间戳对齐** —— 反馈包拿什么时间戳都不污染 `sync_align`;
- 首拍(尚无反馈)该口为空,内核自处理(如按 0)。

**为什么安全**(三个危害各自化解):

| 危害 | 化解 |
|---|---|
| 终止死锁(`A→B→A` 互等对方关闭) | 反馈口不计入排空判定 → 正向侧驱动关闭、级联拆环 |
| 内存无界(内部边无背压) | **去掉 back-edge 后仍是 DAG**(`check_acyclic` 只跳过标记的反馈边),DAG 内存论证成立;反馈口 cap-1 有界 |
| 时间戳非单调 | 反馈口不进对齐集 |

**约束**:非 source 节点必须至少留一个正向输入(否则永不触发,`init` 报错);`back_edges` 名字须是本节点输入口;`sync_set` 分组不得含反馈口;**未被 `back_edges` 打断的拓扑环仍报错**。纯建图期 + 调度器局部改动,无新增阻塞 / 锁。内置 `FeedbackAddKernel`(`out = 正向 + 反馈`)为样板,自环即运行累加。

---

## 8. Python 接口(已实现)

### 8.1 形态

pybind11 模块 `lmflow._lmflow`(`python/src/bindings.cc`)链接 `liblmflow`,
只调用 `include/lmflow/flow.h` 这一层 C ABI —— 和 C++ 算子走同一条路。
Python 包 `lmflow`(`python/lmflow/__init__.py`)在其上提供 `@kernel` 装饰器、
`Kernel` 基类、`Graph` 上下文管理器与类型常量。
Python 既可**注册算子**,也可**驱动图**。

构建:`pip install .`(scikit-build-core 驱动仓库根 CMake:cargo 编引擎 + pybind11 扩展;
pybind11 由 `third_party/pybind11` 子模块提供)。或直接走 CMake:`cmake -B build -DLMFLOW_BUILD_PYTHON=ON`。

```python
@lmflow.kernel("PyOffsetKernel")
class PyOffsetKernel(lmflow.Kernel):
    @staticmethod
    def get_contract(c): c.input_set_any(0); c.output_set_any(0)
    def open(self, cc):    self.offset = cc.option_int("offset", 0)
    def process(self, cc): cc.emit(0, lmflow.Packet.from_int(cc.input(0).as_int() + self.offset))

with lmflow.Graph.from_yaml(CONFIG) as graph:        # with 是硬要求,见 8.3
    poller = graph.add_poller("out"); graph.start()
    source = graph.input("in")
    for i in range(10):
        source.send(lmflow.Packet.from_int(i), ts=i)
        print(poller.next(timeout=5.0).as_int())
    graph.close_all_inputs(); graph.wait_done(timeout=5.0)
```

同一张图里 C++ 算子与 Python 算子平等引用 —— 引擎不区分算子语言。

### 8.2 GIL

- **默认情况下不存在 GIL 争抢**:节点未指定 `executor` 即跑在宿主主线程,
  Python 算子就在 Python 主线程上执行(§7.9)。
- **只有显式把 Python 算子放进线程池时**才有下列问题:`process` 在引擎工作线程上被
  回调、期间持 GIL ⇒ 多个 Python 算子之间无法真并行。重计算应写成 C++ 算子,
  或让 Python 算子留在主线程、把 C++ 算子放进池里(见 `examples/python/opencv_pipeline/opencv_pipeline.py`)。
- **所有可能阻塞的接口必须释放 GIL**(`poller.next` / `wait_done` / `send`),
  否则工作线程拿不到 GIL → 直接死锁。pybind11 用 `py::call_guard<py::gil_scoped_release>()`。
- 引擎线程由 Rust 创建、Python 并不认识:每次 `gil_scoped_acquire` 会建立 thread state,
  线程退出时须清理,否则泄漏。

### 8.3 解释器生命周期(崩溃来源)

图必须在**解释器开始销毁之前**停掉,否则工作线程可能回调进正在析构的解释器 → 崩溃。
因此 `Graph` 实现上下文管理器,`__del__` 只作兜底、不保证时机。文档必须强调 `with`。

### 8.4 数据类型限制

**Python 算子只能收发内建类型**(整数/浮点/布尔/字符串/bytes/`LMFlowBuffer`)。
理由见 ADR #9:保持 payload 的**语言中立性**。表达结构化数据:

| 想传 | 用什么 |
|---|---|
| 检测框、关键点等数值集合 | N×K 的 `LMFlowBuffer`(零拷贝,C++ 侧可直读) |
| 任意元数据 | JSON 字符串(`LMFLOW_TYPE_STR`) |
| 配置参数 | node `options`,本不该进数据流 |

`LMFLOW_TYPE_HOST_OBJECT`(7 号)已预留但**未启用**。若将来开放,应利用「引擎在 init
阶段已知每个节点算子语言」这一点,直接拒绝「原生对象端口接到异语言算子」的拓扑。

### 8.5 零拷贝的正确写法

直觉写法 `send(cv2.imread(...))` 是错的 —— 要么整帧拷贝,要么引擎持有 PyObject 引用,
而引用归零可能发生在工作线程上,那里 `Py_DECREF` 需抢 GIL(死锁隐患)。
正确姿势是**让引擎分配缓冲**,拿零拷贝 numpy view 就地写入:

```python
def process(self, cc):
    src = cc.input(0).as_numpy()                       # 零拷贝只读视图
    packet, dst = cc.new_buffer((h, w, 3), np.uint8)   # 引擎分配,dst 指向引擎内存
    cv2.resize(src, (w, h), dst=dst)                   # 直接写进去
    cc.emit(0, packet)
```

就地改写走 CoW,同样要先 `take_input`:

```python
packet = cc.take_input(0)     # ← 先取走,否则上下文仍持引用 → CoW 必然复制
img = packet.make_mutable()   # 独占 → 零拷贝可写 numpy view
cv2.GaussianBlur(img, (5, 5), 0, dst=img)
cc.emit(0, packet)
```

### 8.6 其它必须处理的点

- **Ctrl-C**:阻塞调用释放 GIL 后停在 Rust 里,Python 信号处理不会运行 ⇒ Ctrl-C 无响应。
  实现上须按短超时轮询,周期性回 Python 检查信号。
- **异常**:Python 异常不得穿越 FFI;蹦床里捕获并转 `LMFLOW_ERR_KERNEL`,原文本走日志。
- **fork**:引擎线程启动后 fork 会得到残缺状态;文档禁止(含 `multiprocessing` 默认 fork 启动法)。
- **静态库双份注册表**:见 §8.5 标题下 —— Python 模块必须链**动态库**,否则算子注册表
  会有两份互不可见的副本。同时对 `cdylib` 做符号可见性控制,只导出 `flow_*`。

---

## 9. FFI 边界与安全

- Rust 内部用 `Result<T, Error>`;`?` 取代错误码宏。
- **每个 `extern "C"` 导出函数**用 `catch_unwind` 包裹,panic → `LMFLOW_ERR_PANIC`
  (Rust panic 穿越 FFI 是 UB)。
- **算子回调方向**:C++ 异常 / Python 异常同样不得逃出,由糖层 / pybind11 蹃床转错误码。
- **ABI 版本**:`LMFLOW_ABI_VERSION` + `lmflow_abi_version()`;`lmflow_graph_new` 内部校验,
  不匹配返回 NULL 并置错误。动态链接时 header 与 `.so` 不一致会导致布局错乱。
- **布局一致性**:`cpp/abi_assert.cc` 的 `static_assert` 与 `core/tests/abi_layout.rs`
  钉在同一组常量上,任一侧改字段而忘同步 → **构建失败**(已实测能拦住)。
- **ABI 演进**:`LMFlowBuffer` 留了 `reserved` 供未来加字段(最可能是 GPU 内存空间);
  一旦改变既有布局,必须提升 `LMFLOW_ABI_VERSION`,所有既有二进制都要重编。

---

## 10. 构建与集成

**Rust/C++ 部分:cargo 主导**

- `lmflow`(`lmflow/core/`):`crate-type = ["lib", "staticlib", "cdylib"]`。
  `lib` 给仓库内 Rust host/测试(不过 FFI);`staticlib`/`cdylib` 给外部宿主。
- `build.rs` **默认什么都不做**(纯 Rust);开 `builtin-kernels` 才用 `cc` 编 `../cpp/*.cc`
  (算子 + ABI 断言)并链入。build.rs 看不到 `#[cfg(feature)]`,故读 cargo 注入的
  `CARGO_FEATURE_BUILTIN_KERNELS` 环境变量;`../cpp` 不存在(= 从 crates.io 装的)时给出
  明确报错而非一堆找不到文件。仓库内 CMake / Python / 移动端 / CI 都显式带该 feature。
- 示例宿主:`cargo run --manifest-path lmflow/examples/rust/hello_world/Cargo.toml`。
- C ABI 的验证:Rust 集成测试直接 `unsafe` 调 `extern "C"` 函数,无需 C 语言 `main`。

**Python 部分:破例引入 CMake**(ADR #4)

- `pyproject.toml`(scikit-build-core)→ CMake 编 pybind11 模块 → 链 `liblmflow`。
- 两步:`cargo build` 出库 → `pip install -e .` 编扩展。
- 外部 C++ 宿主:自带构建系统,链 `liblmflow` + `include/`,不进本仓库构建。

**依赖**:`serde`/`serde_yaml`/`serde_json`;构建期 `cc`;Python 侧 `pybind11`。
`crossbeam-channel`、`parking_lot` 在 M3 引入。

---

## 11. 目录结构

```text
lm-flow/                          仓库根
├── lmflow/                       第一方源码
│   ├── core/                     引擎 crate `lmflow`(包名=库名=lmflow → liblmflow.a)
│   │   │                         **默认纯 Rust**:不编译也不捆绑任何 C++
│   │   ├── build.rs              可选地用 cc 编 ../cpp(仅 builtin-kernels feature,默认关)
│   │   ├── Cargo.toml · Cargo.lock
│   │   ├── src/                  timestamp / packet / edge / node / graph / scheduler / ffi /
│   │   │                         kernel_api(Rust 算子糖)/ builtin(自带默认 Rust 算子)…
│   │   ├── tests/                abi_layout.rs · rust_kernel.rs(纯 Rust,两种配置都跑)
│   │   │                         其余 11 个集成测试带 #![cfg(feature = "builtin-kernels")]
│   │   └── benches/              throughput.rs(Criterion,required-features)
│   ├── include/                  公共头(消费者 #include "lmflow/xxx.h")
│   │   └── lmflow/
│   │       ├── flow.h            C ABI —— 唯一稳定接口(权威定义)
│   │       ├── flow.hpp          C++ 算子糖层(header-only,非 ABI)
│   │       ├── flow_cv.hpp       可选:LMFlowBuffer ↔ cv::Mat(仅需 OpenCV 者 include)
│   │       └── flow_platform_log.hpp  可选:引擎日志接平台日志(logcat/os_log/HiLog)
│   ├── cpp/                      C++ 侧(**非引擎**,且在 crate 之外 → 不随发布的 crate 分发)
│   │   ├── kernels/              18 个内置 C++ 算子(一文件一算子 + register.cc 显式聚合)
│   │   ├── abi_assert.cc         跨界结构体布局的编译期校验
│   │   └── tests/                flow_hpp_test.cc / flow_cv_test.cc + CMakeLists
│   ├── python/                   src/bindings.cc(pybind11)+ lmflow 包 + CMakeLists
│   └── examples/                 examples/<lang>/<name>/:cpp · python · rust · android · ios · harmonyos
├── third_party/pybind11/         vendored 子模块(仅构建 Python wheel 用)
├── cmake/                        engine.cmake · install-sdk.cmake · find_package 配置
├── CMakeLists.txt                顶层构建(驱动 cargo;C/C++ SDK + Python 扩展)
├── pyproject.toml                Python wheel(scikit-build-core → 同一份 CMake)
└── docs/design.md                本文档
```

---

## 12. 里程碑

| 里程碑 | 内容 | 验收 |
|---|---|---|
| **M0** ✅ | C ABI(123 个函数,**接口层已收尾**)+ C++ 糖层 + 10 个算子 + ABI 断言 + 三语言宿主示例 + `timestamp.rs` | 全部编译验证通过;故意破坏布局能被拦住;参数/日志/错误/关闭原因实跑验证 |
| **M1** ✅ | crate 骨架 + `ffi.rs` 全量实现 + `build.rs` + `abi_layout.rs` | `cargo build` / `cargo test` 通过 |
| **M2** ✅ | 主线程执行器 + 注册表 + YAML + Context 读写 + poller/observer | `hello_world` 输出 0..9(Rust 与 C++ 宿主各一份) |
| **M4** ✅ | 关流传播 + `wait_done` / `wait_until_idle` / `cancel` + 生命周期状态机 | 干净退出;所有权记账证明零泄漏 |
| **M5** ✅ | 契约类型校验 + 图校验 7 项 + 错误路径 + `catch_unwind` + 全局水位 | 坏配置/坏算子被拒并给出可读原因 |
| **M3** | 线程池 executor(非默认执行器)+ 并发调度状态机 | 多线程下仍正确;TSan 干净 |
| **M6** | pybind11 绑定 + Python 算子 + 零拷贝 buffer + GIL 处理 | `examples/python/*` 跑通 |
| **A 阶段** | 时间戳同步、bound 传播、back-edge、并行 in-flight、输入策略 | 单列 |

> M3 之所以排在 M4/M5 之后:默认执行器是**宿主主线程**(ADR #16),所以 M2 的
> 同步执行本就是产品的默认行为,而不是临时脚手架 —— 线程池是「显式 opt-in」的增量。

---

## 13. 测试策略(已落地 255 个:Rust 217 + Python 38)

| 测试文件 | 数量 | 覆盖 |
|---|---|---|
| 各模块 `#[cfg(test)]` | 56 | timestamp 哨兵/边界、Packet 三态与 CoW、YAML 校验、端口表(tag/序号/连续性)、错误优先级、全局水位、字符串驻留 |
| `tests/abi_layout.rs` | 10 | 跨界结构体 size/align/offset、状态码、type_id、dtype、时间戳哨兵 —— 与 `cpp/abi_assert.cc` 钉在同一组常量上 |
| `tests/c_abi.rs` | 12 | **完全以 C 调用方的方式**驱动引擎:全流程、内建类型往返、缓冲分配与 CoW、空指针不崩、错误可读、observer、日志回调 |
| `tests/e2e.rs` | 28 | 真实建图 + 真实调 C++ 算子:直通/扇出/多 poller、7 项图校验、状态机、时间戳单调性、跨语言按类型传值、兜底关流、side packet |
| `tests/memory.rs` | 7 | 所有权守恒记账(正常/积压/失败/取消路径)、**CoW 零拷贝不变量**(三级管线)、扇出复制不污染兄弟分支 |

### 13.1 原始策略与补充

- **Rust 单测**:timestamp 边界(已有 9 个)、edge FIFO、就绪判定、YAML 解析、注册表。
- **ABI 布局**:`abi_layout.rs` + `abi_assert.cc` 双向钉死(已实测能拦住破坏)。
- **C ABI 冒烟**:Rust 集成测试直接调 `extern "C"`,覆盖边界①。
- **端到端**:`hello_world` 输出序列断言(0..9,时间戳对应)。
- **并发**(本设计的核心风险):**TSan 是硬门禁 —— 164 个测试 0 条竞态报告**
  (`RUSTFLAGS=-Zsanitizer=thread cargo +nightly test -Zbuild-std --lib --tests`;
  排除 doctest 是因 `-Zbuild-std` 下 rustdoc 会因 sanitizer ABI 不一致而编不过,属工具链限制)。
  覆盖 `max_in_flight` 并行路径:乱序完成仍按序、并行下取消/销毁不漏释放、多输入对齐叠加并行。
  Miri 尚未跑(FFI 大量 `extern "C"` 与外部 C++ 符号,Miri 无法执行)。
- **死锁回归**:专门构造扇出汇合(diamond)拓扑 + 慢分支,验证 §7.5 策略生效。
- **CI**(`.github/workflows/ci.yml`):headers(纯 C/C++ `-Werror`)、rust(fmt/clippy `-D warnings`/test/示例输出比对)、
  **external-host**(header 声明与 `.a` 导出符号对齐 + 编译运行外部 C++ 宿主)、
  **tsan**(硬门禁)、sanitizers-extra(ASan/Miri,暂不门禁)、python(真编扩展 + 跑测 + 示例输出比对)。
- 跨平台矩阵(见 §14)尚未加入。

### 13.2 实现阶段真实抓到的缺陷

这些不是假想 —— 都是 MVP 过程中被测试或工具抓出来并修掉的,已各自留下回归测试:

| 缺陷 | 抓到它的手段 | 教训 |
|---|---|---|
| `pump_step` 自锁死 —— edition 2021 里 `if let` 的临时 `MutexGuard` 活到块结束,`run_node` 内再锁同一队列 | 跑 hello_world 直接挂住 | 正是 R2 锁序规则要防的情形,却被语言的临时值规则绕过;`.lock()` 结果必须先落地到局部变量 |
| **输入槽残留引用** —— 只在下次调用开头才清,导致上游一直持着已处理完的包 | `tests/memory.rs` 的所有权记账(恰好漏 1 个) | 后果不止延迟释放:下游 CoW 永远看到引用数 ≥ 2 而**静默退化成全量拷贝** —— 正是 §3.4 警告的那条不变量失效 |
| CoW 测试用单节点管线,覆盖不到上一条 | 修复后回看测试设计 | 不变量测试必须覆盖「最短能触发的拓扑」而非最简拓扑 |
| `KernelInstance::open/process/close` 是 `pub` 且解引用裸指针却未标 `unsafe` | clippy `not_unsafe_ptr_arg_deref` | |
| `Context` 用 `Mutex` 时,持 guard 的 `&mut` 与回调内从裸指针再造的 `&mut` 构成别名 UB | 手工审查 | 改用 `UnsafeCell` + 令牌不变量 |
| `lmflow_register_builtin_kernels` 在库里但漏出 header;C++ 示例忘了调它(一跑就「算子未注册」) | `nm` 对比 header 声明与库导出符号 | 已固化为 CI 的 external-host 门禁 |
| 「算子未注册」的报错分两处,其中一处没列出可用算子 | e2e 测试断言报错内容 | 报错要能指导用户,不只是标明失败 |
| `SinkKernel` 用 `printf` 抢 stdout,违反自己文档里的规定 | 测试输出里看到 | 内置算子也要遵守对用户提的要求 |
| 必需 side packet 借用计数器承载,会把内部键暴露到用户可见的 counter 列表 | 自我审查 | |
| **丢唤醒** —— `wait_for_activity` 在判空闲之后才捕获活动代数,任务恰在此间隙全部完成就白等到轮询上限(`max_in_flight` 真并行测试里表现为 55ms 假慢) | 并行测试的耗时断言 | 等待前必须先捕获代数,再 `wait_activity_since`;这是「捕获-判定-等待」三步的经典次序 |
| **阻塞 `send` 在线程池图上误报 `WOULD_BLOCK`** —— 它只 `pump_step()`,而那只跑主线程任务;池图上恒为 false,一撞水位就报错而非背压 | 自我发掘 + `blocking_send_applies_backpressure_on_pool…` 回归测试 | 背压等待要区分「本线程能不能推」与「别的执行器还在不在跑」:池仍在跑就等排水,全图空转才算真卡死 |
| **句柄悬空(use-after-free)** —— `LMFlowInput*/LMFlowPoller*` 原本由图槽持有、随 `lmflow_graph_free` 一起释放;Python/C++ 宿主先销毁图、后用句柄(如 `del g` 后 `inp.send`)就读已释放内存(实测挂死) | 自我发掘 + Python `del g` 复现 + `handles_stay_safe_after_graph_free` 回归 | 改为**调用方拥有**句柄:各自持一份 `Arc<GraphInner>`,`lmflow_input_free/lmflow_poller_free` 释放;图先 free 也不失效,再用只得「已结束」错误。原先的 `alive_` shared_ptr 兜底是坏的(no-op deleter,且 Python GC 看不见 C++ 引用) |
| **C++ 算子构造抛异常穿越 FFI** —— `flow.hpp` 的 `KernelAdapter::create` 直接 `new T()`,若 T 构造函数抛异常(如打开设备失败),C++ 异常会穿越 `extern "C"` 回到 Rust(UB;catch_unwind 接不住);且引擎把 create 返回的 null 当「无状态算子」,后续 `process` 会对 null self 解引用 | 自我发掘 + `cpp/flow_hpp_test.cc`(有 bug 版实测 abort/段错误) | create 包 try/catch 失败返回 null;open/process/close 加 self 空判返回错误 —— 与 Python 端 `py_create`/`py_invoke` 的处理对齐 |
| **非连续 numpy 数组静默损坏** —— `lmflow_packet_from_buffer` 号称「逐行拷贝(源可能不连续)」,实则只认倒数第二维的 stride、假定最后一维连续,于是转置 / 步长切片 / 负步长 / 3D 非连续视图全拷错 | 自我发掘 + `a.T`/`a[:,::2]` 端到端实测输出错乱 | 改为按**完整 strides** 的 N 维里程表拷贝:最后一维连续则整行拷、否则逐元素;负步长用 `.offset()` 处理。回归:`c_abi.rs from_buffer_handles_non_contiguous_strides` + Python `test_non_contiguous_ndarray_roundtrip` |

### 13.3 原策略清单

---

## 14. 风险登记

| 风险 | 说明 | 缓解 |
|---|---|---|
| payload 跨线程析构 | 在 A 线程建、B 线程析构 | R3 保证访问串行;`Send` 断言写明前提 |
| panic / 异常穿越 FFI | 双向 UB | 双向 `catch_unwind` / `try-catch` 转错误码 |
| 静态注册被链接器裁剪 | 注册对象无引用被 strip | 内置算子用显式聚合注册;必要时 `--whole-archive` |
| 静态库链两份 → 双注册表 | 算子在一份里注册、另一份看不见 | Python 侧强制用动态库;符号可见性只导出 `flow_*` |
| ABI 布局不一致 | 内存错乱 | `#[repr(C)]` + 双向 `static_assert`;`LMFLOW_ABI_VERSION` 运行期校验 |
| CoW 静默失效 | 引擎多留一份引用 → 恒复制、不报错只变慢 | 写成显式不变量(§3.4)+ 加断言/测试 |
| GIL 拖累吞吐 | Python 算子无法真并行 | 文档明示;重活写 C++;考虑 executor 隔离 |
| 跨平台未验证 | Windows(MSVC)/macOS/交叉编译(ARM) | 列入 CI 矩阵 |
| B 的简化偏离完整语义 | 丢了时间戳对齐 | 明确划入 A 阶段;B 只跑单口/透传场景 |

---

## 15. 待决项的处置

上一版列出的 7 项未决事项已全部拍板,理由并入 ADR(§1):

| 原未决项 | 结论 |
|---|---|
| 时间戳单调性校验放在哪层 | 图输入口强制校验;内部边仅 `debug_assertions` 下校验(ADR #23) |
| 内部边软水位的默认值与告警形式 | 软水位默认取顶层 `max_queue_size`(默认 100);每条边**首次**超限打 WARN,之后按 1/2/4/8… 指数退避,避免日志洪水;深度与丢弃数经 `queue_depth` / `dropped_count` 可查 |
| `type_id` 是否改稳定方案 | **现在就改**为 FNV-1a(修饰名)+ `LMFLOW_DECLARE_TYPE_NAME` 逃生口(ADR #22) |
| 生产可观测性 | 已落地:`LMFlowNodeStats`(running / running_for_us / 耗时统计 / **收发包数 / 队列深度峰值**)、DOT 热力图、`watchdog_ms`、算子自报计数器 |
| 是否启用 `LMFLOW_TYPE_HOST_OBJECT` | **不启用**(ADR #26) |
| stream header | **不做**,用 side packet 覆盖(ADR #24) |
| subgraph 组合 | **已支持**:建图期展开 + `include:` 引外部库(ADR #27,§7.11) |

另外明确**不做**的:程序化构图 API(ADR #25)、observer 注册后的移除(登记于 header)。

### 15.1 结构体前向兼容的两种做法(有意的不一致)

| 结构体 | 做法 | 为什么 |
|---|---|---|
| `LMFlowBuffer` | 固定 `reserved` 字段 | 在**热路径**上、形状稳定(对齐 numpy buffer protocol),固定布局便于零开销传递 |
| `LMFlowNodeStats` | 入参 `struct_size` | **诊断用**、字段天然会持续增加;调用方填 `sizeof`。引擎写出完整结构体,故 `struct_size` 偏小时**明确失败**(溢出护栏)—— 字段增加后老宿主重编即可,拿到的是干净报错而非内存损坏 |

### 15.2 实现阶段的验证结果

| 当初留给实现去验证的 | 结果 |
|---|---|
| CoW 不变量是否被意外破坏 —— 需专门的「不应发生拷贝」测试 | **确实被破坏了**,已修并留下三级管线的零拷贝测试(见 §13.2) |
| 调度状态机的 `rescan` 逻辑 | 单线程下正确;**并发正确性未验证**(M3 才有线程池,届时须过 TSan) |
| 全局水位的实际效果 | 已有测试证明能拦住无限增长;真实内存曲线待 M3 压测 |
| 跨平台 | **仍未验证**:Windows(MSVC)/ macOS / 交叉编译(ARM) |

### 15.3 当前实现的已知边界

诚实记录,避免误以为已完成:

- **只有主线程执行器**。YAML 里的 `executors:` 能被解析和校验,但线程池尚未实现,
  所有节点都跑在宿主主线程(这正是默认行为,见 ADR #16 与 §7.9)。
- **无时间戳同步**(阶段 A):多输入口节点的就绪条件是「每口至少一个包」,不做跨口对齐。
- `lmflow_graph_pause/resume`、`observe_timestamp_bounds`、`lmflow_register_type_name` 尚未实现,
  调用会得到明确的「尚未实现」而不是静默无效。
- 带超时的 `wait_done_timeout` / `poller_next_timeout` 在主线程执行器下等价于不带超时版本
  (没有「等别的线程」的情形);真正的超时语义随线程池一起落地。
- `Packet::new`(Rust 原生值)的 `type_id` 是 `NONE`,**不参与跨语言类型校验**;
  要让 C++/Python 算子按类型读取,须用 `Packet::new_interop` + `fnv1a_type_id`,或内建类型。
