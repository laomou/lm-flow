# lmflow 设计方案

> 状态:**成品**。Rust 引擎、C ABI、C++ 糖层(含 OpenCV 互转)、18 个内置算子、
> Python 绑定(pybind11)、原生 SDK 发布(各平台头文件+库)、三端文档站全部就位;
> **308 个测试**(Rust 265 + soak 2 + doctest 3 + Python 38)全绿,TSan 硬门禁 0 竞态。Rust / C++ / Python 三种宿主的
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

**批处理(`batch` 输入策略)已支持**(见 §7.10):攒够 `capacity` 个包一次交给算子,`process()` 用 `input_count` / `input_at` 读整批,关流刷余批。用于批推理 / 窗口聚合。**多输入口按时间戳对齐**(与 `sync` 同源,见 §7.10)。内置 `BatchSumKernel` 为样板。

**零输入口 source 节点(生成型算子)已支持**(见 §7.4):内核自产数据、`source_done()` 自报产完;源须挂线程池执行器(否则会独占宿主主线程)。内置 `RangeSourceKernel` 为样板。

**子图(subgraph)+ 跨文件 `include` 已支持**(见 §7.11):纯**建图期**变换,把带 `subgraphs` / `node.type` 的配置展平成等价扁平图,运行时引擎 / 调度器不感知子图。

**反馈环(back-edge)已支持**(见 §7.12):把输入口标为 `back_edges` 即「最新值反馈寄存器」—— 容量 1、不参与就绪 / 终止 / 对齐;去掉 back-edge 后的图仍是 DAG。未被 back-edge 打断的拓扑环仍在 `init` 报错。

### 0.3 非目标

- 不做分布式 / 跨进程;单进程内多线程。
- 不做 GPU 内存空间(`LMFlowBuffer.device` 已预留字段,但本版本只有 CPU)。
- ~~不支持图跑完后重跑~~ → **已支持** `reset`(§7.13):保留算子实例复位重跑。

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
| 33 | **`notify_activity` 只在真有等待者时才 wake**(waiters 计数与 gen 同锁) | `notify_activity` 在 `dispatch` 里是**每包每条边**调一次,而 `Condvar::notify_all` **即使没有任何等待者也会走一次 futex 系统调用**。代数递增必须保留(那是防丢唤醒的本体),但 wake 可以跳过:notifier 持锁时读到 `waiters == 0`,则任何「正要等待」的线程都还没拿到那把锁 —— 它随后会看到递增后的 `gen` 而根本不进入等待,故不丢唤醒。实测每跳边际派发成本 **763 → 279 ns(-63%)**;线程池模式只降 5~7%,因为那时宿主确实在等、那些 wake 是必要的 —— 只省掉了无用的那些 |
| 34 | **每次回调的计时可关**(`stats_timing`,默认开);但 `watchdog_ms > 0` 时**强制开** | 计时是每次 `process` **两次** `Instant::now()`(本机约 43 ns,占单跳派发约 15~18%)。实测同链 A/B:depth16 每包 4694 → 3861 ns(**-17.8%**)。关掉的代价是 `total_process_us`/`max_process_us`/`running_for_us` 恒 0、DOT 延迟热力图退化为单色 —— 属**显式取舍**,建图时打 INFO 说明,不静默。watchdog 依赖单次耗时,故与它冲突时强制开启并说明原因(静默失效是本项目明确拒绝的失败模式;有测试用一个睡 2ms 的算子钉住这条,去掉强制开启该测试即失败) |
| 35 | **`max_in_flight == 1`(默认)时跳过按序重排的 BTreeMap;`flush_staging` 不再为整批产出建临时 `Vec`** | perf 采样(非估算)显示:`pending_flush: BTreeMap` 的每次调用插入+删除、以及 `flush_staging` 的临时 `Vec<OutputBatch>`,连带 malloc/free 共占约 13.5%。而 `max_in_flight == 1` 时同一时刻只有一次调用在飞,`seq` 必然等于 `next_flush_seq` —— 重排缓冲纯属白做,可直接接手。`dispatch` 只读 packets,故签名改 `&[Packet]`:图输入口的 `send` 不再为单包 `vec![pkt]` 分配,`staging` 的缓冲清空后放回、容量复用(不再 `finish_grow`)。实测每跳边际 279 → **236 ns**;`remove_leaf_kv` / `SpecFromIterNested` / `finish_grow` 三项从 2.96% / 1.93% / 1.13% 归零,malloc/free 总量 13.5% → 7.5% |
| 36 | **`try_claim` 每个输入口只拿一次队列锁** | 原先每口把**同一把**队列锁拿 3 次:`front_ts`(读队首 ts)、`pop_front`、`queue_len`(算 `inputs_done`)。合成一个临界区。安全性来自 ADR #30:**只有 `try_claim` 会 pop 且全程持 `sched`**,别的线程只 push(追加尾部、不动队首),故队首稳定;`inputs_done` 那处也无差别 —— 它要求 `input_closed`,而关流后不再有 push、长度已稳定。实测每跳边际 236 → **213 ns**(pool1 -7%)。三个分支(正向口 / 反馈口 / 批处理)都合了。`readiness()` 里还有一次(它要跨口算就绪),没并 —— 要把观察值带出来会引入耦合、收益仅 ~20 ns,不值 |
| 37 | **节点级 `on_error: abort\|skip`**,默认 `abort` | 长跑实时管线里一帧坏数据不该杀掉整条流水线,而原先任何一次算子失败都终止全图。`skip` 只丢那一个包并**推进下游边界**(不推进就把一帧错误升级成整图卡死);复用「无产出也要推进边界」那条既有路径,不新写机制。有损行为绝不静默:计入 `errors` + WARN(指数退避)。**只有两个值**——不设单独的 `log`,因为 `skip` 本身一定计数并打日志。定位:能在算子内处理的就在算子内处理(返回成功不产出),`skip` 专治**管不到**的失败(契约校验、panic / C++ 异常、第三方算子)。**只管逐包失败**:`Open` / `Close` 的一次性生命周期失败不受此策略影响(打不开就该让 `start()` 失败,而非空转着每帧报错) |
| 38 | **声明式源定速 `rate: N`(Hz)** | 源本要么内核自己写 sleep、要么灌爆下游。`rate` 让定速变成一行 YAML。实现走**路 A**(源的池线程里 sleep 到点)而非路 B(不占线程的定时唤醒):source 本就必须挂线程池、有专属线程,路 B「不占线程」的收益对它有限,却要新造一整套延迟调度设施并碰调度核心。节流在 `call_kernel` 调算子前、**不持任何引擎锁**(R1 未破),按实际放行时刻记基准防漂移 |
| 39 | **`reset` 保留算子实例的复位重跑** | 每会话重建图 + 重跑 `open`(重载模型)是实打实的开销。reset 复用已 open 的算子跑下一轮。安全靠「静止相」:要求 `Terminated + is_idle`(与 Drop/start 同依据),故 `&self` + 内部可变即可无并发复位 —— **不用 `Arc::get_mut`**(Poller 也持 `Arc<GraphInner>`,宿主留着 poller 时拿不到独占)。不碰线程池(worker 随图存活、park 着复用)。`epoch` 不 reset(只是诊断基准)。最易漏:`Edge::last_sent`(单调性)、`GraphShared` 的 error(无现成清除路径)、`input_bounds` 回 `pre_stream` 而非 `done` |
| 40 | **F16 用自写的软件转换**,不用 `_Float16`、不用 F16C / NEON 内建 | F16 是移动端推理的标准张量 dtype,而张量前处理组此前遇 F16 直接报错 —— 在最相关的场景里用不了。选软件转换的理由:`_Float16` 不是所有目标编译器都有(**MSVC 没有可移植的 half 类型**,而 Windows 是待补平台),内建指令要按架构分派 + 运行期探测;而前处理不在最内层推理热路径上,这点成本换来「任意编译器 / 架构上逐位一致」是值得的 —— 且正因不依赖编译器,舍入行为才**能被测试钉死**。舍入取 IEEE 默认(就近、平局取偶);`double → half` **直接从 double 位模式做、不经 float 中转**,否则会双重舍入(极少数入参偏 1 ulp)。见 §5.3 |
| 41 | **`batch` 多输入口 = `capacity` 个「对齐元组」**,而非「各口各自数够 `capacity` 个」 | 后者实现最省事,但会把 0 号口的第 k 个与 1 号口的第 k 个配成一对,而它们未必是同一帧 —— 图像批与掩码批就此错位,**且不报任何错**。静默的错误配对是本项目明确拒绝的失败模式,故一批 = 把 `sync` 的对齐连续跑 `capacity` 轮,**各口取数允许不同**(`input_count(i)` 本就按口计数,算子侧零改动)。不足一批只在**所有正向口都关闭**后才刷(否则是过早切批)。实现上就绪期快照时间戳前缀 + 算好每口取数,认领期照计划弹出 —— **每口仍只拿一次队列锁**,ADR #36 未破。见 §7.10 |
| 42 | **类型契约做两级校验:静态可证的建图期拒绝,ANY 边保留运行期检查;算子输出也必须兑现契约** | producer output 与 consumer input 都声明具体类型且不同,无需等首包即可判错,故建图失败;任一侧为 ANY 时真实类型仍由包决定,继续逐包检查。输出契约不能只拿来推导下游:否则直接连 graph output 的错误包无人检查,故 process / close 的 staging 在 dispatch 前统一验证。Rust 自定义跨语言类型用 unsafe trait `InteropType` 把 ABI 承诺集中到实现处;任意 id 的 `new_interop` 降为 unsafe 且禁止伪装成内建类型 |
| 43 | **自定义类型身份从裸 `type_id` 收紧为 `(稳定名,size,align)` 描述符** | 仅比较 64 位哈希无法发现碰撞,也无法发现两侧用同一稳定名却声明了不同布局。`lmflow_register_type_descriptor` 对完全相同的重复注册幂等,但同 id 异名、同名异 id、同名同 id 异布局都立即失败。C++ `Packet::Make<T>` / `Contract::InputSet<T>` / `OutputSet<T>` 自动注册,Rust `InteropType` 自动注册。已注册固定布局的 Foreign payload 会按 `size` 纳入字节水位;这是对象本体的浅尺寸,不包含 `std::vector` 等对象内部另行分配的堆内存 |
| 22 | **`type_id` = FNV-1a(修饰名)**,而非 `typeid().hash_code()` | 后者实现定义、不保证跨动态库一致;而本项目 C++ 算子在 core、Python 绑定在另一 `.so`,天然跨产物。事后再改需全量重编,故一开始就用稳定方案 + `LMFLOW_DECLARE_TYPE_NAME` 逃生口。**注意修饰名跨编译器不保证相同**(GCC/Clang 走 Itanium ABI 一致,MSVC 不同),故跨工具链混用算子**必须**用逃生口显式声明稳定名。哈希算法在 C++ 与 Rust 各有一份独立实现,已用同一个字面量在两侧钉死(见 §13.5) |
| 23 | **时间戳单调性:图输入口强制校验,内部边仅 debug 构建校验** | 外部数据进入的唯一门校验一次即可挡住绝大多数乱序;内部边逐包校验是热路径开销,且算子产出乱序属算子 bug,用 `debug_assertions` 捕获即可 |
| 24 | **不做 stream header**,用 side packet 覆盖 | header 会引入「流上的第二种数据」及其生命周期问题;side packet 已能表达「整条流不变的属性」。少一个概念优于多一个 |
| 25 | **不做程序化构图 API**,动态图由宿主生成 YAML | 保持单一真相源;builder API 是一大片表面积,应由真实需求驱动而非预先设计 |
| 26 | **不启用 `LMFLOW_TYPE_HOST_OBJECT`**,且**明确拒绝**而非放任 | 见 ADR #9。原先「未启用」只靠「没有普通构造函数生产它」维持 —— 契约声明 7、C 侧手填 7 或 Rust unsafe `from_foreign` 仍可让数值相等检查放行。现契约、图输入、算子输出、side packet 四条入口都拒;报错给出 `BUFFER` / `STR`+JSON 替代方案。Rust 的 `new_interop` 也禁止使用内建保留区 0..15 |
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
| 类型描述符 | `lmflow_register_type_descriptor` `lmflow_type_name` `lmflow_type_size` `lmflow_type_align`;旧 `lmflow_register_type_name` 仅作诊断兼容 |
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
- **写算子时的断言宏**:`LMFLOW_RET_CHECK(cc, cond)` / `LMFLOW_RET_CHECK_MSG(cc, cond, msg)` ——
  不成立就带着**失败的表达式原文 + file:line** 返回算子失败。
  为什么需要它:C ABI 跨界只能过一个 `int32_t`(ADR #1),错误**文本**必须另经
  `lmflow_ctx_set_error` 存进 Context。也就是说「返回失败」与「说明原因」在本框架里是
  两件事,直接 `return Status::Error()` 会让引擎只拿到一个码、原因为空。这两个宏把它们
  绑成一个动作,让人**难以**漏掉文本。内置 `CastKernel` 是用法样板。
- **异常路径也带原因**:`KernelAdapter` 的 `catch` 会把 `std::exception::what()` 写进 Context
  (非 std 异常则给出固定说明)。此前异常这条路只剩一个错误码、诊断信息为零。
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
  统一走 double 做 dtype 分派,要求连续缓冲。**含 F16**:`buffer_util.hpp` 自带
  binary16 ↔ double 的软件转换(见 §5.3),故 `dtype: f16` 可直接作输入或输出。示例见
  `examples/python/preprocess/preprocess.py`(u8 图 → f32 → 归一化 → clamp)。

### 5.3 F16(binary16)转换

张量前处理组支持 `LMFLOW_DTYPE_F16`,转换由 `cpp/kernels/buffer_util.hpp` 里**自己实现的**
`half_to_float` / `f64_to_half` 承担。**刻意不用 `_Float16`,也不用 F16C / NEON 内建**:

- `_Float16` 不是所有目标编译器都有(MSVC 就没有可移植的 half 类型,而 Windows 是待补平台);
- 内建指令要按架构分派 + 运行期探测;
- 张量前处理不在最内层推理热路径上,这点转换成本换来「任意编译器 / 架构上行为**逐位一致**」
  是值得的 —— 而且正因为不依赖编译器,舍入行为才能被测试钉死。

舍入是**就近取整、平局取偶**(IEEE 默认),`double → half` 直接从 double 位模式做而
**不经 float 中转** —— 否则会双重舍入(两次各取整一次,极少数入参偏一个 ulp)。
上溢到 ±inf、下溢到非规格数 / ±0(保号)、inf/NaN 均按 IEEE 处理。

验证:`cpp/tests/buffer_util_test.cc` —— 期望值全是**硬编码的 IEEE 位模式**(不与任何编译器
内建类型对照,故在 MSVC / arm64 上同样有效),含全部 65536 个位模式的 `half → double → half`
往返、上溢/下溢临界点(65519 → 最大有限值 vs 65520 → inf)、以及相邻 half 的**精确中点**
(平局取偶最容易写错的地方)。CI 在 `-O0` 与 `-O2` 两档都跑(浮点位操作对优化敏感)。
开发时另用 GCC 的 `_Float16` 作 oracle 穷举对照过一次(65536 个 half→float + 46 万个
double→half,含全部相邻 half 中点),逐位一致 —— 但**该对照不进仓库**,因为它不可移植。

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
- **重跑已支持**(§7.13):`Terminated` 后可 `reset` 复位重跑(保留算子实例);或 `free` 释放。

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
- **定速**:两条路。① **内核自定速** —— `process` 里自行阻塞(等帧 / 读下一条);② **声明式 `rate: N`**(节点字段,Hz)—— 引擎保证相邻两次 `process` 至少隔 `1/N` 秒,算子不必写 sleep。实现:在源的池线程里 sleep 到点(该节点本就必须挂线程池,故不额外占线程);按「本次实际放行时刻」记基准,避免累积漂移。`rate` 只对源节点有效(非源由上游驱动,设了建图期报错),须为正的有限值(挡住 0 / 负 / NaN / inf)。定速也顺带缓解「非自定速的源灌爆下游」(内部边不背压,见 §7.5)。source **必须挂线程池执行器**(跑主线程会独占、拖垮全图;`config` 校验强制)。
- **完成**:内核调 `source_done()` → 引擎停止再触发本节点、关其输出边(复用关流级联,§7.6)→ 下游收流 → 图正常终止。无限源(永不 `source_done`)由 host `cancel()` 停。

内置 `RangeSourceKernel`(产 `0..count` 后 `source_done`)为样板。

### 7.5 背压策略

**只有图输入口是限流点;图内部的边不对生产者施加背压。**

| 机制 | 性质 | 满了怎样 |
|---|---|---|
| `max_queue_size`(默认 100) | **只是软水位,对所有边一视同仁** | 仅告警(指数退避)+ 可用 `queue_depth` 观测。**不阻塞任何人** |
| `max_queued_packets` / `max_queued_bytes`(全局水位,**默认 0 = 不限**) | 全图硬预算,但只在图输入口施压 | `send` 等排水,`try_send` 返回 `LMFLOW_ERR_WOULD_BLOCK` |
| `fixed_size` 输入策略(**按节点输入口**,非按边) | 有界 + **有意有损** | 丢最旧;计数 + 首次 WARN。不阻塞上游 |
| 有界 Poller(`capacity` + overflow policy) | 图输出订阅队列的局部上界 | `block` 无损等待宿主排水;`drop_oldest` / `drop_newest` / `latest` 有损并计数告警 |
| `input_queue_capacity` / `input_queue_capacities` | 内部输入口无损包数硬上界 | 满时生产者保留 staging、释放 worker;下游出队后协作式恢复 |
| `input_queue_byte_capacity` / `input_queue_byte_capacities` | 内部输入口可计量 payload 浅字节硬上界 | 队列字节 + pending staging reservation 一起判定;不可计量 payload 明确拒绝 |

> ⚠ **本表曾经是错的**,而且错在一条安全属性上:它写着「图输入口有界(`max_queue_size`),
> 满时 `send` 阻塞至有空位」。实际上 `max_queue_size` 在全引擎**只被 `warn_if_over_soft_limit`
> 读一次**,`send` 从不看它 —— 它对图输入口和内部边同样只是告警。真正让 `send` 等待的是
> **全局水位**,而它**默认为 0(不限)**。也就是说:**不显式配置 `max_queued_packets` /
> `max_queued_bytes` 时,普通内部队列没有任何硬上界,只有 depth 100 时的一条 WARN。
> (`flow.h` 的对应段落曾有同样的错误描述,已一并修正。)

**Poller 队列属于全局水位统计**:每个 Poller 是独立订阅者,因此同一包投给 N 个 Poller
会计 N 个队列槽(引用计数共享 payload,但每个订阅者都能独立滞留它)。默认
`add_poller` 保持历史兼容——无容量限制;需要严格约束输出滞留时用 bounded Poller:

- `block`:满时阻塞产出该边的线程,直到宿主取走一包。无损,但宿主必须从另一线程并发排水;
- `drop_oldest`:丢最旧,保留最近 `capacity` 包;
- `drop_newest`:保留已有积压,拒绝新包;
- `latest`:容量固定为 1,永远只保留最新值。

有损策略都有独立 `dropped_count` 与指数退避 WARN。Poller pop、overflow 丢弃、reset
清空均同步扣减全局 packet/byte 计数。已注册 type descriptor 的自定义 Foreign payload
按固定对象 `size` 计量;这是浅尺寸,不包含 `std::vector` 等对象内部堆分配。

**为什么内部边不能直接阻塞 worker** —— 否则「扇出后再汇合」的合法 DAG 会死锁:

```
        ┌─► B(慢) ─┐
   A ──►┤           ├──► D
        └─► C(快) ─┘
```
C 迅速填满 D 的输入队列而阻塞;D 却要等 B 那一路才能消费;B 又在等 A 推进;
而 A 已阻塞在 C 上 —— **循环等待,且不需要环形拓扑就会发生**。

`input_queue_capacity` 因此不在 `dispatch` 里等待。当前调用完成后若下游容量不足,
其 context 槽与 staging 保留在节点调度状态里,worker 立即返回线程池;该节点暂停认领
新输入。任一下游从输入队列弹包后重试 pending flush。限制的是**节点继续产出**,
不是占住线程等待,diamond 不形成线程循环等待。

`input_queue_capacity` 是节点所有正向输入口的默认包数上限;
`input_queue_capacities: {video: 2, metadata: 32}` 按端口名覆盖,覆盖值 `0` 表示该口不限。
字节限制同理:`input_queue_byte_capacity` 是默认值,
`input_queue_byte_capacities` 按端口覆盖。四个字段都以 `0` 表示不限(历史行为)。

字节计量使用 `Packet::byte_size()` 的 payload **浅尺寸**。队列内已有字节和并发生产者
已经预留、仍留在 staging 的字节一起参与容量判定,因此不会因 flush 尚未真正入队而超限。
内建 payload 与已注册固定布局 descriptor 的 Foreign payload 可计量;Rust `Packet::new`
等不可计量的非空 payload 在字节硬限端口上会明确报错,不能按 0 字节绕过限制。它仍不包含
`std::vector` 等自定义对象内部另行分配的堆内存,也不试图按共享 `Arc` 的唯一物理内存去重。

这些无损容量与 `fixed_size` 互斥:前者暂停生产者,后者有损丢最旧。一次调用向同一输出口
emit 的批量若自身大于包数或字节容量会明确报错,避免永远无法满足的等待。若时间戳对齐
导致下游永远不能消费、所有 worker 又已空闲,等待接口会报告
`internal backpressure cannot make progress`,而不是永久挂起。

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

节点级策略 `on_error`(YAML 节点字段,默认 `abort`;未知值建图期明确报错,不静默):

**`abort`(默认,历史行为)**
- 算子返回非 0 → `record_error` → 置 `AtomicBool has_error`(快路径)+ 存首个错误(含文本)。
- **失败时丢弃该次的 staging**,不传播半成品输出。
- 此后 `try_claim` 一律返回 false(停止调度)、`send` 返回错误、所有 poller 被唤醒返回 false、
  `wait_done` 返回该错误码,`lmflow_graph_last_error` 可取文本。

**`skip`(长跑实时管线用)**
- 丢掉**出错的那一个包**,不置 `has_error`,其余包照常流过,图能正常终止。
- **必须推进下游时间戳边界**,否则下游会永远等这一刻 —— 那等于把「一帧出错」升级成
  「整图卡死」,比 abort 还糟。实现上不新写机制:清空 staging 后**照常走刷新路径**,
  于是落到「无产出」分支自动 `propagate_bound(input_ts + 1)`,与 `Filter` 丢包时同一条路。
- 有损行为**绝不静默**:计入 `LMFlowNodeStats.errors`,并打 WARN(指数退避,避免每帧都错
  时刷爆日志 —— 与边的 `note_dropped` 同法)。

**`on_error` 到底管哪些失败**(容易误解,写清)。它只作用于 **Process 路径**,两个触发点:

1. **算子还没跑就失败** —— 输入包的 `type_id` 与 `GetContract` 声明不符,引擎侧
   `check_input_types` 拦下,`process` 根本没被调用。
2. **`process` 返回非 0** —— 这一个条件涵盖:Rust 算子返回 `Err`、Rust 算子 **panic**
   (`catch_unwind` → `PANIC`)、C++ 算子返回失败 `Status`、C++ 算子**抛异常**
   (`catch (...)` → `KERNEL`)、以及 `create` 失败(构造 panic / 抛异常 → `self` 为 null)。
   所以「算子失败」不只是「返回失败码」,**panic / 异常也算**。

**不受 `on_error` 影响**:

| 阶段 | 行为 | 为什么不适用 |
|---|---|---|
| `Open` 失败 | 一律 `record_error`,**`start()` 直接返回错误** | 算子连打开都没成功(如模型加载失败),它一个包也处理不了 —— `skip` 等于让图空转着每帧报错。这类**一次性生命周期失败**应当场让 `start()` 失败 |
| `Close` 失败 | 一律 `record_error` | 那时已在关流,没有「下一个包」可跳 |

一句话:`on_error` 管的是**逐包**失败(这一帧坏、下一帧可能好),不管**一次性的生命周期失败**
(打不开就是打不开)。

> **什么时候用哪个。** 能在算子内处理的错误,**就在算子内处理** —— 捕获后返回成功但
> 不产出即可,引擎会自动推进边界(与上面 `skip` 走的是同一条路)。这样你能按错误类型
> 精确决定哪些可恢复。`on_error: skip` 是给你**管不到**的失败用的:
> 引擎侧的契约类型校验失败、以及算子的 **panic / C++ 异常**(那些没法「返回成功」);
> 以及你不拥有源码的第三方算子 —— 用 YAML 就能加固,不必改算子。

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
| `batch` | **批处理**:攒够 `capacity` 个**对齐元组**一次交给算子(`process()` 用 `input_count`/`input_at` 读整批),关流刷余批。多输入口按时间戳对齐 | A |

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
末包(下游单调)。用于批推理、窗口聚合;内置 `BatchSumKernel` 为样板。

**多输入口:一批 = `capacity` 个「对齐元组」**(ADR #41)。对齐规则与 `sync` 完全同源 ——
把单包时的那一轮对齐**连续跑 `capacity` 轮**,每轮取各口游标处的全局最小时间戳。因此
**各口取数可以不同**:某口在某个对齐时间戳上没有包,该轮就不取它(`input_count(i)` 本就是
按口计数的,算子侧无需改动)。

刻意**不**做成「各口各自数够 `capacity` 个」:那会把 0 号口的第 k 个与 1 号口的第 k 个配成
一对,而它们未必是同一帧 —— 图像批与掩码批就此错位,且**不报任何错**。静默的错误配对是
本项目明确拒绝的失败模式。

不足一批时**只有所有正向口都已关闭**才刷余量(不可能再来数据了);只要还有口开着就继续等 ——
提前交付就是过早切批,同样是静默的语义偏差。反馈口(`back_edges`)不参与对齐,每次触发读一次
最新值,与其它策略一致(多口 batch 才让 `batch` + `back_edges` 成为可能:单口时凑不出
「至少一个正向口 + 一个反馈口」)。

实现上,就绪判定期已把各口时间戳前缀快照出来并算好每口取数,认领期照计划批量弹出 ——
**每口仍只拿一次队列锁**,保住 ADR #36。

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

### 7.13 重跑(reset)—— 保留算子实例的复位

`Terminated` 后 `reset` 可把图复位为可再次 `start`,**保留已 open 的算子实例** —— 省掉每会话重建图 + 重跑 `open`(如重新加载模型)的开销。处理完视频 A 再处理 B,不必重建整张图。

**前提**:图须 `Terminated` 且静止(`is_idle`,没有 worker 还在算子里),否则返回 `Error::State`。宿主通常先 `wait_done()`。这条静止依据与 `Drop` / `start` 用的是同一个 —— `in_flight == 0` 且 `main_queue` 空 ⇒ 没有 worker 在 `run_node` 中途,故复位无并发。

**不碰线程池**:worker 随图存活、静止时都 park 在 condvar 上、`stop` 仍为 false,下一轮 `start` 直接复用;shutdown + join 只发生在 `Drop`。这也意味着 reset 不付重建线程的代价。

字段分三类:

| 类 | 例 | reset 动作 |
|---|---|---|
| 构建期常量 | 拓扑 / 端口表 / **算子实例** / executor / `on_error` / `min_period` | **不动** |
| 运行期状态 | 队列 / 统计 / `next_seq` / `input_bounds` / `source_done` / `last_fire` / `closed` / `has_error` | **复位** |
| 刻意保留 | `opened`(算子不重建)· `side_packets`(宿主注入的常量,不必重灌)· poller / observer(宿主复用同一句柄取下一轮输出) | 保留 |

**最易漏的三个**(都有专门测试钉住):

1. `Edge::last_sent` 必须回 `unset()` —— 否则单调性校验会拒掉下一轮从图输入口发的第一个包(时间戳通常又从小开始)。
2. `GraphShared::{error, has_error, cancelled}` —— `record_error` 只「首因生效、不覆盖」,**没有反向清除路径**;不清则 reset 后的图带着旧错误出生,`start` 的 `try_claim` 立刻被挡回。为此新增 `reset_run_state()`。
3. `input_bounds` 必须回 `pre_stream()`(**不是**上一轮 `close` 推到的 `done()`)—— 否则 `readiness` / 对齐会认为每个空口「已到流尾」,语义崩坏。

`opened` 的保留由 `start` 侧配合:`start` 见 `opened == true` 就**跳过 `open`**(只重灌 side packet + 复位槽)。这正是「不重跑 open」= 不重载模型的价值所在。

C ABI:`lmflow_graph_reset`;Python:`Graph.reset()`。四层同一语义。

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
└── docs/
    ├── design.md                 本文档
    └── web/                      文档站源码(见 §11.1)
```

### 11.1 文档站

`https://laomou.github.io/lm-flow/`,由 `.github/workflows/docs.yml` 在每次推 main 时构建部署。
一个站点覆盖三端,**三端平等**:

| 路径 | 内容 | 生成方式 |
|---|---|---|
| `/` | 手写英文首页 | `docs/web/index.md` → `build.py` |
| `/rust/` | Rust API(跟 main) | `cargo doc --no-deps`(默认纯 Rust feature) |
| `/cpp/` | 手写英文 C/C++ 指南 | `docs/web/cpp.md` → `build.py` |
| `/python/` | Python API | `pdoc`(内省已安装的包) |
| `/design/` | 本文档(中文) | `docs/design.md` → `build.py` |

三条关键取舍:

1. **C/C++ 端手写,不上 Doxygen。** `flow.h` 的 131 个函数声明**没有一个**前置 Doxygen 可识别
   注释(全是普通 `/* */`),且大量说明是「章节横幅」而非贴着符号(一条注释同时说明 5 个
   `lmflow_packet_as_*`)—— 换标记也绑不到符号上,Doxygen 只会产出空壳签名索引。手写指南是
   唯一能真正解释 ABI 契约(所有权三态、指针生命周期、锁规则)的形式。
2. **doc 注释语言按「谁会读」划分**,而非按语言划分:面向用户的入口(crate 首页、`Graph` /
   `Packet` / `Kernel` / `KernelCtx` / `KernelContract` / `register_kernel` / `Timestamp` /
   `Contract`、`builtin` 与 `ffi` 模块头、Python docstring)用**英文**;引擎内部的实现注释与
   不变量论证仍用**中文**。
3. **零新依赖。** `build.py` 只用 `markdown2` + `pygments`,而这两个都是 `pdoc` 的依赖 ——
   workflow 本来就要 `pip install pdoc`,故依赖清单一行未增。刻意不引 MkDocs / Sphinx:
   为两个手写页面搭一套静态站生成器,还要让它接管整个 `site/`(把 rustdoc / pdoc 产出当
   "静态资源"塞进去),是纯粹的摩擦。

`cargo doc` 的产出根目录**没有 `index.html`**(真入口是 `lmflow/index.html`),故 `build.py`
额外写一个 `site/rust/index.html` 重定向壳,rustdoc 拷贝步骤用 `cp -R .../doc/. site/rust/`
只合并内容、不覆盖它。workflow 末尾有一步产物齐全校验 —— 宁可构建失败,也不发半成品站点。

`cargo test --doc` 此前**验证不了任何东西**(4 个围栏块全是 ```ignore / ```text);现在 crate
首页与 `kernel_api` 模块头的 3 个示例都是可运行 doctest,`RUSTDOCFLAGS: -D warnings` 也一并
把失效的 intra-doc 链接变成硬错误。

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

## 13. 测试策略(已落地 308 个:Rust 265 + soak 2 + doctest 3 + Python 38;另有 3 个独立 C++ 测试)

| 测试文件 | 数量 | 覆盖 |
|---|---|---|
| 各模块 `#[cfg(test)]` | 56 | timestamp 哨兵/边界、Packet 三态与 CoW、YAML 校验、端口表(tag/序号/连续性)、错误优先级、全局水位、字符串驻留 |
| `tests/abi_layout.rs` | 10 | 跨界结构体 size/align/offset、状态码、type_id、dtype、时间戳哨兵 —— 与 `cpp/abi_assert.cc` 钉在同一组常量上 |
| `tests/c_abi.rs` | 12 | **完全以 C 调用方的方式**驱动引擎:全流程、内建类型往返、缓冲分配与 CoW、空指针不崩、错误可读、observer、日志回调 |
| `tests/e2e.rs` | 28 | 真实建图 + 真实调 C++ 算子:直通/扇出/多 poller、7 项图校验、状态机、时间戳单调性、跨语言按类型传值、兜底关流、side packet |
| `tests/type_contracts.rs` | 5 | 类型契约两级校验:具体类型静态拒绝、ANY 两向兼容、运行期动态检查、算子输出兑现契约 |
| `tests/memory.rs` | 7 | 所有权守恒记账(正常/积压/失败/取消路径)、**CoW 零拷贝不变量**(三级管线)、扇出复制不污染兄弟分支 |
| `tests/buffer_ops.rs` | 11 | 张量前处理组端到端:Cast / Affine / Clamp / Reduce、真实前处理链、**F16 输入与输出**(含 u8→f32→归一化→f16 的移动端链路)、非连续缓冲与未知 dtype 被拒 |
| `cpp/tests/` (C++) | 3 | `flow_hpp_test`(异常不穿越 FFI + 构造失败安全)· `buffer_util_test`(**F16 软件转换**:65536 个位模式往返 + 平局取偶 + 上下溢临界,`-O0`/`-O2` 双档)· `flow_cv_test`(装了 OpenCV 才编) |

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
- **死锁回归 / diamond 内存曲线**:`tests/soak.rs::watermark_bounds_memory_in_diamond_topology`
  —— 一条图输入边被两个节点消费(真正的内部扇出),慢分支睡 200µs,两路再汇到一个 `sync`
  节点的两个输入口。同时验证**活性**(内部边无界 ⇒ 不形成循环等待)与**内存**(增长受水位
  约束而非受吞吐约束)。
  > 本条此前**是假的**:文档一直声称有这个测试,而全仓库每个多输入节点都是从**图输入口**
  > 喂的,没有任何一处是内部边扇出后汇合 —— 也就是说 ADR #11 用来拒绝内部边背压的论证,
  > 自己从未被测过。现已补上,实测数据见 §13.4。
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

### 13.4 长跑内存曲线压测(soak)

`tests/soak.rs` —— 默认 `#[ignore]`(不拖慢常规套件),CI 的 `rust` job 里以约 1 GiB
规模显式跑一遍;本地可用 `LMFLOW_SOAK_PACKETS` 放大。

**为什么需要它,以及它证明了什么水位读数证明不了的事。** 此前全局水位只有功能测试
(「撞到水位会转成背压」),而「功能对、内存仍在涨」是完全可能的。做负对照实测过:
把消费算子改成**不释放包**(堆起来),`total_queued_bytes` 峰值**依旧稳稳停在 4 MiB
水位上**、丢包断言也照样通过 —— 而 RSS 跟着总吞吐一路涨了 233 MiB,曲线是干净的线性斜坡。
也就是说:**水位读数正常并不能推出内存有界**,这个盲区只有量真实 RSS 才能盖住。

断言的形式很关键 —— 不是「内存小」,而是:

> **RSS 的增长受水位约束,而不是受总吞吐量约束。**

因为该上界与包数 `n` 无关,泄漏与正常的差距就是两个数量级,不存在阈值调参的模糊地带。
实测(Linux,`/proc/self/status` 的 `VmRSS`):

| 吞吐 | 峰值积压字节 | RSS 增长 |
|---|---|---|
| 250 MiB(默认) | 4096 KiB = 水位 | **4364 KiB** |
| 2.5 GiB(10×) | 4096 KiB = 水位 | **4364 KiB**(与规模无关) |
| 250 MiB(注入泄漏的负对照) | 4096 KiB = 水位 | 238896 KiB → 测试失败 ✓ |

另外三条辅助断言各有职责:**水位真的被压到过**(否则一次「跑得太快、从不积压」的运行
也会绿,却什么都没证明)、**软水位的超出量有界**(允许 Relaxed 快照滞后几个包,但不该无界)、
**无丢包**。RSS 读数是 Linux 专属;其它平台自动跳过该条,其余断言仍然有效。

**扇出 + 汇合(diamond)拓扑** —— `watermark_bounds_memory_in_diamond_topology`。
上面那条是单节点线性图,而 ADR #11 唯一要保护的形状恰恰是 diamond,此前**零覆盖**
(§13.1 曾声称有这个测试,实际没有)。拓扑:

```text
   in ──┬─► slow(睡 200µs) ─► s ─┐
        └─► fast ───────────► f ─┴─► join(默认 sync,按时间戳对齐两路)
```

一条图输入边被**两个**节点消费 = 真正的内部扇出;两路再汇到 `join` 的两个输入口。
水位按**个数**(`max_queued_packets: 64`)而非字节 —— 因为 `Payload::byte_size()` 对
`Native` / `Foreign` 计 0,字节水位只对内建 payload 有效,而个数水位对所有形态成立。
注意扇出后**每包占 2 个队列槽**(`on_enqueue` 是按消费者调用的)。

| 吞吐 | join 触发 | 峰值在途 | RSS 增长 |
|---|---|---|---|
| 250 MiB | 2000 / 2000 | 65 槽(水位 64) | **4364 KiB** |
| 2.5 GiB(10×) | 20000 / 20000 | 65 槽 | **4368 KiB** |

与线性图的 4364 / 4364 **数值一致**,且同样与吞吐无关;2.5 GiB 跑完 5.54 秒、未挂住,
`sync` 汇合把每个时间戳都对齐处理掉、无丢失、无错配。

**这条测试同时是 ADR #11 的活性回归**:内部边一旦被加上阻塞式硬上界,慢分支会让
`join` 永不触发,`wait_done_timeout` 就会在这里超时 —— 那正是 §7.5 描述的循环等待。

---

### 13.5 `type_id` 的跨编译器行为(ADR #22 的验证)

`type_id` 有两个此前**完全没有测试**的面,而它们的失效都是**静默**的:

1. **哈希算法在两侧各有一份手写实现** —— C++ 在 `flow.hpp` 的 `Fnv1a` + `NormalizeTypeId`,
   Rust 在 `packet::fnv1a_type_id`。任何一侧动了 offset basis、乘子、或那个 `< 16` 的
   内建区规避分支,同一个类型在两边就会算出不同 id,契约校验形同虚设 —— **不报任何错**。
2. **`LMFLOW_DECLARE_TYPE_NAME` 是跨工具链的唯一逃生口**,却只在一个示例里用过。
   若它哪天悄悄退化成 no-op,type_id 就会静默退回修饰名 —— 而修饰名 MSVC 与
   GCC/Clang 不同,于是跨编译器混用算子时类型校验静默失配。

对应的钉法:

| 位置 | 形式 | 钉住什么 |
|---|---|---|
| `cpp/abi_assert.cc` | 编译期 `static_assert` | FNV offset basis;`Fnv1a("lmflow.test.Stable")` 的确切值;`NormalizeTypeId` 在 0/15/16/17 的边界 |
| `core/tests/abi_layout.rs` | 运行期 `assert_eq!` | **同一个字面量** `0xBFB531B283179309`,加 `"i"`/`"d"` 两个修饰名样例,加 2000 个取样都 ≥ 16 |
| `cpp/tests/flow_hpp_test.cc` | 运行期 `assert` | 声明稳定名后 id 由该字符串决定;**id 与修饰名算出的值不同**(证明宏真生效,不是 no-op);未声明的类型 id 就是修饰名哈希;两个不同类型声明同名 → 同 id |

两处期望值都由**第三方实现(Python)独立算出**,不是从任一侧抄来的 —— 否则就是拿实现验证
自己。为让编译期断言能穿透整条链,`NormalizeTypeId` 从 `inline` 改成了 `constexpr`。

**上面这些都还抓不到跨编译器分歧** —— 它们钉的是**哈希函数**,在任何编译器上都同样通过。
真正的互操作身份来自 `typeid(T).name()`,故另加一条:

```cpp
assert(lmflow::TypeId<int>() == 12638195996648667684ULL);   // 与 packet.rs 同源常量
```

`packet.rs` 断言「Rust 对字符串 `"i"` 的哈希 == 该常量」,**没有**断言「本编译器的
`typeid(int).name()` 真的是 `"i"`」。这两件事不同,而后者才是跨语言实际比对的东西。

分歧比「修饰方案不同」更彻底:Itanium ABI(GCC/Clang)下是 `"i"`,而 **MSVC 的
`type_info::name()` 返回未修饰的可读名** —— `"int"`、`"struct Foo"`(修饰形式在
`raw_name()`)。故 `FNV("i")` 与 `FNV("int")` 毫不相干。这条在新编译器上失败**正是想要的
信号**:该平台自定义类型的 type_id 与其它平台不一致,跨工具链传该类型必须改用
`LMFLOW_DECLARE_TYPE_NAME`。让它成为**被审阅的显式决定**,而不是静默上线。

「证明宏真生效」那条做过负对照:把 `LMFLOW_DECLARE_TYPE_NAME` 改成 no-op,测试立即断言失败。
只断言「id == 某常量」是不够的 —— 那条单独看,一个 no-op 宏也可能巧合通过。

---

### 13.6 多配置生成器下的 cargo profile(构建正确性)

`CMAKE_BUILD_TYPE` **只对单配置生成器有意义**(Unix Makefiles、单配置 Ninja);多配置
生成器(Ninja Multi-Config、Visual Studio)的配置是**构建期**由 `--config` 决定的,配置期
读那个变量通常为空。`cmake/engine.cmake` 原先在配置期做 `if(CMAKE_BUILD_TYPE MATCHES ...)`,
于是在多配置生成器下会**静默**退到 debug:

```text
cmake -B b -G "Ninja Multi-Config"
cmake --build b --config Release --target flow_engine
→ Finished `dev` profile [unoptimized + debuginfo]      # 要的是 Release
```

后果是 C++ 侧按 Release 编、却链进一个**未优化的 debug 引擎**,而且不给任何提示 ——
正是本项目明确拒绝的静默失效。**这条在 Linux 上就能复现**,不是 Windows 专属问题
(它是做 Windows 侦查时被发现的:windows-latest 默认生成器是多配置的 Visual Studio)。

改法:用生成器表达式让 profile 跟随**实际构建的配置**;imported target 改用
`IMPORTED_LOCATION_<CONFIG>` 并把 `RelWithDebInfo`/`MinSizeRel` 映到 release
(cargo 只有 dev/release 两档);`COMMAND_EXPAND_LISTS` 让 Debug 下 `--release`
展开为**无参数**而不是空字符串参数(cargo 会拒空参数)。单配置生成器行为不变。

CI 守卫在 `cmake-sdk` job 里:配置一次 Ninja Multi-Config,对 Release / RelWithDebInfo /
Debug 三个配置做 `ninja -n` 干跑,断言 profile 与 `--config` 一致,并额外确认真实的
`--release` flag 在生成的构建文件里。只配置 + 干跑,不真编译,秒级。
做过负对照:把 `engine.cmake` 改回原样,守卫立即失败。

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
| 跨平台未验证 | Windows(MSVC)—— **仅剩这一个**;macOS 原生 test 与 iOS/Android/linux-aarch64 交叉编译已在 CI | 已列入 CI 矩阵;Windows 待补 |
| B 的简化偏离完整语义 | 丢了时间戳对齐 | 明确划入 A 阶段;**B/A 两阶段均已落地**,对齐语义有专门测试(§13) |

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
| 调度状态机的 `rescan` 逻辑 | 线程池已落地,**并发正确性由 TSan 硬门禁常绿保证**(0 竞态);`max_in_flight > 1` 的认领/按序重排路径同在门禁内 |
| 全局水位的实际效果 | 已有功能测试证明能拦住无限增长;**内存曲线压测已补上**(`tests/soak.rs`,见 §13.4)—— 实测 RSS 增长与总吞吐**无关**:250 MiB 与 2.5 GiB 两种规模下增长同为 ~4.3 MiB(≈ 水位本身) |
| 跨平台 | macOS(原生 test)、iOS / Android / linux-aarch64(交叉编译 + 桥接链接)均在 CI 内;**Windows(MSVC)仍未验证** |

### 15.3 当前实现的已知边界

诚实记录,避免误以为已完成。**本节只写当下仍然成立的边界** —— 一旦某条被实现,就从这里删掉
(否则它会反过来低报完成度:曾有一版这里还写着「线程池尚未实现」,而线程池早已落地并进了 TSan 门禁)。

**引擎语义上的边界**

- **无通用引擎级 timer**:源的**定速**已有声明式支持(`rate: N` Hz,ADR #38 / §7.4),
  但没有更一般的定时设施(如「每 N 毫秒给某个非源节点发一次 tick」)。源仍必须挂线程池
  执行器(config 强制校验);**未**设 `rate` 且自身不阻塞的源会灌爆下游 —— 内部边不背压(§7.5)。
- **`LMFLOW_TYPE_HOST_OBJECT` 未启用**(ADR #26):跨语言算子只收发内建类型。
  它现在是**主动拒绝**,不再是「没人生产它」的默认状态 —— 契约声明它在建图期报错;
  图输入、算子输出和 side packet 带它都在各自进入数据流的边界报错,且不会先派给
  poller / observer。所有报错都给出替代方案(数值集合用 `BUFFER`,任意元数据用 `STR` 装 JSON)。
  钉在 `tests/host_object.rs`,含一条**负对照**:把运行期判断挪到 `want == 0` 短路之后,
  声明 `any` 的端口那条测试立刻失败 —— 那正是最容易漏的配置。
- `Packet::new`(Rust 原生值)的 `type_id` 是 `NONE`,**不参与跨语言类型校验** ——
  它是**有意如此**,不是待补的功能:Rust 的 `std::any::TypeId` 与 C++ 修饰名哈希是两套
  身份空间,自动映射会造出「看着能跨语言、实际只与自己一致」的 id,那是**静默**失配,
  比现在的明确报错糟得多。出路两条:内建 payload 用 `Packet::from_i64` / `from_f64` /
  `from_builtin`(它们带正确 `type_id`,也正是跨语言算子该交换的东西,见 ADR #9);
  自定义类型优先实现 unsafe `InteropType` 后用 `Packet::from_interop`;底层逃生口
  `Packet::new_interop` 是 unsafe,要求调用方自行证明 Rust/C++ ABI 布局一致,且拒绝
  0..15 内建保留 id。双方通常以 C++ 侧 `LMFLOW_DECLARE_TYPE_NAME` 的稳定名字对齐。
  仍留在本节是因为**用户确实会撞上**:`Packet::new` 是最自然的名字,却是唯一过不了类型
  契约的构造函数,且失败在运行期第一个包而非建图期。缓解是把出路写进错误本身 ——
  `got == NONE` 时错误会点名 `Packet::new` 并列出该改用哪几个构造函数(有配对测试钉住:
  一条断言提示出现,一条证明它指的路真的可行)。

**验证覆盖上的边界**

- **Windows(MSVC)未验证**:CI 覆盖 linux-x86_64/aarch64、macOS、iOS、Android;无 Windows。
  已做过一轮**只读侦查**,结论是缺口比字面看着窄,记录在此免得下次重查:
  - **Rust 引擎与四个头文件已经是可移植的**(用构建验证过,不是读代码):
    `cargo check --all-targets --target x86_64-pc-windows-msvc` 干净,`clippy -D warnings` 干净,
    windows-gnu 可完整链接。`executor.rs` 的绑核/实时优先级**已有真正的 no-op 兜底分支**
    (`cfg(not(any(linux, android, macos, ios)))`),且**零 libc crate 依赖**(在 cfg 内直接
    声明符号)。四个头无 `__attribute__`/`typeof`/VLA/匿名联合/`#pragma`,全用定宽整型
    → LLP64 非问题。18 个 C++ 算子亦无 POSIX 头、无 GCC 扩展。
  - **符号导出不需要 `.def`/`dllexport`**:131 个 C ABI 符号**全部**是 Rust 侧 `#[no_mangle]`
    定义的(零个来自 C++),`objdump -p lmflow.dll` 实测导出 130 个(差的那个是
    feature-gated 的 `lmflow_register_builtin_kernels`,符合预期)。这一点设计时已考虑过 ——
    若该 C ABI 符号由 C++ 定义,rustc 不会把它放进 DLL 导出表,那才是硬阻塞。
  - **真实工作量是 CMake + CI**,约 16 处构建相关改动:库名/后缀、链接库列表里的 `m`
    (`${CMAKE_DL_LIBS}` 在 Windows 为空、`Threads::Threads` 在 MSVC 是 no-op,这两个**无需**
    条件化),以及需要补上 Windows 系统导入库(`kernel32/ntdll/userenv/ws2_32/dbghelp`)。
    另需处理 MSVC 的 CRT 一致性(rustc 恒发 `/defaultlib:msvcrt`,而 CMake 在 Debug 下默认 `/MDd`)。
  - **TSan 在 windows-msvc 上不可用**(仅 Linux/macOS)。而并发是本设计的核心风险、TSan 是硬门禁,
    故 Windows 若接入,在安全矩阵里**必然是二等**(ASan 可部分替代,并发无等价门禁)。
    这应作为**明示的接受限制**,不是遗漏。
  - 暂缓的真正原因不是难度,而是**本机无 MSVC、完全无法本地验证**,只能盲写 + 跟 CI 迭代。
- **Miri 跑不动**:FFI 里大量 `extern "C"` 与外部 C++ 符号,Miri 无法执行,故只作 advisory
  (`continue-on-error`),不是门禁。ASan 同样是 advisory(build-std + C++ 侧易误报)。
- **`pool4` 类多线程基准在本机噪声达 ±13~25%**,不能用于归因单次改动(见
  `benches/dispatch.rs` 文件头;那里也记了本机系统调用被放大约 5 倍这件事)。
