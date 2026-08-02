/*
 * flow.h — Flow 引擎 C ABI(设计基准)
 *
 * 这是手写的权威定义;正式实现里由 cbindgen 从 Rust 的 ffi.rs 生成等价的一份。
 * 所有跨界结构体布局稳定(Rust 侧 #[repr(C)])。纯 C 头,C/C++ 均可包含。
 *
 * 通用约定
 *  - 返回 LMFlowStatus 的函数:0 = 成功;失败时可用 lmflow_last_error() 取可读描述。
 *  - 传入的 const char* 仅在调用期间被读取,引擎会自行拷贝需要留存的部分。
 *  - 返回的 const char*(错误、名字、options、dump)由引擎持有;
 *    生命周期见各函数注释,调用方不得 free。
 *  - LMFlowContext* / LMFlowContract* 仅在对应回调期间有效,算子不得存留。
 */
#ifndef LMFLOW_H_
#define LMFLOW_H_

#include <stddef.h>
#include <stdint.h>
#ifndef __cplusplus
#include <stdbool.h>
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* ---------- ABI 版本 ----------
 * 动态链接时 header 与 .so 版本不一致会导致结构体布局错乱。
 * 宿主启动时应校验 lmflow_abi_version() == LMFLOW_ABI_VERSION;
 * lmflow_graph_new 内部亦会校验,不匹配返回 NULL 并置错误。 */
#define LMFLOW_ABI_VERSION 1u
uint32_t lmflow_abi_version(void);

/* ---------- 状态码 ---------- */
typedef int32_t LMFlowStatus; /* 0 = OK */
#define LMFLOW_OK 0
#define LMFLOW_ERR_INVALID_ARG 1  /* 配置/参数非法(端口名未定义、拓扑不合法等)*/
#define LMFLOW_ERR_NOT_FOUND 2    /* 名字查不到(kernel 未注册、端口不存在)*/
#define LMFLOW_ERR_KERNEL 3       /* 算子回调返回失败或抛异常 */
#define LMFLOW_ERR_PANIC 4        /* Rust 侧 catch_unwind 兜底 */
#define LMFLOW_ERR_WOULD_BLOCK 5  /* 非阻塞接口:队列满 / 暂无数据 */
#define LMFLOW_ERR_TIMEOUT 6      /* 带超时接口:超时 */
#define LMFLOW_ERR_CANCELLED 7    /* 图已被 cancel */
#define LMFLOW_ERR_CLOSED 8       /* 端口已关闭 / 图已终止 */
#define LMFLOW_ERR_ABI 9          /* ABI 版本不匹配 */
#define LMFLOW_ERR_UNSUPPORTED 10 /* 配置用到了本版本尚未实现的特性(不静默忽略)*/
#define LMFLOW_ERR_STATE 11       /* 图状态不允许该操作(如 start 两次、未 start 就 send)*/

/* 最近一次失败的可读描述(线程局部)。仅在下一次本线程调用 flow_* 之前有效。
 * 成功路径不保证清空,只应在拿到非 0 状态或 NULL 句柄后读取。 */
const char* lmflow_last_error(void);

/* ---------- 日志 ----------
 * 不设置则引擎静默(不抢占 stdout)。回调可能在任意工作线程被调用,须自行加锁。
 * 引擎保证:调用本回调时**不持有任何引擎内部锁** —— 因此回调里可以安全地做加锁、
 * 抢 GIL 之类的重活,不会与引擎形成锁序环。但请勿在回调内调用 lmflow_graph_* 接口。 */
typedef enum {
  LMFLOW_LOG_ERROR = 0,
  LMFLOW_LOG_WARN = 1,
  LMFLOW_LOG_INFO = 2,
  LMFLOW_LOG_DEBUG = 3
} LMFlowLogLevel;
void lmflow_set_log_callback(void (*cb)(void* user, LMFlowLogLevel level, const char* msg), void* user);

/* ---------- 时间戳哨兵 ----------
 * 数值空间划分:
 *   UNSET < UNSTARTED < PRE_STREAM < MIN ... MAX < POST_STREAM < ONE_OVER_POST_STREAM < DONE
 * 其中 [MIN, MAX] 为普通数据区间;PRE_STREAM/POST_STREAM 为流首/流尾单包位置;
 * DONE 表示端口已关闭且不会再有数据。 */
#define LMFLOW_TS_UNSET INT64_MIN
#define LMFLOW_TS_UNSTARTED (INT64_MIN + 1)
#define LMFLOW_TS_PRE_STREAM (INT64_MIN + 2)
#define LMFLOW_TS_MIN (INT64_MIN + 3)
#define LMFLOW_TS_MAX (INT64_MAX - 3)
#define LMFLOW_TS_POST_STREAM (INT64_MAX - 2)
#define LMFLOW_TS_ONE_OVER_POST_STREAM (INT64_MAX - 1)
#define LMFLOW_TS_DONE INT64_MAX

/* 端口序号无效值(by-tag 查询失败时返回)*/
#define LMFLOW_INVALID_ID ((size_t)-1)

/* ---------- 不透明句柄 ---------- */
typedef struct LMFlowGraph LMFlowGraph;
typedef struct LMFlowInput LMFlowInput;       /* 图输入口句柄(热路径免查表)*/
typedef struct LMFlowPoller LMFlowPoller;
typedef struct LMFlowContext LMFlowContext;   /* 回调期借用 */
typedef struct LMFlowContract LMFlowContract; /* get_contract 期借用 */

/* ---------- Packet(跨界表示)----------
 * 三种所有权语义,由 owner 字段区分:
 *  1) 宿主/算子新建:owner=NULL,drop_fn 非空。提交给引擎(send/emit)后引擎接管,
 *     内部以引用计数持有,引用归零时调用一次 drop_fn。
 *  2) 引擎借出(lmflow_ctx_input / observer 回调):owner 非空,**借用**,
 *     调用方不得 drop、不得在回调结束后继续使用。
 *  3) 引擎移交(lmflow_poller_next*):owner 非空且已为宿主递增引用,
 *     宿主**必须**调 lmflow_packet_drop,否则泄漏。
 * payload==NULL 表示空包(仅携带时间戳,用于时间戳边界 / 关流)。
 *
 * timestamp 约定:提交时若为 LMFLOW_TS_UNSET,引擎自动继承当前 input_timestamp
 * (与 lmflow_ctx_forward 行为一致);图输入口上提交 UNSET 则视为非法。 */
typedef struct {
  void* payload;                  /* 数据指针,视为不可变共享 */
  uint64_t type_id;               /* 类型标识,见 lmflow_type_id 说明 */
  int64_t timestamp;
  void* owner;                    /* 引擎内部引用句柄;NULL = 无主 */
  void (*drop_fn)(void* payload); /* 由引擎在引用归零时调用一次 */
} LMFlowPacket;

/* 释放一个由调用方持有的包。两种持有形态都能正确处理:
 *   owner != NULL  —— 归还引擎引用(引用归零时引擎调 drop_fn);
 *   owner == NULL  —— 直接调用 drop_fn(未提交的自建包)。
 * **借用**得来的包(lmflow_ctx_input / observer 回调参数)不得调用本函数。
 * 调用后 pkt 字段被清零,可安全重复调用。 */
void lmflow_packet_drop(LMFlowPacket* pkt);

/* 可读的调试串,形如 "Packet{type=Buffer[3x224x224 f32], ts=42}"。
 * 返回**线程局部**缓冲,生命周期至本线程下次调用本函数。仅用于日志/排障。 */
const char* lmflow_packet_debug_string(const LMFlowPacket* pkt);

/* type_id 说明:
 *  引擎只做「相等性」比对,不解释其含义。0..15 为内建类型保留;0 表示「不声明类型」,
 *  引擎跳过校验。
 *
 *  C++ 糖层的取值方式:FNV-1a(typeid(T).name())(修饰名),而非 hash_code ——
 *  后者是实现定义的、不保证跨动态库一致。修饰名跨编译器仍可能不同(GCC/Clang 一致,
 *  MSVC 不同),需要跨工具链互通时用 LMFLOW_DECLARE_TYPE_NAME 显式指定稳定名。
 *
 *  为让错误信息可读(「期望 Buffer 得到 I64」而不是两个数字),可为 type_id 注册名字。
 *  内建类型的名字由引擎预置。重复注册同一 id 以最后一次为准。 */
LMFlowStatus lmflow_register_type_name(uint64_t type_id, const char* name);
const char* lmflow_type_name(uint64_t type_id); /* 未注册则返回 "type#<id>" 形式 */

/* ---------- 数据类型模型 ----------
 * 引擎对 payload **完全不作解释**:它只搬引用(引用计数 +1/-1)、只在需要时按
 * type_id 做一次相等性校验。因此**任意数据类型都能在图里流动**,不需要引擎认识它。
 *
 * 有两条路可选:
 *
 *  (1) 任意自定义类型(推荐给纯 C++ / 纯 Rust 管线)
 *      调用方自备 payload 指针与 drop_fn,type_id 自取(C++ 糖层默认用 typeid 哈希)。
 *      引擎零参与,零约束 —— cv::Mat、自定义结构体、模型张量对象都同等对待。
 *
 *  (2) 内建类型(给**跨语言**场景:Python / Go / C# …)
 *      跨语言时对方无法解释一个不透明的 C++ 对象,所以约定了下面这几种布局,
 *      并由引擎负责分配/复制/释放,免去在别的语言里构造 drop_fn。
 *      这些类型对引擎**没有特权**,只是一套双方都认识的内存约定。
 *
 * 下列构造函数返回的包 **owner 非空**(引擎已为调用方持有一份引用):
 *   - 提交给引擎(send / emit)= 移交这份引用,调用方不再负责;
 *   - 不提交 = 必须 lmflow_packet_drop 释放。
 * (对称地,emit/send 也接受 owner==NULL 的自建包,此时引擎会接管 payload+drop_fn。) */
#define LMFLOW_TYPE_NONE 0u  /* 不声明类型,跳过校验 */
#define LMFLOW_TYPE_BYTES 1u /* 一维字节块 */
#define LMFLOW_TYPE_I64 2u
#define LMFLOW_TYPE_F64 3u
#define LMFLOW_TYPE_BOOL 4u
#define LMFLOW_TYPE_STR 5u    /* UTF-8,以 NUL 结尾 */
#define LMFLOW_TYPE_BUFFER 6u /* N 维带步长缓冲,见 LMFlowBuffer */
/* 7 号预留给「宿主语言原生对象」(如 PyObject),本版本**未启用**,见下方说明。 */
#define LMFLOW_TYPE_HOST_OBJECT 7u

LMFlowPacket lmflow_packet_from_bytes(const void* data, size_t len, int64_t ts);
LMFlowPacket lmflow_packet_from_i64(int64_t value, int64_t ts);
LMFlowPacket lmflow_packet_from_f64(double value, int64_t ts);
LMFlowPacket lmflow_packet_from_bool(bool value, int64_t ts);
LMFlowPacket lmflow_packet_from_str(const char* utf8, int64_t ts);

/* 读取内建类型;type_id 不符或空包返回 false。返回的指针在包存活期间有效。 */
bool lmflow_packet_as_bytes(const LMFlowPacket* pkt, const void** data, size_t* len);
bool lmflow_packet_as_i64(const LMFlowPacket* pkt, int64_t* out);
bool lmflow_packet_as_f64(const LMFlowPacket* pkt, double* out);
bool lmflow_packet_as_bool(const LMFlowPacket* pkt, bool* out);
bool lmflow_packet_as_str(const LMFlowPacket* pkt, const char** out);

/* ---------- 为什么跨语言算子只允许内建类型 ----------
 * 本版本规定:**非 C++/非 Rust 的算子(如 Python)只能收发上面这些内建类型。**
 * 不是实现难度问题,而是为了保持 payload 的**语言中立性**:
 *
 *   图是 YAML 描述的,看不出某个节点是用什么语言写的。一旦允许「宿主语言原生对象」
 *   (PyObject 之类)进入数据流,就出现了两级类型系统 —— 某些包只能在纯 Python
 *   子图里流动,把它接到 C++ 算子上会拿到无法解读的不透明指针,且只在运行时才暴露。
 *   另有一层风险:这类对象的引用归零可能发生在引擎工作线程上,而 Py_DECREF 需要
 *   抢 GIL,是死锁隐患。
 *
 * 结构化数据的表达方式:
 *   - 数值集合(检测框、关键点)→ LMFLOW_TYPE_BUFFER(N×K),零拷贝且 C++ 可直读;
 *   - 任意元数据            → LMFLOW_TYPE_STR 装 JSON;
 *   - 配置参数              → 走 node options,本不该进数据流。
 *
 * 若将来确实要开放,用预留的 LMFLOW_TYPE_HOST_OBJECT,并且必须是**显式选择**:
 * 引擎在 init 阶段已知每个节点的算子语言,可据此拒绝「原生对象端口接到异语言算子」
 * 的拓扑,把运行时故障提前成配置错误。 */

/* ---------- LMFlowBuffer:N 维带步长缓冲 ----------
 * 跨语言传递「大块数值数据」的统一约定 —— 图像、张量、音频帧都是它,不再各设一种类型:
 *     灰度图     ndim=2  shape=[H,W]
 *     彩色图     ndim=3  shape=[H,W,C]      (cv::Mat / numpy HWC)
 *     推理张量   ndim=4  shape=[N,C,H,W]    (模型输入/输出)
 *     音频       ndim=2  shape=[帧数,声道]
 * 语义与 numpy buffer protocol 一致:strides 以**字节**计,行优先无需连续,
 * 第 (i,j,…) 个元素地址 = data + i*strides[0] + j*strides[1] + …
 * 本结构体是**纯 C**,不引入任何图像/张量库依赖(cv::Mat 转换见可选头 flow_cv.hpp)。 */
#define LMFLOW_MAX_DIMS 8

#define LMFLOW_DTYPE_U8 0
#define LMFLOW_DTYPE_I8 1
#define LMFLOW_DTYPE_U16 2
#define LMFLOW_DTYPE_I16 3
#define LMFLOW_DTYPE_I32 4
#define LMFLOW_DTYPE_I64 5
#define LMFLOW_DTYPE_F16 6
#define LMFLOW_DTYPE_F32 7
#define LMFLOW_DTYPE_F64 8

#define LMFLOW_BUF_FLAG_NONE 0u
#define LMFLOW_BUF_FLAG_READONLY 1u /* 只读视图(由 as_buffer 取得时置位)*/

#define LMFLOW_DEVICE_CPU 0 /* 预留:未来可扩展到 GPU 等其它内存空间 */

typedef struct {
  void* data;                     /* 首字节 */
  int64_t shape[LMFLOW_MAX_DIMS];   /* 各维元素数 */
  int64_t strides[LMFLOW_MAX_DIMS]; /* 各维字节步长 */
  int32_t ndim;                   /* 1..LMFLOW_MAX_DIMS */
  int32_t dtype;                  /* LMFLOW_DTYPE_* */
  uint32_t flags;                 /* LMFLOW_BUF_FLAG_* */
  int32_t device;                 /* LMFLOW_DEVICE_* */
  int64_t reserved[2];            /* 预留:置零。见下方 ABI 演进说明 */
} LMFlowBuffer;

/* ABI 演进:本结构体的 reserved 是**一次性预留**,用于未来新增字段(最可能是
 * GPU/其它内存空间的描述)而不改变 sizeof —— 一旦改变布局就必须提升
 * LMFLOW_ABI_VERSION,所有既有二进制都要重编。构造时请整体清零(C++: LMFlowBuffer b{};
 * C: memset),不要假设未使用字段的内容。 */

size_t lmflow_dtype_size(int32_t dtype); /* 未知 dtype 返回 0 */

/* 【推荐的零拷贝入口】由**引擎**分配缓冲(连续、按 SIMD 友好边界对齐),
 * 通过 out 返回可写视图。调用方(Python 用 numpy、C++ 用 cv::Mat)直接包住
 * out->data 就地写入 —— 全程不发生跨语言引用计数,这避免了「在引擎工作线程上
 * 析构 PyObject 需抢 GIL」的死锁风险。 */
LMFlowPacket lmflow_packet_new_buffer(int32_t ndim, const int64_t* shape, int32_t dtype, int64_t ts,
                                  LMFlowBuffer* out);

/* 从外部缓冲**拷贝**一份(最简单也最安全;src 可在返回后立即失效)。 */
LMFlowPacket lmflow_packet_from_buffer(const LMFlowBuffer* src, int64_t ts);

/* 取只读视图;非 BUFFER 包返回 false。视图在包存活期间有效。
 * **只读契约**:payload 是引用计数共享的(扇出时多个下游持同一份),
 * 经本函数拿到的 data 不得写入 —— 要就地改写请走 lmflow_packet_make_mutable_buffer。 */
bool lmflow_packet_as_buffer(const LMFlowPacket* pkt, LMFlowBuffer* out);

/* ---------- 引用与写时复制(CoW)----------
 * payload 默认是**不可变共享**:Forward / 扇出只递增引用计数,不拷贝数据。
 * 需要就地改写时用 CoW:独占则零拷贝改写,被共享才复制一份。
 *
 * 「省拷贝」的正确写法(线性管线上全程零拷贝):
 *     LMFlowPacket p = lmflow_ctx_take_input(ctx, 0);   // 先移出输入槽!
 *     LMFlowBuffer buf;
 *     if (lmflow_packet_make_mutable_buffer(&p, &buf) == LMFLOW_OK) { ...原地写 buf.data... }
 *     lmflow_ctx_emit(ctx, 0, p);                      // 移交给引擎
 *
 * ⚠ 若不 take_input 而只是 as_buffer + clone,上下文仍持有一份引用 ——
 *   引用数 >= 2,CoW 必然复制,省拷贝的意图就落空了。 */

/* 引用计数 +1,得到一份**调用方拥有**的包(不拷贝数据)。用完须 emit/send 或 lmflow_packet_drop。 */
LMFlowPacket lmflow_packet_clone(const LMFlowPacket* pkt);

/* 取得独占可写视图(CoW)。
 *  - 引用数 == 1 → 原地返回可写视图,**零拷贝**;
 *  - 引用数 > 1  → 复制一份,*pkt 改为指向副本,再返回其可写视图。
 * 前置条件:pkt 必须为调用方所拥有(owner 非空或自建包),不能是借用的输入包。
 * 仅支持**引擎持有的内建 payload**(BUFFER / BYTES / 标量);对自定义 C++ payload
 * 引擎只有 drop_fn、无从复制,返回 LMFLOW_ERR_INVALID_ARG(该情形请自行拷贝)。 */
LMFlowStatus lmflow_packet_make_mutable_buffer(LMFlowPacket* pkt, LMFlowBuffer* out);
LMFlowStatus lmflow_packet_make_mutable_bytes(LMFlowPacket* pkt, void** data, size_t* len);

/* ---------- 算子 vtable(C++ 提供,Rust 调用)----------
 * 除 process 外均可为 NULL。回调内不得让异常/panic 穿越边界。 */
typedef struct {
  void* (*create)(void* factory);
  void (*get_contract)(void* factory, LMFlowContract* out);
  LMFlowStatus (*open)(void* self, LMFlowContext* ctx);
  LMFlowStatus (*process)(void* self, LMFlowContext* ctx);
  LMFlowStatus (*close)(void* self, LMFlowContext* ctx);
  void (*destroy)(void* self);
} LMFlowKernelVTable;

/* 注册**捆绑的内置算子**(PassThrough / Scale / Sum / Split / Zip / Filter /
 * Stringify / Sink / Invert / Normalize)。
 *
 * 注意:本函数由捆绑的算子库(cpp/kernels.cc)提供,而非引擎本体 —— 但链接
 * libflow_core 时二者在同一产物内。宿主须在 init_from_yaml **之前**调用一次,
 * 否则会得到「算子未注册」。用显式函数而非静态初始化,是因为静态初始化对象在
 * 静态库中可能被链接器裁剪(见 docs/design.md §14 风险登记)。幂等。 */
void lmflow_register_builtin_kernels(void);

/* 注册算子。同名重复注册返回 LMFLOW_ERR_INVALID_ARG。
 * vt 须指向静态存储(引擎只保存指针)。 */
LMFlowStatus lmflow_register_kernel(const char* name, const LMFlowKernelVTable* vt, void* factory);

/* ---------- Contract:在 get_contract 里声明端口类型约束 ----------
 * 端口的数量与名字来自 YAML,故此处可查询后再逐个声明。 */
size_t lmflow_contract_num_inputs(const LMFlowContract*);
size_t lmflow_contract_num_outputs(const LMFlowContract*);
/* 按 tag 取序号(YAML 端口名支持 "TAG:index:name" 三段式);无则返回 LMFLOW_INVALID_ID */
size_t lmflow_contract_input_id(const LMFlowContract*, const char* tag, size_t index);
size_t lmflow_contract_output_id(const LMFlowContract*, const char* tag, size_t index);
const char* lmflow_contract_input_name(const LMFlowContract*, size_t idx);
const char* lmflow_contract_output_name(const LMFlowContract*, size_t idx);
size_t lmflow_contract_input_index(const LMFlowContract*, const char* name);
size_t lmflow_contract_output_index(const LMFlowContract*, const char* name);
void lmflow_contract_input_set_any(LMFlowContract*, size_t idx);
void lmflow_contract_input_set_type(LMFlowContract*, size_t idx, uint64_t type_id);
void lmflow_contract_output_set_any(LMFlowContract*, size_t idx);
void lmflow_contract_output_set_type(LMFlowContract*, size_t idx, uint64_t type_id);

/* 声明本算子**必需**的 side packet。宿主若未注入,init 阶段即报错并指出缺哪个名字 ——
 * 而不是留到 open 里由算子自己查、或运行时才发现拿到空包。 */
void lmflow_contract_require_side_packet(LMFlowContract*, const char* name);

/* ---------- 端口的命名与定位 ----------
 * 有**两套不同的标识符**,分工明确,别混用:
 *
 *  ① 端口名(name)—— 属于**图**,用来连接。
 *     上游节点的某个 output_ports 名与下游节点的某个 input_ports 名相同,二者即连成一条边。
 *     整张图中每个名字**只能有一个生产者**(否则 init 报错)。算子一般不关心这个名字。
 *
 *  ② 标签(tag)—— 属于**算子**,用来表达「我的哪个口是什么语义」。
 *     算子用 tag 定位端口,从而**不依赖 YAML 的书写顺序**,也不会因为改了边名而出错。
 *
 * 端口声明语法(YAML 的 input_ports / output_ports 元素):
 *
 *     "name"                无 tag(归入空 tag ""),index 按同 tag 内出现次序自动编号
 *     "TAG:name"            有 tag,index 自动 = 该 tag 下的第几个
 *     "TAG:index:name"      有 tag,index 显式指定
 *
 *   例:input_ports: ["VIDEO:frames", "AUDIO:pcm", "MASK:0:m0", "MASK:1:m1"]
 *       算子里:cc.InputId("VIDEO")   -> 0
 *               cc.InputId("MASK", 1) -> 3
 *
 * 规则:
 *   - tag 约定大写字母 / 数字 / 下划线,不含 ':';空 tag 表示「无标签」。
 *   - 同一算子、同一 tag 下的 index 必须从 0 连续,不得重复(否则 init 报错)。
 *   - **扁平序号 = YAML 里的声明顺序**(第 0 个声明即序号 0)。
 *     即 lmflow_ctx_input(ctx, 0) 取的就是 input_ports 里写的第一个口 —— 直观、无意外。
 *
 * 三种定位方式,按需选用:
 *     lmflow_ctx_input_id(ctx, "VIDEO", 0)   按 tag(**推荐**:语义稳定)
 *     lmflow_ctx_input_index(ctx, "frames")  按边名(通用/路由类算子偶尔需要)
 *     直接用序号 0,1,2…                    最省事,但依赖声明顺序
 * 查不到时返回 LMFLOW_INVALID_ID。 */

/* ---------- Context:算子在 open/process/close 内读写 ---------- */
size_t lmflow_ctx_num_inputs(const LMFlowContext*);
size_t lmflow_ctx_num_outputs(const LMFlowContext*);
/* 按 tag 取序号,避免依赖 YAML 书写顺序;无则 LMFLOW_INVALID_ID */
size_t lmflow_ctx_input_id(const LMFlowContext*, const char* tag, size_t index);
size_t lmflow_ctx_output_id(const LMFlowContext*, const char* tag, size_t index);
const char* lmflow_ctx_input_name(const LMFlowContext*, size_t idx);  /* 生命周期随 graph */
const char* lmflow_ctx_output_name(const LMFlowContext*, size_t idx);
/* 按**边名**取序号(与 by-tag 互补);查不到返回 LMFLOW_INVALID_ID */
size_t lmflow_ctx_input_index(const LMFlowContext*, const char* name);
size_t lmflow_ctx_output_index(const LMFlowContext*, const char* name);

bool lmflow_ctx_input_is_empty(const LMFlowContext*, size_t in_idx);
/* 该输入口是否已终结:上游已关闭且队列已排空 —— 即「再也不会有数据了」。
 * 与 is_empty 的区别:is_empty 只说此刻没有,is_done 说的是永远不会再有。 */
bool lmflow_ctx_input_is_done(const LMFlowContext*, size_t in_idx);
LMFlowPacket lmflow_ctx_input(const LMFlowContext*, size_t in_idx); /* 借用,勿 drop */
/* 快路径:只要数据指针,省去按值返回整个结构体 */
const void* lmflow_ctx_input_payload(const LMFlowContext*, size_t in_idx);
int64_t lmflow_ctx_input_timestamp(const LMFlowContext*);

/* **取走**输入包:所有权移交调用方,该输入槽变空。
 * 这是 CoW 省拷贝路径的关键一步 —— 上下文不再持有引用,独占时 make_mutable 才能零拷贝。
 * 取走后须 emit/send 或 lmflow_packet_drop。对空槽调用返回空包。 */
LMFlowPacket lmflow_ctx_take_input(LMFlowContext*, size_t in_idx);

void lmflow_ctx_emit(LMFlowContext*, size_t out_idx, LMFlowPacket pkt);   /* 移交所有权 */
void lmflow_ctx_forward(LMFlowContext*, size_t in_idx, size_t out_idx); /* 直通,复用同一 payload */
void lmflow_ctx_set_next_ts_bound(LMFlowContext*, size_t out_idx, int64_t bound);

/* ---------- 算子的自我信息、日志与错误 ----------
 * 算子是用户代码的主体,必须能报告「我是谁、发生了什么」。 */

/* 本节点在图中的名字(YAML 的 name,未写则由 kernel 名派生并去重)与算子类型名。
 * 生命周期随 graph。日志与错误消息里应带上它。 */
const char* lmflow_ctx_node_name(const LMFlowContext*);
const char* lmflow_ctx_kernel_name(const LMFlowContext*);

/* 算子打日志 —— 走引擎的日志回调,不抢 stdout。引擎会自动加上节点名前缀。 */
void lmflow_ctx_log(const LMFlowContext*, LMFlowLogLevel level, const char* msg);

/* 设置本次失败的原因文本,然后从 open/process/close 返回非 0 状态码。
 * 引擎会把它并入图级错误(可由宿主的 lmflow_graph_last_error 读到),并自动加节点名前缀。
 * 不调用本函数也能失败,但宿主就只剩一个错误码、无从诊断。 */
void lmflow_ctx_set_error(const LMFlowContext*, const char* msg);

/* 算子自报计数器(累加,按名字聚合到图上)。用于「处理了多少帧 / 命中多少次缓存」
 * 这类业务指标,宿主经 lmflow_graph_counter_value 读取。名字建议加节点名前缀以免撞车。 */
void lmflow_ctx_counter_add(const LMFlowContext*, const char* name, int64_t delta);

/* 源算子(0 输入口的生成型算子)自报「已产完」。调用后引擎不再触发本节点的 process,
 * 并关闭其输出边(下游据此收流),图随之正常终止。仅对源节点有意义。 */
void lmflow_ctx_source_done(const LMFlowContext*);

/* ---------- 关闭原因 ----------
 * 算子在 close 里据此决定是否写出结果 / 提交事务 / 落盘。 */
typedef enum {
  LMFLOW_CLOSE_NORMAL = 0,    /* 所有输入正常关闭并已排空 */
  LMFLOW_CLOSE_ERROR = 1,     /* 图内发生了错误 */
  LMFLOW_CLOSE_CANCELLED = 2, /* 图被 cancel */
} LMFlowCloseReason;
LMFlowCloseReason lmflow_ctx_close_reason(const LMFlowContext*);

/* ---------- 算子参数(node options)----------
 * YAML:
 *     - name: "det"
 *       kernel: "Detector"
 *       options:
 *         threshold: 0.5
 *         model: "a.onnx"
 *         mean: [0.485, 0.456, 0.406]     # 数组
 *         roi:  { x: 10, y: 20 }          # 嵌套
 *
 * 三种读法,按需选用:
 *   1) 标量 accessor —— 算子侧零依赖,最常用。支持**点号路径**访问嵌套:
 *          lmflow_ctx_option_f64(ctx, "threshold", 0.3)
 *          lmflow_ctx_option_i64(ctx, "roi.x", 0)
 *   2) 数组 accessor —— 归一化均值、anchor 尺寸、类别名这类常见配置。
 *   3) lmflow_ctx_options_json —— 任意复杂结构,自行解析。
 *
 * 带 def 的 accessor 在「key 不存在」或「类型不符」时返回 def(不报错)。
 * ⚠ 这意味着**把 key 名字拼错会静默走默认值**,配置错误被掩盖。因此对算子而言
 *   必不可少的参数,请用下面的 lmflow_ctx_require_* —— 缺失或类型不符即返回错误,
 *   算子在 open 里直接失败,配置问题当场暴露而不是留到跑歪。 */
bool lmflow_ctx_has_option(const LMFlowContext*, const char* key);

int64_t lmflow_ctx_option_i64(const LMFlowContext*, const char* key, int64_t def);
double lmflow_ctx_option_f64(const LMFlowContext*, const char* key, double def);
bool lmflow_ctx_option_bool(const LMFlowContext*, const char* key, bool def);
/* 返回值生命周期随 graph;key 不存在则原样返回 def(不拷贝) */
const char* lmflow_ctx_option_str(const LMFlowContext*, const char* key, const char* def);

/* 必需参数:缺失 / 类型不符 → LMFLOW_ERR_INVALID_ARG,并置 lmflow_last_error 说明是哪个 key */
LMFlowStatus lmflow_ctx_require_option_i64(const LMFlowContext*, const char* key, int64_t* out);
LMFlowStatus lmflow_ctx_require_option_f64(const LMFlowContext*, const char* key, double* out);
LMFlowStatus lmflow_ctx_require_option_bool(const LMFlowContext*, const char* key, bool* out);
LMFlowStatus lmflow_ctx_require_option_str(const LMFlowContext*, const char* key, const char** out);

/* 数组:返回元素个数;out 可为 NULL 仅查长度。实际写入 min(个数, cap) 个元素。
 * key 不存在或不是数组 → 返回 0。 */
size_t lmflow_ctx_option_count(const LMFlowContext*, const char* key);
size_t lmflow_ctx_option_i64_array(const LMFlowContext*, const char* key, int64_t* out, size_t cap);
size_t lmflow_ctx_option_f64_array(const LMFlowContext*, const char* key, double* out, size_t cap);
/* 字符串数组:out[i] 指向引擎持有的字符串,生命周期随 graph */
size_t lmflow_ctx_option_str_array(const LMFlowContext*, const char* key, const char** out, size_t cap);

/* 整个 options 子树的 JSON;无 options 时返回 "{}"。生命周期随 graph。 */
const char* lmflow_ctx_options_json(const LMFlowContext*);

/* ---------- 执行器(线程池)与算子的归属 ----------
 * 图在 YAML 里定义**命名线程池**,节点按名字选择在哪个池上执行:
 *
 *   executors:
 *     - name: "cpu"                 # 名字供节点引用
 *       type: "ThreadPoolExecutor"
 *       num_threads: 4
 *     - name: "io"
 *       type: "ThreadPoolExecutor"
 *       num_threads: 1
 *   nodes:
 *     - name: "decode"
 *       kernel: "Decoder"
 *       executor: "io"              # 跑在 io 池
 *     - name: "detect"
 *       kernel: "Detector"
 *       executor: "cpu"             # 跑在 cpu 池
 *     - name: "draw"
 *       kernel: "Overlay"
 *                                   # 未指定 → 跑在**宿主主线程**
 *
 * 默认(节点未写 executor)= **宿主主线程**,不是线程池。这样:
 *   - 默认零并发、执行顺序确定、断点调试直观;并发是**显式 opt-in**;
 *   - Python 算子默认就跑在 Python 主线程上,**完全没有 GIL 争抢**
 *     (只有显式把 Python 算子放进线程池时才需要考虑 GIL,见设计文档 §8.2)。
 *
 * ⚠ 主线程任务的执行时机:引擎不能凭空占用宿主线程,只能在宿主**进入引擎**时借用它。
 *   因此主线程节点的任务在宿主调用下列**阻塞接口**期间被抽取执行:
 *       lmflow_graph_wait_done / _timeout
 *       lmflow_graph_wait_until_idle / _timeout
 *       lmflow_poller_next / _timeout
 *       lmflow_input_send(阻塞等待空位时)
 *   若宿主只 send 而从不调用上述任一接口,主线程上的节点将**不会推进**。
 *   反之,这些接口在等待期间一律会抽取并执行主线程任务,故不会因此死锁。
 *
 * 节点引用了未定义的 executor 名字 → init 阶段报错(见 lmflow_graph_init_from_yaml)。 */

/* ---------- Side packet:常量输入 ----------
 * 整个 run 期间不变的**任意对象** —— 已加载的模型句柄、标定矩阵、查找表、
 * 词表、外部资源上下文等。与 node options 的分工:
 *   options      = YAML 里的标量 / JSON,描述**配置**;
 *   side packet  = 宿主在运行前注入的**对象**,可以是任意 payload(含自定义类型)。
 * 没有 side packet 就无法把「一个已经初始化好的模型」交给算子 —— options 做不到。
 *
 * 必须在 lmflow_graph_start 之前设置(之后返回 LMFLOW_ERR_STATE)。
 * 传入即移交所有权;引擎持有到 graph 释放。同名重复设置以最后一次为准。 */
LMFlowStatus lmflow_graph_set_side_packet(LMFlowGraph*, const char* name, LMFlowPacket pkt);

/* 算子在 open/process/close 内按名字读取(**借用**,不得 drop)。
 * 名字不存在时返回空包,可用 lmflow_ctx_has_side_packet 先判断。 */
LMFlowPacket lmflow_ctx_side_packet(const LMFlowContext*, const char* name);
bool lmflow_ctx_has_side_packet(const LMFlowContext*, const char* name);

/* ---------- 图:构造与启动 ---------- */
LMFlowGraph* lmflow_graph_new(void); /* 失败返回 NULL(见 lmflow_last_error)*/
/* 解析 + 校验 + 建图。校验项(任一不通过即返回错误,并可由 lmflow_last_error 取原因):
 *   - 端口名引用不到上游生产者;
 *   - 同一端口名有多个生产者;
 *   - 图输入口与某节点的输出口同名;
 *   - 拓扑成环(本版本不支持 back-edge,成环会死锁,故直接拒绝);
 *   - 节点的 executor 名未在 executors 中定义;
 *   - 零输入口节点(本版本不支持,见设计文档 §6.4);
 *   - 用到本版本尚未实现的字段(如 max_in_flight > 1)→ LMFLOW_ERR_UNSUPPORTED。
 * 最后一条尤其重要:**宁可报错也不静默忽略** —— 否则用户以为开了并行,实际没有。 */
LMFlowStatus lmflow_graph_init_from_yaml(LMFlowGraph*, const char* yaml);
/* 同上,从文件读取(读文件失败亦返回错误并置 lmflow_last_error)。 */
LMFlowStatus lmflow_graph_init_from_yaml_file(LMFlowGraph*, const char* path);
LMFlowStatus lmflow_graph_start(LMFlowGraph*);
void lmflow_graph_free(LMFlowGraph*); /* 内部先 cancel + wait,再释放 */

/* ---------- 图输入 ----------
 * 句柄式为热路径推荐用法(免去每包按名字查表);句柄生命周期随 graph,无需释放。
 * ---- 背压策略(重要)----
 * **只有图输入口是限流点;图内部的边不对生产者施加背压。**
 *   - 图输入口:有界队列。满时 lmflow_input_send 阻塞至有空位或图终止;
 *     lmflow_input_try_send 立即返回 LMFLOW_ERR_WOULD_BLOCK。
 *   - 内部边(节点→节点):不阻塞生产者,只在超过软水位时告警/计数
 *     (可用 lmflow_graph_queue_depth 观测)。
 *
 * 为什么内部边不设硬上界:否则「扇出后再汇合」的合法 DAG 会死锁 ——
 *      A ─┬─► B(慢) ─┐
 *         └─► C(快) ─┴─► D
 *   C 迅速填满 D 的输入队列而阻塞,D 却要等 B 那一路才能消费,B 又等 A 推进,
 *   A 已阻塞在 C 上 —— 循环等待,且不需要环形拓扑就会发生。
 *   生产者永不阻塞在内部边上,循环等待即无从形成;
 *   内存总量则由图输入口的限流 + DAG 有界的扇出倍数间接约束。 */
LMFlowInput* lmflow_graph_input(LMFlowGraph*, const char* port);
LMFlowStatus lmflow_input_send(LMFlowInput*, LMFlowPacket pkt);
LMFlowStatus lmflow_input_try_send(LMFlowInput*, LMFlowPacket pkt);
void lmflow_input_close(LMFlowInput*);
/* 归还输入句柄。句柄由调用方拥有,持有一份对引擎的引用 —— 即使先 lmflow_graph_free
 * 了图,句柄仍安全(之后再用只会返回「图已结束」错误,不会 use-after-free)。
 * 不释放会泄漏引擎(句柄的引用一直撑着)。可传 NULL。 */
void lmflow_input_free(LMFlowInput*);
/* 便捷式(内部查表),等价于 lmflow_graph_input + lmflow_input_send */
LMFlowStatus lmflow_graph_add_packet(LMFlowGraph*, const char* port, LMFlowPacket pkt);
LMFlowStatus lmflow_graph_close_input(LMFlowGraph*, const char* port);
void lmflow_graph_close_all_inputs(LMFlowGraph*);

/* ---------- 图输出:poller(拉)或 observer(推)---------- */
LMFlowPoller* lmflow_graph_add_poller(LMFlowGraph*, const char* port);
/* 变体:observe_timestamp_bounds=true 时,除数据包外还会收到「时间戳边界推进」
 * 产生的**空包**(payload==NULL,仅带时间戳)—— 下游据此知道「该时刻之前不会再有数据」。
 * 默认 false。完整的边界传播属 A 阶段,本版本仅保留接口。 */
LMFlowPoller* lmflow_graph_add_poller_ex(LMFlowGraph*, const char* port, bool observe_timestamp_bounds);
/* 阻塞取下一包;图结束且队空返回 false。取得的包须 lmflow_packet_drop。 */
bool lmflow_poller_next(LMFlowPoller*, LMFlowPacket* out);
/* 非阻塞:无数据返回 false(用 lmflow_last_error 区分「暂无」与「已结束」)*/
bool lmflow_poller_try_next(LMFlowPoller*, LMFlowPacket* out);
/* 带超时:LMFLOW_OK / LMFLOW_ERR_TIMEOUT / LMFLOW_ERR_CLOSED */
LMFlowStatus lmflow_poller_next_timeout(LMFlowPoller*, LMFlowPacket* out, int64_t timeout_ms);
/* 归还 poller 句柄。与 lmflow_input_free 同理:调用方拥有,持一份对引擎的引用,
 * 图 free 后仍安全;不释放会泄漏引擎。可传 NULL。 */
void lmflow_poller_free(LMFlowPoller*);
/* 同一端口上可以同时挂多个 poller 与多个 observer,各自**独立收到一份**
 * (引擎按订阅者数递增引用计数,不复制 payload)。注册后不支持移除。
 *
 * 推模式:回调在工作线程执行,pkt 为**借用**(回调返回后失效,需要留存请自行深拷贝)。
 * 回调内不得调用 lmflow_graph_* 生命周期函数,否则死锁。 */
LMFlowStatus lmflow_graph_observe(LMFlowGraph*, const char* port, void (*cb)(void* user, LMFlowPacket pkt),
                              void* user);
LMFlowStatus lmflow_graph_observe_ex(LMFlowGraph*, const char* port, bool observe_timestamp_bounds,
                                 void (*cb)(void* user, LMFlowPacket pkt), void* user);

/* ---------- 终止 ----------
 * cancel 的确切语义(**不是抢占**):停止调度新任务、丢弃在途包、唤醒所有等待者;
 * 已经在执行中的算子回调**不会被中断**,会自然跑完。因此 cancel 返回后可能仍有
 * 一个算子在跑,须 wait_done 才能确认全部静止。之后 wait_done 返回 LMFLOW_ERR_CANCELLED。 */
void lmflow_graph_cancel(LMFlowGraph*);

/* 等待图跑完(所有输入已关闭且排空)。返回首个错误,或 OK。 */
LMFlowStatus lmflow_graph_wait_done(LMFlowGraph*);
/* 带超时版本。算子逻辑有误(该产出未产出)会让图静止而非结束,
 * 无超时的等待就是永久挂起且无从诊断 —— 生产代码建议一律用本函数。
 * 返回 LMFLOW_OK / LMFLOW_ERR_TIMEOUT / 首个错误。 */
LMFlowStatus lmflow_graph_wait_done_timeout(LMFlowGraph*, int64_t timeout_ms);

/* 等待「当前在途的包都处理完」,但**不结束图** —— 输入口仍开着,可继续送包。
 * 适用于「送一批 → 等这批处理完 → 再送下一批」的批处理模式。
 * 注意:idle 只表示此刻无待处理任务;若别的线程仍在送包,返回后可能立刻又变忙。 */
LMFlowStatus lmflow_graph_wait_until_idle(LMFlowGraph*);
LMFlowStatus lmflow_graph_wait_until_idle_timeout(LMFlowGraph*, int64_t timeout_ms);

/* 暂停 / 恢复调度(调试、限速用)。暂停期间已在执行的算子不受影响;
 * 送包仍会入队,只是不被调度。cancel 优先于 pause。 */
void lmflow_graph_pause(LMFlowGraph*);
void lmflow_graph_resume(LMFlowGraph*);

/* 图级错误描述。注意 lmflow_last_error() 是**线程局部**的:算子在引擎工作线程上
 * 失败时,其错误文本不会出现在宿主线程的 lmflow_last_error() 里。要拿到那条信息,
 * 用本函数(返回值生命周期至下次对同一 graph 调用本函数)。 */
const char* lmflow_graph_last_error(LMFlowGraph*);

/* ---------- 全局水位:内部边无界的兜底 ----------
 * 设计上内部边不对生产者背压(否则「扇出后汇合」的 DAG 会死锁,见设计文档 §7.5),
 * 代价是内部边可以无限增长:图输入口 100 帧 × 扇出 6 条边 × 每帧 6MB ≈ 3.6GB,
 * 而且若某条分支偏慢,单条边就能堆到内存耗尽 —— 没有任何机制会拦它。
 *
 * 因此给整张图一个总预算(YAML 顶层):
 *     max_queued_packets: 500          # 全图在途包数上限(0 = 不限)
 *     max_queued_bytes:   1073741824   # 或按字节(0 = 不限)
 * 超限时**把压力转化成图输入口的背压**:lmflow_input_send 阻塞、try_send 返回
 * LMFLOW_ERR_WOULD_BLOCK。只在图输入口刹车是安全的 —— 它在图内没有上游,
 * 不可能参与循环等待,所以这个兜底不会把 diamond 死锁带回来。
 *
 * ⚠ 按字节统计只对**引擎分配的内建 payload** 有效(引擎知道大小);
 *   自定义 payload 引擎只有指针与 drop_fn、无从得知尺寸,计为 0 ——
 *   那种场景请用 max_queued_packets。 */
size_t lmflow_graph_total_queued(LMFlowGraph*);         /* 全图在途包数 */
uint64_t lmflow_graph_total_queued_bytes(LMFlowGraph*); /* 仅可计量部分之和 */

/* ---------- 内省 / 排障 ---------- */
/* 当前图状态(状态机见设计文档 §6.3)。调试与 UI 用。 */
typedef enum {
  LMFLOW_STATE_CREATED = 0,
  LMFLOW_STATE_INITIALIZED = 1,
  LMFLOW_STATE_RUNNING = 2,
  LMFLOW_STATE_DRAINING = 3,  /* 输入已关,仍在排空 */
  LMFLOW_STATE_TERMINATED = 4,
} LMFlowGraphState;
LMFlowGraphState lmflow_graph_state(LMFlowGraph*);

/* 拓扑枚举(供可视化 / 调试器 / 通用工具使用;名字生命周期随 graph)。 */
size_t lmflow_graph_num_input_ports(LMFlowGraph*);
const char* lmflow_graph_input_port_name(LMFlowGraph*, size_t idx);
size_t lmflow_graph_num_output_ports(LMFlowGraph*);
const char* lmflow_graph_output_port_name(LMFlowGraph*, size_t idx);
size_t lmflow_graph_num_nodes(LMFlowGraph*);
const char* lmflow_graph_node_name(LMFlowGraph*, size_t idx);

/* 已注册算子清单(全局)。「kernel 未注册」报错时可据此列出可用名字。 */
size_t lmflow_registered_kernel_count(void);
const char* lmflow_registered_kernel_name(size_t idx);

/* ---------- 节点级统计:让「卡死」可定位 ----------
 * 若某算子内部阻塞(网络调用、死循环、等锁),它会占住一个 executor 线程;
 * 若干个就把线程池掏空,整图静止。此时 wait_done_timeout 只能告诉你「超时了」,
 * **不能告诉你是哪个节点** —— 生产环境里就是一个毫无线索的挂起。
 *
 * 引擎在进入算子回调时记时间戳、退出时清除(开销可忽略),于是:
 *   LMFlowNodeStats st = { .struct_size = sizeof(st) };
 *   lmflow_graph_node_stats(g, i, &st);
 *   → st.running == true && st.running_for_us == 42'300'000  即「detect 卡了 42 秒」
 *
 * 另可在 YAML 顶层配 watchdog:
 *     watchdog_ms: 5000    # 单次回调超过此时长即打一条 WARN 日志(0 = 关闭)
 *
 * ⚠ 引擎**无法中断**卡住的算子(与 cancel 同理,没有抢占)。这一层能做的只有
 *   「让你看见」;真正的修复是算子自身要有超时逻辑。
 *
 * struct_size 约定:调用方填 sizeof(LMFlowNodeStats),引擎只写它认得且装得下的字段。
 * 这里用 struct_size 而非固定预留字段(与 LMFlowBuffer 的做法不同)—— 因为统计项
 * 天然会持续增加,而 LMFlowBuffer 在热路径上、形状稳定,两者取舍不同。 */
typedef struct {
  uint32_t struct_size; /* 入参:sizeof(LMFlowNodeStats) */
  uint32_t reserved0;
  const char* node_name;   /* 生命周期随 graph */
  const char* kernel_name;
  bool running;            /* 当前是否正在执行算子回调 */
  int64_t running_for_us;  /* running 时:本次已执行多久;否则 0 */
  uint64_t processed;      /* 累计 process 调用次数 */
  uint64_t errors;         /* 累计失败次数 */
  int64_t total_process_us; /* 累计耗时,可算均值 */
  int64_t max_process_us;   /* 最慢的一次 */
  size_t queued;            /* 该节点所有输入边的积压总数 */
} LMFlowNodeStats;

bool lmflow_graph_node_stats(LMFlowGraph*, size_t node_idx, LMFlowNodeStats* out);

/* 算子自报计数器的读取(见 lmflow_ctx_counter_add);不存在返回 0 */
int64_t lmflow_graph_counter_value(LMFlowGraph*, const char* name);
size_t lmflow_graph_counter_count(LMFlowGraph*);
const char* lmflow_graph_counter_name(LMFlowGraph*, size_t idx);
/* 拓扑与状态的可读快照(节点、边、队列深度)。
 * 返回值存放于**线程局部**缓冲,生命周期至本线程下次调用本函数 ——
 * 故多线程同时调用不会互相踩踏。 */
const char* lmflow_graph_dump(LMFlowGraph*);
/* 指定边的当前积压包数;端口不存在返回 LMFLOW_INVALID_ID。 */
size_t lmflow_graph_queue_depth(LMFlowGraph*, const char* port);

/* 该边累计**被丢弃**的包数(仅 fixed_size 输入策略会丢包,见下)。
 * 丢包绝不静默:除本计数外,首次丢弃还会打一条 WARN 日志。 */
uint64_t lmflow_graph_dropped_count(LMFlowGraph*, const char* port);

/* ---------- 输入策略(节点级可配,在 YAML 里声明)----------
 * 「多个输入口如何凑成一次 Process」+「队列满了怎么办」被抽成**可插拔策略**,
 * 而不是写死在引擎里 —— 这样实时丢帧与(A 阶段的)时间戳对齐共用同一个扩展点。
 *
 *   nodes:
 *     - name: "detect"
 *       kernel: "Detector"
 *       input_ports: ["frames"]
 *       input_policy: { type: "fixed_size", capacity: 2 }
 *
 * type 取值:
 *   "sync"       默认。所有输入口齐备才触发。
 *                B 阶段 = 每口至少一个包;A 阶段 = 按时间戳对齐。
 *   "immediate"  各输入口独立触发,不等其它口。适合无需对齐的旁路处理。
 *   "fixed_size" 有界 + **满则丢弃最旧的包**,容量由 capacity 指定(默认 1)。
 *                实时场景必备:摄像头 30fps 而算子只跑 10fps 时,
 *                无界队列会让内存无限增长,丢旧帧才是正确取舍。
 *
 * 注意 fixed_size 是**有意的有损**策略,且不会阻塞上游 ——
 * 因此它与「内部边不背压」(见设计文档 §7.5)并不冲突,而是其配套的内存约束手段。 */

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* LMFLOW_H_ */
